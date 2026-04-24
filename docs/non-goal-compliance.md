# Non-goal compliance audit (baseline)

This file is a point-in-time audit of every match found by
`scripts/non-goal-audit.sh` against the PMos repository, with a
one-line classification per match. The script greps for patterns that
would indicate accidental drift against the v1 non-goals in
`specs/001-browser-os-v1/spec.md` (FR-040 through FR-044): cloud
storage URLs, authentication keywords, WebGL / WebGPU use,
raw TCP/IP socket types, and multi-user identity APIs. This is a
non-blocking gate in v1 — every match below has been reviewed and
classified as `false positive`, `legitimate`, or `TODO` (a real
drift that needs a follow-up T-ID).

Tracked under T222 and `/speckit.analyze` finding **U5**.

## How this is kept current

```bash
bash scripts/non-goal-audit.sh > /tmp/non-goal.txt
diff /tmp/non-goal.txt <(sed -n '/^```text/,/^```$/p' docs/non-goal-compliance.md \
    | grep -v '^```')
```

If the diff is empty, the audit is unchanged since the last review.
If non-empty, update the grep blocks below and add a one-line
justification for every new match. The `just test-non-goal-audit`
recipe runs the script and prints a one-line summary, but never
fails the test matrix (the task brief explicitly says
"non-blocking gate in v1").

Patterns are matched case-insensitively, so `Login`, `LOGIN`, and
`login` all produce a hit. `tasks.md:464` contains the T222 task
description itself, which verbatim enumerates every forbidden
pattern; every such hit is a false positive by construction.

---

## Cloud service URLs (FR-040 / FR-041)

```text
./specs/001-browser-os-v1/tasks.md:464:- [ ] T222 [P] Non-goal compliance audit. ... cloud service URLs (`s3://`, `gs://`, `azure`, `supabase`, `firebase`) ... [× 4 hits, one per pattern]
```

- `tasks.md:464` (all four) — false positive — the T222 task
  description enumerates every forbidden pattern; scanning the
  spec finds them verbatim.

No real `s3://`, `gs://`, `azure.com`, `supabase`, or `firebase`
references exist anywhere in the codebase. PMos has no backend
and no cloud integration, as required by Principle III.

## Authentication keywords (FR-041)

```text
./.specify/extensions/git/commands/speckit.git.feature.md:42:- Preserve technical terms and acronyms (OAuth2, API, JWT, etc.)
./.specify/templates/spec-template.md:95:- **FR-006**: System MUST authenticate users via [NEEDS CLARIFICATION: auth method not specified - email/password, SSO, OAuth?]
./specs/001-browser-os-v1/spec.md:808:  has this browser profile; there is no login, account, or concept of
./specs/001-browser-os-v1/tasks.md:464: ... authentication keywords (`login`, `signup`, `oauth`, `jwt`, `session_token`) ... [× 5 hits]
```

- `speckit.git.feature.md:42` — false positive — third-party
  spec-kit tooling template lists OAuth2 / JWT as example
  technical terms to preserve verbatim in branch names. Not a
  PMos feature.
- `spec-template.md:95` — false positive — generic spec-kit
  template showing how to flag an underspecified requirement;
  this is boilerplate imported from the spec-kit project and
  does not describe PMos.
- `spec.md:808` — legitimate — the PMos spec explicitly states
  "there is no login, account, or concept of 'other users'";
  this match is the non-goal being documented.
- `tasks.md:464` (all five) — false positive — T222 task
  description enumerates every forbidden keyword.

No real authentication code, session-token handling, OAuth
client, JWT parser, or login UI exists. PMos runs against a
single anonymous browser profile.

## WebGL / WebGPU (FR-043)

```text
./CLAUDE.md:53:  `putImageData`. No GPU / no WebGPU in v1.
./specs/001-browser-os-v1/research.md:410:- **WebGPU is out of v1.** The future hook ("WebGPU-accelerated
./specs/001-browser-os-v1/research.md:427:- **WebGL/WebGPU compositing**: ruled out as a v1 non-goal.
./specs/001-browser-os-v1/tasks.md:464: ... WebGL/WebGPU imports outside the documented compositor stub (`webgl`, `webgpu`, `GPUDevice`) ... [× 3 hits]
```

- `CLAUDE.md:53` — legitimate — agent-facing brief states "No
  GPU / no WebGPU in v1"; this is the non-goal being
  reaffirmed.
- `research.md:410` — legitimate — research doc explains that
  WebGPU is a deferred v2 hook.
- `research.md:427` — legitimate — alternatives considered
  section explicitly rules out WebGL / WebGPU compositing.
- `tasks.md:464` (all three) — false positive — T222 task
  description enumerates every forbidden pattern.

No `GPUDevice`, `getContext('webgl')`, `WebGLRenderingContext`,
or `navigator.gpu` call sites exist. Compositor uses
`OffscreenCanvas` + `putImageData` only, per Principle III and
the approved research doc.

## Raw TCP/IP (FR-044)

```text
./specs/001-browser-os-v1/tasks.md:464: ... raw TCP/IP imports (`net::TcpStream`, `net::TcpListener`, `net::UdpSocket`) ... [× 3 hits]
```

- `tasks.md:464` (all three) — false positive — T222 task
  description enumerates every forbidden pattern.

No `std::net::TcpStream`, `TcpListener`, or `UdpSocket`
imports exist in any crate. User programs get high-level
browser-facility network access only (`fetch` via the
`net` driver), never raw sockets.

## Multi-user APIs (FR-040 / FR-041)

```text
./crates/kernel/src/vfs/mount.rs:101:    /// * `"/etc/passwd"`  → ("/" mount, "etc/passwd")
./crates/kernel/tests/syscall.rs:9627:    // target_pid < -1 is process-group wait (POSIX waitpid(-gid, ...)).
./specs/001-browser-os-v1/data-model.md:226:uid              u32        // reserved; single-user -> 1000
./specs/001-browser-os-v1/data-model.md:227:gid              u32        // reserved -> 1000
./specs/001-browser-os-v1/data-model.md:554:/proc/<pid>/status           (name, state, pid, ppid, uid, vmsize, ...)
./specs/001-browser-os-v1/tasks.md:464: ... multi-user APIs (`uid`, `gid`, `getpwnam`, `/etc/passwd`, `/etc/shadow`) ... [× 5 hits]
```

- `mount.rs:101` — false positive — doc comment uses
  `"/etc/passwd"` as an example VFS path to illustrate how
  `longest_prefix` resolves a path string against mounts.
  No real password file is read or created; the string is a
  generic Unix-y path for the example.
- `syscall.rs:9627` — false positive — inline comment in a
  test describing POSIX `waitpid(-gid, ...)` semantics; the
  test body then asserts PMos returns `EINVAL` because v1
  does not implement process groups. No `gid` value is read
  or stored.
- `data-model.md:226` — false positive — inode record reserves
  a `uid` field hard-wired to `1000` for single-user
  compatibility with WASI / POSIX stat layouts. Not a
  multi-user feature; explicitly documented as "reserved;
  single-user".
- `data-model.md:227` — false positive — same as above for
  `gid`, reserved to `1000`.
- `data-model.md:554` — false positive — `/proc/<pid>/status`
  schema lists `uid` as one of the fields exposed, for POSIX
  compatibility, always `1000`.
- `tasks.md:464` (all five) — false positive — T222 task
  description enumerates every forbidden pattern.

No `getpwnam`, `/etc/shadow`, `setuid`, or multi-user
privilege-separation code exists. The `uid` / `gid` fields are
reserved stat struct members held at the single-user constant.

---

## Summary

| Category                                  | Matches | False positive | Legitimate | TODO |
| ----------------------------------------- | ------: | -------------: | ---------: | ---: |
| Cloud service URLs (FR-040 / FR-041)      |       4 |              4 |          0 |    0 |
| Authentication keywords (FR-041)          |       8 |              7 |          1 |    0 |
| WebGL / WebGPU (FR-043)                   |       7 |              4 |          3 |    0 |
| Raw TCP/IP (FR-044)                       |       3 |              3 |          0 |    0 |
| Multi-user APIs (FR-040 / FR-041)         |      10 |             10 |          0 |    0 |
| **Total**                                 |  **32** |         **28** |      **4** | **0** |

No TODO items. All 32 matches are either false positives (spec
self-references, POSIX compatibility comments, template
boilerplate) or legitimate mentions of the non-goals being
reaffirmed. The audit is re-runnable with
`bash scripts/non-goal-audit.sh`; any new match that is not a
false positive MUST be classified as a TODO with a linked
T-ID before this baseline is updated.
