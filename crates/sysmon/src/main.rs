//! `/usr/bin/sysmon` — live PMos process monitor.
//!
//! The shipped WASI binary is an ordinary toolkit client. It reads the
//! documented `/proc/<pid>/status` and `/proc/<pid>/fd` interfaces once per
//! second, and only calls `proc_kill(SIGKILL)` after a capability check and an
//! explicit user confirmation. The native binary keeps a `--proc-root` CLI so
//! the exact proc parser can be tested without a browser.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use display_proto::events::{key_state, pointer_button_state, KeyboardKey, PointerButton};
use display_proto::Interface;
#[cfg(not(target_arch = "wasm32"))]
use sysmon::collect_processes;
use sysmon::{
    format_process_row, MonitorAction, MonitorKey, MonitorMode, MonitorState, PointerTarget,
    ProcessCollection, ProcessScanStep, ProcessScanner, RefreshSchedule,
};
use toolkit::draw::font::GLYPH_HEIGHT;
use toolkit::draw::{Canvas, Color, Rect};
use toolkit::{watch_theme, App, BufferPool, Theme, Window};

#[cfg(target_arch = "wasm32")]
mod wasm_main {
    use super::*;
    #[link(wasm_import_module = "wasi_snapshot_preview1")]
    extern "C" {
        fn proc_exit(rval: i32) -> !;
    }

    #[link(wasm_import_module = "pmos_ext")]
    extern "C" {
        fn cap_check(cap_id: i32) -> i32;
        fn proc_self() -> i32;
        fn proc_kill(target_pid: i32, signum: i32) -> i32;
    }

    pub fn run() -> ! {
        println!("sysmon: starting");
        let connection = match toolkit::wasi::FdConnection::connect() {
            Ok(connection) => connection,
            Err(errno) => unsafe { proc_exit(errno) },
        };

        let pid = unsafe { proc_self() };
        let self_pid = (pid > 0).then_some(pid as u32);
        let terminate_capable = unsafe { cap_check(abi::cap::Cap::ProcKillAny as i32) } == 1;
        let result = run_window(connection, self_pid, terminate_capable, |target| {
            let rc = unsafe { proc_kill(target as i32, abi::ext::sig::SIGKILL as i32) };
            if rc == 0 {
                Ok(())
            } else {
                Err(format!("errno {}", -rc))
            }
        });
        unsafe { proc_exit(if result.is_ok() { 0 } else { 1 }) };
    }
}

#[cfg(target_arch = "wasm32")]
extern crate alloc;

const DEFAULT_WIDTH: u32 = 720;
const DEFAULT_HEIGHT: u32 = 440;
const TITLEBAR_HEIGHT: u32 = 22;
const TOOLBAR_Y: i32 = TITLEBAR_HEIGHT as i32;
const TOOLBAR_HEIGHT: u32 = 28;
const HEADER_Y: i32 = TOOLBAR_Y + TOOLBAR_HEIGHT as i32;
const HEADER_HEIGHT: u32 = 22;
const LIST_TOP: i32 = HEADER_Y + HEADER_HEIGHT as i32;
const STATUS_HEIGHT: u32 = 24;
const ROW_HEIGHT: i32 = 18;
const SCROLLBAR_WIDTH: i32 = 22;
const REFRESH_INTERVAL_MS: u64 = 1_000;
const PROCESS_SCAN_QUANTA_PER_TURN: usize = 2;
const MAX_PENDING_SCAN_TURNS_WITHOUT_THEME_DRAIN: usize = 16;
const PROCESS_LOGS_PER_TURN: usize = 4;
const MAX_PENDING_PROCESS_LOGS: usize = 513;
const MAX_PROCESS_LOG_NAME_CHARS: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum ProcessLogKey {
    Process(u32),
    Status,
}

#[derive(Default)]
struct ProcessLogQueue {
    order: VecDeque<ProcessLogKey>,
    messages: BTreeMap<ProcessLogKey, String>,
}

impl ProcessLogQueue {
    fn push(&mut self, key: ProcessLogKey, message: String) {
        if self.messages.insert(key, message).is_some() {
            return;
        }
        if self.order.len() == MAX_PENDING_PROCESS_LOGS {
            if let Some(retired) = self.order.pop_front() {
                self.messages.remove(&retired);
            }
        }
        self.order.push_back(key);
    }

    fn drain_turn<F>(&mut self, mut emit: F) -> usize
    where
        F: FnMut(&str),
    {
        let mut drained = 0;
        for _ in 0..PROCESS_LOGS_PER_TURN {
            let Some(key) = self.order.pop_front() else {
                break;
            };
            if let Some(message) = self.messages.remove(&key) {
                emit(&message);
                drained += 1;
            }
        }
        drained
    }

    fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.order.len()
    }
}

#[derive(Default)]
struct ThemeDrainSchedule {
    pending_scan_turns: usize,
    scan_continuation: bool,
}

impl ThemeDrainSchedule {
    fn take_due(&mut self, scan_pending: bool, scan_completed: bool) -> bool {
        let scan_started = scan_pending && !self.scan_continuation;
        self.scan_continuation = scan_pending;
        if scan_started || scan_completed || !scan_pending {
            self.pending_scan_turns = 0;
            return true;
        }

        // Watch readiness is level-triggered, so queued records survive the
        // scan's immediate follow-up turns. Keep the deferral bounded for a
        // pathologically large process table.
        self.pending_scan_turns = self.pending_scan_turns.saturating_add(1);
        if self.pending_scan_turns >= MAX_PENDING_SCAN_TURNS_WITHOUT_THEME_DRAIN {
            self.pending_scan_turns = 0;
            true
        } else {
            false
        }
    }
}

fn advance_process_scan_turn<F>(mut step: F) -> Option<ProcessCollection>
where
    F: FnMut() -> ProcessScanStep,
{
    for _ in 0..PROCESS_SCAN_QUANTA_PER_TURN {
        if let ProcessScanStep::Complete(collection) = step() {
            return Some(collection);
        }
    }
    None
}

fn bounded_log_name(name: &str) -> String {
    let Some((end, _)) = name.char_indices().nth(MAX_PROCESS_LOG_NAME_CHARS) else {
        return name.to_string();
    };
    let mut bounded = name[..end].to_string();
    bounded.push('…');
    bounded
}

#[derive(Default, Clone, Copy)]
struct Modifiers {
    ctrl: bool,
}

mod sc {
    pub const KEY_K: u32 = 0x0e;
    pub const KEY_Q: u32 = 0x14;
    pub const KEY_R: u32 = 0x15;
    pub const ENTER: u32 = 0x28;
    pub const ESCAPE: u32 = 0x29;
    pub const DELETE: u32 = 0x4c;
    pub const HOME: u32 = 0x4a;
    pub const PAGE_UP: u32 = 0x4b;
    pub const END: u32 = 0x4d;
    pub const PAGE_DOWN: u32 = 0x4e;
    pub const ARROW_DOWN: u32 = 0x51;
    pub const ARROW_UP: u32 = 0x52;
    pub const CONTROL_LEFT: u32 = 0xe0;
    pub const CONTROL_RIGHT: u32 = 0xe4;
}

impl Modifiers {
    fn update(&mut self, scancode: u32, pressed: bool) -> bool {
        match scancode {
            sc::CONTROL_LEFT | sc::CONTROL_RIGHT => {
                self.ctrl = pressed;
                true
            }
            _ => false,
        }
    }
}

fn decode_key(scancode: u32, modifiers: Modifiers) -> Option<MonitorKey> {
    match scancode {
        sc::ARROW_UP => Some(MonitorKey::Up),
        sc::ARROW_DOWN => Some(MonitorKey::Down),
        sc::PAGE_UP => Some(MonitorKey::PageUp),
        sc::PAGE_DOWN => Some(MonitorKey::PageDown),
        sc::HOME => Some(MonitorKey::Home),
        sc::END => Some(MonitorKey::End),
        sc::DELETE | sc::KEY_K => Some(MonitorKey::Terminate),
        sc::KEY_R => Some(MonitorKey::Refresh),
        sc::ENTER => Some(MonitorKey::Enter),
        sc::ESCAPE => Some(MonitorKey::Escape),
        sc::KEY_Q if modifiers.ctrl => Some(MonitorKey::Close),
        _ => None,
    }
}

fn run_window<C, K>(
    connection: C,
    self_pid: Option<u32>,
    terminate_capable: bool,
    mut terminate: K,
) -> Result<(), toolkit::ClientError>
where
    C: toolkit::protocol::Connection,
    K: FnMut(u32) -> Result<(), String>,
{
    // Theme-aware apps read the canonical preference synchronously during
    // startup, then use the toolkit's bounded VFS watcher in their ordinary
    // event loop. No preference value crosses the display protocol.
    let mut theme_watcher = watch_theme();
    let mut theme = theme_watcher.current();
    #[cfg(target_arch = "wasm32")]
    let mut preference_watch = toolkit::PathWatch::new("/etc/preferences.toml")
        .map_err(toolkit::ClientError::Transport)?;
    let mut app = App::connect_with_shell(connection)?;
    let mut window = Window::new(&mut app)?;
    window.set_title("System Monitor")?;
    window.set_app_id("pmos.sysmon")?;
    window.commit()?;

    let mut state = MonitorState::new(self_pid, terminate_capable);
    let mut process_logs = ProcessLogQueue::default();
    let mut pending_refresh = match ProcessScanner::start(Path::new("/proc")) {
        Ok(scanner) => Some(scanner),
        Err(error) => {
            let _ = refresh_and_log(&mut state, Err(error), 1, &mut process_logs);
            None
        }
    };
    let mut manual_refresh_queued = false;
    let mut theme_drain_schedule = ThemeDrainSchedule::default();
    let started = Instant::now();
    let mut refresh_schedule = RefreshSchedule::new(0, REFRESH_INTERVAL_MS);
    let mut modifiers = Modifiers::default();
    let mut needs_paint = true;
    let mut configured = false;
    let mut ready_logged = false;
    let mut pool: Option<BufferPool> = None;
    let mut size = (DEFAULT_WIDTH, DEFAULT_HEIGHT);

    loop {
        let events = window.dispatch()?;
        if window.take_close_requested() {
            return Ok(());
        }
        let rows = visible_rows(size.1);
        let mut manual_refresh_requested = false;
        for event in events {
            match (event.interface, event.opcode) {
                (Interface::Keyboard, 1) => {
                    if let Ok(key) = KeyboardKey::decode(&event.payload) {
                        let pressed = key.state == key_state::PRESSED;
                        let is_modifier = modifiers.update(key.key, pressed);
                        if pressed && !is_modifier {
                            if let Some(key) = decode_key(key.key, modifiers) {
                                needs_paint = true;
                                if let Some(action) = state.handle_key(key, rows) {
                                    match execute_action(&mut state, action, &mut terminate) {
                                        GuiAction::Continue => {}
                                        GuiAction::Refresh => manual_refresh_requested = true,
                                        GuiAction::Close => return Ok(()),
                                    }
                                }
                            }
                        }
                    }
                }
                (Interface::Pointer, 2) => {
                    if let Ok(button) = PointerButton::decode(&event.payload) {
                        if button.button == 1 && button.state == pointer_button_state::PRESSED {
                            if let Some(target) =
                                pointer_target(&state, button.x, button.y, size.0, size.1)
                            {
                                needs_paint = true;
                                if let Some(action) = state.handle_pointer(target, rows) {
                                    match execute_action(&mut state, action, &mut terminate) {
                                        GuiAction::Continue => {}
                                        GuiAction::Refresh => manual_refresh_requested = true,
                                        GuiAction::Close => return Ok(()),
                                    }
                                }
                            }
                        }
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

        if !configured && window.is_configured() {
            configured = true;
            let (width, height) = window.configured_size();
            if width > 0 && height > 0 {
                // The compositor proposes the whole work area on the initial
                // configure. Sysmon is a normal window, not a maximized shell,
                // so cap that proposal at its useful table dimensions.
                size = (
                    width.clamp(480, DEFAULT_WIDTH),
                    height.clamp(320, DEFAULT_HEIGHT),
                );
            }
            BufferPool::replace(&mut pool, window.app_mut(), size.0, size.1)?;
            needs_paint = true;
        }

        let scheduled_refresh = if pending_refresh.is_none() {
            let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
            refresh_schedule.take_due(elapsed_ms)
        } else {
            false
        };
        if manual_refresh_requested {
            if pending_refresh.is_some() {
                manual_refresh_queued = true;
            } else {
                match ProcessScanner::start(Path::new("/proc")) {
                    Ok(scanner) => pending_refresh = Some(scanner),
                    Err(error) => {
                        needs_paint |=
                            refresh_and_log(&mut state, Err(error), rows, &mut process_logs);
                    }
                }
            }
        } else if scheduled_refresh && pending_refresh.is_none() {
            match ProcessScanner::start(Path::new("/proc")) {
                Ok(scanner) => pending_refresh = Some(scanner),
                Err(error) => {
                    needs_paint |= refresh_and_log(&mut state, Err(error), rows, &mut process_logs);
                }
            }
        }

        let completed_refresh = pending_refresh
            .as_mut()
            .and_then(|scanner| advance_process_scan_turn(|| scanner.step()));
        let scan_completed = completed_refresh.is_some();
        if let Some(collection) = completed_refresh {
            pending_refresh = None;
            needs_paint |= refresh_and_log(&mut state, Ok(collection), rows, &mut process_logs);
            if manual_refresh_queued {
                manual_refresh_queued = false;
                match ProcessScanner::start(Path::new("/proc")) {
                    Ok(scanner) => pending_refresh = Some(scanner),
                    Err(error) => {
                        needs_paint |=
                            refresh_and_log(&mut state, Err(error), rows, &mut process_logs);
                    }
                }
            }
        }

        let theme_drain_due =
            theme_drain_schedule.take_due(pending_refresh.is_some(), scan_completed);
        #[cfg(target_arch = "wasm32")]
        let next_theme = if theme_drain_due
            && preference_watch
                .drain()
                .map_err(toolkit::ClientError::Transport)?
        {
            theme_watcher.refresh()
        } else {
            None
        };
        #[cfg(not(target_arch = "wasm32"))]
        let next_theme = if theme_drain_due {
            theme_watcher.poll()
        } else {
            None
        };
        if let Some(next_theme) = next_theme {
            theme = next_theme;
            needs_paint = true;
            println!("sysmon: theme changed {}", theme.name);
        }

        if configured && needs_paint {
            let buffers = pool.as_mut().expect("buffer pool configured");
            if let Some(mut canvas) = buffers.acquire_back_canvas() {
                paint_sysmon(&mut canvas, size.0, size.1, &state, theme);
                drop(canvas);
                let _ = buffers.commit_and_swap(&mut window)?;
                needs_paint = false;
                if !ready_logged {
                    println!(
                        "sysmon: ready processes={} terminate={}",
                        state.processes().len(),
                        if state.terminate_capable() {
                            "enabled"
                        } else {
                            "read-only"
                        }
                    );
                    ready_logged = true;
                }
            }
        }

        window.flush_outbound()?;
        process_logs.drain_turn(|message| println!("{message}"));
        if pending_refresh.is_some()
            || !process_logs.is_empty()
            || (pool.as_ref().is_some_and(BufferPool::commit_pending) && !window.outbound_pending())
        {
            continue;
        }
        let elapsed_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let timeout = Duration::from_millis(refresh_schedule.remaining_ms(elapsed_ms));
        #[cfg(target_arch = "wasm32")]
        window.wait_with(&preference_watch.wait_fds(), Some(timeout))?;
        #[cfg(not(target_arch = "wasm32"))]
        window.wait(Some(timeout))?;
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum GuiAction {
    Continue,
    Refresh,
    Close,
}

fn execute_action<K: FnMut(u32) -> Result<(), String>>(
    state: &mut MonitorState,
    action: MonitorAction,
    terminate: &mut K,
) -> GuiAction {
    match action {
        MonitorAction::Refresh => GuiAction::Refresh,
        MonitorAction::Terminate(pid) => {
            let result = terminate(pid);
            let succeeded = result.is_ok();
            state.complete_termination(pid, result);
            println!("sysmon: {}", state.status());
            if succeeded {
                GuiAction::Refresh
            } else {
                GuiAction::Continue
            }
        }
        MonitorAction::Close => GuiAction::Close,
    }
}

fn refresh_and_log(
    state: &mut MonitorState,
    result: Result<ProcessCollection, String>,
    visible_rows: usize,
    logs: &mut ProcessLogQueue,
) -> bool {
    let before = state.clone();
    if let Ok(collection) = &result {
        for process in &collection.processes {
            let previous = state.processes().iter().find(|old| old.pid == process.pid);
            let change = match previous {
                None => Some("observed"),
                Some(old)
                    if old.name != process.name
                        || old.vm_size_kib != process.vm_size_kib
                        || old.open_fds != process.open_fds =>
                {
                    Some("updated")
                }
                Some(_) => None,
            };
            if let Some(change) = change {
                logs.push(
                    ProcessLogKey::Process(process.pid),
                    format!(
                        "sysmon: {change} pid={} name={} vm_kib={} fds={}",
                        process.pid,
                        bounded_log_name(&process.name),
                        process.vm_size_kib,
                        process
                            .open_fds
                            .map(|count| count.to_string())
                            .unwrap_or_else(|| "?".to_string()),
                    ),
                );
            }
        }
        for process in state.processes() {
            if !collection
                .processes
                .iter()
                .any(|new| new.pid == process.pid)
            {
                logs.push(
                    ProcessLogKey::Process(process.pid),
                    format!(
                        "sysmon: process exited pid={} name={}",
                        process.pid,
                        bounded_log_name(&process.name),
                    ),
                );
            }
        }
    }
    state.apply_refresh(result, visible_rows);
    if state.status().starts_with("Error:") || state.status().starts_with("Warning:") {
        logs.push(ProcessLogKey::Status, format!("sysmon: {}", state.status()));
    }
    *state != before
}

fn visible_rows(height: u32) -> usize {
    ((height as i32 - STATUS_HEIGHT as i32 - LIST_TOP).max(ROW_HEIGHT) / ROW_HEIGHT) as usize
}

fn pointer_target(
    state: &MonitorState,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> Option<PointerTarget> {
    if (TOOLBAR_Y..TOOLBAR_Y + TOOLBAR_HEIGHT as i32).contains(&y) {
        return match x {
            6..=91 => Some(PointerTarget::Refresh),
            98..=201 => Some(PointerTarget::Terminate),
            _ if x >= width as i32 - 62 && x < width as i32 - 6 => Some(PointerTarget::Close),
            _ => None,
        };
    }
    let status_top = height as i32 - STATUS_HEIGHT as i32;
    if y < LIST_TOP || y >= status_top || !matches!(state.mode(), MonitorMode::Browse) {
        return None;
    }
    if x >= width as i32 - SCROLLBAR_WIDTH {
        return if y < LIST_TOP + 24 {
            Some(PointerTarget::ScrollUp)
        } else if y >= status_top - 24 {
            Some(PointerTarget::ScrollDown)
        } else {
            None
        };
    }
    let row = ((y - LIST_TOP) / ROW_HEIGHT) as usize;
    Some(PointerTarget::Row(state.scroll().saturating_add(row)))
}

fn paint_sysmon(
    canvas: &mut Canvas<'_>,
    width: u32,
    height: u32,
    state: &MonitorState,
    theme: Theme,
) {
    let background = theme.window_background;
    let titlebar = theme.titlebar_active;
    let toolbar = theme.titlebar_inactive;
    let button = theme.button_fill;
    let selected = theme.border_active;
    let alternate = theme.text_input_bg;
    let text = theme.label_text;
    let muted = theme.text_input_placeholder_fg;

    canvas.fill_rect(Rect::new(0, 0, width, height), background);
    canvas.fill_rect(Rect::new(0, 0, width, TITLEBAR_HEIGHT), titlebar);
    canvas.draw_text(
        8,
        ((TITLEBAR_HEIGHT as i32 - GLYPH_HEIGHT as i32) / 2).max(0),
        "System Monitor",
        theme.titlebar_text_active,
    );

    canvas.fill_rect(Rect::new(0, TOOLBAR_Y, width, TOOLBAR_HEIGHT), toolbar);
    draw_button(canvas, 6, 86, "Refresh (R)", button, text);
    draw_button(
        canvas,
        98,
        104,
        "Terminate (K)",
        if state.terminate_capable() {
            Color::rgb(0xd6, 0xb4, 0xb4)
        } else {
            Color::rgb(0xca, 0xca, 0xca)
        },
        if state.terminate_capable() {
            text
        } else {
            muted
        },
    );
    draw_button(
        canvas,
        width as i32 - 62,
        56,
        "Close",
        theme.close_button,
        text,
    );

    canvas.fill_rect(
        Rect::new(0, HEADER_Y, width, HEADER_HEIGHT),
        theme.button_fill_pressed,
    );
    canvas.draw_text(
        8,
        HEADER_Y + 6,
        "PID    NAME             STATE        PPID    VM KiB    FDS",
        text,
    );

    let rows = visible_rows(height);
    if state.processes().is_empty() {
        canvas.draw_text(10, LIST_TOP + 6, "(no processes visible)", muted);
    }
    for (row, (index, process)) in state
        .processes()
        .iter()
        .enumerate()
        .skip(state.scroll())
        .take(rows)
        .enumerate()
    {
        let y = LIST_TOP + row as i32 * ROW_HEIGHT;
        let is_selected = state.selected_index() == Some(index);
        canvas.fill_rect(
            Rect::new(
                0,
                y,
                width.saturating_sub(SCROLLBAR_WIDTH as u32),
                ROW_HEIGHT as u32,
            ),
            if is_selected {
                selected
            } else if index % 2 == 1 {
                alternate
            } else {
                background
            },
        );
        canvas.draw_text(
            8,
            y + 5,
            &format_process_row(process),
            if is_selected {
                Color::rgb(0xff, 0xff, 0xff)
            } else {
                text
            },
        );
    }

    let status_top = height as i32 - STATUS_HEIGHT as i32;
    draw_scrollbar(
        canvas,
        width,
        status_top,
        state.processes().len(),
        state.scroll(),
        rows,
        toolbar,
        button,
        theme.border_active,
        text,
    );
    canvas.fill_rect(Rect::new(0, status_top, width, STATUS_HEIGHT), toolbar);
    let footer = format!(
        "{} | 1 s live refresh | arrows/page scroll | Ctrl+Q closes",
        state.status()
    );
    canvas.draw_text(
        8,
        status_top + 6,
        &elide(&footer, width.saturating_sub(12)),
        if state.status().starts_with("Error:") || state.status().starts_with("Warning:") {
            Color::rgb(0x9a, 0x18, 0x18)
        } else {
            text
        },
    );

    if let MonitorMode::ConfirmTerminate { pid, name } = state.mode() {
        draw_confirm(canvas, width, height, *pid, name, theme);
    }
}

fn draw_button(
    canvas: &mut Canvas<'_>,
    x: i32,
    width: u32,
    label: &str,
    background: Color,
    foreground: Color,
) {
    canvas.fill_rect(
        Rect::new(x, TOOLBAR_Y + 3, width, TOOLBAR_HEIGHT - 6),
        background,
    );
    canvas.draw_text(x + 5, TOOLBAR_Y + 8, label, foreground);
}

#[allow(clippy::too_many_arguments)]
fn draw_scrollbar(
    canvas: &mut Canvas<'_>,
    width: u32,
    status_top: i32,
    total: usize,
    offset: usize,
    visible: usize,
    track_color: Color,
    button_color: Color,
    thumb_color: Color,
    text_color: Color,
) {
    let x = width as i32 - SCROLLBAR_WIDTH;
    let list_height = (status_top - LIST_TOP).max(48);
    canvas.fill_rect(
        Rect::new(x, LIST_TOP, SCROLLBAR_WIDTH as u32, list_height as u32),
        track_color,
    );
    canvas.fill_rect(
        Rect::new(x + 2, LIST_TOP + 2, SCROLLBAR_WIDTH as u32 - 4, 20),
        button_color,
    );
    canvas.draw_text(x + 7, LIST_TOP + 7, "^", text_color);
    canvas.fill_rect(
        Rect::new(x + 2, status_top - 22, SCROLLBAR_WIDTH as u32 - 4, 20),
        button_color,
    );
    canvas.draw_text(x + 7, status_top - 17, "v", text_color);
    if total <= visible.max(1) {
        return;
    }
    let track_top = LIST_TOP + 24;
    let track_height = (list_height - 48).max(1);
    let thumb_height = ((track_height as usize * visible.max(1)) / total)
        .max(18)
        .min(track_height as usize) as i32;
    let max_scroll = total.saturating_sub(visible.max(1));
    let travel = track_height.saturating_sub(thumb_height);
    let thumb_y =
        track_top + ((offset.min(max_scroll) * travel as usize) / max_scroll.max(1)) as i32;
    canvas.fill_rect(
        Rect::new(
            x + 5,
            thumb_y,
            SCROLLBAR_WIDTH as u32 - 10,
            thumb_height as u32,
        ),
        thumb_color,
    );
}

fn draw_confirm(
    canvas: &mut Canvas<'_>,
    width: u32,
    height: u32,
    pid: u32,
    name: &str,
    theme: Theme,
) {
    let dialog_width = width.saturating_sub(160).max(300);
    let x = ((width.saturating_sub(dialog_width)) / 2) as i32;
    let y = (height as i32 / 2).saturating_sub(54);
    canvas.fill_rect(Rect::new(x, y, dialog_width, 108), theme.titlebar_active);
    canvas.draw_text(
        x + 12,
        y + 14,
        "Terminate process?",
        theme.titlebar_text_active,
    );
    canvas.draw_text(
        x + 12,
        y + 42,
        &elide(&format!("PID {pid}  {name}"), dialog_width - 24),
        theme.titlebar_text_active,
    );
    canvas.draw_text(
        x + 12,
        y + 76,
        "Enter confirms SIGKILL | Esc cancels",
        theme.titlebar_text_inactive,
    );
}

fn elide(value: &str, width: u32) -> String {
    let chars = width.saturating_div(8) as usize;
    if value.chars().count() <= chars {
        return value.to_string();
    }
    if chars <= 3 {
        return value.chars().take(chars).collect();
    }
    let mut out: String = value.chars().take(chars - 3).collect();
    out.push_str("...");
    out
}

fn main() -> ExitCode {
    #[cfg(target_arch = "wasm32")]
    {
        wasm_main::run();
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        run_cli()
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn run_cli() -> ExitCode {
    let proc_root = match parse_args() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("sysmon: {error}");
            return ExitCode::from(1);
        }
    };
    let collection = match collect_processes(&proc_root) {
        Ok(collection) => collection,
        Err(error) => {
            eprintln!("sysmon: {error}");
            return ExitCode::from(1);
        }
    };

    println!(
        "{:<7} {:<16}  {:<11} {:<7} {:>8} {:>5}",
        "PID", "NAME", "STATE", "PPID", "VM KiB", "FDS"
    );
    println!(
        "{:<7} {:<16}  {:<11} {:<7} {:>8} {:>5}",
        "-----", "----------------", "----------", "-----", "--------", "-----"
    );
    for process in &collection.processes {
        println!("{}", format_process_row(process));
    }
    for warning in &collection.warnings {
        eprintln!("sysmon: {warning}: failed to parse status");
    }
    ExitCode::SUCCESS
}

#[cfg(not(target_arch = "wasm32"))]
fn parse_args() -> Result<PathBuf, String> {
    let mut args = std::env::args().skip(1);
    let mut proc_root = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--proc-root" => {
                let value = args
                    .next()
                    .ok_or_else(|| String::from("--proc-root requires a value"))?;
                proc_root = Some(PathBuf::from(value));
            }
            other => return Err(format!("unrecognised argument: {other}")),
        }
    }
    Ok(proc_root.unwrap_or_else(|| PathBuf::from("/proc")))
}

#[cfg(test)]
mod paint_tests {
    use super::*;

    fn process(pid: u32, suffix: &str) -> sysmon::ProcessSnapshot {
        sysmon::ProcessSnapshot {
            pid,
            name: format!("/bin/process-{pid}-{suffix}"),
            state: "Running".to_string(),
            ppid: 1,
            vm_size_kib: u64::from(pid),
            vm_peak_kib: u64::from(pid),
            open_fds: Some(3),
        }
    }

    #[test]
    fn existing_monitor_frame_repaints_from_light_to_dark_palette() {
        let state = MonitorState::new(Some(42), false);
        let mut light = Canvas::new(480, 320);
        let mut dark = Canvas::new(480, 320);

        paint_sysmon(&mut light, 480, 320, &state, Theme::LIGHT);
        paint_sysmon(&mut dark, 480, 320, &state, Theme::DARK);

        assert_eq!(
            light.pixel(300, 10),
            Some(
                &[
                    Theme::LIGHT.titlebar_active.r(),
                    Theme::LIGHT.titlebar_active.g(),
                    Theme::LIGHT.titlebar_active.b(),
                    0xff,
                ][..]
            )
        );
        assert_eq!(
            dark.pixel(300, 10),
            Some(
                &[
                    Theme::DARK.titlebar_active.r(),
                    Theme::DARK.titlebar_active.g(),
                    Theme::DARK.titlebar_active.b(),
                    0xff,
                ][..]
            )
        );
        assert_ne!(light.pixels(), dark.pixels());
    }

    #[test]
    fn fourteen_production_scan_quanta_take_seven_turns_with_at_most_two_each() {
        let mut remaining = 14usize;
        let mut turns = 0usize;
        let mut quanta_per_turn = Vec::new();

        loop {
            turns += 1;
            let before = remaining;
            let completed = advance_process_scan_turn(|| {
                remaining -= 1;
                if remaining == 0 {
                    ProcessScanStep::Complete(ProcessCollection::default())
                } else {
                    ProcessScanStep::Pending
                }
            });
            quanta_per_turn.push(before - remaining);
            if completed.is_some() {
                break;
            }
        }

        assert_eq!(turns, 7);
        assert_eq!(quanta_per_turn, &[2; 7]);
        assert!(quanta_per_turn
            .iter()
            .all(|quanta| *quanta <= PROCESS_SCAN_QUANTA_PER_TURN));
    }

    #[test]
    fn theme_drain_runs_at_normal_scan_start_and_completion() {
        let mut schedule = ThemeDrainSchedule::default();
        let mut drains = 0;

        for _ in 0..6 {
            drains += usize::from(schedule.take_due(true, false));
        }
        drains += usize::from(schedule.take_due(false, true));

        assert_eq!(drains, 2);
    }

    #[test]
    fn long_scan_never_defers_theme_drain_beyond_sixteen_turns() {
        let mut schedule = ThemeDrainSchedule::default();

        assert!(schedule.take_due(true, false));
        for _ in 1..MAX_PENDING_SCAN_TURNS_WITHOUT_THEME_DRAIN {
            assert!(!schedule.take_due(true, false));
        }
        assert!(schedule.take_due(true, false));
        for _ in 1..MAX_PENDING_SCAN_TURNS_WITHOUT_THEME_DRAIN {
            assert!(!schedule.take_due(true, false));
        }
        assert!(schedule.take_due(true, true));
        assert!(!schedule.take_due(true, false));
        assert!(schedule.take_due(false, false));
    }

    #[test]
    fn identical_snapshot_neither_repaints_nor_enqueues_logs() {
        let mut state = MonitorState::new(None, false);
        let mut logs = ProcessLogQueue::default();
        let collection = ProcessCollection {
            processes: (1..=11).map(|pid| process(pid, "stable")).collect(),
            warnings: Vec::new(),
        };

        assert!(refresh_and_log(
            &mut state,
            Ok(collection.clone()),
            19,
            &mut logs,
        ));
        while !logs.is_empty() {
            logs.drain_turn(|_| {});
        }

        assert!(!refresh_and_log(&mut state, Ok(collection), 19, &mut logs,));
        assert!(logs.is_empty());
    }

    #[test]
    fn maximum_process_snapshot_logs_only_one_fixed_quantum_per_gui_turn() {
        let mut state = MonitorState::new(None, false);
        let mut logs = ProcessLogQueue::default();
        let collection = ProcessCollection {
            processes: (1..=256).map(|pid| process(pid, "initial")).collect(),
            warnings: Vec::new(),
        };
        assert!(refresh_and_log(&mut state, Ok(collection), 256, &mut logs,));
        assert_eq!(logs.len(), 256);

        let mut emitted = Vec::new();
        assert_eq!(
            logs.drain_turn(|message| emitted.push(message.to_string())),
            PROCESS_LOGS_PER_TURN,
        );
        assert_eq!(logs.len(), 256 - PROCESS_LOGS_PER_TURN);

        let mut display_turns = 1;
        while !logs.is_empty() {
            display_turns += 1;
            assert!(logs.drain_turn(|message| emitted.push(message.to_string())) > 0);
        }
        assert_eq!(display_turns, 256 / PROCESS_LOGS_PER_TURN);
        assert_eq!(emitted.len(), 256);
        assert!(emitted
            .iter()
            .all(|message| message.starts_with("sysmon: observed pid=")));
    }

    #[test]
    fn full_process_churn_is_bounded_coalesced_and_eventually_drained() {
        let mut state = MonitorState::new(None, false);
        let mut logs = ProcessLogQueue::default();
        let initial = ProcessCollection {
            processes: (1..=256).map(|pid| process(pid, "old")).collect(),
            warnings: Vec::new(),
        };
        refresh_and_log(&mut state, Ok(initial), 256, &mut logs);
        while !logs.is_empty() {
            logs.drain_turn(|_| {});
        }

        let replacement = ProcessCollection {
            processes: (257..=512).map(|pid| process(pid, "new")).collect(),
            warnings: Vec::new(),
        };
        refresh_and_log(&mut state, Ok(replacement), 256, &mut logs);
        assert_eq!(logs.len(), 512);
        assert!(logs.len() <= MAX_PENDING_PROCESS_LOGS);

        let mut emitted = Vec::new();
        let mut turns = 0;
        while !logs.is_empty() {
            turns += 1;
            assert!(logs.drain_turn(|message| emitted.push(message.to_string())) > 0);
        }
        assert_eq!(turns, 512 / PROCESS_LOGS_PER_TURN);
        assert_eq!(emitted.len(), 512);
        assert_eq!(
            emitted
                .iter()
                .filter(|message| message.contains("process exited"))
                .count(),
            256,
        );
        assert_eq!(
            emitted
                .iter()
                .filter(|message| message.contains("observed"))
                .count(),
            256,
        );
    }

    #[test]
    fn repeated_pid_changes_coalesce_to_the_latest_bounded_log() {
        let mut state = MonitorState::new(None, false);
        let mut logs = ProcessLogQueue::default();
        refresh_and_log(
            &mut state,
            Ok(ProcessCollection {
                processes: vec![process(9, "first")],
                warnings: Vec::new(),
            }),
            1,
            &mut logs,
        );
        refresh_and_log(&mut state, Ok(ProcessCollection::default()), 1, &mut logs);

        assert_eq!(logs.len(), 1);
        let mut emitted = Vec::new();
        logs.drain_turn(|message| emitted.push(message.to_string()));
        assert_eq!(emitted.len(), 1);
        assert!(emitted[0].contains("process exited pid=9"));
    }
}
