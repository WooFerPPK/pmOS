# External-developer quickstart validation

**Date**: 2026-08-08
**Scope**: SC-012 / T221 authoring, build, package, and raw display-protocol
workflows
**Result**: PASS for the scoped clean-snapshot gate described below

## Provenance and isolation

The validation source was frozen only after the toolkit-free implementation and
documentation owner explicitly declared it stable. A source-only copy was then
created at `/tmp/pmos-quickstart-final.OR7pAj` from the current working tree.
The copy excluded `.git/`, `.worktrees/`, `target/`, `build/`, `dist/`, every
`node_modules/`, `graphify-out/`, Playwright/Vitest results, local Codex state,
and local certificates.

- Base Git commit: `4f9c8404a4d97c9c8e1855bcdb77cfe9b260a54a`.
- Frozen snapshot: 547 files, 14,570,208 bytes.
- Snapshot content digest:
  `75773cb7b6809c6a230ae4d31b1cfa4b45563ceab936f5791dbce5c8e3cdd3fa`.
- Initial excluded-directory check: no `target`, `build`, `dist`,
  `node_modules`, `graphify-out`, `test-results`, or `playwright-report`
  directory existed in the snapshot.

The base commit is not the content identifier because the release candidate was
an intentionally dirty working tree; the snapshot digest above identifies the
validated bytes. Docker and Podman were unavailable, so this was source-tree
isolation on Debian 12 rather than a new OS container. The Cargo registry and
already-downloaded Playwright browser payloads were host prerequisites, while
all compile outputs and installed project dependencies began absent. `npm ci`
used a new cache at `/tmp/pmos-quickstart-state.JnFwPu/npm-cache`.

## Environment

- Git 2.39.5.
- GCC/`cc` 12.2.0.
- Rust/Cargo 1.94.1 stable.
- Installed Rust targets: `x86_64-unknown-linux-gnu`,
  `wasm32-unknown-unknown`, and `wasm32-wasip1`.
- just 1.49.0.
- Node.js 22.23.2 and npm 10.9.8. Node 22 satisfies the documented 20-or-newer
  floor.
- GNU tar 1.34.

## Exact command and result record

1. `cd web && npm ci`

   PASS. Installed 54 packages, audited 55 packages, and reported 0 known
   vulnerabilities.

2. `npx playwright install --with-deps chromium firefox webkit`

   PASS. The exact documented command completed successfully. Debian reported
   every required native browser library already installed: 0 upgraded and 0
   newly installed.

3. `just build`

   PASS from an empty build tree. It built the kernel for
   `wasm32-unknown-unknown`, all selected userland (including
   `toolkit-free-client`) for `wasm32-wasip1`, all four JavaScript entry points,
   and a 68-entry `dist/manifest.json`.

4. Focused isolation/static gates:

   - `just test-kernel`: PASS, 933 tests.
   - `just test-display-server`: PASS, 219 tests.
   - `just test-toolkit`: PASS, 240 tests.
   - `just test-build-tools`: PASS, 10 tests.
   - `just test-typescript`: PASS, strict TypeScript emitted no errors.
   - `just test-drivers`: PASS, 32 Vitest files / 692 tests. Its required
     incremental rebuild again assembled 68 manifest entries.

5. Toolkit-free native and compile-time gates:

   - `cargo test --locked -p toolkit-free-client`: PASS, 14 request/codec tests
     plus 6 production-session/real-server conformance tests.
   - `cargo check --locked --target wasm32-wasip1 -p toolkit-free-client`:
     PASS.
   - `cargo tree --locked -p toolkit-free-client --edges normal`: PASS. The
     complete runtime dependency tree contained only `display-proto`; it did
     not contain `toolkit` or `display-server`.

6. Local static-server smoke, using the coordination-only port substitution:

   ```text
   cargo run --locked -p xtask -- dev-server --dir=dist --port=8084
   ```

   PASS. `GET /index.html` and `GET /assets/kernel.wasm` both returned 200.
   Responses carried `Cross-Origin-Opener-Policy: same-origin`,
   `Cross-Origin-Embedder-Policy: require-corp`,
   `Cross-Origin-Resource-Policy: same-origin`, and `Cache-Control: no-store`.
   The WASM response used `Content-Type: application/wasm`. Port 8084 was
   explicitly stopped and verified free; port 8081 was never used by this lane.

7. Production toolkit-free browser gate:

   ```text
   npx --no-install playwright test \
     tests/integration/toolkit-free-client.spec.ts \
     --project=chromium --project=firefox
   ```

   The snapshot-only Playwright port constant was changed from 8081 to 8084 so
   the shared release server remained untouched. PASS, 2/2: Chromium completed
   in 3.0 seconds and Firefox in 5.4 seconds (10.3 seconds total). Each test
   launched the real `/bin/toolkit-free-client` in its own Worker, observed its
   mapped 320x200 frame, routed a real keyboard event, observed the repaint,
   closed through the display protocol, and required the Worker to exit.

8. Toolkit app walkthrough:

   - `cargo new --bin crates/hello-app`: PASS. Cargo automatically added exactly
     one workspace member, matching the quickstart's conditional instruction.
   - The complete `sample-app/src/main.rs` transport scaffold was copied, its
     cfg-gated allocator/module/main retained, and the documented cfg-gated
     imports and `run_window` substituted.
   - The documented `Cargo.toml` and `pkg.toml` were used.
   - First intentional lock update:
     `cargo build --release --target wasm32-wasip1 -p hello-app`: PASS.
   - Repeatability check:
     `cargo clippy --locked -p hello-app --all-targets -- -D warnings`: PASS.
   - `cargo build --locked --release --target wasm32-wasip1 -p hello-app`:
     PASS after the lock update.
   - `cargo run --locked -p xtask -- package hello-app`: PASS. It wrote
     `dist/pkgs/hello-0.1.0.pmpkg.tar` (70,144 bytes) containing exactly
     `manifest.toml` and `bin/hello.wasm` (67,350 bytes).
   - The packaged payload SHA-256 was
     `b852fa32b5ed9a4c32ecacfe4555aa686cfab9022556c33f7f4952ca9068a3cf`,
     exactly matching the generated manifest integrity entry.

9. `just --show clean`, followed by `just clean` after evidence capture

   PASS. The recipe inspection exposed one documentation mismatch: `just clean`
   also runs `cargo clean` and removes `web/node_modules` and web test reports,
   not only `build/` and `dist/`. The quickstart comment was corrected. After
   every result above had been captured, the actual recipe removed 3,846 Cargo
   files (1.3 GiB) plus `build/`, `dist/`, installed web dependencies, and web
   test reports. A final check found none of those generated paths remaining.

## Deviations and coverage boundary

- The only intentional command substitution was port 8084 for local/browser
  serving. This changed no product behavior and kept the shared 8081 release
  lane untouched.
- During the walkthrough, the validator first transcribed the app snippet while
  accidentally omitting the four cfg annotations already present in the frozen
  quickstart. Strict host Clippy correctly rejected that transcription as dead
  code. Comparing against the frozen document exposed the operator error; the
  exact documented form then passed Clippy, locked WASI build, packaging, and
  integrity verification. This was not counted as a documentation defect.
- This lane ran the focused static/isolation commands above rather than invoking
  the monolithic `just test`. The final release lane owns the complete workspace,
  dependency-audit, performance, and full Playwright gates. Likewise, the custom
  tutorial archive was built and validated here; its guest import/install/launch
  behavior is covered by the separate `third-party-install.spec.ts` release gate.
- The clean-source snapshot is equivalent for stale-artifact detection but is
  not represented as a literal fresh Ubuntu/Alpine container. This limitation
  is explicit rather than hidden behind T221's historical wording.

## SC-012 / T221 disposition

The public instructions now independently produce a toolkit application WASI
binary and a validated package from no prior project outputs. The direct
protocol route has both real-server isolation evidence and a separately isolated
Chromium/Firefox production workflow with no toolkit runtime dependency. This
closes the original U4 documentation/executable-evidence gap under the scoped
clean-snapshot method. Final release qualification still combines this record
with the canonical full-suite and third-party-install gates; this record does
not silently substitute for those separate release checks.

## Constitution Check

- **I — PASS**: validation exercised real kernel/userland binaries and package
  artifacts; it introduced no simulation path.
- **II — PASS**: the raw client reached the display server through
  kernel-mediated `/run/display`; no direct server call entered production.
- **III — PASS**: the generated product remained a 68-entry static directory;
  no backend, account, or telemetry dependency was added.
- **IV — PASS**: documentation-only remediation changed no persistence or
  offline runtime behavior.
- **V — PASS**: the browser gate required the raw client to appear and disappear
  as a separately counted Worker.
- **VI — PASS**: the examples use WASI plus the documented `display_connect`
  PMos extension.
- **VII — PASS**: native and Chromium/Firefox evidence proves that the wire
  protocol works without `toolkit` linked.
- **VIII — PASS**: native/server isolation and WASI compile checks ran before
  the production browser test.
- **IX — PASS (no performance impact)**: the only repository change from this
  validation is documentation. The 3.0/5.4-second browser test durations are
  workflow durations, not cold-load or input-latency measurements, and are not
  misreported as performance-budget figures.
- **X — PASS**: kernel, display-server, toolkit, build-tool, TypeScript/driver,
  raw-client isolation, and focused full-stack tests all passed.

---

## Exact-source Ubuntu 24.04 validation

**Date**: 2026-08-11
**Scope**: definitive T221/T241 prerequisite, build, complete canonical gate,
tutorial-package, toolkit-free, integrity, and clean validation
**Result**: PASS; completed at `2026-08-11T09:53:57Z`

This record supplements rather than rewrites the Rust 1.94.1 source-isolated
2026-08-08 record above. The executable inputs were the exact release-candidate
archive at `/tmp/pmos-source-freeze-final.jAM9XkYd/pmos-source.tar`, SHA-256
`f65d3376abb62da21ad61932544ba97bd9b464c7a016e53c6806696808c2f9e7`,
and its 567-entry `source-files.sha256`, SHA-256
`3569e63c88850318730de432625ad5e903de99ab0b953d9fedced52af03f2335`.
The archive audit found 750 members: 183 directories and 567 regular files.

### Ubuntu isolation and environment

The runner used an Ubuntu 24.04.4 LTS disposable full copy in private
mount/PID/IPC/UTS namespaces, with a PID-1 chroot and source-only read-only
binds. Network remained shared for prerequisite/dependency retrieval, and the
evidence output was intentionally writable. This is retained release-state
isolation, not a VM-grade adversarial or supply-chain sandbox. The prepared
root digest was identical before and after the run:
`92616086db6d777ad208df67ff4ec2668888322fb1824e1f8d227fac39c51825`.

- Rust/rustc 1.97.1 (`8bab26f4f`), Cargo 1.97.1, LLVM 22.1.6.
- `wasm32-unknown-unknown`, `wasm32-wasip1`, and native targets installed.
- Node.js 22.23.2, npm 10.9.8, GNU tar 1.35.
- just 1.58.0 and Playwright 1.59.1.
- Host-provided Linux kernel 7.0.14-11-pve, x86-64.

### Exact step record

Every recorded step exited zero:

| Step | Started UTC | Finished UTC |
| --- | --- | --- |
| apt prerequisites | 2026-08-11T09:36:36Z | 2026-08-11T09:36:55Z |
| install just | 2026-08-11T09:36:55Z | 2026-08-11T09:37:46Z |
| `npm ci` | 2026-08-11T09:37:46Z | 2026-08-11T09:37:48Z |
| documented Playwright install | 2026-08-11T09:37:48Z | 2026-08-11T09:38:52Z |
| `just build` | 2026-08-11T09:38:54Z | 2026-08-11T09:39:10Z |
| canonical `just test` | 2026-08-11T09:39:11Z | 2026-08-11T09:53:28Z |
| tutorial third-party browser gate | 2026-08-11T09:53:29Z | 2026-08-11T09:53:46Z |
| toolkit-free browser gate | 2026-08-11T09:53:47Z | 2026-08-11T09:53:56Z |

The build produced the canonical 68-entry static distribution. Its release ID
was `aef00ef852f00af26a712cfd86a78dbbbd59411839cdaf9158794e5d61660e59`;
the retained canonical `dist/manifest.json` has SHA-256
`1835b98518772d0b8f5477f0746b7b385e84939c6e9c1ad7cc6607787afe98ee`.
The dev-server smoke verified the static index and kernel WASM plus the required
COOP/COEP/CORP, no-store, and WASM content-type headers.

The canonical gate passed 34 Vitest files / 767 tests and all 63 Playwright
tests, including Chromium and Firefox session restore, persistence,
windowing/close, toolkit-free, third-party install, cold/warm/input/typical
desktop performance, and the unsupported-substrate WebKit stop screen. The
idle lane passed its 21 unit tests and completed 4 browser runs, 8 baselines,
12 comparisons, and 20 measurements; every incremental value was within 2% of
one core. The native 1,000-iteration input sink reported 2 us p95. Exact Ubuntu
browser measurements remain in the canonical log and are intentionally not
substituted for the local release table in `perf-results.md`.

The documented tutorial built, packaged, and integrity-checked
`hello-0.1.0.pmpkg.tar`, SHA-256
`1036116f6052e076e47d5eac2bcdb787e751c3c26b844923d759a702a5dd6e61`.
The real third-party workflow passed 2/2 in Chromium/Firefox. Native
toolkit-free tests passed 14 client tests and 9 conformance tests; locked WASI
check and dependency-tree checks passed; its production browser workflow then
passed 2/2. `just clean` completed; the post-canonical source-manifest check had
passed before the intentional tutorial workspace mutation, and no validation
process remained.

### Retained evidence

- Evidence directory: `/tmp/pmos-ubuntu24-evidence.c5w5tRSc`.
- Evidence-manifest SHA-256:
  `b423cc15a85262b4a04d0a336162e12371c34f97eab1f157084220c9b30e855e`.
- Full transcript SHA-256:
  `2a0a1130131b74c9d11b4c5ef19aa752b3d6667833398a8bb5e0ce28a66666a6`.
- Canonical `just test` log SHA-256:
  `358a80a829abc7d1a05c98907d2e1ce87d947361593f64037a4fde5e59c2cbb3`.
- Both `validation.exit` and `chroot.exit` contain `0`; evidence capture
  finished at `2026-08-11T09:54:02Z`.

The frozen archive identifies the validated executable source. This appended
documentation reconciliation occurred afterward and is not represented as
archive content; it changes no built artifact, runtime behavior, release ID,
tag, or deployment state.

### Constitution Check

Principles I–X remain **PASS**. The Ubuntu walk exercised the real OS layers,
kernel-mediated display protocol, isolated Workers/WASM memories, OPFS/offline
paths, documented WASI-plus-extension surface, toolkit-free wire path, and all
layer and browser gates. Principle IX is directly evidenced by the strict
canonical performance and idle measurements above. This section is docs-only
and has no runtime performance impact.
