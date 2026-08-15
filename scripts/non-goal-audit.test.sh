#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
AUDIT_TEST_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/pmos-non-goal-audit.XXXXXX")"

cleanup() {
  rm -rf -- "$AUDIT_TEST_ROOT"
}
trap cleanup EXIT

TEST_REPO="$AUDIT_TEST_ROOT/repo"
mkdir -p "$TEST_REPO/scripts" "$TEST_REPO/fixtures"
cp "$SCRIPT_DIR/non-goal-audit.sh" "$TEST_REPO/scripts/non-goal-audit.sh"
cp "$SCRIPT_DIR/non-goal-audit.test.sh" "$TEST_REPO/scripts/non-goal-audit.test.sh"

cat > "$TEST_REPO/fixtures/positive.txt" <<'EOF'
we now emulate RISC-V CPU opcodes
the x86 instruction-set emulator is enabled
dependency = "unicorn-engine"
EOF

cat > "$TEST_REPO/fixtures/negative.txt" <<'EOF'
terminal emulator
x86_64 build target
CPU budget
EOF

CLEAN_OUTPUT="$AUDIT_TEST_ROOT/clean.txt"
CONTAMINATED_OUTPUT="$AUDIT_TEST_ROOT/contaminated.txt"
OUTSIDE_OUTPUT="$AUDIT_TEST_ROOT/outside.txt"

bash "$TEST_REPO/scripts/non-goal-audit.sh" > "$CLEAN_OUTPUT"

mkdir -p "$TEST_REPO/.claude"
cat > "$TEST_REPO/.claude/local-skill.md" <<'EOF'
emulate RISC-V CPU
x86 instruction-set emulator
unicorn-engine
EOF

bash "$TEST_REPO/scripts/non-goal-audit.sh" > "$CONTAMINATED_OUTPUT"
(
  cd "$AUDIT_TEST_ROOT"
  bash "$TEST_REPO/scripts/non-goal-audit.sh" > "$OUTSIDE_OUTPUT"
)

cmp "$CLEAN_OUTPUT" "$CONTAMINATED_OUTPUT"
cmp "$CLEAN_OUTPUT" "$OUTSIDE_OUTPUT"

CPU_RECORDS="$(awk '
  /^## CPU emulation \(FR-042\)$/ { in_section = 1; next }
  /^## / && in_section { exit }
  in_section && NF { print }
' "$CLEAN_OUTPUT")"

require_once() {
  local needle="$1"
  local count
  count="$(grep -F -c -- "$needle" <<< "$CPU_RECORDS" || true)"
  if [[ "$count" -ne 1 ]]; then
    echo "expected one FR-042 match for '$needle', found $count" >&2
    exit 1
  fi
}

require_once 'emulate RISC-V CPU'
require_once 'x86 instruction-set emulator'
require_once 'unicorn-engine'

if grep -F -q 'negative.txt' <<< "$CPU_RECORDS"; then
  echo "non-emulation architecture terms triggered the FR-042 audit" >&2
  exit 1
fi

if [[ "$(grep -c . <<< "$CPU_RECORDS")" -ne 3 ]]; then
  echo "expected exactly three FR-042 fixture matches" >&2
  exit 1
fi
