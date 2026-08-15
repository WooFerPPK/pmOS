//! `sh::run` REPL driver tests for the
//! `${VAR:?error}` "error if unset" parameter expansion
//! form (T142 follow-up to `set -u`).
//!
//! POSIX `${NAME:?error}` (and the no-arg `${NAME:?}`):
//! when NAME is set AND non-empty the expansion is the
//! var's value; when NAME is unset OR empty the shell
//! writes `sh: <name>: <error>\n` (or `sh: <name>:
//! parameter null or not set\n` when no error message was
//! provided) to stderr and TERMINATES the REPL with status
//! one. The failing-expansion command never runs (the
//! dispatch_builtin call is skipped). Critically — this
//! fires REGARDLESS of `set -u`. The `:?` form is its own
//! diagnostic mechanism; it doesn't depend on the nounset
//! flag. Like `${NAME:-default}`, the colon prefix means
//! empty-string-set is treated as unset (the no-colon
//! `${NAME?error}` form would distinguish empty vs unset;
//! deferred to a future T142 partial).
//!
//! These tests drive `run_with_env` directly so they can
//! pin the "REPL terminates with status 1, NOT a generic
//! command-not-found 127" semantic — the load-bearing
//! property that distinguishes required-form failures
//! from runtime command failures. Both polarities of the
//! `set -u` interaction are explicitly covered: that the
//! `:?` form fires WITHOUT nounset (independent), and
//! that under nounset+unset-var the `:?` diagnostic wins
//! over the nounset diagnostic (precedence — the brace
//! arm runs the modifier scan before falling through to
//! the bare-name nounset check).

use std::collections::BTreeMap;
use std::io::{BufReader, Cursor};

use sh::{run_with_env, ExitStatus, ShellFlags};

/// Drive `run_with_env` with a byte-string stdin, a fresh
/// empty env map, and a fresh default `ShellFlags`; return
/// `(status, stdout, stderr)` for assertion. Mirror of the
/// helper in `tests/set_u.rs` minus the post-loop flags
/// return value (none of these tests need to inspect flag
/// mutations because the required-form error path is purely
/// expansion-layer — no flag bit changes).
fn drive(input: &str) -> (ExitStatus, String, String) {
    let stdin = BufReader::new(Cursor::new(input.as_bytes().to_vec()));
    let mut stdout = Vec::<u8>::new();
    let mut stderr = Vec::<u8>::new();
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    let mut flags = ShellFlags::default();
    let status = run_with_env(stdin, &mut stdout, &mut stderr, &mut env, &mut flags);
    let out = String::from_utf8(stdout).expect("stdout must be utf-8");
    let err = String::from_utf8(stderr).expect("stderr must be utf-8");
    (status, out, err)
}

#[test]
fn required_form_with_set_var_expands_to_value() {
    // `${X:?error}` with X set + non-empty → expands to the
    // var's value, message discarded, REPL stays alive,
    // follow-up `exit` runs cleanly. Pins the set-var short-
    // circuit before the error path. The `echo` line writes
    // `hello\n` to stdout because that's the expanded value.
    let (status, stdout, stderr) = drive("export X=hello\necho ${X:?error}\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("hello\n"),
        "echo with set var must produce value: {stdout:?}"
    );
}

#[test]
fn required_form_with_unset_var_terminates_with_default_message() {
    // `${UNSET:?}` with the empty-message form: when the
    // var is unset the shell uses the POSIX default phrase
    // `parameter null or not set`. The REPL terminates with
    // Exit(1) immediately; the second `echo unreached` line
    // never runs. Pins both the default-message substitution
    // AND the failing-expansion-skips-dispatch semantic.
    let (status, stdout, stderr) = drive("echo ${UNSET:?}\necho unreached\n");
    assert_eq!(status, ExitStatus::Exit(1));
    assert!(
        stderr.contains("sh: UNSET: parameter null or not set"),
        "stderr missing default-message diagnostic: {stderr:?}"
    );
    assert!(
        !stdout.contains("unreached"),
        "echo after required-form fire must NOT run: {stdout:?}"
    );
}

#[test]
fn required_form_with_unset_var_uses_custom_message() {
    // `${UNSET:?my custom error}` — the message bytes
    // between `:?` and `}` become the diagnostic. Pin both
    // the var name AND the custom phrase in stderr. Default
    // phrase MUST NOT appear (that would mean the parser
    // didn't recognise the message bytes). The expansion is
    // wrapped in double quotes so the unquoted-tokeniser's
    // whitespace split does NOT cut the brace region across
    // multiple tokens — the same workaround the
    // `default_value_can_contain_spaces` inline test
    // documents for `:-` defaults.
    let (status, _stdout, stderr) = drive("echo \"${UNSET:?my custom error}\"\n");
    assert_eq!(status, ExitStatus::Exit(1));
    assert!(
        stderr.contains("sh: UNSET: my custom error"),
        "stderr missing custom-message diagnostic: {stderr:?}"
    );
    assert!(
        !stderr.contains("parameter null or not set"),
        "default phrase MUST NOT appear when custom message provided: {stderr:?}"
    );
}

#[test]
fn required_form_with_empty_var_terminates_like_unset() {
    // The colon prefix in `:?` means empty-string-set is
    // treated as unset. With X set to the empty string,
    // `${X:?empty}` fires the same way as `${UNSET:?empty}`.
    // Pins the empty-equals-unset semantic that
    // distinguishes `:?` from a future no-colon `${X?}`
    // form (which would treat empty as set).
    let (status, _stdout, stderr) = drive("export X=\necho ${X:?empty}\n");
    assert_eq!(status, ExitStatus::Exit(1));
    assert!(
        stderr.contains("sh: X: empty"),
        "stderr missing empty-var diagnostic: {stderr:?}"
    );
}

#[test]
fn required_form_message_can_have_spaces_and_punctuation() {
    // The message survives spaces AND a literal colon
    // INSIDE the message — the brace parser scans the whole
    // region as one chunk, so the first `:?` is the
    // separator and any subsequent `:` characters are part
    // of the message bytes. Pin against a regression where
    // the parser might split on every `:` and lose the
    // tail. As with the custom-message test, the expansion
    // is wrapped in double quotes so the unquoted-
    // tokeniser's whitespace split does NOT cut the brace
    // region across multiple tokens.
    let (status, _stdout, stderr) = drive("echo \"${UNSET:?cannot run: missing var}\"\n");
    assert_eq!(status, ExitStatus::Exit(1));
    assert!(
        stderr.contains("sh: UNSET: cannot run: missing var"),
        "stderr missing full-message-including-colon: {stderr:?}"
    );
}

#[test]
fn required_form_fires_independently_of_set_u() {
    // `${UNSET:?required}` with NO `set -u` line — the
    // form still fires because the `:?` mechanism is its
    // own diagnostic path that does not consult
    // `flags.nounset`. Pin against a regression where the
    // brace arm might gate `:?` on the nounset flag and
    // accidentally swallow the diagnostic when nounset is
    // off.
    let (status, _stdout, stderr) = drive("echo ${UNSET:?required}\n");
    assert_eq!(status, ExitStatus::Exit(1));
    assert!(
        stderr.contains("sh: UNSET: required"),
        "stderr missing diagnostic when nounset off: {stderr:?}"
    );
}

#[test]
fn required_form_does_not_fire_when_set_var_used_with_set_u() {
    // `set -u` AND a SET non-empty var via `${X:?required}`
    // — neither mechanism fires. The set-var branch of
    // `:?` short-circuits to the value (so `:?` doesn't
    // fire) AND `set -u` is moot because X is defined.
    // Pin that the combination of both safeguards still
    // lets a clean expansion through. The trailing `exit\n`
    // pins the cleanly-terminated REPL via Exit(0) rather
    // than relying on the EOF code which is a different
    // ExitStatus variant.
    let (status, stdout, stderr) = drive("set -u\nexport X=value\necho ${X:?required}\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
    assert!(
        stdout.contains("value\n"),
        "echo with set var must produce value: {stdout:?}"
    );
}

#[test]
fn required_form_with_set_u_and_unset_var_fires_required_first() {
    // Both mechanisms COULD fire here (`set -u` is on,
    // var is unset), but `:?` runs in the brace arm BEFORE
    // any nounset check on the bare-name fallback path.
    // Pins precedence: stderr should contain the custom
    // "required" message, NOT the nounset "parameter not
    // set" phrase. A regression here would mean the brace
    // arm fell through to the nounset check before
    // recognising the `:?` modifier — semantically wrong
    // because the user explicitly asked for a custom
    // diagnostic.
    let (status, _stdout, stderr) = drive("set -u\necho ${UNSET:?required}\n");
    assert_eq!(status, ExitStatus::Exit(1));
    assert!(
        stderr.contains("sh: UNSET: required"),
        "stderr missing required-form precedence: {stderr:?}"
    );
    assert!(
        !stderr.contains("parameter not set"),
        "nounset diagnostic MUST NOT win when :? is present: {stderr:?}"
    );
}

#[test]
fn required_form_braced_form_only() {
    // The bare `$VAR:?error` form is NOT supported — the
    // `:?` modifier only applies INSIDE braces. So
    // `echo $UNSET:?error` tokenises as `echo` plus the
    // expansion of `$UNSET` (empty under the default
    // nounset-off rules) followed by literal `:?error`.
    // Result: no required-form diagnostic, REPL stays alive
    // (the unset-var expands to empty, the literal `:?error`
    // tail remains as-is). Pins that the modifier scan is
    // brace-scoped only — a regression here would mean the
    // bare `$VAR` scanner accidentally honoured `:?`.
    let (status, _stdout, stderr) = drive("echo $UNSET:?error\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(stderr.is_empty(), "unexpected stderr: {stderr:?}");
}

#[test]
fn required_form_unterminated_brace_does_not_error() {
    // `${UNSET:?no_close` with no closing `}` is the
    // existing unterminated-brace lenient-literal path —
    // the parser preserves the literal bytes from `${`
    // onward. The `:?` parsing only runs AFTER the
    // matching `}` is found, so the unterminated case
    // bypasses the required-form error path entirely.
    // Pin that the recovery semantic doesn't accidentally
    // surface a `:?` diagnostic for an unparseable form.
    // The `echo` itself runs (with a literal-preserving
    // arg) and produces stdout; the REPL stays alive; the
    // `exit` runs cleanly.
    let (status, stdout, stderr) = drive("echo ${UNSET:?no_close\nexit\n");
    assert_eq!(status, ExitStatus::Exit(0));
    assert!(
        stderr.is_empty(),
        "unterminated brace MUST NOT trigger :?: {stderr:?}"
    );
    // The `echo` ran — its output reflects the literal-
    // preserved bytes. We don't assert on the exact
    // content because the lenient-recovery shape is the
    // load-bearing property; what matters is that we
    // didn't take the Exit(1) path.
    assert!(
        !stdout.is_empty(),
        "echo with unterminated-brace arg must still run and produce output"
    );
}
