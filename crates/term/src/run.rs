//! Terminal app event loop.
//!
//! [`run_term`] is the production driver: connect via
//! [`toolkit::App::connect_with_shell`] (which also binds
//! `pmd_seat` + `pmd_keyboard` + `pmd_pointer` when the
//! server advertises them), create a top-level surface, set
//! the title, drive the configure handshake, paint a
//! scrollback buffer per [`crate::rasterizer`], and route
//! `pmd_keyboard.key` events through [`crate::keymap`] into
//! the embedded [`Terminal`].
//!
//! Native isolation tests may use [`Terminal`]'s embedded shell. Production
//! passes a persistent isolated `/bin/sh` runner whose stdin/stdout join the
//! window's readiness wait set; committed lines therefore use the same parser,
//! process spawn, and pipeline implementation as the CLI shell without
//! blocking display dispatch.

use display_proto::events::{key_state, pointer_button_state, KeyboardKey, PointerButton};
use display_proto::Interface;
use toolkit::draw::{Canvas, Color, Rect};
use toolkit::theme::Theme;
use toolkit::widget::frame::{
    PointerOutcome as ChromePointerOutcome, WindowFrame, BORDER_WIDTH, TITLEBAR_HEIGHT,
};
use toolkit::{
    App, BufferPool, ClientError, CommitProgress, Connection, CurrentPatch, WaitFd, Window,
    WindowFramePatch, WindowFramePatchProgress,
};

use crate::keymap::{translate, Modifiers};
use crate::pmos_shell::StepwiseCommandRunner;
use crate::rasterizer::{
    default_font, rasterize_snapshot_region_with_palette_and_font,
    rasterize_snapshot_with_palette_and_font, BitmapFont, Palette, RasterRegion, BYTES_PER_PIXEL,
    PADDING,
};
use crate::terminal::{CommandRunner, Key, KeyFeedResult, Terminal, TerminalOptions};

/// Default window size when the server defers to the client
/// (`configure(0, 0)`).
pub const DEFAULT_WIDTH: u32 = 720;
/// Default window height in the same case.
pub const DEFAULT_HEIGHT: u32 = 480;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PaintRequest {
    #[default]
    None,
    InputEdit,
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct SlotFrame {
    full_generation: Option<u64>,
    input: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingFullFrame {
    buffer_index: usize,
    full_generation: u64,
    input: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputDamage {
    None,
    Rect(Rect),
    Full,
}

struct TerminalPainter {
    pool: Option<BufferPool>,
    size: (u32, u32),
    request: PaintRequest,
    full_generation: u64,
    slots: [SlotFrame; 2],
    pending_full: Option<PendingFullFrame>,
    chrome_patch: Option<WindowFramePatch>,
    focused: bool,
    maximized: bool,
}

impl Default for TerminalPainter {
    fn default() -> Self {
        Self {
            pool: None,
            size: (0, 0),
            request: PaintRequest::None,
            full_generation: 0,
            slots: [SlotFrame::default(), SlotFrame::default()],
            pending_full: None,
            chrome_patch: None,
            focused: false,
            maximized: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalWindowGeometry {
    current: (u32, u32),
    preferred_normal: (u32, u32),
    last_configure: Option<((u32, u32), u32)>,
    configured: bool,
    was_resizing: bool,
}

impl Default for TerminalWindowGeometry {
    fn default() -> Self {
        let preferred = (DEFAULT_WIDTH, DEFAULT_HEIGHT);
        Self {
            current: preferred,
            preferred_normal: preferred,
            last_configure: None,
            configured: false,
            was_resizing: false,
        }
    }
}

impl TerminalWindowGeometry {
    fn observe_configure(&mut self, offered: (u32, u32), states: u32, resizing_seen: bool) -> bool {
        if !resizing_seen && self.last_configure == Some((offered, states)) {
            return false;
        }

        let first_configure = !self.configured;
        let maximized = states & display_proto::xdg_toplevel_state::MAXIMIZED != 0;
        let resizing = states & display_proto::xdg_toplevel_state::RESIZING != 0;
        let explicit_offer = (offered.0 > 0 && offered.1 > 0).then_some(offered);

        let next = if maximized {
            explicit_offer.unwrap_or(self.current)
        } else if resizing_seen || resizing || self.was_resizing {
            if let Some(size) = explicit_offer {
                self.preferred_normal = size;
            }
            self.preferred_normal
        } else {
            let limit = explicit_offer.unwrap_or(self.preferred_normal);
            (
                self.preferred_normal.0.min(limit.0),
                self.preferred_normal.1.min(limit.1),
            )
        };

        let changed = first_configure || self.current != next;
        self.current = next;
        self.last_configure = Some((offered, states));
        self.configured = true;
        self.was_resizing = resizing;
        changed
    }

    const fn size(self) -> (u32, u32) {
        self.current
    }
}

impl TerminalPainter {
    fn observe_chrome(&mut self, focused: bool, maximized: bool) {
        let focus_changed = self.focused != focused;
        let maximized_changed = self.maximized != maximized;
        self.focused = focused;
        self.maximized = maximized;
        if maximized_changed {
            self.request_full();
        } else if focus_changed && self.request != PaintRequest::Full && self.size != (0, 0) {
            self.chrome_patch = Some(WindowFramePatch::new(&terminal_window_frame(
                self.size.0,
                self.size.1,
                self.focused,
                self.maximized,
            )));
        }
    }

    fn request_full(&mut self) {
        self.full_generation = self.full_generation.wrapping_add(1);
        self.request = PaintRequest::Full;
        self.chrome_patch = None;
    }

    fn request_input_edit(&mut self) {
        if self.request == PaintRequest::None {
            self.request = PaintRequest::InputEdit;
        }
    }

    fn commit_pending(&self) -> bool {
        self.pool.as_ref().is_some_and(BufferPool::commit_pending)
    }

    fn has_local_work(&self) -> bool {
        if self.commit_pending() {
            return true;
        }
        if self.chrome_patch.is_some() {
            return true;
        }
        match self.request {
            PaintRequest::None => false,
            PaintRequest::Full => self
                .pool
                .as_ref()
                .is_none_or(|pool| !pool.is_in_use(pool.back_index())),
            PaintRequest::InputEdit => self.pool.as_ref().is_none_or(|pool| {
                pool.current_index().is_some() || !pool.is_in_use(pool.back_index())
            }),
        }
    }

    fn handle_release(&mut self, buffer_id: display_proto::ObjectId) {
        if let Some(pool) = self.pool.as_mut() {
            let _ = pool.handle_release(buffer_id);
        }
    }

    fn progress<C: Connection>(&mut self, window: &mut Window<'_, C>) -> Result<(), ClientError> {
        let Some(pool) = self.pool.as_mut().filter(|pool| pool.commit_pending()) else {
            return Ok(());
        };
        if pool.progress_commit(window)? == CommitProgress::Committed {
            self.finish_full_commit();
        }
        Ok(())
    }

    fn finish_full_commit(&mut self) {
        let Some(frame) = self.pending_full.take() else {
            return;
        };
        self.slots[frame.buffer_index] = SlotFrame {
            full_generation: Some(frame.full_generation),
            input: frame.input,
        };
    }

    fn paint_if_needed<C: Connection>(
        &mut self,
        window: &mut Window<'_, C>,
        terminal: &Terminal,
        font: &BitmapFont,
        size: (u32, u32),
    ) -> Result<(), ClientError> {
        if (self.request == PaintRequest::None && self.chrome_patch.is_none())
            || self.commit_pending()
        {
            return Ok(());
        }
        if self.size != size {
            BufferPool::replace(&mut self.pool, window.app_mut(), size.0, size.1)?;
            self.size = size;
            self.slots = [SlotFrame::default(), SlotFrame::default()];
            self.pending_full = None;
            self.chrome_patch = None;
            self.request = PaintRequest::Full;
        }

        // Make the focused edge visible with the first bounded chrome tile,
        // then let latency-sensitive terminal input patches preempt the
        // remaining cosmetic titlebar/border tiles.
        if self
            .chrome_patch
            .as_ref()
            .is_some_and(|patch| patch.completed_tiles() == 0)
        {
            return self.progress_chrome(window);
        }

        match self.request {
            PaintRequest::None => self.progress_chrome(window),
            PaintRequest::Full => self.paint_full(window, terminal, font),
            PaintRequest::InputEdit => self.paint_input_edit(window, terminal, font),
        }
    }

    fn progress_chrome<C: Connection>(
        &mut self,
        window: &mut Window<'_, C>,
    ) -> Result<(), ClientError> {
        let Some(patch) = self.chrome_patch.as_mut() else {
            return Ok(());
        };
        let pool = self
            .pool
            .as_mut()
            .expect("chrome patch requires paint pool");
        match patch.progress(pool, window)? {
            WindowFramePatchProgress::Complete => self.chrome_patch = None,
            WindowFramePatchProgress::Unavailable => {
                self.chrome_patch = None;
                self.request_full();
            }
            WindowFramePatchProgress::Deferred | WindowFramePatchProgress::Pending => {}
        }
        Ok(())
    }

    fn paint_full<C: Connection>(
        &mut self,
        window: &mut Window<'_, C>,
        terminal: &Terminal,
        font: &BitmapFont,
    ) -> Result<(), ClientError> {
        let pool = self.pool.as_mut().expect("paint pool initialized");
        let buffer_index = pool.back_index();
        if pool.is_in_use(buffer_index) {
            return Ok(());
        }
        let Some(mut canvas) = pool.acquire_back_canvas() else {
            return Ok(());
        };
        paint_into_canvas(
            &mut canvas,
            self.size.0,
            self.size.1,
            terminal,
            font,
            self.focused,
            self.maximized,
        );
        drop(canvas);

        let frame = PendingFullFrame {
            buffer_index,
            full_generation: self.full_generation,
            input: terminal.input_buffer().to_string(),
        };
        let progress = pool.commit_and_swap(window)?;
        self.request = PaintRequest::None;
        self.pending_full = Some(frame);
        if progress == CommitProgress::Committed {
            self.finish_full_commit();
        }
        Ok(())
    }

    fn paint_input_edit<C: Connection>(
        &mut self,
        window: &mut Window<'_, C>,
        terminal: &Terminal,
        font: &BitmapFont,
    ) -> Result<(), ClientError> {
        let pool = self.pool.as_mut().expect("paint pool initialized");
        if pool.commit_pending() || window.outbound_pending() {
            return Ok(());
        }
        let Some(buffer_index) = pool.current_index() else {
            self.request = PaintRequest::Full;
            return self.paint_full(window, terminal, font);
        };
        let current = &self.slots[buffer_index];
        if current.full_generation != Some(self.full_generation) {
            self.request = PaintRequest::Full;
            return self.paint_full(window, terminal, font);
        }

        let desired_input = terminal.input_buffer();
        let damage = input_edit_damage(
            terminal.prompt(),
            &current.input,
            desired_input,
            self.size.0,
            self.size.1,
            font,
        );
        let damage = match damage {
            InputDamage::None => {
                self.slots[buffer_index].input = desired_input.to_string();
                self.request = PaintRequest::None;
                return Ok(());
            }
            InputDamage::Full => {
                self.request = PaintRequest::Full;
                return self.paint_full(window, terminal, font);
            }
            InputDamage::Rect(damage) => damage,
        };
        let Some(packed) = packed_damage_pixels(terminal, font, self.size.0, self.size.1, damage)
        else {
            self.request = PaintRequest::Full;
            return self.paint_full(window, terminal, font);
        };
        if packed.len() > display_proto::MAX_SURFACE_PATCH_BYTES {
            self.request = PaintRequest::Full;
            return self.paint_full(window, terminal, font);
        }

        match pool.patch_current(window, damage, &packed)? {
            CurrentPatch::Patched {
                buffer_index: patched,
            } => {
                debug_assert_eq!(patched, buffer_index);
                self.slots[patched].input = desired_input.to_string();
                self.request = PaintRequest::None;
            }
            CurrentPatch::Deferred { .. } => {}
            CurrentPatch::Unavailable => {
                self.request = PaintRequest::Full;
                return self.paint_full(window, terminal, font);
            }
        }
        Ok(())
    }
}

/// Result of running [`run_term`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermExit {
    /// The server requested the window close via
    /// `xdg_toplevel::close`.
    CloseRequested,
    /// The embedded shell's `exit` builtin fired.
    ShellExited,
    /// `max_dispatch_iterations` exhausted without exit.
    IterationLimit,
}

/// Connect to the display server, drive a terminal window
/// against it, and route keystrokes through the embedded
/// [`Terminal`] until the user closes the window or the
/// shell exits.
///
/// `max_dispatch_iterations` caps the loop so unit tests can
/// assert non-hanging behaviour. Production passes
/// `u32::MAX`.
pub fn run_term<C: Connection>(
    connection: C,
    max_dispatch_iterations: u32,
) -> Result<TermExit, ClientError> {
    run_term_with_options(
        connection,
        max_dispatch_iterations,
        TerminalOptions {
            max_lines: 1024,
            banner: vec![
                "PMos Terminal".to_string(),
                "Type a command and press Enter. `help` for builtins, `exit` to quit.".to_string(),
            ],
            prompt: "$ ".to_string(),
        },
    )
}

/// Variant of [`run_term`] that accepts pre-baked
/// [`TerminalOptions`] — used by tests to control the
/// banner / prompt without inheriting the production
/// defaults.
pub fn run_term_with_options<C: Connection>(
    connection: C,
    max_dispatch_iterations: u32,
    options: TerminalOptions,
) -> Result<TermExit, ClientError> {
    run_term_inner(
        connection,
        max_dispatch_iterations,
        options,
        None,
        None,
        default_font(),
    )
}

/// [`run_term_with_options`] with an explicit startup font.
pub fn run_term_with_font<C: Connection>(
    connection: C,
    max_dispatch_iterations: u32,
    options: TerminalOptions,
    font: &BitmapFont,
) -> Result<TermExit, ClientError> {
    run_term_inner(
        connection,
        max_dispatch_iterations,
        options,
        None,
        None,
        font,
    )
}

/// Run the graphical terminal with a persistent out-of-process shell.
pub fn run_term_with_runner<C: Connection>(
    connection: C,
    max_dispatch_iterations: u32,
    options: TerminalOptions,
    runner: &mut dyn CommandRunner,
) -> Result<TermExit, ClientError> {
    run_term_inner(
        connection,
        max_dispatch_iterations,
        options,
        Some(runner),
        None,
        default_font(),
    )
}

/// Production runner variant with an explicit startup font.
pub fn run_term_with_runner_and_font<C: Connection>(
    connection: C,
    max_dispatch_iterations: u32,
    options: TerminalOptions,
    runner: &mut dyn CommandRunner,
    font: &BitmapFont,
) -> Result<TermExit, ClientError> {
    run_term_inner(
        connection,
        max_dispatch_iterations,
        options,
        Some(runner),
        None,
        font,
    )
}

/// Production event-loop variant. Commands start without waiting for a prompt;
/// shell stdout and any backpressured stdin join the display wait set.
pub fn run_term_with_stepwise_runner_and_font<C: Connection>(
    connection: C,
    max_dispatch_iterations: u32,
    options: TerminalOptions,
    runner: &mut dyn StepwiseCommandRunner,
    font: &BitmapFont,
) -> Result<TermExit, ClientError> {
    run_term_inner(
        connection,
        max_dispatch_iterations,
        options,
        None,
        Some(runner),
        font,
    )
}

fn run_term_inner<C: Connection>(
    connection: C,
    max_dispatch_iterations: u32,
    options: TerminalOptions,
    mut runner: Option<&mut dyn CommandRunner>,
    mut stepwise_runner: Option<&mut dyn StepwiseCommandRunner>,
    font: &BitmapFont,
) -> Result<TermExit, ClientError> {
    let mut app = App::connect_with_shell(connection)?;
    let mut window = Window::new(&mut app)?;
    window.set_title("Terminal")?;
    window.set_app_id("pmos.term")?;
    window.commit()?;

    let mut terminal = Terminal::new(options);
    let mut mods = Modifiers::default();

    let mut painter = TerminalPainter::default();
    let mut geometry = TerminalWindowGeometry::default();
    let mut stepwise_busy = false;
    let mut stepwise_ready = stepwise_runner
        .as_deref()
        .map(StepwiseCommandRunner::is_ready)
        .unwrap_or(true);

    for _ in 0..max_dispatch_iterations {
        let events = window.dispatch()?;

        if window.is_configured()
            && geometry.observe_configure(
                window.configured_size(),
                window.states(),
                window.resizing_seen_in_last_dispatch(),
            )
        {
            painter.request_full();
        }
        painter.observe_chrome(window.is_activated(), window.is_maximized());

        if window.close_requested() {
            if let Some(command_runner) = stepwise_runner.as_deref_mut() {
                command_runner.terminate();
            }
            return Ok(TermExit::CloseRequested);
        }

        let mut stepwise_exited = false;
        if let Some(command_runner) = stepwise_runner.as_deref_mut() {
            if !stepwise_exited {
                if command_runner.flush_input().is_err() {
                    command_runner.terminate();
                    terminal.append_output(b"term: shell input transport failed\n");
                    terminal.finish_external_output();
                    stepwise_busy = false;
                    stepwise_exited = true;
                    painter.request_full();
                } else {
                    let update = command_runner.drain_output();
                    stepwise_ready |= update.ready || command_runner.is_ready();
                    if !update.output.is_empty() {
                        terminal.append_output(&update.output);
                        painter.request_full();
                    }
                    if update.completed {
                        terminal.finish_external_output();
                        stepwise_busy = false;
                        painter.request_full();
                    }
                    stepwise_exited = update.exited;
                }
            }
        }

        for event in events {
            match (event.interface, event.opcode) {
                (Interface::Keyboard, 1 /* key */) => {
                    if let Ok(key) = KeyboardKey::decode(&event.payload) {
                        let pressed = key.state == key_state::PRESSED;
                        let was_modifier = mods.update(key.key, pressed);
                        if !pressed || was_modifier {
                            continue;
                        }
                        let Some(translated) = translate(key.key, mods) else {
                            continue;
                        };
                        let feed_result = if let Some(command_runner) =
                            stepwise_runner.as_deref_mut()
                        {
                            if stepwise_busy {
                                let input_result = match translated {
                                    Key::Char(ch) => {
                                        let mut encoded = [0u8; 4];
                                        command_runner
                                            .send_input(ch.encode_utf8(&mut encoded).as_bytes())
                                    }
                                    Key::Enter => command_runner.send_input(b"\n"),
                                    // PMos pipes deliberately do not pretend to
                                    // provide PTY line editing/job control.
                                    Key::Backspace => Ok(()),
                                };
                                if input_result.is_err() {
                                    command_runner.terminate();
                                    terminal.append_output(b"term: shell input transport failed\n");
                                    terminal.finish_external_output();
                                    stepwise_busy = false;
                                    stepwise_exited = true;
                                    KeyFeedResult::Edited
                                } else {
                                    KeyFeedResult::Ignored
                                }
                            } else if translated == Key::Enter {
                                if stepwise_ready {
                                    let line = terminal.begin_external_command();
                                    if command_runner.start_command(&line).is_ok() {
                                        stepwise_busy = true;
                                        KeyFeedResult::Edited
                                    } else {
                                        command_runner.terminate();
                                        terminal.append_output(
                                            b"term: shell command transport failed\n",
                                        );
                                        terminal.finish_external_output();
                                        stepwise_exited = true;
                                        KeyFeedResult::Edited
                                    }
                                } else {
                                    // Keep the edit buffer intact until the
                                    // real shell's startup marker arrives.
                                    KeyFeedResult::Ignored
                                }
                            } else {
                                terminal.feed_key(translated)
                            }
                        } else {
                            match runner.as_deref_mut() {
                                Some(command_runner) => {
                                    terminal.feed_key_with_runner(translated, command_runner)
                                }
                                None => terminal.feed_key(translated),
                            }
                        };
                        match feed_result {
                            KeyFeedResult::Edited => {
                                if translated == Key::Enter || stepwise_exited {
                                    painter.request_full();
                                } else {
                                    painter.request_input_edit();
                                }
                            }
                            KeyFeedResult::Committed { exited, .. } => {
                                painter.request_full();
                                if exited {
                                    // Paint the final state
                                    // before bailing so the
                                    // user sees the last
                                    // line of output.
                                    if !painter.commit_pending() {
                                        painter.paint_if_needed(
                                            &mut window,
                                            &terminal,
                                            font,
                                            geometry.size(),
                                        )?;
                                    }
                                    return Ok(TermExit::ShellExited);
                                }
                            }
                            KeyFeedResult::Ignored => {}
                        }
                    }
                }
                (Interface::Pointer, 2 /* button */) => {
                    if let Ok(button) = PointerButton::decode(&event.payload) {
                        if button.button != 1 || button.state != pointer_button_state::PRESSED {
                            continue;
                        }
                        let mut chrome = WindowFrame::new(
                            Rect::new(0, 0, geometry.size().0, geometry.size().1),
                            "Terminal",
                        );
                        chrome.set_theme(Theme::DARK);
                        chrome.set_maximized(window.is_maximized());
                        match chrome.pointer_down(button.x, button.y) {
                            ChromePointerOutcome::Minimize => window.set_minimized()?,
                            ChromePointerOutcome::ToggleMaximize => {
                                if window.is_maximized() {
                                    window.unset_maximized()?;
                                } else {
                                    window.set_maximized()?;
                                }
                            }
                            ChromePointerOutcome::Close => {
                                if let Some(command_runner) = stepwise_runner.as_deref_mut() {
                                    command_runner.terminate();
                                }
                                return Ok(TermExit::CloseRequested);
                            }
                            ChromePointerOutcome::Titlebar if !window.is_maximized() => {
                                window.request_move(button.serial)?;
                            }
                            ChromePointerOutcome::Titlebar
                            | ChromePointerOutcome::Content
                            | ChromePointerOutcome::Outside => {}
                        }
                    }
                }
                (Interface::Buffer, 1 /* release */) => {
                    painter.handle_release(event.object_id);
                }
                _ => {}
            }
        }

        painter.progress(&mut window)?;
        if window.is_configured() {
            painter.paint_if_needed(&mut window, &terminal, font, geometry.size())?;
        }
        if stepwise_exited {
            return Ok(TermExit::ShellExited);
        }
        window.flush_outbound()?;
        if painter.has_local_work() && !window.outbound_pending() {
            continue;
        }
        if stepwise_runner.is_some() {
            let command_runner = stepwise_runner
                .as_deref_mut()
                .expect("stepwise wait owns a runner");
            let mut wait_fds = Vec::with_capacity(3);
            if let Some(fd) = command_runner.readable_output_fd() {
                wait_fds.push(WaitFd::readable(fd as i32));
            }
            if let Some(fd) = command_runner.signal_fd() {
                wait_fds.push(WaitFd::readable(fd as i32));
            }
            if let Some(fd) = command_runner.writable_input_fd() {
                wait_fds.push(WaitFd::writable(fd as i32));
            }
            window.wait_with(&wait_fds, None)?;
        } else {
            window.wait(None)?;
        }
    }

    if let Some(command_runner) = stepwise_runner {
        command_runner.terminate();
    }

    Ok(TermExit::IterationLimit)
}

fn input_edit_damage(
    prompt: &str,
    old_input: &str,
    new_input: &str,
    width: u32,
    height: u32,
    font: &BitmapFont,
) -> InputDamage {
    let content_height = terminal_content_height(height);
    if old_input == new_input || width <= 2 * PADDING || content_height <= 2 * PADDING {
        return InputDamage::None;
    }
    let cell_width = font.cell_width();
    let cell_height = font.cell_height();
    if cell_width == 0 || cell_height == 0 {
        return InputDamage::Full;
    }
    let cols = ((width - 2 * PADDING) / cell_width) as usize;
    let rows = (content_height - 2 * PADDING) / cell_height;
    if cols == 0 || rows == 0 {
        return InputDamage::None;
    }

    let old_chars = prompt.chars().chain(old_input.chars());
    let new_chars = prompt.chars().chain(new_input.chars());
    let common = old_chars
        .clone()
        .zip(new_chars.clone())
        .take_while(|(old, new)| old == new)
        .count();
    let old_len = old_chars.count();
    let new_len = new_chars.count();
    let occupied_end = |len: usize| {
        if len < cols {
            len + 1 // fixed cursor cell
        } else {
            cols
        }
    };
    let start_col = common.min(cols);
    let end_col = occupied_end(old_len).max(occupied_end(new_len));
    if start_col >= end_col {
        return InputDamage::None;
    }

    let x = PADDING as u64 + start_col as u64 * u64::from(cell_width);
    let y =
        u64::from(TITLEBAR_HEIGHT) + PADDING as u64 + u64::from(rows - 1) * u64::from(cell_height);
    let damage_width = (end_col - start_col) as u64 * u64::from(cell_width);
    let Ok(x) = i32::try_from(x) else {
        return InputDamage::Full;
    };
    let Ok(y) = i32::try_from(y) else {
        return InputDamage::Full;
    };
    let Ok(damage_width) = u32::try_from(damage_width) else {
        return InputDamage::Full;
    };
    let packed_bytes = u64::from(damage_width)
        .checked_mul(u64::from(cell_height))
        .and_then(|pixels| pixels.checked_mul(BYTES_PER_PIXEL as u64));
    if packed_bytes.is_none_or(|bytes| bytes > display_proto::MAX_SURFACE_PATCH_BYTES as u64) {
        return InputDamage::Full;
    }
    InputDamage::Rect(Rect::new(x, y, damage_width, cell_height))
}

fn packed_damage_pixels(
    terminal: &Terminal,
    font: &BitmapFont,
    width: u32,
    height: u32,
    damage: Rect,
) -> Option<Vec<u8>> {
    let content_height = terminal_content_height(height);
    let content_x = u32::try_from(damage.x).ok()?;
    let content_y = u32::try_from(damage.y).ok()?.checked_sub(TITLEBAR_HEIGHT)?;
    rasterize_snapshot_region_with_palette_and_font(
        &terminal.snapshot(),
        width,
        content_height,
        RasterRegion::new(content_x, content_y, damage.width, damage.height),
        Palette::default(),
        font,
    )
}

const fn terminal_content_height(height: u32) -> u32 {
    height.saturating_sub(TITLEBAR_HEIGHT + BORDER_WIDTH)
}

/// Rasterise the terminal snapshot beneath the shared client-side chrome.
fn paint_into_canvas(
    canvas: &mut Canvas<'_>,
    width: u32,
    height: u32,
    terminal: &Terminal,
    font: &BitmapFont,
    focused: bool,
    maximized: bool,
) {
    // Background fill is performed by the rasterizer itself;
    // the canvas wipe below is defensive — any pixel the
    // rasterizer skipped (out-of-bounds glyph row, padding
    // edge) lands at a known colour rather than garbage from
    // a previous frame still resident in the back buffer.
    canvas.clear(Color::rgb(0x0a, 0x0e, 0x14));
    let snapshot = terminal.snapshot();
    let content_height = terminal_content_height(height);
    let bytes = rasterize_snapshot_with_palette_and_font(
        &snapshot,
        width,
        content_height,
        Palette::default(),
        font,
    );
    {
        let pixels = canvas.pixels_mut();
        let destination_start = (TITLEBAR_HEIGHT as usize)
            .saturating_mul(width as usize)
            .saturating_mul(BYTES_PER_PIXEL)
            .min(pixels.len());
        let copy_len = pixels
            .len()
            .saturating_sub(destination_start)
            .min(bytes.len());
        pixels[destination_start..destination_start + copy_len].copy_from_slice(&bytes[..copy_len]);
    }
    debug_assert_eq!(
        bytes.len(),
        (width as usize) * (content_height as usize) * BYTES_PER_PIXEL
    );

    let chrome = terminal_window_frame(width, height, focused, maximized);
    chrome.draw(canvas);
}

fn terminal_window_frame(width: u32, height: u32, focused: bool, maximized: bool) -> WindowFrame {
    let mut chrome = WindowFrame::new(Rect::new(0, 0, width, height), "Terminal");
    chrome.set_theme(Theme::DARK);
    chrome.set_focused(focused);
    chrome.set_maximized(maximized);
    chrome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_backspace_damage_only_the_old_and_new_cursor_cells() {
        let font = default_font();
        let expected = Rect::new(20, 460, 16, 14);

        assert_eq!(
            input_edit_damage("$ ", "", "a", DEFAULT_WIDTH, DEFAULT_HEIGHT, font),
            InputDamage::Rect(expected),
        );
        assert_eq!(
            input_edit_damage("$ ", "a", "", DEFAULT_WIDTH, DEFAULT_HEIGHT, font),
            InputDamage::Rect(expected),
        );
        assert_eq!(
            expected.width as usize * expected.height as usize * BYTES_PER_PIXEL,
            896,
        );
    }

    #[test]
    fn packed_input_damage_matches_full_content_crop_without_full_sized_output() {
        let font = default_font();
        let mut terminal = Terminal::new(TerminalOptions {
            max_lines: 8,
            banner: Vec::new(),
            prompt: "$ ".to_string(),
        });
        assert_eq!(terminal.feed_key(Key::Char('a')), KeyFeedResult::Edited);
        let damage = match input_edit_damage("$ ", "", "a", DEFAULT_WIDTH, DEFAULT_HEIGHT, font) {
            InputDamage::Rect(damage) => damage,
            other => panic!("expected bounded input damage, got {other:?}"),
        };
        let bounded = packed_damage_pixels(&terminal, font, DEFAULT_WIDTH, DEFAULT_HEIGHT, damage)
            .expect("damage is inside terminal content");
        let content_height = terminal_content_height(DEFAULT_HEIGHT);
        let full = rasterize_snapshot_with_palette_and_font(
            &terminal.snapshot(),
            DEFAULT_WIDTH,
            content_height,
            Palette::default(),
            font,
        );
        let row_bytes = damage.width as usize * BYTES_PER_PIXEL;
        let stride = DEFAULT_WIDTH as usize * BYTES_PER_PIXEL;
        let x = damage.x as usize;
        let y = (damage.y - TITLEBAR_HEIGHT as i32) as usize;
        let mut expected = Vec::with_capacity(row_bytes * damage.height as usize);
        for row in 0..damage.height as usize {
            let start = (y + row) * stride + x * BYTES_PER_PIXEL;
            expected.extend_from_slice(&full[start..start + row_bytes]);
        }

        assert_eq!(bounded, expected);
        assert_eq!(bounded.len(), 896);
        assert_eq!(full.len(), 1_316_160);
    }

    #[test]
    fn invisible_or_oversize_coalesced_input_chooses_safe_fallback() {
        let font = default_font();
        let offscreen = "a".repeat(100);
        let mut farther_offscreen = offscreen.clone();
        farther_offscreen.push('b');
        assert_eq!(
            input_edit_damage(
                "$ ",
                &offscreen,
                &farther_offscreen,
                DEFAULT_WIDTH,
                DEFAULT_HEIGHT,
                font,
            ),
            InputDamage::None,
        );

        assert_eq!(
            input_edit_damage(
                "$ ",
                "",
                &"a".repeat(60),
                DEFAULT_WIDTH,
                DEFAULT_HEIGHT,
                font,
            ),
            InputDamage::Full,
        );
    }

    #[test]
    fn tiny_surfaces_paint_chrome_without_slicing_past_the_canvas() {
        let terminal = Terminal::new(TerminalOptions {
            max_lines: 8,
            banner: Vec::new(),
            prompt: "$ ".to_string(),
        });
        for (width, height) in [(1, 1), (32, TITLEBAR_HEIGHT)] {
            let mut canvas = Canvas::new(width, height);
            paint_into_canvas(
                &mut canvas,
                width,
                height,
                &terminal,
                default_font(),
                true,
                false,
            );
            assert_eq!(canvas.pixels().len(), width as usize * height as usize * 4);
        }
    }

    #[test]
    fn ordinary_work_area_offer_keeps_preferred_normal_geometry() {
        let mut geometry = TerminalWindowGeometry::default();

        assert!(geometry.observe_configure((1024, 736), 0, false));
        assert_eq!(geometry.size(), (DEFAULT_WIDTH, DEFAULT_HEIGHT));
        assert!(!geometry.observe_configure((1024, 736), 0, false));

        assert!(geometry.observe_configure((640, 400), 0, false));
        assert_eq!(geometry.size(), (640, 400));
    }

    #[test]
    fn maximize_uses_offer_and_restore_recovers_normal_geometry() {
        let mut geometry = TerminalWindowGeometry::default();
        geometry.observe_configure((1024, 736), 0, false);

        assert!(geometry.observe_configure(
            (1024, 736),
            display_proto::xdg_toplevel_state::MAXIMIZED,
            false,
        ));
        assert_eq!(geometry.size(), (1024, 736));

        assert!(geometry.observe_configure((0, 0), 0, false));
        assert_eq!(geometry.size(), (DEFAULT_WIDTH, DEFAULT_HEIGHT));
    }

    #[test]
    fn resize_updates_the_normal_geometry_restored_after_maximize() {
        let mut geometry = TerminalWindowGeometry::default();
        geometry.observe_configure((1024, 736), 0, false);

        assert!(geometry.observe_configure(
            (840, 540),
            display_proto::xdg_toplevel_state::RESIZING,
            true,
        ));
        assert_eq!(geometry.size(), (840, 540));
        assert!(!geometry.observe_configure((840, 540), 0, false));

        assert!(geometry.observe_configure(
            (1024, 736),
            display_proto::xdg_toplevel_state::MAXIMIZED,
            false,
        ));
        assert!(geometry.observe_configure((0, 0), 0, false));
        assert_eq!(geometry.size(), (840, 540));
    }

    #[test]
    fn resize_seen_before_a_batched_final_configure_is_not_lost() {
        let mut geometry = TerminalWindowGeometry::default();
        geometry.observe_configure((900, 560), 0, false);
        assert_eq!(geometry.size(), (DEFAULT_WIDTH, DEFAULT_HEIGHT));

        assert!(geometry.observe_configure((900, 560), 0, true));
        assert_eq!(geometry.size(), (900, 560));
    }
}
