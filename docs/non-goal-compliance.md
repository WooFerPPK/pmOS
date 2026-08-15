# Non-goal compliance audit (baseline)

This is the reviewed baseline for `scripts/non-goal-audit.sh` as of
2026-08-11. The audit searches the active PMos source tree for terms that can
indicate drift against FR-040 through FR-044: cloud-service URLs,
authentication, CPU emulation, WebGL/WebGPU, raw TCP/IP, and multi-user
identity APIs.

The audit is deliberately non-blocking. Every emitted match must still be
classified as one of:

- `false positive`: a search hit that cannot be a PMos runtime capability,
  such as an audit self-reference, project history, generic tooling text, or
  an adversarial path fixture;
- `legitimate`: an intentional reference or implementation detail that stays
  within the v1 boundary, such as a statement of the non-goal, static-host
  deployment instructions, or fixed single-user compatibility metadata;
- `TODO`: actual or suspected drift that needs a linked task before the
  baseline can be accepted.

The script excludes generated outputs (`target`, `node_modules`, `dist`, and
`build`), Git metadata, repository-local `.worktrees`, and user-local `.claude`
tooling. Those directories are not active PMos product source; scanning them
duplicates stale source or imports generic agent examples. The audit script,
its fixture, and this baseline are also excluded so their pattern definitions
do not self-trigger. The fixture explicitly contaminates `.claude` and proves
that it does not affect the live product ledger.

## Reproducing the baseline

Run the audit twice and compare the byte-for-byte output before reviewing any
count change:

```bash
bash scripts/non-goal-audit.sh > /tmp/pmos-non-goal-first.txt
bash scripts/non-goal-audit.sh > /tmp/pmos-non-goal-second.txt
cmp /tmp/pmos-non-goal-first.txt /tmp/pmos-non-goal-second.txt
```

The 2026-08-11 baseline produced identical files on two consecutive runs. The
ledger below records every emitted match by category and `path:line`. Because
the script scans each pattern independently, one source line can emit several
matches; the `Hits` column is the number of records that line contributes to
the category total.

## Cloud service URLs (FR-040 / FR-041)

| Source | Hits | Classification and reason |
| --- | ---: | --- |
| `SESSION-NOTES.md:318` | 4 | false positive — historical T222 audit note enumerating the patterns |
| `docs/deploy-s3-cloudfront.md:50` | 1 | legitimate — static-site bucket setup; it adds no PMos runtime backend or per-user cloud data |
| `docs/deploy-s3-cloudfront.md:58` | 1 | legitimate — uploads the static `dist/` artifact; it is deployment tooling, not a runtime fetch path |
| `specs/001-browser-os-v1/analyze-findings.md:40` | 4 | false positive — U5 resolution describes the audit patterns |
| `specs/001-browser-os-v1/tasks.md:586` | 4 | false positive — T222 defines the audit patterns |

There is no cloud-service client or hosted per-user data path in PMos. The two
S3 URLs document one permitted way to host the static release artifact.

## Authentication keywords (FR-041)

| Source | Hits | Classification and reason |
| --- | ---: | --- |
| `.specify/extensions/git/commands/speckit.git.feature.md:42` | 1 | false positive — imported template text uses JWT as an acronym example |
| `.specify/templates/spec-template.md:95` | 1 | false positive — imported requirement-template example mentions OAuth |
| `SESSION-NOTES.md:318` | 5 | false positive — historical T222 audit note enumerating all five patterns |
| `specs/001-browser-os-v1/analyze-findings.md:40` | 5 | false positive — U5 resolution describes all five audit patterns |
| `specs/001-browser-os-v1/spec.md:845` | 1 | legitimate — explicitly states that PMos has no login or other-user concept |
| `specs/001-browser-os-v1/tasks.md:586` | 5 | false positive — T222 defines all five audit patterns |

There is no authentication flow, account model, session-token handling, OAuth
client, JWT parser, or login UI in the product.

## CPU emulation (FR-042)

| Source | Hits | Classification and reason |
| --- | ---: | --- |
| `specs/001-browser-os-v1/spec.md:721` | 1 | legitimate — FR-042 explicitly states that PMos does not emulate another processor or instruction set |
| `specs/001-browser-os-v1/analyze-findings.md:38` | 1 | false positive — reconstructs the hypothetical drift risk that motivated this audit category |

There is no processor emulator, guest ISA interpreter, or machine-emulation
runtime in the active source tree. WebAssembly is PMos's native browser CPU
substrate, not an emulated legacy architecture.

## WebGL / WebGPU (FR-043)

| Source | Hits | Classification and reason |
| --- | ---: | --- |
| `CLAUDE.md:56` | 1 | legitimate — the project guide states that WebGPU is outside v1 |
| `SESSION-NOTES.md:318` | 3 | false positive — historical T222 audit note enumerating all three patterns |
| `specs/001-browser-os-v1/analyze-findings.md:38` | 1 | false positive — describes a hypothetical WebGL-import drift risk |
| `specs/001-browser-os-v1/analyze-findings.md:40` | 2 | false positive — U5 resolution describes the WebGL and WebGPU patterns |
| `specs/001-browser-os-v1/research.md:419` | 1 | legitimate — records WebGPU acceleration as deferred beyond v1 |
| `specs/001-browser-os-v1/research.md:436` | 2 | legitimate — records both WebGL and WebGPU compositing as rejected v1 alternatives |
| `specs/001-browser-os-v1/tasks.md:586` | 3 | false positive — T222 defines all three audit patterns |

No `GPUDevice`, WebGL context creation, or `navigator.gpu` call site exists in
the active source tree. The v1 compositor remains CPU-backed.

## Raw TCP/IP (FR-044)

| Source | Hits | Classification and reason |
| --- | ---: | --- |
| `SESSION-NOTES.md:318` | 3 | false positive — historical T222 audit note enumerating the socket types |
| `specs/001-browser-os-v1/analyze-findings.md:40` | 1 | false positive — U5 resolution names `TcpStream` as the representative pattern |
| `specs/001-browser-os-v1/tasks.md:586` | 3 | false positive — T222 defines all three socket patterns |

No crate imports `TcpStream`, `TcpListener`, or `UdpSocket`. Application
networking remains limited to documented high-level browser facilities.

## Multi-user APIs (FR-040 / FR-041)

| Source | Hits | Classification and reason |
| --- | ---: | --- |
| `SESSION-NOTES.md:318` | 5 | false positive — historical T222 audit note enumerating all five patterns |
| `SESSION-NOTES.md:322` | 2 | false positive — implementation history discusses deferred fixed UID/GID proc fields |
| `SESSION-NOTES.md:332` | 2 | false positive — implementation history discusses deferred fixed UID/GID proc fields |
| `SESSION-NOTES.md:492` | 2 | false positive — implementation history records single-user proc compatibility work |
| `SESSION-NOTES.md:504` | 2 | false positive — implementation history records fixed UID/GID proc output |
| `SESSION-NOTES.md:506` | 2 | false positive — implementation history records fixed UID/GID proc output |
| `SESSION-NOTES.md:508` | 2 | false positive — implementation history records fixed UID/GID proc output |
| `SESSION-NOTES.md:510` | 2 | false positive — implementation history records fixed UID/GID proc output |
| `crates/files/tests/files.rs:127` | 1 | false positive — adversarial filename sanitisation fixture using `/etc/passwd` |
| `crates/kernel/src/sys.rs:2759` | 2 | legitimate — explicitly documents that v1 has no UID/GID ownership model |
| `crates/kernel/src/vfs/mount.rs:147` | 1 | false positive — illustrative path-splitting example; no password file is accessed |
| `crates/kernel/tests/syscall.rs:15040` | 1 | false positive — documents unsupported POSIX `waitpid(-gid, ...)` behavior in a rejection test |
| `crates/pkg/src/lib.rs:457` | 2 | legitimate — archive UID/GID fields are written as fixed zero compatibility metadata |
| `crates/pkg/src/lib.rs:820` | 1 | false positive — traversal-rejection fixture using `../etc/passwd` |
| `crates/pkg/src/lib.rs:832` | 1 | false positive — absolute-path rejection assertion using `/etc/passwd` |
| `crates/pkg/tests/validate.rs:43` | 1 | false positive — package validation fixture rejects `/etc/passwd` |
| `crates/sh/src/builtin.rs:783` | 2 | legitimate — documents deliberately unsupported UID/GID-dependent file-test predicates |
| `crates/sh/src/builtin.rs:959` | 1 | legitimate — explains that shell access checks do not implement effective-UID semantics |
| `crates/sh/src/builtin.rs:969` | 1 | legitimate — states that WASI preview 1 exposes no UID ownership concept |
| `specs/001-browser-os-v1/analyze-findings.md:38` | 1 | false positive — describes a hypothetical inherited UID-variable drift risk |
| `specs/001-browser-os-v1/analyze-findings.md:40` | 4 | false positive — U5 resolution describes four matched multi-user patterns |
| `specs/001-browser-os-v1/data-model.md:231` | 1 | legitimate — reserves UID as the fixed single-user value 1000 |
| `specs/001-browser-os-v1/data-model.md:232` | 1 | legitimate — reserves GID as the fixed single-user value 1000 |
| `specs/001-browser-os-v1/data-model.md:631` | 1 | legitimate — documents the fixed compatibility field in proc status |
| `specs/001-browser-os-v1/tasks.md:329` | 2 | legitimate — implementation note records the omission of UID/GID-dependent shell semantics |
| `specs/001-browser-os-v1/tasks.md:405` | 2 | legitimate — implementation note records fixed single-user proc compatibility fields |
| `specs/001-browser-os-v1/tasks.md:586` | 5 | false positive — T222 defines all five audit patterns |

No identity database, password or shadow file, user-switching syscall, or
multi-user permission model exists. UID/GID references in implementation code
are fixed compatibility metadata or explicit statements that ownership
semantics are unavailable.

## Summary

| Category | Matches | False positive | Legitimate | TODO |
| --- | ---: | ---: | ---: | ---: |
| Cloud service URLs (FR-040 / FR-041) | 14 | 12 | 2 | 0 |
| Authentication keywords (FR-041) | 18 | 17 | 1 | 0 |
| CPU emulation (FR-042) | 2 | 1 | 1 | 0 |
| WebGL / WebGPU (FR-043) | 13 | 9 | 4 | 0 |
| Raw TCP/IP (FR-044) | 7 | 7 | 0 | 0 |
| Multi-user APIs (FR-040 / FR-041) | 50 | 35 | 15 | 0 |
| **Total** | **104** | **81** | **23** | **0** |

There are no TODO items or real v1 non-goal violations in this baseline. A new
match must be reviewed and classified; suspected drift must remain visible as
a `TODO` with a linked task rather than being excluded or described away.
