// PMos service worker.
//
// A cache generation is usable only after every asset in the emitted
// manifest has been fetched successfully. The ready marker is written last;
// activation and fetches ignore caches without that marker. This keeps a
// failed update from replacing the last-known-good offline image.

/// <reference lib="webworker" />
declare const self: ServiceWorkerGlobalScope;

const CACHE_PREFIX = "pmos-r";
const READY_MARKER_PATH = ".pmos-cache-ready";
const GENERATIONS_TO_KEEP = 2;
const SHA256_PATTERN = /^[0-9a-f]{64}$/;

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
] as const;

interface Manifest {
  readonly version: number;
  readonly release: string;
  readonly assets: readonly string[];
  readonly deployment: readonly string[];
  readonly integrity: Readonly<Record<string, string>>;
}

interface LoadedManifest {
  readonly manifest: Manifest;
  readonly response: Response;
}

interface ReadyGeneration {
  readonly cacheName: string;
  readonly release: string;
  readonly sequence: number;
}

let readyGenerations: readonly ReadyGeneration[] | null = null;

function scopeUrl(path: string): string {
  return new URL(path, self.registration.scope).href;
}

function normalizeAssetPath(path: string): string {
  const relative = path.replace(/^\/+/, "");
  if (relative.length === 0) {
    throw new Error("manifest contains an empty asset path");
  }

  const scope = new URL(self.registration.scope);
  const asset = new URL(relative, scope);
  if (asset.origin !== scope.origin || !asset.pathname.startsWith(scope.pathname)) {
    throw new Error(`manifest asset escapes the service-worker scope: ${path}`);
  }
  return relative;
}

function parseManifest(value: unknown): Manifest {
  if (typeof value !== "object" || value === null) {
    throw new Error("manifest is not an object");
  }

  const candidate = value as {
    version?: unknown;
    release?: unknown;
    assets?: unknown;
    deployment?: unknown;
    integrity?: unknown;
  };
  if (!Number.isSafeInteger(candidate.version) || (candidate.version as number) <= 0) {
    throw new Error("manifest version must be a positive integer");
  }
  if (typeof candidate.release !== "string" || !SHA256_PATTERN.test(candidate.release)) {
    throw new Error("manifest release must be a lowercase SHA-256 digest");
  }
  if (!Array.isArray(candidate.assets)) {
    throw new Error("manifest assets must be an array");
  }
  if (!Array.isArray(candidate.deployment)) {
    throw new Error("manifest deployment paths must be an array");
  }

  const assets = candidate.assets.map((asset) => {
    if (typeof asset !== "string") {
      throw new Error("manifest asset paths must be strings");
    }
    return normalizeAssetPath(asset);
  });
  const assetSet = new Set(assets);
  if (assetSet.size !== assets.length) {
    throw new Error("manifest asset paths must be unique");
  }
  for (let index = 1; index < assets.length; index += 1) {
    if ((assets[index - 1] as string) >= (assets[index] as string)) {
      throw new Error("manifest asset paths must be sorted");
    }
  }
  for (const critical of CRITICAL_ASSETS) {
    if (!assetSet.has(critical)) {
      throw new Error(`manifest is missing critical asset: ${critical}`);
    }
  }

  const deployment = candidate.deployment.map((path) => {
    if (typeof path !== "string") {
      throw new Error("manifest deployment paths must be strings");
    }
    return normalizeAssetPath(path);
  });
  const deploymentSet = new Set(deployment);
  if (deploymentSet.size !== deployment.length) {
    throw new Error("manifest deployment paths must be unique");
  }
  for (let index = 1; index < deployment.length; index += 1) {
    if ((deployment[index - 1] as string) >= (deployment[index] as string)) {
      throw new Error("manifest deployment paths must be sorted");
    }
  }
  const allPaths = [...assets, ...deployment].sort();
  if (new Set(allPaths).size !== allPaths.length) {
    throw new Error("manifest asset and deployment paths must be disjoint");
  }

  if (
    typeof candidate.integrity !== "object" ||
    candidate.integrity === null ||
    Array.isArray(candidate.integrity)
  ) {
    throw new Error("manifest integrity must be an object");
  }
  const rawIntegrity = candidate.integrity as Record<string, unknown>;
  if (Object.keys(rawIntegrity).length !== allPaths.length) {
    throw new Error("manifest integrity must cover exactly the declared files");
  }
  const integrity: Record<string, string> = Object.create(null) as Record<string, string>;
  for (const path of allPaths) {
    if (!Object.prototype.hasOwnProperty.call(rawIntegrity, path)) {
      throw new Error(`manifest integrity is missing file: ${path}`);
    }
    const digest = rawIntegrity[path];
    if (typeof digest !== "string" || !SHA256_PATTERN.test(digest)) {
      throw new Error(`manifest integrity for ${path} is not a lowercase SHA-256 digest`);
    }
    integrity[path] = digest;
  }

  return {
    version: candidate.version as number,
    release: candidate.release,
    assets,
    deployment,
    integrity,
  };
}

function cacheNameFor(manifest: Manifest): string {
  return `${CACHE_PREFIX}${manifest.release}`;
}

function hexDigest(buffer: ArrayBuffer): string {
  return [...new Uint8Array(buffer)]
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
}

async function sha256(bytes: ArrayBuffer): Promise<string> {
  return hexDigest(await crypto.subtle.digest("SHA-256", bytes));
}

async function validateManifestRelease(manifest: Manifest): Promise<void> {
  const canonical = [...manifest.assets, ...manifest.deployment]
    .sort()
    .map((path) => `${path}\0${manifest.integrity[path] as string}\n`)
    .join("");
  const actual = await sha256(new TextEncoder().encode(canonical).buffer);
  if (actual !== manifest.release) {
    throw new Error("manifest release does not match its asset inventory");
  }
}

async function loadManifest(): Promise<LoadedManifest> {
  const response = await fetch(scopeUrl("manifest.json"), { cache: "no-store" });
  if (!response.ok) {
    throw new Error(`manifest fetch failed with status ${response.status}`);
  }
  const manifest = parseManifest(await response.clone().json());
  await validateManifestRelease(manifest);
  return { manifest, response };
}

function readyMarkerUrl(): string {
  return scopeUrl(READY_MARKER_PATH);
}

async function installGeneration(): Promise<void> {
  const { manifest, response: manifestResponse } = await loadManifest();
  const cacheName = cacheNameFor(manifest);
  const existing = await readReadyGeneration(cacheName);
  if (existing !== null) {
    await self.skipWaiting();
    return;
  }

  const priorGenerations = await findReadyGenerations();
  const sequence =
    priorGenerations.reduce(
      (highest, generation) => Math.max(highest, generation.sequence),
      0,
    ) + 1;

  // A content-derived name is never shared by different valid releases. Clear
  // any unready residue left by an interrupted attempt before rebuilding it.
  await caches.delete(cacheName);
  const cache = await caches.open(cacheName);
  try {
    await cache.put(scopeUrl("manifest.json"), manifestResponse);
    // Verify and publish one asset at a time. Keeping every fetched body in a
    // Promise.all result retained the whole OS image in memory and saturated
    // browser fetch/hash work during the user's first desktop interaction.
    // The generation remains atomic because fetch routing ignores it until
    // the ready marker below exists; a failure deletes all partial entries.
    for (const asset of manifest.assets) {
      const url = scopeUrl(asset);
      const response = await fetch(url, { cache: "no-store" });
      if (!response.ok) {
        throw new Error(`asset fetch failed for ${asset} with status ${response.status}`);
      }
      const bytes = await response.arrayBuffer();
      const actual = await sha256(bytes);
      if (actual !== manifest.integrity[asset]) {
        throw new Error(`asset integrity mismatch: ${asset}`);
      }
      await cache.put(
        url,
        new Response(bytes, {
          status: response.status,
          statusText: response.statusText,
          headers: response.headers,
        }),
      );
    }
    await cache.put(
      readyMarkerUrl(),
      new Response(JSON.stringify({ cacheName, release: manifest.release, sequence }), {
        headers: { "content-type": "application/json" },
      }),
    );
    readyGenerations = null;
    await self.skipWaiting();
  } catch (error) {
    await caches.delete(cacheName);
    throw error;
  }
}

async function readReadyGeneration(cacheName: string): Promise<ReadyGeneration | null> {
  if (!cacheName.startsWith(CACHE_PREFIX)) {
    return null;
  }
  try {
    const cache = await caches.open(cacheName);
    const marker = await cache.match(readyMarkerUrl());
    if (marker === undefined) {
      return null;
    }
    const value = (await marker.json()) as {
      cacheName?: unknown;
      release?: unknown;
      sequence?: unknown;
    };
    if (
      value.cacheName !== cacheName ||
      typeof value.release !== "string" ||
      !SHA256_PATTERN.test(value.release) ||
      cacheName !== `${CACHE_PREFIX}${value.release}` ||
      !Number.isSafeInteger(value.sequence) ||
      (value.sequence as number) <= 0
    ) {
      return null;
    }
    return {
      cacheName,
      release: value.release,
      sequence: value.sequence as number,
    };
  } catch {
    return null;
  }
}

async function findReadyGenerations(): Promise<readonly ReadyGeneration[]> {
  const names = await caches.keys();
  const generations = (
    await Promise.all(names.map((name) => readReadyGeneration(name)))
  ).filter((generation): generation is ReadyGeneration => generation !== null);
  generations.sort((a, b) => b.sequence - a.sequence);
  return generations;
}

async function getReadyGenerations(): Promise<readonly ReadyGeneration[]> {
  if (readyGenerations === null) {
    readyGenerations = await findReadyGenerations();
  }
  return readyGenerations;
}

async function activateGeneration(): Promise<void> {
  const generations = await findReadyGenerations();
  readyGenerations = generations;

  // Retain the active generation and one verified predecessor for rollback.
  // Delete only caches whose ready marker proves PMos created them. In
  // particular, an old unverified cache is never allowed to trigger
  // deletion of the last-known-good cache during an offline activation.
  const stale = generations.slice(GENERATIONS_TO_KEEP);
  await Promise.all(stale.map(({ cacheName }) => caches.delete(cacheName)));
  if (stale.length > 0) {
    const staleNames = new Set(stale.map(({ cacheName }) => cacheName));
    readyGenerations = generations.filter(({ cacheName }) => !staleNames.has(cacheName));
  }
  await self.clients.claim();
}

async function matchReadyCache(request: Request): Promise<Response | undefined> {
  for (const { cacheName } of await getReadyGenerations()) {
    const cache = await caches.open(cacheName);
    const response = await cache.match(request, { ignoreSearch: true });
    if (response !== undefined) {
      return response;
    }
  }
  return undefined;
}

async function serve(request: Request): Promise<Response> {
  if (request.method !== "GET") {
    return fetch(request);
  }

  const cached = await matchReadyCache(request);
  if (cached !== undefined) {
    return cached;
  }

  try {
    return await fetch(request);
  } catch (error) {
    if (request.mode === "navigate") {
      const fallback = await matchReadyCache(new Request(scopeUrl("index.html")));
      if (fallback !== undefined) {
        return fallback;
      }
    }
    throw error;
  }
}

self.addEventListener("install", (event: ExtendableEvent) => {
  event.waitUntil(installGeneration());
});

self.addEventListener("activate", (event: ExtendableEvent) => {
  event.waitUntil(activateGeneration());
});

self.addEventListener("fetch", (event: FetchEvent) => {
  event.respondWith(serve(event.request));
});

export {};
