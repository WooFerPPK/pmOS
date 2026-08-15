//! `/usr/bin/edit` — plain-text editor (T157).
//!
//! Toolkit window with an editable `edit::EditBuffer`. Read on
//! startup, write on Ctrl+S. The keyboard event loop mirrors
//! the one in `term::run` — `pmd_keyboard.key` events are
//! decoded, a [`Modifiers`] bag tracks Shift/Ctrl, and the
//! decoded key drives the buffer.
//!
//! CLI subcommands (host build):
//!   edit                      — print the default file
//!   edit <path>               — print <path>
//!   edit save <path> <text>   — write <text> to <path>

use std::process::ExitCode;

#[cfg(not(target_arch = "wasm32"))]
use edit::{read_file, write_file};

#[cfg(target_arch = "wasm32")]
use display_proto::events::{key_state, pointer_button_state, KeyboardKey, PointerButton};
#[cfg(target_arch = "wasm32")]
use display_proto::Interface;
#[cfg(target_arch = "wasm32")]
use edit::{
    DocumentJob, DocumentJobSuccess, DocumentJobTurn, DocumentWait, DocumentWaitInterest,
    EditorEffect, EditorIoJob, EditorIoTurn, EditorMode, EditorStepwiseEffect, PathAction,
    StdDocumentStore,
};
#[cfg(any(target_arch = "wasm32", test))]
use edit::{EditorInput, EditorSession};
#[cfg(target_arch = "wasm32")]
use toolkit::draw::font::GLYPH_HEIGHT;
#[cfg(target_arch = "wasm32")]
use toolkit::draw::{Color, Rect};
#[cfg(target_arch = "wasm32")]
use toolkit::{App, BufferPool, WaitFd, Window};

#[cfg(any(target_arch = "wasm32", test))]
const TITLEBAR_HEIGHT: u32 = 22;
#[cfg(any(target_arch = "wasm32", test))]
const FILE_MENU_HEIGHT: u32 = 18;

#[cfg(any(target_arch = "wasm32", test))]
fn file_menu_input(x: i32, y: i32, width: u32) -> Option<EditorInput> {
    if !(TITLEBAR_HEIGHT as i32..(TITLEBAR_HEIGHT + FILE_MENU_HEIGHT) as i32).contains(&y) {
        return None;
    }
    let close_left = width.saturating_sub(58) as i32;
    let close_right = width.saturating_sub(6) as i32;
    if (close_left..close_right).contains(&x) {
        return Some(EditorInput::RequestClose);
    }
    match x {
        50..=91 => Some(EditorInput::New),
        96..=137 => Some(EditorInput::Open),
        142..=183 => Some(EditorInput::Save),
        188..=251 => Some(EditorInput::SaveAs),
        _ => None,
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm_main {
    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    extern "C" {
        fn proc_exit(rval: i32) -> !;
    }

    pub fn run() {
        println!("edit: starting");
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

#[cfg(target_arch = "wasm32")]
struct StartupLoad {
    path: String,
    explicit: bool,
    remaining: std::collections::VecDeque<String>,
    document: Option<DocumentJob>,
}

#[cfg(target_arch = "wasm32")]
enum StartupTurn {
    Progress,
    Blocked(DocumentWait),
    Complete,
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Debug, Default)]
struct StatusReporter {
    ready: bool,
}

#[cfg(any(target_arch = "wasm32", test))]
impl StatusReporter {
    fn observe(
        &mut self,
        startup_active: bool,
        prior_status: &str,
        session: &EditorSession,
    ) -> Option<String> {
        if !self.ready {
            if startup_active {
                return None;
            }
            self.ready = true;
            return Some(format!(
                "edit: ready path={} status={}",
                session.document_label(),
                session.status()
            ));
        }
        (session.status() != prior_status).then(|| format!("edit: {}", session.status()))
    }
}

/// Begin argv/starter loading without issuing a filesystem syscall. Each body
/// read is advanced later, after display dispatch, by [`StartupLoad::step`].
#[cfg(target_arch = "wasm32")]
fn begin_startup_load(store: &mut StdDocumentStore) -> (EditorSession, Option<StartupLoad>) {
    let explicit = std::env::args().nth(1);
    let (path, explicit, remaining) = if let Some(path) = explicit {
        (path, true, std::collections::VecDeque::new())
    } else {
        let mut candidates = std::collections::VecDeque::from([
            "/home/user/Documents/welcome.txt".to_string(),
            "/home/user/README.md".to_string(),
        ]);
        (
            candidates
                .pop_front()
                .expect("starter candidates are nonempty"),
            false,
            candidates,
        )
    };
    let mut session = if explicit {
        EditorSession::new_at(path.clone())
    } else {
        EditorSession::new()
    };
    match store.start_open(&path) {
        Ok(document) => {
            session.set_status(format!("opening {path}"));
            (
                session,
                Some(StartupLoad {
                    path,
                    explicit,
                    remaining,
                    document: Some(document),
                }),
            )
        }
        Err(error) => {
            session.set_error(format!("open failed: {error}"));
            (session, None)
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl StartupLoad {
    fn step(&mut self, session: &mut EditorSession, store: &mut StdDocumentStore) -> StartupTurn {
        let turn = self
            .document
            .as_mut()
            .expect("startup load owns document job")
            .step(store);
        match turn {
            DocumentJobTurn::Progress => StartupTurn::Progress,
            DocumentJobTurn::Blocked(wait) => StartupTurn::Blocked(wait),
            DocumentJobTurn::Complete(DocumentJobSuccess::Opened(document)) => {
                self.document = None;
                *session = EditorSession::from_open_document(self.path.clone(), document)
                    .expect("bounded document job returned oversized contents");
                StartupTurn::Complete
            }
            DocumentJobTurn::Complete(_) => {
                self.document = None;
                session.set_error("open failed: invalid startup document job result");
                StartupTurn::Complete
            }
            DocumentJobTurn::Failed(error) if error.is_not_found() && self.explicit => {
                self.document = None;
                *session = EditorSession::new_at(self.path.clone());
                StartupTurn::Complete
            }
            DocumentJobTurn::Failed(error) if error.is_not_found() => {
                self.document = None;
                if let Some(next) = self.remaining.pop_front() {
                    self.path = next;
                    match store.start_open(&self.path) {
                        Ok(document) => {
                            self.document = Some(document);
                            session.set_status(format!("opening {}", self.path));
                            StartupTurn::Progress
                        }
                        Err(error) => {
                            session.set_error(format!("open failed: {error}"));
                            StartupTurn::Complete
                        }
                    }
                } else {
                    *session = EditorSession::new();
                    StartupTurn::Complete
                }
            }
            DocumentJobTurn::Failed(error) => {
                self.document = None;
                session.set_error(format!("open failed: {error}"));
                StartupTurn::Complete
            }
        }
    }

    fn cancel(&mut self, store: &mut StdDocumentStore) {
        if let Some(document) = self.document.take() {
            let _ = store.cancel_job(document);
        }
    }
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Default, Debug, Clone, Copy)]
struct Modifiers {
    shift: bool,
    ctrl: bool,
}

#[cfg(any(target_arch = "wasm32", test))]
mod sc {
    pub const KEY_A: u32 = 0x04;
    pub const KEY_Z: u32 = 0x1D;
    pub const DIGIT_1: u32 = 0x1E;
    pub const DIGIT_0: u32 = 0x27;
    pub const ENTER: u32 = 0x28;
    pub const ESCAPE: u32 = 0x29;
    pub const BACKSPACE: u32 = 0x2A;
    pub const TAB: u32 = 0x2B;
    pub const SPACE: u32 = 0x2C;
    pub const MINUS: u32 = 0x2D;
    pub const EQUAL: u32 = 0x2E;
    pub const BRACKET_LEFT: u32 = 0x2F;
    pub const BRACKET_RIGHT: u32 = 0x30;
    pub const BACKSLASH: u32 = 0x31;
    pub const SEMICOLON: u32 = 0x33;
    pub const QUOTE: u32 = 0x34;
    pub const BACKQUOTE: u32 = 0x35;
    pub const COMMA: u32 = 0x36;
    pub const PERIOD: u32 = 0x37;
    pub const SLASH: u32 = 0x38;
    pub const ARROW_RIGHT: u32 = 0x4F;
    pub const ARROW_LEFT: u32 = 0x50;
    pub const ARROW_DOWN: u32 = 0x51;
    pub const ARROW_UP: u32 = 0x52;
    pub const SHIFT_LEFT: u32 = 0xE1;
    pub const SHIFT_RIGHT: u32 = 0xE5;
    pub const CONTROL_LEFT: u32 = 0xE0;
    pub const CONTROL_RIGHT: u32 = 0xE4;
}

#[cfg(any(target_arch = "wasm32", test))]
impl Modifiers {
    fn update(&mut self, scancode: u32, pressed: bool) -> bool {
        match scancode {
            sc::SHIFT_LEFT | sc::SHIFT_RIGHT => {
                self.shift = pressed;
                true
            }
            sc::CONTROL_LEFT | sc::CONTROL_RIGHT => {
                self.ctrl = pressed;
                true
            }
            _ => false,
        }
    }
}

#[cfg(any(target_arch = "wasm32", test))]
enum DecodedKey {
    Char(char),
    Enter,
    Escape,
    Backspace,
    Tab,
    Left,
    Right,
    Up,
    Down,
    /// Ctrl-modified printable, e.g. Ctrl+S => Save.
    CtrlChar(char),
}

#[cfg(any(target_arch = "wasm32", test))]
fn decode(scancode: u32, mods: Modifiers) -> Option<DecodedKey> {
    if scancode == sc::ENTER {
        return Some(DecodedKey::Enter);
    }
    if scancode == sc::BACKSPACE {
        return Some(DecodedKey::Backspace);
    }
    if scancode == sc::ESCAPE {
        return Some(DecodedKey::Escape);
    }
    if scancode == sc::TAB {
        return Some(DecodedKey::Tab);
    }
    if scancode == sc::SPACE {
        return Some(DecodedKey::Char(' '));
    }
    if scancode == sc::ARROW_LEFT {
        return Some(DecodedKey::Left);
    }
    if scancode == sc::ARROW_RIGHT {
        return Some(DecodedKey::Right);
    }
    if scancode == sc::ARROW_UP {
        return Some(DecodedKey::Up);
    }
    if scancode == sc::ARROW_DOWN {
        return Some(DecodedKey::Down);
    }
    if (sc::KEY_A..=sc::KEY_Z).contains(&scancode) {
        let lower = (b'a' + (scancode - sc::KEY_A) as u8) as char;
        if mods.ctrl {
            return Some(DecodedKey::CtrlChar(lower));
        }
        let ch = if mods.shift {
            lower.to_ascii_uppercase()
        } else {
            lower
        };
        return Some(DecodedKey::Char(ch));
    }
    if (sc::DIGIT_1..=sc::DIGIT_0).contains(&scancode) {
        let unshifted = match scancode {
            sc::DIGIT_1 => '1',
            0x1F => '2',
            0x20 => '3',
            0x21 => '4',
            0x22 => '5',
            0x23 => '6',
            0x24 => '7',
            0x25 => '8',
            0x26 => '9',
            sc::DIGIT_0 => '0',
            _ => return None,
        };
        let shifted = match unshifted {
            '1' => '!',
            '2' => '@',
            '3' => '#',
            '4' => '$',
            '5' => '%',
            '6' => '^',
            '7' => '&',
            '8' => '*',
            '9' => '(',
            '0' => ')',
            _ => unshifted,
        };
        return Some(DecodedKey::Char(if mods.shift {
            shifted
        } else {
            unshifted
        }));
    }
    let punct = match scancode {
        sc::MINUS => Some(if mods.shift { '_' } else { '-' }),
        sc::EQUAL => Some(if mods.shift { '+' } else { '=' }),
        sc::BRACKET_LEFT => Some(if mods.shift { '{' } else { '[' }),
        sc::BRACKET_RIGHT => Some(if mods.shift { '}' } else { ']' }),
        sc::BACKSLASH => Some(if mods.shift { '|' } else { '\\' }),
        sc::SEMICOLON => Some(if mods.shift { ':' } else { ';' }),
        sc::QUOTE => Some(if mods.shift { '"' } else { '\'' }),
        sc::BACKQUOTE => Some(if mods.shift { '~' } else { '`' }),
        sc::COMMA => Some(if mods.shift { '<' } else { ',' }),
        sc::PERIOD => Some(if mods.shift { '>' } else { '.' }),
        sc::SLASH => Some(if mods.shift { '?' } else { '/' }),
        _ => None,
    };
    punct.map(DecodedKey::Char)
}

#[cfg(target_arch = "wasm32")]
fn run_window<C: toolkit::protocol::Connection>(connection: C) -> Result<(), toolkit::ClientError> {
    let mut app = App::connect_with_shell(connection)?;
    let mut window = Window::new(&mut app)?;
    window.set_title("Editor")?;
    window.set_app_id("pmos.edit")?;
    window.commit()?;

    let mut store = StdDocumentStore::default();
    let (mut session, mut startup) = begin_startup_load(&mut store);
    let mut active_io: Option<EditorIoJob> = None;
    let mut status_reporter = StatusReporter::default();
    let mut mods = Modifiers::default();
    let mut needs_paint = true;
    let mut configured_once = false;
    let (w, h) = (640u32, 420u32);
    let mut pool: Option<BufferPool> = None;

    loop {
        let events = window.dispatch()?;
        if window.take_close_requested() {
            if handle_editor_input(
                &mut session,
                EditorInput::RequestClose,
                &mut store,
                &mut startup,
                &mut active_io,
            ) {
                return Ok(());
            }
            needs_paint = true;
        }
        for event in events {
            match (event.interface, event.opcode) {
                (Interface::Keyboard, 1) => {
                    let Ok(key) = KeyboardKey::decode(&event.payload) else {
                        continue;
                    };
                    let pressed = key.state == key_state::PRESSED;
                    let was_mod = mods.update(key.key, pressed);
                    if !pressed || was_mod {
                        continue;
                    }
                    let Some(decoded) = decode(key.key, mods) else {
                        continue;
                    };
                    let input = match decoded {
                        DecodedKey::Char(character) => EditorInput::Character(character),
                        DecodedKey::Enter => EditorInput::Enter,
                        DecodedKey::Escape => EditorInput::Escape,
                        DecodedKey::Backspace => EditorInput::Backspace,
                        DecodedKey::Tab => EditorInput::Tab,
                        DecodedKey::Left => EditorInput::Left,
                        DecodedKey::Right => EditorInput::Right,
                        DecodedKey::Up => EditorInput::Up,
                        DecodedKey::Down => EditorInput::Down,
                        DecodedKey::CtrlChar('n') => EditorInput::New,
                        DecodedKey::CtrlChar('o') => EditorInput::Open,
                        DecodedKey::CtrlChar('s') if mods.shift => EditorInput::SaveAs,
                        DecodedKey::CtrlChar('s') => EditorInput::Save,
                        DecodedKey::CtrlChar('q') | DecodedKey::CtrlChar('w') => {
                            EditorInput::RequestClose
                        }
                        DecodedKey::CtrlChar(_) => continue,
                    };
                    if handle_editor_input(
                        &mut session,
                        input,
                        &mut store,
                        &mut startup,
                        &mut active_io,
                    ) {
                        return Ok(());
                    }
                    needs_paint = true;
                }
                (Interface::Pointer, 2) => {
                    let Ok(button) = PointerButton::decode(&event.payload) else {
                        continue;
                    };
                    if button.button != 1 || button.state != pointer_button_state::PRESSED {
                        continue;
                    }
                    let Some(input) = file_menu_input(button.x, button.y, w) else {
                        continue;
                    };
                    if handle_editor_input(
                        &mut session,
                        input,
                        &mut store,
                        &mut startup,
                        &mut active_io,
                    ) {
                        return Ok(());
                    }
                    needs_paint = true;
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

        if !configured_once && window.is_configured() {
            configured_once = true;
            BufferPool::replace(&mut pool, window.app_mut(), w, h)?;
            needs_paint = true;
        }

        if needs_paint && configured_once {
            let p = pool.as_mut().expect("pool initialised");
            if let Some(mut canvas) = p.acquire_back_canvas() {
                paint_editor(&mut canvas, w, h, &session);
                drop(canvas);
                let _ = p.commit_and_swap(&mut window)?;
                needs_paint = false;
            }
        }
        window.flush_outbound()?;

        // Document work is deliberately after display dispatch, buffer
        // release/progress, paint, and protocol flush. A successful quantum
        // returns to the top of the outer loop before another filesystem op.
        let prior_document_status = session.status().to_string();
        let mut document_wait = None;
        let mut document_progress = false;
        let mut close_after_document = false;
        if let Some(load) = startup.as_mut() {
            match load.step(&mut session, &mut store) {
                StartupTurn::Progress => document_progress = true,
                StartupTurn::Blocked(wait) => document_wait = Some(wait),
                StartupTurn::Complete => {
                    startup = None;
                    needs_paint = true;
                    document_progress = true;
                }
            }
        } else if let Some(job) = active_io.as_mut() {
            match job.step(&mut session, &mut store) {
                EditorIoTurn::Progress => document_progress = true,
                EditorIoTurn::Blocked(wait) => document_wait = Some(wait),
                EditorIoTurn::Complete(effect) => {
                    active_io = None;
                    needs_paint = true;
                    close_after_document = effect == EditorEffect::Close;
                    document_progress = true;
                }
            }
        }
        if let Some(line) =
            status_reporter.observe(startup.is_some(), &prior_document_status, &session)
        {
            println!("{line}");
        }
        if close_after_document {
            return Ok(());
        }
        if document_progress {
            continue;
        }
        if pool.as_ref().is_some_and(BufferPool::commit_pending) && !window.outbound_pending() {
            continue;
        }
        if let Some(wait) = document_wait {
            let wait = match wait.interest {
                DocumentWaitInterest::Read => WaitFd::readable(wait.fd),
                DocumentWaitInterest::Write => WaitFd::writable(wait.fd),
            };
            window.wait_with(&[wait], None)?;
        } else {
            window.wait(None)?;
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn handle_editor_input(
    session: &mut EditorSession,
    input: EditorInput,
    store: &mut StdDocumentStore,
    startup: &mut Option<StartupLoad>,
    active_io: &mut Option<EditorIoJob>,
) -> bool {
    let prior_status = session.status().to_string();
    let effect = if let Some(load) = startup.as_mut() {
        if input == EditorInput::RequestClose {
            load.cancel(store);
            *startup = None;
            EditorEffect::Close
        } else {
            session.set_status(format!("opening {}; input waits", load.path));
            EditorEffect::Continue
        }
    } else if active_io.is_some() {
        session.handle_during_io(input, active_io, store)
    } else {
        match session.handle_stepwise(input, store) {
            EditorStepwiseEffect::Continue => EditorEffect::Continue,
            EditorStepwiseEffect::Close => EditorEffect::Close,
            EditorStepwiseEffect::Started(job) => {
                *active_io = Some(*job);
                EditorEffect::Continue
            }
        }
    };
    if session.status() != prior_status {
        println!("edit: {}", session.status());
    }
    effect == EditorEffect::Close
}

#[cfg(target_arch = "wasm32")]
fn draw_file_menu_button(
    canvas: &mut toolkit::draw::Canvas<'_>,
    x: i32,
    width: u32,
    label: &str,
    background: Color,
    foreground: Color,
) {
    canvas.fill_rect(
        Rect {
            x,
            y: TITLEBAR_HEIGHT as i32 + 1,
            width,
            height: FILE_MENU_HEIGHT.saturating_sub(2),
        },
        background,
    );
    canvas.draw_text(x + 4, TITLEBAR_HEIGHT as i32 + 4, label, foreground);
}

#[cfg(target_arch = "wasm32")]
fn paint_editor(canvas: &mut toolkit::draw::Canvas<'_>, w: u32, h: u32, session: &EditorSession) {
    let path = session.document_label();
    let buffer = session.buffer();
    let mode = session.mode();
    let bg = Color::rgb(0xfa, 0xfa, 0xfa);
    let titlebar = Color::rgb(0x6a, 0x4a, 0x8a);
    let menu_bar = Color::rgb(0xe0, 0xe0, 0xe4);
    let menu_button = Color::rgb(0xcb, 0xcb, 0xd2);
    let gutter_bg = Color::rgb(0xee, 0xee, 0xf2);
    let gutter_fg = Color::rgb(0x80, 0x80, 0x90);
    let status_bg = Color::rgb(0xe0, 0xe0, 0xe4);
    let text_fg = Color::rgb(0x10, 0x10, 0x10);
    let cursor_color = Color::rgb(0x10, 0x10, 0x10);
    let titlebar_h = TITLEBAR_HEIGHT;
    let menubar_h = FILE_MENU_HEIGHT;
    let status_h = 18u32;
    let gutter_w = 36u32;
    let line_h = 14_i32;

    canvas.fill_rect(
        Rect {
            x: 0,
            y: 0,
            width: w,
            height: h,
        },
        bg,
    );
    canvas.fill_rect(
        Rect {
            x: 0,
            y: 0,
            width: w,
            height: titlebar_h,
        },
        titlebar,
    );
    let dirty_marker = if buffer.dirty() { "*" } else { "" };
    let title = format!("Editor — {}{}", path, dirty_marker);
    canvas.draw_text(
        8,
        ((titlebar_h as i32 - GLYPH_HEIGHT as i32) / 2).max(0),
        &title,
        Color::rgb(0xff, 0xff, 0xff),
    );

    canvas.fill_rect(
        Rect {
            x: 0,
            y: titlebar_h as i32,
            width: w,
            height: menubar_h,
        },
        menu_bar,
    );
    canvas.draw_text(8, titlebar_h as i32 + 4, "File:", text_fg);
    draw_file_menu_button(canvas, 50, 42, "New", menu_button, text_fg);
    draw_file_menu_button(canvas, 96, 42, "Open", menu_button, text_fg);
    draw_file_menu_button(canvas, 142, 42, "Save", menu_button, text_fg);
    draw_file_menu_button(canvas, 188, 64, "Save As", menu_button, text_fg);
    draw_file_menu_button(
        canvas,
        w as i32 - 58,
        52,
        "Close",
        Color::rgb(0xd8, 0xb8, 0xb8),
        text_fg,
    );

    let content_y_top = (titlebar_h + menubar_h) as i32 + 6;
    let content_y_bot = h as i32 - status_h as i32;
    let content_h = (content_y_bot - content_y_top).max(0) as u32;
    let visible_lines = (content_h as i32 / line_h).max(0) as usize;

    canvas.fill_rect(
        Rect {
            x: 0,
            y: content_y_top,
            width: gutter_w,
            height: content_h,
        },
        gutter_bg,
    );

    let (cursor_line, cursor_col) = buffer.cursor();
    let scroll_top = if cursor_line >= visible_lines {
        cursor_line + 1 - visible_lines
    } else {
        0
    };

    for (i, line) in buffer
        .lines()
        .iter()
        .skip(scroll_top)
        .take(visible_lines)
        .enumerate()
    {
        let line_no = scroll_top + i + 1;
        let y = content_y_top + (i as i32) * line_h;
        canvas.draw_text(4, y, &format!("{:>4}", line_no), gutter_fg);
        let display: String = line.chars().take(96).collect();
        canvas.draw_text(gutter_w as i32 + 6, y, &display, text_fg);
        if line_no - 1 == cursor_line {
            let prefix_chars = line.chars().take(cursor_col).count();
            let cursor_x = gutter_w as i32 + 6 + (prefix_chars as i32) * 8;
            canvas.fill_rect(
                Rect {
                    x: cursor_x,
                    y,
                    width: 2,
                    height: GLYPH_HEIGHT,
                },
                cursor_color,
            );
        }
    }

    canvas.fill_rect(
        Rect {
            x: 0,
            y: content_y_bot,
            width: w,
            height: status_h,
        },
        status_bg,
    );
    let visible_status: String = session.status().chars().take(66).collect();
    let status = format!(
        "{}   |   {} line{}   |   ln {}, col {}",
        visible_status,
        buffer.line_count(),
        if buffer.line_count() == 1 { "" } else { "s" },
        cursor_line + 1,
        cursor_col + 1,
    );
    canvas.draw_text(8, content_y_bot + 4, &status, text_fg);

    match mode {
        EditorMode::Editing => {}
        EditorMode::Path { action, input, .. } => {
            let box_width = w.saturating_sub(80);
            let box_x = 40;
            let box_y = (h as i32 / 2) - 36;
            canvas.fill_rect(
                Rect {
                    x: box_x,
                    y: box_y,
                    width: box_width,
                    height: 72,
                },
                Color::rgb(0xf4, 0xf4, 0xf8),
            );
            let label = match action {
                PathAction::Open => "Open path:",
                PathAction::SaveAs => "Save as path:",
            };
            canvas.draw_text(box_x + 10, box_y + 10, label, text_fg);
            canvas.fill_rect(
                Rect {
                    x: box_x + 10,
                    y: box_y + 28,
                    width: box_width.saturating_sub(20),
                    height: 18,
                },
                Color::rgb(0xff, 0xff, 0xff),
            );
            let visible: String = input
                .chars()
                .rev()
                .take(64)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            canvas.draw_text(box_x + 14, box_y + 32, &visible, text_fg);
            canvas.draw_text(
                box_x + 10,
                box_y + 54,
                "Enter confirms; Esc cancels",
                Color::rgb(0x50, 0x50, 0x58),
            );
        }
        EditorMode::ConfirmDiscard(_) => {
            let box_width = w.saturating_sub(120);
            let box_x = 60;
            let box_y = (h as i32 / 2) - 28;
            canvas.fill_rect(
                Rect {
                    x: box_x,
                    y: box_y,
                    width: box_width,
                    height: 56,
                },
                Color::rgb(0xff, 0xf4, 0xd6),
            );
            canvas.draw_text(
                box_x + 10,
                box_y + 10,
                "Unsaved changes",
                Color::rgb(0x50, 0x30, 0x10),
            );
            canvas.draw_text(
                box_x + 10,
                box_y + 30,
                "S Save   D Discard   C/Esc Cancel",
                text_fg,
            );
        }
        EditorMode::Busy(_) => {
            let box_width = w.saturating_sub(180);
            let box_x = 90;
            let box_y = (h as i32 / 2) - 20;
            canvas.fill_rect(
                Rect {
                    x: box_x,
                    y: box_y,
                    width: box_width,
                    height: 40,
                },
                Color::rgb(0xe8, 0xee, 0xf8),
            );
            canvas.draw_text(
                box_x + 10,
                box_y + 14,
                session.status(),
                Color::rgb(0x20, 0x30, 0x50),
            );
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn run_cli() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("save") => {
            let Some(path) = args.next() else {
                eprintln!("usage: edit save <path> <text>");
                return ExitCode::from(2);
            };
            let text = args.next().unwrap_or_default();
            match write_file(&path, &text) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("edit: failed to write {}: {}", path, e);
                    ExitCode::FAILURE
                }
            }
        }
        Some(path) => print_file(path),
        None => print_file("/etc/preferences.toml"),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn print_file(path: &str) -> ExitCode {
    match read_file(path) {
        Ok(contents) => {
            println!("editor (host build) — {path} (ok)");
            for (index, line) in contents.lines().take(20).enumerate() {
                println!("{:4}: {}", index + 1, line);
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("edit: failed to open {error}");
            ExitCode::FAILURE
        }
    }
}

fn main() -> ExitCode {
    #[cfg(target_arch = "wasm32")]
    {
        wasm_main::run();
        // wasm_main::run terminates the process via proc_exit;
        // returning here is unreachable. Keep the type signature
        // ExitCode for the host build by emitting SUCCESS as a
        // formal fallthrough.
        #[allow(unreachable_code)]
        ExitCode::SUCCESS
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        run_cli()
    }
}

#[cfg(test)]
mod gui_tests {
    use super::*;

    #[test]
    fn ctrl_shift_s_decodes_as_save_shortcut_with_shift_preserved() {
        let mods = Modifiers {
            shift: true,
            ctrl: true,
        };
        assert!(matches!(
            decode(0x16, mods),
            Some(DecodedKey::CtrlChar('s'))
        ));
        assert!(mods.shift);
    }

    #[test]
    fn escape_decodes_for_cancelling_editor_dialogs() {
        assert!(matches!(
            decode(sc::ESCAPE, Modifiers::default()),
            Some(DecodedKey::Escape)
        ));
    }

    #[test]
    fn file_menu_pointer_targets_every_document_action() {
        let y = TITLEBAR_HEIGHT as i32 + 4;
        assert_eq!(file_menu_input(70, y, 640), Some(EditorInput::New));
        assert_eq!(file_menu_input(110, y, 640), Some(EditorInput::Open));
        assert_eq!(file_menu_input(160, y, 640), Some(EditorInput::Save));
        assert_eq!(file_menu_input(210, y, 640), Some(EditorInput::SaveAs));
        assert_eq!(
            file_menu_input(610, y, 640),
            Some(EditorInput::RequestClose)
        );
        assert_eq!(file_menu_input(20, y, 640), None);
        assert_eq!(file_menu_input(70, TITLEBAR_HEIGHT as i32 - 1, 640), None);
    }

    #[test]
    fn modifier_transitions_and_printable_decode_are_stateful() {
        let mut modifiers = Modifiers::default();
        assert!(modifiers.update(sc::SHIFT_LEFT, true));
        assert!(modifiers.update(sc::CONTROL_RIGHT, true));
        assert!(modifiers.shift);
        assert!(modifiers.ctrl);
        assert!(matches!(
            decode(sc::KEY_A, modifiers),
            Some(DecodedKey::CtrlChar('a'))
        ));
        assert!(modifiers.update(sc::SHIFT_RIGHT, false));
        assert!(modifiers.update(sc::CONTROL_LEFT, false));

        let decoded = decode(sc::KEY_A, Modifiers::default());
        match decoded {
            Some(DecodedKey::Char(character)) => assert_eq!(character, 'a'),
            _ => panic!("expected decoded character"),
        }
    }

    #[test]
    fn status_reporter_defers_ready_until_startup_finishes_and_reports_it_once() {
        let mut reporter = StatusReporter::default();
        let mut session = EditorSession::new_at("/home/user/Documents/editing.md");
        session.set_status("opening /home/user/Documents/editing.md");

        assert_eq!(
            reporter.observe(true, "opening /home/user/Documents/editing.md", &session),
            None
        );

        session.set_status("opened /home/user/Documents/editing.md bytes=12");
        assert_eq!(
            reporter.observe(false, "opening /home/user/Documents/editing.md", &session),
            Some(
                "edit: ready path=/home/user/Documents/editing.md status=opened /home/user/Documents/editing.md bytes=12"
                    .to_string()
            )
        );
        assert_eq!(
            reporter.observe(
                false,
                "opened /home/user/Documents/editing.md bytes=12",
                &session,
            ),
            None
        );
    }

    #[test]
    fn status_reporter_emits_async_completion_transition_once_after_ready() {
        let mut reporter = StatusReporter::default();
        let mut session = EditorSession::new_at("/home/user/Documents/draft.txt");
        session.set_status("new file /home/user/Documents/draft.txt");
        assert!(reporter
            .observe(false, "new file /home/user/Documents/draft.txt", &session)
            .is_some());

        session.set_status("saved /home/user/Documents/draft.txt bytes=17");
        assert_eq!(
            reporter.observe(false, "saving /home/user/Documents/draft.txt", &session,),
            Some("edit: saved /home/user/Documents/draft.txt bytes=17".to_string())
        );
        assert_eq!(
            reporter.observe(
                false,
                "saved /home/user/Documents/draft.txt bytes=17",
                &session,
            ),
            None
        );
    }
}
