// PMos service worker.
//
// Precaches every OS asset on install so subsequent loads can boot
// offline indefinitely (FR-016, Principle IV). Cache-first fetch
// strategy with a versioned cache name — on upgrade, `install`
// populates a fresh cache and `activate` deletes old caches.
//
// Asset list comes from `/manifest.json` (written by `xtask
// assemble-dist`): every relative path under `assets[]` is fetched
// and cached. The manifest's `version` field is folded into the
// cache name so a `dist/` rebuild with new assets evicts the
// previous cache automatically.
//
// On install:
//   1. Fetch `/manifest.json`. If the manifest is unreachable
//      (network down on first visit), skipWaiting + activate with
//      an empty cache so the page still loads — the cache-first
//      handler falls through to the network for any asset that
//      isn't precached.
//   2. Open `pmos-v<manifest.version>` and `addAll` the asset
//      paths.
//   3. `skipWaiting()` so the new SW activates without a
//      page refresh.
//
// On activate:
//   * Delete every `pmos-*` cache that doesn't match the current
//     `CACHE_VERSION`.
//   * `clients.claim()` so the active SW immediately controls the
//     page.
//
// On fetch:
//   * `caches.match(request)` first — every precached asset
//     resolves from cache without a network round-trip.
//   * On miss, fall through to the network. Failed fetches return
//     the network error verbatim; user-visible offline-error UI is
//     surfaced by the bootstrap, not the SW.

/// <reference lib="webworker" />
declare const self: ServiceWorkerGlobalScope;

/** Default cache name when the manifest is unreachable. */
const FALLBACK_CACHE_VERSION = "pmos-v0";

/** Manifest path. Mirrors xtask assemble-dist's output location. */
const MANIFEST_URL = "/manifest.json";

interface Manifest {
  readonly version: number;
  readonly assets: readonly string[];
}

async function loadManifest(): Promise<Manifest | null> {
  try {
    const resp = await fetch(MANIFEST_URL, { cache: "no-store" });
    if (!resp.ok) return null;
    const json = (await resp.json()) as Manifest;
    if (
      typeof json.version !== "number" ||
      !Array.isArray(json.assets)
    ) {
      return null;
    }
    return json;
  } catch {
    return null;
  }
}

/**
 * Build the cache name from a manifest. `pmos-v<n>` where `n` is
 * the manifest version. Folds the version into the name so a
 * `dist/` rebuild with new assets gets a fresh cache.
 */
function cacheNameFor(manifest: Manifest | null): string {
  if (manifest === null) return FALLBACK_CACHE_VERSION;
  return `pmos-v${manifest.version}`;
}

self.addEventListener("install", (event: ExtendableEvent) => {
  event.waitUntil(
    (async () => {
      const manifest = await loadManifest();
      const cacheName = cacheNameFor(manifest);
      const cache = await caches.open(cacheName);
      if (manifest !== null && manifest.assets.length > 0) {
        // Precache every asset. `addAll` rejects on the first
        // failure; we tolerate per-asset misses by using a
        // best-effort loop instead so a single 404 doesn't block
        // the whole install.
        await Promise.all(
          manifest.assets.map(async (asset) => {
            const url = asset.startsWith("/") ? asset : `/${asset}`;
            try {
              const resp = await fetch(url, { cache: "no-store" });
              if (resp.ok) {
                await cache.put(url, resp);
              }
            } catch {
              // Per-asset miss; the fetch handler will fall through
              // to the network at use time.
            }
          }),
        );
      }
      await self.skipWaiting();
    })(),
  );
});

self.addEventListener("activate", (event: ExtendableEvent) => {
  event.waitUntil(
    (async () => {
      const manifest = await loadManifest();
      const currentCache = cacheNameFor(manifest);
      const names = await caches.keys();
      await Promise.all(
        names
          .filter((n) => n.startsWith("pmos-") && n !== currentCache)
          .map((n) => caches.delete(n)),
      );
      await self.clients.claim();
    })(),
  );
});

self.addEventListener("fetch", (event: FetchEvent) => {
  event.respondWith(
    (async () => {
      const cached = await caches.match(event.request);
      if (cached) return cached;
      return fetch(event.request);
    })(),
  );
});

export {};
