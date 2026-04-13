# PMos demo build — how to run it

This directory is a self-contained static deployment of the PMos
boot-screen demo. It shows the PMos boot sequence in a browser,
verifies every environment capability the real kernel will need,
and stalls at "kernel loading…" because the kernel WASM is not
yet compiled.

**The entire bundle is static files.** There is no server
component, no backend, no database, no account system. This is
the v1 architectural promise — deploying the real OS later is
the same as deploying this demo: drop `dist/` onto a static host
that supports COOP/COEP headers, open it, done.

## Required headers

PMos needs **cross-origin isolation** to enable
`SharedArrayBuffer`. Two HTTP response headers must be set on
every asset:

```
Cross-Origin-Opener-Policy: same-origin
Cross-Origin-Embedder-Policy: require-corp
```

Without them you will see a "Cross-origin isolation (COOP/COEP)"
line turn **FAIL** in the boot screen, and the real kernel
would be unable to boot at all.

A `_headers` file is included; Cloudflare Pages, Netlify, and
similar hosts read it automatically.

## Option 1 — The bundled Python server (zero deps)

The simplest way to see it in a browser right now:

```
python3 ../serve-demo.py --port 8080
```

then open http://localhost:8080/ in Chromium, Firefox, or Safari.

To expose it on your LAN (e.g. to test from a phone):

```
python3 ../serve-demo.py --host 0.0.0.0 --port 8080
```

## Option 2 — nginx

```nginx
server {
    listen 443 ssl http2;
    server_name pmos.example.com;

    ssl_certificate     /etc/letsencrypt/live/pmos.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/pmos.example.com/privkey.pem;

    root /var/www/pmos;  # point at this dist/ directory
    index index.html;

    add_header Cross-Origin-Opener-Policy   "same-origin" always;
    add_header Cross-Origin-Embedder-Policy "require-corp" always;
    add_header Cross-Origin-Resource-Policy "same-origin" always;

    location ~ \.wasm$ {
        default_type application/wasm;
    }
}
```

## Option 3 — Caddy

```
pmos.example.com {
    root * /var/www/pmos
    file_server
    header Cross-Origin-Opener-Policy "same-origin"
    header Cross-Origin-Embedder-Policy "require-corp"
    header Cross-Origin-Resource-Policy "same-origin"
    @wasm path *.wasm
    header @wasm Content-Type application/wasm
}
```

## Option 4 — Cloudflare Pages / Netlify

Drop `dist/` into the project (or push it to a branch that the
host is watching). The included `_headers` file takes care of
COOP/COEP automatically.

## Option 5 — GitHub Pages

GitHub Pages does not support custom headers directly. Put a
Cloudflare Worker in front of your Pages URL that injects the
two headers on every response. The `docs/deploy-github-pages.md`
write-up (T209 polish task in `tasks.md`) covers this.

## What you should see

A dark blue terminal-style boot screen with:

```
PMos 0.1.0-demo
browser-hosted operating system — demo build

[  OK  ] Cross-origin isolation (COOP/COEP)    crossOriginIsolated === true
[  OK  ] SharedArrayBuffer                     typeof === 'function'
[  OK  ] Atomics.wait                          available
[  OK  ] Origin Private Filesystem (OPFS)      navigator.storage.getDirectory ok
[  OK  ] Service worker                        navigator.serviceWorker present
[  OK  ] OffscreenCanvas                       transfer-to-worker supported
[ WAIT ] Kernel WASM load (/assets/kernel.wasm) HTTP 404 — not yet built
[  --  ] Display server                        not wired in the demo
[  --  ] Desktop shell                         not wired in the demo
```

If every row above "Kernel WASM load" is OK, your deployment is
correct and the next build (real Rust kernel + display server +
toolkit + shell) will take over automatically. If any of those
rows is FAIL, your headers are wrong and you should fix them
before adding more moving parts.

## What's NOT in this demo

* No actual kernel. The WASM binary at `/assets/kernel.wasm`
  does not exist yet — the "Kernel WASM load" line stalls
  deliberately to prove the real fetch path works.
* No display server, window toolkit, desktop shell, or any
  bundled applications. Those are Phase 2+ of the real build
  (T098+ in `tasks.md`).
* No OPFS filesystem persistence. The demo only *checks* that
  OPFS is available; it does not write or read anything.
* No service worker precache. The `sw.js` is a skeleton that
  will be filled in by T087 once the manifest lists real
  assets to cache.

## Next steps to see the real kernel

1. Install Rust with `rustup` and add the
   `wasm32-unknown-unknown` target.
2. Run `just build` from the repo root (requires `just`, `node`,
   `cargo`, and the `wasm32-unknown-unknown` target).
3. `dist/assets/kernel.wasm` will be produced.
4. Reload the page. The "Kernel WASM load" line will turn OK
   and the boot sequence will continue into the real kernel
   init. (Real kernel wire-up to `bootstrap.ts` lands in
   T085 of `tasks.md`.)

## Reporting problems

If the boot screen shows **FAIL** for any row that should be
**OK** in your browser:

1. Open the browser devtools console. `bootstrap.js` logs every
   check it runs and every fetch it attempts, prefixed with
   `[pmos-bootstrap]`. Copy those lines.
2. Check the Network tab: every response should include the
   three Cross-Origin-* headers.
3. File an issue in the project repo with the console log +
   the response-header screenshot.

## Source

This demo is built from the PMos source tree at
`specs/001-browser-os-v1/`. See `spec.md`, `plan.md`, and
`tasks.md` for the full architecture and build plan, and
`.specify/memory/constitution.md` for the ten non-negotiable
principles this project is organised around.
