// PMos service worker.
//
// Precaches every OS asset on install so that subsequent loads can
// boot offline indefinitely (FR-016, Principle IV). Cache-first
// fetch strategy with a versioned cache name — on upgrade, `install`
// populates a fresh cache and `activate` deletes old caches.
//
// This file is the Phase 1 skeleton. T087 populates the precache
// list from dist/manifest.json (written by xtask assemble-dist) and
// wires the full install/activate/fetch handlers.

/// <reference lib="webworker" />
declare const self: ServiceWorkerGlobalScope;

const CACHE_VERSION = "pmos-v0";

self.addEventListener("install", (event: ExtendableEvent) => {
  // T087 will read dist/manifest.json and precache every asset.
  // For the Phase 1 skeleton, we just open the cache so install
  // completes; nothing is precached yet.
  event.waitUntil(
    (async () => {
      await caches.open(CACHE_VERSION);
      await self.skipWaiting();
    })(),
  );
});

self.addEventListener("activate", (event: ExtendableEvent) => {
  event.waitUntil(
    (async () => {
      const names = await caches.keys();
      await Promise.all(
        names
          .filter((n) => n.startsWith("pmos-") && n !== CACHE_VERSION)
          .map((n) => caches.delete(n)),
      );
      await self.clients.claim();
    })(),
  );
});

self.addEventListener("fetch", (event: FetchEvent) => {
  // T087 will implement cache-first: try cache, fall back to
  // network, on network failure return a friendly offline error.
  // Phase 1 skeleton is pass-through.
  event.respondWith(
    (async () => {
      const cached = await caches.match(event.request);
      if (cached) return cached;
      return fetch(event.request);
    })(),
  );
});

export {};
