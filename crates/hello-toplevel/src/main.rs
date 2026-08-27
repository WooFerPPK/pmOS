//! `/bin/hello-toplevel` — minimal app that opens a real
//! pmd_xdg_toplevel and paints a solid color.
//!
//! Useful as the launcher's first "this actually works"
//! demo: clicking the launcher → kernel spawns this binary
//! → it connects to the display server → shell-manager
//! broadcasts a window_created → the desktop shell paints
//! a taskbar entry → the toplevel surface composites into
//! the framebuffer alongside the wallpaper.
//!
//! The shared toolkit frame supplies ordinary client-owned
//! move, minimize, maximize, and close controls while all
//! state changes still travel over the display protocol.

use display_proto::events::{pointer_button_state, PointerButton};
use display_proto::objects::Interface;
use toolkit::draw::{Color, Rect};
use toolkit::widget::frame::{PointerOutcome as ChromePointerOutcome, WindowFrame};
use toolkit::Theme;
use toolkit::{App, BufferPool, Window, WindowFramePatch, WindowFramePatchProgress};

const NORMAL_WINDOW_SIZE: (u32, u32) = (320, 200);

#[cfg(target_arch = "wasm32")]
mod wasm_main {
    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    extern "C" {
        fn proc_exit(rval: i32) -> !;
    }

    pub fn run() {
        println!("hello-toplevel: starting");
        let conn = match toolkit::wasi::FdConnection::connect() {
            Ok(connection) => connection,
            Err(errno) => unsafe { proc_exit(errno) },
        };
        match super::run_window(conn) {
            Ok(_) => unsafe { proc_exit(0) },
            Err(_) => unsafe { proc_exit(1) },
        }
    }
}

#[cfg(target_arch = "wasm32")]
extern crate alloc;

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn run_window<C: toolkit::protocol::Connection>(connection: C) -> Result<(), toolkit::ClientError> {
    run_window_inner(connection, None)
}

#[cfg(test)]
fn run_window_with_iteration_limit<C: toolkit::protocol::Connection>(
    connection: C,
    max_dispatch_iterations: u32,
) -> Result<(), toolkit::ClientError> {
    run_window_inner(connection, Some(max_dispatch_iterations))
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn run_window_inner<C: toolkit::protocol::Connection>(
    connection: C,
    mut remaining_iterations: Option<u32>,
) -> Result<(), toolkit::ClientError> {
    let mut app = App::connect_with_shell(connection)?;
    let mut window = Window::new(&mut app)?;
    window.set_title("Hello Window")?;
    window.set_app_id("pmos.hello")?;
    window.commit()?;

    let mut size = NORMAL_WINDOW_SIZE;
    let mut needs_paint = true;
    let mut observed_configure = None;
    let mut was_activated = false;
    let mut pool: Option<BufferPool> = None;
    let mut chrome_patch: Option<WindowFramePatch> = None;

    loop {
        if remaining_iterations == Some(0) {
            return Ok(());
        }
        if let Some(remaining) = remaining_iterations.as_mut() {
            *remaining -= 1;
        }
        let events = window.dispatch()?;
        let focus_changed = window.is_activated() != was_activated;
        if focus_changed {
            was_activated = window.is_activated();
        }
        if window.take_close_requested() {
            return Ok(());
        }

        for event in events {
            match (event.interface, event.opcode) {
                (Interface::Pointer, 2) => {
                    let Ok(button) = PointerButton::decode(&event.payload) else {
                        continue;
                    };
                    if button.button != 1 || button.state != pointer_button_state::PRESSED {
                        continue;
                    }
                    let mut frame =
                        hello_window_frame(size, window.is_activated(), window.is_maximized());
                    match frame.pointer_down(button.x, button.y) {
                        ChromePointerOutcome::Minimize => window.set_minimized()?,
                        ChromePointerOutcome::ToggleMaximize => {
                            if window.is_maximized() {
                                window.unset_maximized()?;
                            } else {
                                window.set_maximized()?;
                            }
                        }
                        ChromePointerOutcome::Close => return Ok(()),
                        ChromePointerOutcome::Titlebar => {
                            if !window.is_maximized() {
                                window.request_move(button.serial)?;
                            }
                        }
                        ChromePointerOutcome::Content | ChromePointerOutcome::Outside => {}
                    }
                }
                (Interface::Buffer, 1) => {
                    if let Some(buffers) = pool.as_mut() {
                        let _ = buffers.handle_release(event.object_id);
                    }
                }
                _ => {}
            }
        }

        if let Some(buffers) = pool.as_mut().filter(|buffers| buffers.commit_pending()) {
            let _ = buffers.progress_commit(&mut window)?;
        }

        if window.is_configured() && !pool.as_ref().is_some_and(BufferPool::commit_pending) {
            let offered = window.configured_size();
            let next_size = if window.is_maximized() && offered.0 > 0 && offered.1 > 0 {
                offered
            } else {
                NORMAL_WINDOW_SIZE
            };
            let configure = (next_size, window.is_maximized());
            if pool.is_none() || observed_configure != Some(configure) {
                if pool.is_none() || next_size != size {
                    size = next_size;
                    BufferPool::replace(&mut pool, window.app_mut(), size.0, size.1)?;
                }
                observed_configure = Some(configure);
                needs_paint = true;
            }
        }

        if needs_paint {
            chrome_patch = None;
        } else {
            if focus_changed && window.is_configured() {
                chrome_patch = Some(WindowFramePatch::new(&hello_window_frame(
                    size,
                    was_activated,
                    window.is_maximized(),
                )));
            }
            if let (Some(patch), Some(buffers)) = (chrome_patch.as_mut(), pool.as_mut()) {
                match patch.progress(buffers, &mut window)? {
                    WindowFramePatchProgress::Complete => chrome_patch = None,
                    WindowFramePatchProgress::Unavailable => {
                        chrome_patch = None;
                        needs_paint = true;
                    }
                    WindowFramePatchProgress::Deferred | WindowFramePatchProgress::Pending => {}
                }
            }
        }

        if needs_paint && window.is_configured() {
            let buffers = pool.as_mut().expect("configured window has a buffer pool");
            if let Some(mut canvas) = buffers.acquire_back_canvas() {
                paint_window(&mut canvas, size, was_activated, window.is_maximized());
                drop(canvas);
                let _ = buffers.commit_and_swap(&mut window)?;
                needs_paint = false;
                chrome_patch = None;
            }
        }

        window.flush_outbound()?;
        if (pool.as_ref().is_some_and(BufferPool::commit_pending) || chrome_patch.is_some())
            && !window.outbound_pending()
        {
            continue;
        }
        window.wait(None)?;
    }
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
fn paint_window(
    canvas: &mut toolkit::draw::Canvas<'_>,
    size: (u32, u32),
    focused: bool,
    maximized: bool,
) {
    let fill = Color::rgb(0xc8, 0x60, 0xa8);
    canvas.fill_rect(Rect::new(0, 0, size.0, size.1), fill);

    let frame = hello_window_frame(size, focused, maximized);
    let content = frame.content_rect();
    canvas.draw_text(
        content.x + 13,
        content.y + 24,
        "Spawned via the desktop launcher.",
        Color::rgb(0xff, 0xff, 0xff),
    );
    frame.draw(canvas);
}

fn hello_window_frame(size: (u32, u32), focused: bool, maximized: bool) -> WindowFrame {
    let mut frame = WindowFrame::new(Rect::new(0, 0, size.0, size.1), "Hello Window");
    frame.set_theme(Theme::LIGHT);
    frame.set_focused(focused);
    frame.set_maximized(maximized);
    frame
}

fn main() {
    #[cfg(target_arch = "wasm32")]
    wasm_main::run();
    #[cfg(not(target_arch = "wasm32"))]
    println!(
        "hello-toplevel (host build): use `cargo build --target wasm32-wasip1 -p hello-toplevel`"
    );
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::time::Duration;

    use display_proto::events::{RegistryGlobal, XdgToplevelConfigure};
    use display_proto::requests::SurfacePatchCurrent;
    use display_proto::{MessageHeader, ObjectId, HEADER_SIZE};
    use toolkit::protocol::Connection;

    use super::run_window_with_iteration_limit;

    const REGISTRY_ID: ObjectId = ObjectId::new(3);
    const SURFACE_ID: ObjectId = ObjectId::new(19);
    const TOPLEVEL_ID: ObjectId = ObjectId::new(21);
    const POOL_ID: ObjectId = ObjectId::new(23);

    struct FocusPatchConnection {
        outbound: Vec<u8>,
        inbound: VecDeque<Vec<u8>>,
        recorded: Rc<RefCell<Vec<u8>>>,
        activation_boundary: Rc<Cell<Option<usize>>>,
        blocked: bool,
        initial_configure_sent: bool,
        initial_buffer_attached: bool,
        activation_sent: bool,
    }

    impl FocusPatchConnection {
        fn new(
            recorded: Rc<RefCell<Vec<u8>>>,
            activation_boundary: Rc<Cell<Option<usize>>>,
        ) -> Self {
            let mut globals = Vec::new();
            globals.extend(global_event(1, "pmd_compositor"));
            globals.extend(global_event(2, "pmd_shm"));
            globals.extend(global_event(3, "pmd_xdg_shell"));
            globals.extend(global_event(4, "pmd_seat"));
            Self {
                outbound: Vec::new(),
                inbound: VecDeque::from([globals]),
                recorded,
                activation_boundary,
                blocked: false,
                initial_configure_sent: false,
                initial_buffer_attached: false,
                activation_sent: false,
            }
        }

        fn observe_requests(&mut self, mut bytes: &[u8]) {
            while bytes.len() >= HEADER_SIZE {
                let header = MessageHeader::decode(bytes).expect("valid request framing");
                let length = header.length as usize;
                assert!(bytes.len() >= length, "complete request framing");
                let request = &bytes[..length];
                if header.object_id == ObjectId::DISPLAY && header.opcode == 1 {
                    self.inbound.push_back(sync_done(request));
                }
                if header.object_id == SURFACE_ID && header.opcode == 7 {
                    if !self.initial_configure_sent {
                        self.inbound.push_back(configure_event(1, 1024, 736, 0));
                        self.initial_configure_sent = true;
                    }
                } else if header.object_id == SURFACE_ID && header.opcode == 2 {
                    self.initial_buffer_attached = true;
                }
                bytes = &bytes[length..];
            }
            assert!(bytes.is_empty(), "no partial request remains");
        }
    }

    impl Connection for FocusPatchConnection {
        fn send(&mut self, bytes: &[u8]) {
            self.outbound.extend_from_slice(bytes);
            self.recorded.borrow_mut().extend_from_slice(bytes);
            self.blocked |= !bytes.is_empty();
            self.observe_requests(bytes);
        }

        fn drain_outbound(&mut self) -> Vec<u8> {
            self.blocked = false;
            core::mem::take(&mut self.outbound)
        }

        fn recv(&mut self) -> Vec<u8> {
            self.inbound.pop_front().unwrap_or_default()
        }

        fn flush_outbound(&mut self) -> Result<(), i32> {
            self.outbound.clear();
            self.blocked = false;
            Ok(())
        }

        fn outbound_pending(&self) -> bool {
            self.blocked
        }

        fn incremental_uploads(&self) -> bool {
            true
        }

        fn wait(&mut self, _timeout: Option<Duration>) -> Result<(), i32> {
            if self.initial_buffer_attached && !self.activation_sent {
                self.activation_boundary
                    .set(Some(self.recorded.borrow().len()));
                self.inbound.push_back(configure_event(
                    2,
                    320,
                    200,
                    display_proto::xdg_toplevel_state::ACTIVATED,
                ));
                self.activation_sent = true;
            }
            Ok(())
        }
    }

    fn framed(object_id: ObjectId, opcode: u16, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0; HEADER_SIZE + payload.len()];
        MessageHeader::try_new(object_id, opcode, payload.len(), 0)
            .expect("valid test message")
            .encode(&mut bytes[..HEADER_SIZE])
            .expect("header encoding");
        bytes[HEADER_SIZE..].copy_from_slice(payload);
        bytes
    }

    fn global_event(name: u32, interface: &str) -> Vec<u8> {
        let event = RegistryGlobal {
            name,
            interface: interface.to_string(),
            version: 1,
        };
        let mut payload = Vec::new();
        event.encode(&mut payload);
        framed(REGISTRY_ID, 1, &payload)
    }

    fn configure_event(serial: u32, width: i32, height: i32, states: u32) -> Vec<u8> {
        let event = XdgToplevelConfigure {
            serial,
            width,
            height,
            states,
        };
        let mut payload = Vec::new();
        event.encode(&mut payload);
        framed(TOPLEVEL_ID, 1, &payload)
    }

    fn sync_done(request: &[u8]) -> Vec<u8> {
        let callback_id = ObjectId::new(u32::from_le_bytes(
            request[HEADER_SIZE..HEADER_SIZE + 4]
                .try_into()
                .expect("sync callback id"),
        ));
        framed(callback_id, 1, &0u32.to_le_bytes())
    }

    fn requests(mut bytes: &[u8]) -> Vec<(MessageHeader, &[u8])> {
        let mut requests = Vec::new();
        while bytes.len() >= HEADER_SIZE {
            let header = MessageHeader::decode(bytes).expect("valid request framing");
            let length = header.length as usize;
            assert!(bytes.len() >= length, "complete request framing");
            requests.push((header, &bytes[HEADER_SIZE..length]));
            bytes = &bytes[length..];
        }
        assert!(bytes.is_empty(), "no partial request remains");
        requests
    }

    #[test]
    fn activated_only_configure_uses_bounded_current_patches() {
        let recorded = Rc::new(RefCell::new(Vec::new()));
        let activation_boundary = Rc::new(Cell::new(None));
        let connection = FocusPatchConnection::new(recorded.clone(), activation_boundary.clone());

        run_window_with_iteration_limit(connection, 64).expect("window loop succeeds");

        let recorded = recorded.borrow();
        let boundary = activation_boundary
            .get()
            .expect("initial frame reaches the display before activation");
        let initial = requests(&recorded[..boundary]);
        assert!(initial
            .iter()
            .any(|(header, _)| header.object_id == POOL_ID && header.opcode == 4));
        assert_eq!(
            initial
                .iter()
                .filter(|(header, _)| header.object_id == SURFACE_ID && header.opcode == 2)
                .count(),
            1,
            "the cold frame attaches exactly one buffer",
        );

        let after_activation = requests(&recorded[boundary..]);
        assert_eq!(
            after_activation
                .iter()
                .filter(|(header, _)| header.object_id == POOL_ID && header.opcode == 4)
                .count(),
            0,
            "focus-only configure must not upload a full buffer",
        );
        assert_eq!(
            after_activation
                .iter()
                .filter(|(header, _)| header.object_id == SURFACE_ID && header.opcode == 2)
                .count(),
            0,
            "focus-only configure must not attach the alternate buffer",
        );
        let patches = after_activation
            .iter()
            .filter(|(header, _)| header.object_id == SURFACE_ID && header.opcode == 8)
            .map(|(_, payload)| {
                SurfacePatchCurrent::decode(payload).expect("typed patch_current request")
            })
            .collect::<Vec<_>>();
        assert!(!patches.is_empty(), "activation must repaint window chrome");
        assert!(patches
            .iter()
            .all(|patch| patch.pixels.len() <= display_proto::MAX_SURFACE_PATCH_BYTES));
    }
}
