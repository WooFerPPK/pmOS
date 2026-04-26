//! PID 1 — desktop boot variant.
//!
//! T127 boot-to-desktop entry point. Where the regular
//! `init` binary spawns the demo flow (hello-std + display-
//! server + two demo clients, then SIGTERMs the server when
//! the demo clients close), this variant boots a real
//! desktop:
//!
//!   1. Spawn `/bin/display-server` (full Cap::DisplayServer).
//!   2. Spawn `/bin/shell` (full caps for the desktop:
//!      Cap::DisplayClient + Shell + ProcEnumerate +
//!      KeymapAdmin).
//!   3. Loop on `proc_wait(WAIT_ANY, 0)` forever, reaping
//!      whichever child exits. If a child crashes, log it
//!      and keep waiting — neither the display-server nor
//!      the shell is supposed to exit during normal use.
//!
//! Selected via `#boot-to-desktop` URL hash (handled by
//! `web/src/bootstrap.ts`); the hash maps to `bootBinary =
//! "/bin/init-desktop"`. The Playwright spec in
//! `web/tests/integration/boot-to-desktop.spec.ts` asserts
//! the expected boot sequence (init-desktop spawns →
//! display-server starts → shell connects + draws wallpaper).

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "pmos_ext")]
extern "C" {
    fn proc_spawn(path_ptr: *const u8, path_len: u32, caps: u64) -> i32;
    fn proc_wait(target_pid: i32, options: i32, status_out_ptr: i32) -> i32;
}

#[cfg(not(target_arch = "wasm32"))]
unsafe fn proc_spawn(_path_ptr: *const u8, _path_len: u32, _caps: u64) -> i32 {
    0
}
#[cfg(not(target_arch = "wasm32"))]
unsafe fn proc_wait(_target_pid: i32, _options: i32, _status_out_ptr: i32) -> i32 {
    0
}

const WAIT_ANY: i32 = -1;
const ECHILD: i32 = 9;

fn main() {
    println!("init-desktop starting");

    const DISPLAY_SERVER: &[u8] = b"/bin/display-server";
    let ds_rc =
        unsafe { proc_spawn(DISPLAY_SERVER.as_ptr(), DISPLAY_SERVER.len() as u32, u64::MAX) };
    if ds_rc < 0 {
        println!("init-desktop: proc_spawn /bin/display-server failed errno={}", -ds_rc);
        std::process::exit(1);
    }
    println!("init-desktop spawned display-server pid={}", ds_rc);

    const SHELL: &[u8] = b"/bin/shell";
    let sh_rc = unsafe { proc_spawn(SHELL.as_ptr(), SHELL.len() as u32, u64::MAX) };
    if sh_rc < 0 {
        println!("init-desktop: proc_spawn /bin/shell failed errno={}", -sh_rc);
        std::process::exit(2);
    }
    println!("init-desktop spawned shell pid={}", sh_rc);

    println!("init-desktop entering supervision loop");

    let mut status_out: i64 = 0;
    loop {
        let status_ptr = &mut status_out as *mut i64 as i32;
        let rc = unsafe { proc_wait(WAIT_ANY, 0, status_ptr) };
        if rc < 0 {
            if rc == -ECHILD {
                // No more children to wait on — every spawned
                // process has been reaped. Under v1 desktop
                // boot this means both display-server and shell
                // have exited; print a final marker and fall
                // through.
                println!("init-desktop: no more children, exiting");
                break;
            }
            println!("init-desktop: proc_wait errno={}", -rc);
            break;
        }
        println!("init-desktop reaped child pid={}", rc);
    }

    println!("init-desktop exiting");
}
