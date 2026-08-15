import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const SCOPE = "https://example.test/pmos/";
const READY_MARKER = `${SCOPE}.pmos-cache-ready`;
const CRITICAL_ASSETS = [
  "index.html",
  "sw.js",
  "assets/bootstrap.js",
  "assets/kernel-worker.js",
  "assets/user-worker.js",
  "assets/kernel.wasm",
  "assets/bin/init-desktop.wasm",
  "assets/bin/display-server.wasm",
  "assets/bin/shell.wasm",
];
const OLD_RELEASES = ["1".repeat(64), "2".repeat(64), "3".repeat(64)] as const;

type RequestKey = string | URL | Request;

function requestUrl(request: RequestKey): string {
  return request instanceof Request ? request.url : new URL(String(request), SCOPE).href;
}

class FakeCache {
  readonly entries = new Map<string, Response>();

  async addAll(requests: readonly RequestKey[]): Promise<void> {
    const pending: Array<[string, Response]> = [];
    for (const request of requests) {
      const response = await fetch(request);
      if (!response.ok) {
        throw new TypeError(`cache add failed: ${response.status}`);
      }
      pending.push([requestUrl(request), response.clone()]);
    }
    for (const [url, response] of pending) {
      this.entries.set(url, response);
    }
  }

  async put(request: RequestKey, response: Response): Promise<void> {
    this.entries.set(requestUrl(request), response.clone());
  }

  async match(
    request: RequestKey,
    options?: { ignoreSearch?: boolean },
  ): Promise<Response | undefined> {
    const requested = new URL(requestUrl(request));
    if (options?.ignoreSearch) requested.search = "";
    for (const [url, response] of this.entries) {
      const candidate = new URL(url);
      if (options?.ignoreSearch) candidate.search = "";
      if (candidate.href === requested.href) return response.clone();
    }
    return undefined;
  }
}

class FakeCacheStorage {
  readonly buckets = new Map<string, FakeCache>();
  readonly deleted: string[] = [];

  async open(name: string): Promise<FakeCache> {
    let cache = this.buckets.get(name);
    if (cache === undefined) {
      cache = new FakeCache();
      this.buckets.set(name, cache);
    }
    return cache;
  }

  async keys(): Promise<string[]> {
    return [...this.buckets.keys()];
  }

  async delete(name: string): Promise<boolean> {
    this.deleted.push(name);
    return this.buckets.delete(name);
  }

  async seedReady(release: string, sequence: number): Promise<string> {
    const name = `pmos-r${release}`;
    const cache = await this.open(name);
    await cache.put(
      READY_MARKER,
      new Response(JSON.stringify({ cacheName: name, release, sequence })),
    );
    return name;
  }
}

type WaitUntilEvent = { waitUntil(promise: Promise<unknown>): void };
type FetchLikeEvent = {
  readonly request: Request;
  respondWith(response: Promise<Response>): void;
};
type Listener = (event: WaitUntilEvent & FetchLikeEvent) => void;

function hexDigest(buffer: ArrayBuffer): string {
  return [...new Uint8Array(buffer)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}

async function sha256Text(value: string): Promise<string> {
  return hexDigest(
    await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value)),
  );
}

async function manifestFor(
  assetPaths: readonly string[],
  bodyFor: (asset: string) => string = (asset) => `asset:${SCOPE}${asset}`,
  deploymentPaths: readonly string[] = [],
): Promise<{
  readonly version: number;
  readonly release: string;
  readonly assets: readonly string[];
  readonly deployment: readonly string[];
  readonly integrity: Readonly<Record<string, string>>;
}> {
  const assets = [...assetPaths].sort();
  const deployment = [...deploymentPaths].sort();
  const files = [...assets, ...deployment].sort();
  const integrity = Object.fromEntries(
    await Promise.all(
      files.map(async (asset) => [asset, await sha256Text(bodyFor(asset))] as const),
    ),
  );
  const canonical = files.map((asset) => `${asset}\0${integrity[asset] as string}\n`).join("");
  return {
    version: 40,
    release: await sha256Text(canonical),
    assets,
    deployment,
    integrity,
  };
}

describe("PMos service worker", () => {
  let storage: FakeCacheStorage;
  let listeners: Map<string, Listener>;
  let skipWaiting = vi.fn((): Promise<void> => Promise.resolve());
  let claim = vi.fn((): Promise<void> => Promise.resolve());

  beforeEach(async () => {
    vi.resetModules();
    storage = new FakeCacheStorage();
    listeners = new Map();
    skipWaiting = vi.fn(() => Promise.resolve());
    claim = vi.fn(() => Promise.resolve());
    vi.stubGlobal("caches", storage);
    vi.stubGlobal("self", {
      registration: { scope: SCOPE },
      addEventListener: (type: string, listener: Listener) => {
        listeners.set(type, listener);
      },
      skipWaiting,
      clients: { claim },
    });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  async function loadWorker(): Promise<void> {
    await import("../../src/sw");
  }

  function dispatchLifetime(type: "install" | "activate"): Promise<unknown> {
    let pending: Promise<unknown> | undefined;
    listeners.get(type)?.({
      waitUntil(promise) {
        pending = promise;
      },
    } as WaitUntilEvent & FetchLikeEvent);
    if (pending === undefined) throw new Error(`missing ${type} listener`);
    return pending;
  }

  it("atomically installs every manifest asset using scope-relative URLs", async () => {
    const manifest = await manifestFor(
      [...CRITICAL_ASSETS, "assets/theme.bin"],
      undefined,
      ["_headers"],
    );
    const fetchMock = vi.fn(async (request: RequestKey) => {
      const url = requestUrl(request);
      return url.endsWith("manifest.json")
        ? new Response(JSON.stringify(manifest))
        : new Response(`asset:${url}`);
    });
    vi.stubGlobal("fetch", fetchMock);

    await loadWorker();
    await dispatchLifetime("install");

    const cache = storage.buckets.get(`pmos-r${manifest.release}`);
    expect(cache).toBeDefined();
    expect(cache?.entries.has(READY_MARKER)).toBe(true);
    expect(cache?.entries.has(`${SCOPE}index.html`)).toBe(true);
    expect(cache?.entries.has(`${SCOPE}assets/theme.bin`)).toBe(true);
    expect(cache?.entries.has(`${SCOPE}_headers`)).toBe(false);
    expect(fetchMock.mock.calls.every(([request]) => requestUrl(request).startsWith(SCOPE))).toBe(true);
    expect(fetchMock.mock.calls.some(([request]) => requestUrl(request).endsWith("_headers"))).toBe(
      false,
    );
    expect(skipWaiting).toHaveBeenCalledOnce();
  });

  it("bounds install fetch concurrency to one verified asset body", async () => {
    const manifest = await manifestFor([...CRITICAL_ASSETS, "assets/extra-a.bin", "assets/extra-b.bin"]);
    let activeAssetFetches = 0;
    let peakAssetFetches = 0;
    vi.stubGlobal(
      "fetch",
      vi.fn(async (request: RequestKey) => {
        const url = requestUrl(request);
        if (url.endsWith("manifest.json")) {
          return new Response(JSON.stringify(manifest));
        }
        activeAssetFetches += 1;
        peakAssetFetches = Math.max(peakAssetFetches, activeAssetFetches);
        await Promise.resolve();
        activeAssetFetches -= 1;
        return new Response(`asset:${url}`);
      }),
    );

    await loadWorker();
    await dispatchLifetime("install");

    expect(peakAssetFetches).toBe(1);
    expect(
      storage.buckets.get(`pmos-r${manifest.release}`)?.entries.has(READY_MARKER),
    ).toBe(true);
  });

  it("does not mark or activate a generation when one critical fetch fails", async () => {
    const oldName = await storage.seedReady(OLD_RELEASES[0], 1);
    const manifest = await manifestFor(CRITICAL_ASSETS, () => "ok");
    vi.stubGlobal("fetch", vi.fn(async (request: RequestKey) => {
      const url = requestUrl(request);
      if (url.endsWith("manifest.json")) return new Response(JSON.stringify(manifest));
      if (url.endsWith("assets/kernel.wasm")) return new Response("bad", { status: 503 });
      return new Response("ok");
    }));

    await loadWorker();
    await expect(dispatchLifetime("install")).rejects.toThrow(
      "asset fetch failed for assets/kernel.wasm with status 503",
    );

    expect(storage.buckets.has(`pmos-r${manifest.release}`)).toBe(false);
    expect(storage.buckets.get(oldName)?.entries.has(READY_MARKER)).toBe(true);
    expect(skipWaiting).not.toHaveBeenCalled();
  });

  it("rejects an offline install without creating a fallback generation", async () => {
    const oldName = await storage.seedReady(OLD_RELEASES[0], 1);
    vi.stubGlobal("fetch", vi.fn(() => Promise.reject(new TypeError("offline"))));

    await loadWorker();
    await expect(dispatchLifetime("install")).rejects.toThrow("offline");

    expect(await storage.keys()).toEqual([oldName]);
    expect(skipWaiting).not.toHaveBeenCalled();
  });

  it("keeps the active and previous verified caches and never deletes an unverified cache", async () => {
    const oldest = await storage.seedReady(OLD_RELEASES[0], 1);
    const previous = await storage.seedReady(OLD_RELEASES[1], 2);
    const active = await storage.seedReady(OLD_RELEASES[2], 3);
    await storage.open("pmos-v0");
    const fetchMock = vi.fn(() => Promise.reject(new TypeError("offline")));
    vi.stubGlobal("fetch", fetchMock);

    await loadWorker();
    await dispatchLifetime("activate");

    expect(storage.deleted).toEqual([oldest]);
    expect(await storage.keys()).toEqual([previous, active, "pmos-v0"]);
    expect(fetchMock).not.toHaveBeenCalled();
    expect(claim).toHaveBeenCalledOnce();
  });

  it("rejects bytes that do not match the manifest integrity map", async () => {
    const oldName = await storage.seedReady(OLD_RELEASES[0], 1);
    const manifest = await manifestFor(CRITICAL_ASSETS, () => "expected");
    vi.stubGlobal("fetch", vi.fn(async (request: RequestKey) => {
      const url = requestUrl(request);
      if (url.endsWith("manifest.json")) return new Response(JSON.stringify(manifest));
      return new Response("tampered");
    }));

    await loadWorker();
    await expect(dispatchLifetime("install")).rejects.toThrow("asset integrity mismatch");

    expect(storage.buckets.has(`pmos-r${manifest.release}`)).toBe(false);
    expect(storage.buckets.get(oldName)?.entries.has(READY_MARKER)).toBe(true);
  });
});
