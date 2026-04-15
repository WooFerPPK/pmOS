// src/sw.ts
var CACHE_VERSION = "pmos-v0";
self.addEventListener("install", (event) => {
  event.waitUntil(
    (async () => {
      await caches.open(CACHE_VERSION);
      await self.skipWaiting();
    })()
  );
});
self.addEventListener("activate", (event) => {
  event.waitUntil(
    (async () => {
      const names = await caches.keys();
      await Promise.all(
        names.filter((n) => n.startsWith("pmos-") && n !== CACHE_VERSION).map((n) => caches.delete(n))
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
