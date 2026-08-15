// src/sw.ts
var CACHE_PREFIX = "pmos-r";
var READY_MARKER_PATH = ".pmos-cache-ready";
var GENERATIONS_TO_KEEP = 2;
var SHA256_PATTERN = /^[0-9a-f]{64}$/;
var CRITICAL_ASSETS = [
  "index.html",
  "sw.js",
  "assets/bootstrap.js",
  "assets/kernel-worker.js",
  "assets/user-worker.js",
  "assets/kernel.wasm",
  "assets/bin/init-desktop.wasm",
  "assets/bin/display-server.wasm",
  "assets/bin/shell.wasm"
];
var readyGenerations = null;
function scopeUrl(path) {
  return new URL(path, self.registration.scope).href;
}
function normalizeAssetPath(path) {
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
function parseManifest(value) {
  if (typeof value !== "object" || value === null) {
    throw new Error("manifest is not an object");
  }
  const candidate = value;
  if (!Number.isSafeInteger(candidate.version) || candidate.version <= 0) {
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
    if (assets[index - 1] >= assets[index]) {
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
    if (deployment[index - 1] >= deployment[index]) {
      throw new Error("manifest deployment paths must be sorted");
    }
  }
  const allPaths = [...assets, ...deployment].sort();
  if (new Set(allPaths).size !== allPaths.length) {
    throw new Error("manifest asset and deployment paths must be disjoint");
  }
  if (typeof candidate.integrity !== "object" || candidate.integrity === null || Array.isArray(candidate.integrity)) {
    throw new Error("manifest integrity must be an object");
  }
  const rawIntegrity = candidate.integrity;
  if (Object.keys(rawIntegrity).length !== allPaths.length) {
    throw new Error("manifest integrity must cover exactly the declared files");
  }
  const integrity = /* @__PURE__ */ Object.create(null);
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
    version: candidate.version,
    release: candidate.release,
    assets,
    deployment,
    integrity
  };
}
function cacheNameFor(manifest) {
  return `${CACHE_PREFIX}${manifest.release}`;
}
function hexDigest(buffer) {
  return [...new Uint8Array(buffer)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}
async function sha256(bytes) {
  return hexDigest(await crypto.subtle.digest("SHA-256", bytes));
}
async function validateManifestRelease(manifest) {
  const canonical = [...manifest.assets, ...manifest.deployment].sort().map((path) => `${path}\0${manifest.integrity[path]}
`).join("");
  const actual = await sha256(new TextEncoder().encode(canonical).buffer);
  if (actual !== manifest.release) {
    throw new Error("manifest release does not match its asset inventory");
  }
}
async function loadManifest() {
  const response = await fetch(scopeUrl("manifest.json"), { cache: "no-store" });
  if (!response.ok) {
    throw new Error(`manifest fetch failed with status ${response.status}`);
  }
  const manifest = parseManifest(await response.clone().json());
  await validateManifestRelease(manifest);
  return { manifest, response };
}
function readyMarkerUrl() {
  return scopeUrl(READY_MARKER_PATH);
}
async function installGeneration() {
  const { manifest, response: manifestResponse } = await loadManifest();
  const cacheName = cacheNameFor(manifest);
  const existing = await readReadyGeneration(cacheName);
  if (existing !== null) {
    await self.skipWaiting();
    return;
  }
  const priorGenerations = await findReadyGenerations();
  const sequence = priorGenerations.reduce(
    (highest, generation) => Math.max(highest, generation.sequence),
    0
  ) + 1;
  await caches.delete(cacheName);
  const cache = await caches.open(cacheName);
  try {
    await cache.put(scopeUrl("manifest.json"), manifestResponse);
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
          headers: response.headers
        })
      );
    }
    await cache.put(
      readyMarkerUrl(),
      new Response(JSON.stringify({ cacheName, release: manifest.release, sequence }), {
        headers: { "content-type": "application/json" }
      })
    );
    readyGenerations = null;
    await self.skipWaiting();
  } catch (error) {
    await caches.delete(cacheName);
    throw error;
  }
}
async function readReadyGeneration(cacheName) {
  if (!cacheName.startsWith(CACHE_PREFIX)) {
    return null;
  }
  try {
    const cache = await caches.open(cacheName);
    const marker = await cache.match(readyMarkerUrl());
    if (marker === void 0) {
      return null;
    }
    const value = await marker.json();
    if (value.cacheName !== cacheName || typeof value.release !== "string" || !SHA256_PATTERN.test(value.release) || cacheName !== `${CACHE_PREFIX}${value.release}` || !Number.isSafeInteger(value.sequence) || value.sequence <= 0) {
      return null;
    }
    return {
      cacheName,
      release: value.release,
      sequence: value.sequence
    };
  } catch {
    return null;
  }
}
async function findReadyGenerations() {
  const names = await caches.keys();
  const generations = (await Promise.all(names.map((name) => readReadyGeneration(name)))).filter((generation) => generation !== null);
  generations.sort((a, b) => b.sequence - a.sequence);
  return generations;
}
async function getReadyGenerations() {
  if (readyGenerations === null) {
    readyGenerations = await findReadyGenerations();
  }
  return readyGenerations;
}
async function activateGeneration() {
  const generations = await findReadyGenerations();
  readyGenerations = generations;
  const stale = generations.slice(GENERATIONS_TO_KEEP);
  await Promise.all(stale.map(({ cacheName }) => caches.delete(cacheName)));
  if (stale.length > 0) {
    const staleNames = new Set(stale.map(({ cacheName }) => cacheName));
    readyGenerations = generations.filter(({ cacheName }) => !staleNames.has(cacheName));
  }
  await self.clients.claim();
}
async function matchReadyCache(request) {
  for (const { cacheName } of await getReadyGenerations()) {
    const cache = await caches.open(cacheName);
    const response = await cache.match(request, { ignoreSearch: true });
    if (response !== void 0) {
      return response;
    }
  }
  return void 0;
}
async function serve(request) {
  if (request.method !== "GET") {
    return fetch(request);
  }
  const cached = await matchReadyCache(request);
  if (cached !== void 0) {
    return cached;
  }
  try {
    return await fetch(request);
  } catch (error) {
    if (request.mode === "navigate") {
      const fallback = await matchReadyCache(new Request(scopeUrl("index.html")));
      if (fallback !== void 0) {
        return fallback;
      }
    }
    throw error;
  }
}
self.addEventListener("install", (event) => {
  event.waitUntil(installGeneration());
});
self.addEventListener("activate", (event) => {
  event.waitUntil(activateGeneration());
});
self.addEventListener("fetch", (event) => {
  event.respondWith(serve(event.request));
});
