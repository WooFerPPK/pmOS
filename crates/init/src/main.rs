// PID 1. Reads /etc/init.conf, spawns display server, waits for
// /run/display, spawns desktop shell, enters reap loop.
//
// At each proc_spawn, also reads /etc/preferences.toml and applies
// the dynamic env-var whitelist (TZ from timezone.iana with UTC
// fallback) per `contracts/init-conf.md §3.6`.
//
// Populated in Phase 2 T095..T097.

fn main() {
    // stub
}
