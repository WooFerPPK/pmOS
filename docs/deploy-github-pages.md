# Deploying PMos on GitHub Pages

How to host PMos on GitHub Pages while still satisfying the cross-origin-isolation headers the kernel's syscall transport requires.

## Overview

PMos only boots when the browser reports `window.crossOriginIsolated
=== true`. That flag flips on exactly when the page is served with
both `Cross-Origin-Opener-Policy: same-origin` and
`Cross-Origin-Embedder-Policy: require-corp`, which in turn is what
unlocks `SharedArrayBuffer` and `Atomics.wait` — the two primitives
the syscall ring buffer between the kernel Worker and each app Worker
is built on. Vanilla GitHub Pages cannot attach custom response
headers to its output, so by itself it is not a viable host. The
workaround is to front GitHub Pages with a Cloudflare Worker that
refetches every response and **pre-pends** (sets/overrides) the two
headers before returning it to the browser. See
[`../.specify/memory/constitution.md`](../.specify/memory/constitution.md)
Principle IV (Offline-First And Persistent) for the upstream
constraint — without cross-origin isolation the OS cannot boot at
all, let alone persist.

## Prerequisites

- A GitHub repository with GitHub Pages already enabled (free tier is
  fine).
- A Cloudflare account (free tier is fine).
- A domain whose authoritative DNS is on Cloudflare. This can be an
  apex (`example.com`) or a sub-domain (`pmos.example.com`); the
  recipe is identical.
- PMos build artefacts in `dist/` — i.e. you have run `just build`
  and have `dist/index.html`, `dist/assets/`, and `dist/.nojekyll`
  sitting on disk.

## Deploy steps

1. **Publish `dist/` on GitHub Pages.** The simplest route is a
   dedicated `gh-pages` branch at the repo root:

   ```shell
   $ cd dist
   $ git init -b gh-pages
   $ touch .nojekyll            # disable Jekyll; PMos ships _headers etc.
   $ git add -A
   $ git commit -m "pmos dist"
   $ git remote add origin git@github.com:<user>/<repo>.git
   $ git push -f origin gh-pages
   ```

   Then, in the repo's **Settings → Pages**, set the source to the
   `gh-pages` branch, `/` (root). Alternatively, you can configure
   Pages to serve from `/docs` on the default branch; the rest of
   this recipe does not care which layout you chose.

2. **Point DNS at GitHub Pages.** In the Cloudflare dashboard, open
   the zone for your domain and add a CNAME:

   ```text
   Type:    CNAME
   Name:    @        (apex)  OR  pmos     (sub-domain)
   Target:  <user>.github.io
   Proxy:   Proxied (orange cloud) — REQUIRED for the Worker to run
   ```

   The orange-cloud proxy is what routes the request through
   Cloudflare's network; without it the Worker will never see the
   traffic.

3. **Create the Worker.** In the Cloudflare dashboard, open
   **Workers & Pages → Create → Worker**. Paste the script from
   [§ The Cloudflare Worker](#the-cloudflare-worker) below, save,
   and deploy. If you prefer `wrangler`, create `wrangler.toml`
   with a recent `compatibility_date` (see troubleshooting) and
   `wrangler deploy`.

4. **Add a route.** Back in the zone view, open **Workers Routes
   → Add route**:

   ```text
   Route:   yourdomain.com/*
   Worker:  pmos-coop-coep   (whatever you named it)
   ```

   Every request to your domain now passes through the Worker,
   which refetches from GitHub Pages and rewrites the headers.

5. **Smoke-test.** Load `https://yourdomain.com/` in a browser,
   open DevTools, and in the Console run:

   ```js
   console.log(window.crossOriginIsolated)  // must print: true
   ```

   If you see `true`, PMos will boot. If you see `false`, jump to
   [§ Troubleshooting](#troubleshooting).

## The Cloudflare Worker

Save as the Worker's `worker.js`. Uses the modern ES-module handler
(`export default { fetch }`); the deprecated
`addEventListener("fetch", ...)` form is not used because it is
incompatible with several recent runtime features and is scheduled
for removal.

```js
export default {
  async fetch(request, env, ctx) {
    const upstream = await fetch(request);

    // Clone the upstream headers so we inherit Content-Type,
    // Cache-Control, ETag, and everything else GH Pages set.
    const headers = new Headers(upstream.headers);

    // PMos constitution Principle IV: cross-origin isolation is
    // required for SharedArrayBuffer / Atomics.wait in the kernel.
    headers.set("Cross-Origin-Opener-Policy", "same-origin");
    headers.set("Cross-Origin-Embedder-Policy", "require-corp");

    return new Response(upstream.body, {
      status: upstream.status,
      statusText: upstream.statusText,
      headers,
    });
  },
};
```

Notes:

- We **do not** touch `Cache-Control`, so GitHub Pages' own caching
  directives survive and Cloudflare's edge cache behaves as usual.
- `upstream.body` is passed through as a stream — no buffering,
  so large assets (WASM binaries in `dist/assets/bin/`) are not
  held in Worker memory.
- `headers.set` overwrites any upstream value; GH Pages never sets
  COOP/COEP so there is nothing to preserve, but the `set` call is
  the correct semantic for "force this value regardless of what
  upstream sent."

## Verifying the deploy

```shell
$ curl -sI https://example.com/ | grep -E -i 'cross-origin-(opener|embedder)-policy'
cross-origin-opener-policy: same-origin
cross-origin-embedder-policy: require-corp
```

(Replace `example.com` with your actual domain.) Both lines must
appear exactly once. Then, in the browser DevTools console on the
deployed page:

```js
console.log(window.crossOriginIsolated)  // true
```

If both checks pass, the deploy is good and the PMos bootstrap will
find a usable runtime.

## Troubleshooting

### `crossOriginIsolated` is false

Almost always caused by a sub-resource (image, script, font, iframe)
served from a third-party origin without an opt-in header. Under
`require-corp`, every cross-origin resource must itself send either
`Cross-Origin-Resource-Policy: cross-origin` or a matching CORS
policy. Ad networks, analytics snippets, and CDN-hosted fonts are
the usual culprits.

Fix:

1. Open DevTools **Network** tab, filter by "blocked", and identify
   the offending origins.
2. If the resource is one you control, add CORP on its response.
3. If the resource is proxied through your domain, extend the Worker
   to set `Cross-Origin-Resource-Policy: cross-origin` on its
   response too.
4. If the resource is third-party and uncontrollable, remove it.
   PMos itself loads nothing from outside the deployment origin by
   design, so the fault is always in customisations you added.

### GH Pages returns 404 on deep links

GH Pages requires an `index.html` at the root of the served branch,
and — because PMos ships non-Jekyll assets — a `.nojekyll` file must
also be present or GH Pages will strip files whose names begin with
an underscore.

Fix:

1. Confirm both `index.html` and `.nojekyll` exist at the root of
   the `gh-pages` branch: `git ls-tree gh-pages | grep -E
   '^.*(index\.html|\.nojekyll)$'`.
2. If `.nojekyll` is missing, the PMos build pipeline should be
   emitting it during `xtask assemble-dist`; re-run `just build`
   and re-push.
3. If `index.html` is missing, the build is broken or you pushed the
   wrong directory; re-check that you pushed `dist/`, not the repo
   root.

### Worker errors or times out

Cloudflare Workers are versioned by a `compatibility_date`. The
modern ES-module handler syntax and a number of `Headers` /
`Response` behaviours require a reasonably recent date. An old
date will produce a deployment error, a runtime 1101, or silent
fall-through to origin.

Fix:

1. Open `wrangler.toml` (or the Worker's dashboard settings →
   Settings → Variables → Compatibility date) and set a date no
   earlier than `2023-10-01`:

   ```toml
   # wrangler.toml
   name = "pmos-coop-coep"
   main = "worker.js"
   compatibility_date = "2023-10-01"
   ```

2. Redeploy. If the errors persist, tail the Worker logs
   (`wrangler tail` or the dashboard's **Logs** tab) — a thrown
   exception in `fetch` will surface there, most commonly an
   `AbortError` from a cancelled upstream request.
