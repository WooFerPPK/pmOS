// src/sw.ts
var FALLBACK_CACHE_VERSION = "pmos-v0";
var MANIFEST_URL = "/manifest.json";
async function loadManifest() {
  try {
    const resp = await fetch(MANIFEST_URL, { cache: "no-store" });
    if (!resp.ok) return null;
    const json = await resp.json();
    if (typeof json.version !== "number" || !Array.isArray(json.assets)) {
      return null;
    }
    return json;
  } catch {
    return null;
  }
}
function cacheNameFor(manifest) {
  if (manifest === null) return FALLBACK_CACHE_VERSION;
  return `pmos-v${manifest.version}`;
}
self.addEventListener("install", (event) => {
  event.waitUntil(
    (async () => {
      const manifest = await loadManifest();
      const cacheName = cacheNameFor(manifest);
      const cache = await caches.open(cacheName);
      if (manifest !== null && manifest.assets.length > 0) {
        await Promise.all(
          manifest.assets.map(async (asset) => {
            const url = asset.startsWith("/") ? asset : `/${asset}`;
            try {
              const resp = await fetch(url, { cache: "no-store" });
              if (resp.ok) {
                await cache.put(url, resp);
              }
            } catch {
            }
          })
        );
      }
      await self.skipWaiting();
    })()
  );
});
self.addEventListener("activate", (event) => {
  event.waitUntil(
    (async () => {
      const manifest = await loadManifest();
      const currentCache = cacheNameFor(manifest);
      const names = await caches.keys();
      await Promise.all(
        names.filter((n) => n.startsWith("pmos-") && n !== currentCache).map((n) => caches.delete(n))
      );
      await self.clients.claim();
    })()
  );
});
self.addEventListener("fetch", (event) => {
  event.respondWith(
    (async () => {
      const cached = await caches.match(event.request);
      if (cached) return cached;
      return fetch(event.request);
    })()
  );
});
