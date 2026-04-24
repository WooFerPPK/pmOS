#!/usr/bin/env bash
# PMos non-goal compliance audit.
#
# Re-runnable grep pass that flags patterns indicating accidental drift
# against FR-040 through FR-044 (the spec's "non-goals" section). The
# script is deliberately non-blocking: it always exits 0. A reviewer
# classifies every match in docs/non-goal-compliance.md as false
# positive, legitimate mention, or TODO. See T222 in
# specs/001-browser-os-v1/tasks.md.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

EXCLUDES=(
  --exclude-dir=target
  --exclude-dir=node_modules
  --exclude-dir=dist
  --exclude-dir=build
  --exclude-dir=.git
)

# Exclude the audit script itself + the generated compliance doc so they
# don't self-trigger every pattern they enumerate.
SELF_EXCLUDES=(
  --exclude=non-goal-audit.sh
  --exclude=non-goal-compliance.md
)

# Sorted, deterministic grep. `LC_ALL=C` pins collation. `|| true`
# tolerates "no matches" under `set -e`.
scan() {
  local pattern="$1"
  LC_ALL=C grep -rniE "${EXCLUDES[@]}" "${SELF_EXCLUDES[@]}" "$pattern" . 2>/dev/null \
    | LC_ALL=C sort \
    || true
}

section() {
  local heading="$1"
  shift
  echo "## $heading"
  local any=0
  for pattern in "$@"; do
    local hits
    hits="$(scan "$pattern")"
    if [[ -n "$hits" ]]; then
      any=1
      while IFS= read -r line; do
        echo "$line"
      done <<< "$hits"
    fi
  done
  if [[ "$any" -eq 0 ]]; then
    echo "(no matches)"
  fi
  echo
}

section "Cloud service URLs (FR-040 / FR-041)" \
  's3://' \
  'gs://' \
  'azure\.com' \
  'supabase' \
  'firebase'

section "Authentication keywords (FR-041)" \
  '\blogin\b' \
  '\bsignup\b' \
  '\boauth\b' \
  '\bjwt\b' \
  '\bsession_token\b'

section "WebGL / WebGPU (FR-043)" \
  '\bwebgl\b' \
  '\bwebgpu\b' \
  'GPUDevice'

section "Raw TCP/IP (FR-044)" \
  'net::TcpStream' \
  'net::TcpListener' \
  'net::UdpSocket'

section "Multi-user APIs (FR-040 / FR-041)" \
  '\buid\b' \
  '\bgid\b' \
  'getpwnam' \
  '/etc/passwd' \
  '/etc/shadow'

exit 0
