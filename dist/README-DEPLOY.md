# PMos build — how to run it

This directory is a self-contained static deployment of PMos.
The kernel, display server, desktop shell, and every bundled
binary are present as `.wasm` files; opening `index.html` in a
COOP/COEP-isolated browser tab boots the kernel, spawns init,
and renders the desktop wallpaper.

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

## URL routes

The default URL boots straight to the desktop. Use a URL hash
to select a different boot path:

* `/` (no hash) → spawns init-desktop, which spawns the full
  display-server + shell binaries; the desktop renders a
  wallpaper and a taskbar. `#boot-to-desktop` is an explicit
  alias for the same path.
* `/#real-kernel` → legacy demo flow (4-pid tree:
  init + hello-std + display-server + display-client-demo × 2;
  ends with `display-server fb blit ok` + `init exiting`).
* `/#input-echo` → boots `/bin/hello_input_echo` (no kernel
  scheduler activity beyond a single user Worker; types echoed
  to console).
* `/#mock-kernel` → keeps the legacy preview boot screen with
  capability checks instead of running the kernel.

## What you should see

For `/#boot-to-desktop`:

```
init-desktop starting
init-desktop spawned display-server pid=3
init-desktop spawned shell pid=4
init-desktop entering supervision loop
display-server starting
shell: starting
shell: connected to /run/display
display-server served client 0
```

After the served-client line lands, the desktop's wallpaper +
taskbar paints to the framebuffer canvas. Cold-load is ~400 ms
in headless Chromium.

For the default real-kernel boot:

```
init starting
init spawned hello-std pid=3
init spawned display-server pid=4
init spawned display-client-demo pid=5
…
display-server served client 0
display-server served client 1
init reaped child pid=…
init sent SIGTERM to display-server pid=4
display-server fb blit ok
init exiting
```

## Troubleshooting

If the boot screen shows **FAIL** for any environment row,
your headers are wrong. Open devtools → Network → click any
asset → Headers tab; every response must include
`Cross-Origin-Opener-Policy: same-origin` and
`Cross-Origin-Embedder-Policy: require-corp`. Without them
`SharedArrayBuffer` won't construct and the kernel can't run.

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
