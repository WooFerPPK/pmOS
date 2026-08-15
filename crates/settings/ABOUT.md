# Settings About pane contract

The graphical About pane is a read-only view of the running PMos instance. It
does not use browser APIs or host data. It reads these paths through the
process's PMos VFS preopen:

| Field | Source | Maximum content retained |
| --- | --- | ---: |
| Kernel version | `/proc/version` | 1,024 bytes |
| Storage counters | `/proc/storage` | 1,024 bytes |
| License metadata | `/usr/share/doc/pmos/LICENSE.txt` | 4,096 bytes |
| Credits metadata | `/usr/share/doc/pmos/CREDITS.txt` | 4,096 bytes |
| Syscall ABI | linked `abi::version` constants | no file read |

`/proc/storage` must contain exactly `quota_bytes used_bytes file_count` as
three decimal integers. A zero quota is presented as the observable volatile
root fallback. License and credits content is not rendered wholesale; the pane
shows the first non-empty line, the file size, and whether its bounded preview
was truncated.

Opening the About tab performs a refresh. Enter, `R`, and the pointer Refresh
button repeat it. Each field refreshes independently. A failed read remains
visible as unavailable; if an earlier value exists, the pane retains it with a
`stale` label and reports the failed field in the status line. About actions
never write `/etc/preferences.toml`.

All display lines are capped before painting. Reads occur only on About entry
or an explicit refresh, never on the steady-state repaint path. The bounded
reader may consume one additional sentinel byte to detect truncation; that byte
is discarded before parsing or display.
