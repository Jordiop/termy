//! `SessionManager`: every session owns its PTY, an independent
//! `alacritty_terminal::Term` fed through `vte::ansi::Processor`, its cwd and
//! detected project (Git root), an activity state (`Working` / `Waiting` /
//! `Done`) and attention flags. Closed sessions are archived to disk and can be
//! restored from the sidebar.
//!
//! Threading: the PTY reader thread parses output straight into the `Term`
//! under a short-lived mutex and then calls `wake` (coalesced through a dirty
//! flag), so the UI thread never waits on PTY I/O; it only locks the `Term`
//! briefly to take a [`GridSnapshot`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::{Config as TermConfig, Osc52, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Color, CursorShape as VteCursorShape, CursorStyle, NamedColor, Processor, Rgb as VteRgb,
};
use crossbeam_channel::Sender;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::Wake;
use crate::config::{AgentProfile, AppConfig, CursorShape, expand_tilde};
use crate::pty::{ExitInfo, PtyHandle, SpawnOptions, default_shell};
use crate::theme::{Rgb, Theme};

pub type SessionId = u64;

/// Tab id of the usage dashboard: a workspace tab with no terminal behind it.
/// Never handed out by [`SessionManager`], so `get(USAGE_TAB)` is always `None`.
pub const USAGE_TAB: SessionId = SessionId::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ActivityState {
    /// Produced output within the activity timeout.
    Working,
    /// Alive and quiet: waiting for input or approval.
    Waiting,
    /// The process exited.
    Done,
}

impl ActivityState {
    pub fn label(self) -> &'static str {
        match self {
            ActivityState::Working => "Working",
            ActivityState::Waiting => "Waiting",
            ActivityState::Done => "Done",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SessionFilter {
    #[default]
    All,
    Waiting,
    Working,
}

// ---------------------------------------------------------------------------
// Project detection & command building
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Project {
    pub name: String,
    pub root: PathBuf,
}

/// Nearest ancestor containing `.git` (dir, or file for worktrees/submodules);
/// otherwise the directory itself.
pub fn detect_project(cwd: &Path) -> Project {
    let root = cwd
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .unwrap_or(cwd)
        .to_path_buf();
    let name = if dirs::home_dir().as_deref() == Some(root.as_path()) {
        "~".to_owned()
    } else {
        root.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string())
    };
    Project { name, root }
}

/// `"/usr/local/bin/claude"`, `"pwsh.exe"` → `"claude"`, `"pwsh"`.
fn program_name(program: &str) -> String {
    let base = program.rsplit(['/', '\\']).next().unwrap_or(program);
    base.strip_suffix(".exe").unwrap_or(base).to_lowercase()
}

/// Builds the command for a session. Agents are launched through the user's
/// login shell on Unix: GUI apps start with a minimal `PATH`, and that's
/// where `claude`/`codex`/`gemini` usually aren't. `exec` keeps the agent as
/// the PTY's main process, so its exit ends the session.
pub fn build_command(profile: Option<&AgentProfile>, shell: (String, Vec<String>)) -> (String, Vec<String>) {
    let Some(profile) = profile else { return shell };
    let (shell_program, _) = shell;
    if cfg!(windows) {
        return (profile.command.clone(), profile.args.clone());
    }
    let mut args = vec!["-l".to_owned(), "-c".to_owned()];
    match program_name(&shell_program).as_str() {
        "zsh" | "bash" | "sh" | "dash" | "ksh" => args.push(r#"exec "$0" "$@""#.to_owned()),
        "fish" => args.push("exec $argv".to_owned()),
        // Unknown shell syntax: run the agent directly.
        _ => return (profile.command.clone(), profile.args.clone()),
    }
    args.push(profile.command.clone());
    args.extend(profile.args.iter().cloned());
    (shell_program, args)
}

/// Strips spinner glyphs and status markers agents put in front of titles
/// (`"✳ Fix logos"`, `"⠂ Working"`).
pub fn clean_title(raw: &str) -> String {
    raw.trim_start_matches(|c: char| !c.is_alphanumeric() && c != '~' && c != '/' && c != '.')
        .trim()
        .to_owned()
}

/// `27s`, `3m`, `1h`, `2d` (as in the sidebar).
pub fn format_elapsed(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3_600 => format!("{}m", s / 60),
        3_600..86_400 => format!("{}h", s / 3_600),
        _ => format!("{}d", s / 86_400),
    }
}

fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Terminal state shared with the PTY threads
// ---------------------------------------------------------------------------

/// Grid size as seen by `alacritty_terminal`.
#[derive(Clone, Copy, Debug)]
struct GridSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Default colors answered to OSC 4/10/11/12 queries (TUIs use them to pick
/// a light or dark palette).
#[derive(Clone, Copy, Debug)]
struct QueryPalette {
    foreground: Rgb,
    background: Rgb,
    cursor: Rgb,
    ansi: [Rgb; 16],
}

impl QueryPalette {
    fn from_theme(theme: &Theme) -> Self {
        Self {
            foreground: theme.colors.foreground,
            background: theme.colors.background,
            cursor: theme.colors.cursor,
            ansi: std::array::from_fn(|i| theme.ansi(i)),
        }
    }
}

/// State touched from the PTY threads, the `Term` listener and the UI.
struct Shared {
    pty_input: OnceLock<Sender<Vec<u8>>>,
    title: Mutex<Option<String>>,
    clipboard_request: Mutex<Option<String>>,
    palette: Mutex<QueryPalette>,
    window_size: Mutex<WindowSize>,
    exit: Mutex<Option<ExitInfo>>,
    attention: AtomicBool,
    dirty: AtomicBool,
    /// Milliseconds since `epoch` of the last PTY output (0 = never).
    last_output_ms: AtomicU64,
    epoch: Instant,
    wake: Wake,
}

impl Shared {
    fn mark_dirty(&self) {
        // Coalesce: only wake the UI on the clean → dirty transition.
        if !self.dirty.swap(true, Ordering::AcqRel) {
            (self.wake)();
        }
    }

    fn reply(&self, text: impl Into<Vec<u8>>) {
        if let Some(tx) = self.pty_input.get() {
            let _ = tx.send(text.into());
        }
    }
}

/// `Term` event sink (opaque; exposed only through [`Session::with_term`]).
#[derive(Clone)]
pub struct Listener(Arc<Shared>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        let shared = &self.0;
        match event {
            TermEvent::PtyWrite(text) => shared.reply(text),
            TermEvent::Title(title) => *shared.title.lock() = Some(title),
            TermEvent::ResetTitle => *shared.title.lock() = None,
            TermEvent::Bell => shared.attention.store(true, Ordering::Release),
            TermEvent::ClipboardStore(_, text) => *shared.clipboard_request.lock() = Some(text),
            TermEvent::ColorRequest(index, format) => {
                let p = *shared.palette.lock();
                let rgb = match index {
                    0..=15 => p.ansi[index],
                    256 => p.foreground,
                    257 => p.background,
                    258 => p.cursor,
                    _ => return,
                };
                shared.reply(format(VteRgb { r: rgb.r, g: rgb.g, b: rgb.b }));
            }
            TermEvent::TextAreaSizeRequest(format) => {
                let size = *shared.window_size.lock();
                shared.reply(format(size));
            }
            // OSC 52 reads are disabled (`Osc52::OnlyCopy`); exits are
            // reported by the PTY waiter thread.
            TermEvent::ClipboardLoad(..)
            | TermEvent::Wakeup
            | TermEvent::MouseCursorDirty
            | TermEvent::CursorBlinkingChange
            | TermEvent::Exit
            | TermEvent::ChildExit(_) => {}
        }
    }
}

/// The emulator proper: grid + parser. Usable without a PTY (tests).
pub struct TermState {
    term: Term<Listener>,
    parser: Processor,
}

impl TermState {
    fn new(config: TermConfig, cols: u16, rows: u16, listener: Listener) -> Self {
        let size = GridSize { cols: cols.max(1) as usize, rows: rows.max(1) as usize };
        Self { term: Term::new(config, &size, listener), parser: Processor::new() }
    }

    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Applies a pending synchronized update (DEC mode 2026) whose timeout
    /// expired without the closing sequence.
    fn flush_expired_sync(&mut self) {
        if self.parser.sync_timeout().sync_timeout().is_some_and(|t| t <= Instant::now()) {
            self.parser.stop_sync(&mut self.term);
        }
    }
}

fn term_config(config: &AppConfig) -> TermConfig {
    let shape = match config.cursor.shape {
        CursorShape::Block => VteCursorShape::Block,
        CursorShape::Beam => VteCursorShape::Beam,
        CursorShape::Underline => VteCursorShape::Underline,
    };
    TermConfig {
        scrolling_history: config.terminal.scrollback,
        default_cursor_style: CursorStyle { shape, blinking: config.cursor.blink },
        osc52: Osc52::OnlyCopy,
        ..TermConfig::default()
    }
}

// ---------------------------------------------------------------------------
// Render snapshot
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct RenderCell {
    pub c: char,
    /// Combining characters (accents, ZWJ sequences…).
    pub zerowidth: Option<Box<[char]>>,
    pub fg: Rgb,
    pub bg: Rgb,
    pub flags: Flags,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotCursorShape {
    Block,
    HollowBlock,
    Underline,
    Beam,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CursorSnapshot {
    pub row: usize,
    pub col: usize,
    pub shape: SnapshotCursorShape,
    pub color: Rgb,
    pub text_color: Rgb,
    /// Spans two cells (cursor on a wide character).
    pub wide: bool,
}

/// Everything the renderer needs for one frame of one pane, with theme
/// colors already resolved.
#[derive(Clone, Debug, PartialEq)]
pub struct GridSnapshot {
    pub cols: usize,
    pub rows: usize,
    /// Row-major, `cols * rows` cells.
    pub cells: Vec<RenderCell>,
    pub cursor: Option<CursorSnapshot>,
    pub display_offset: usize,
    pub background: Rgb,
}

#[cfg(test)]
impl GridSnapshot {
    pub fn cell(&self, row: usize, col: usize) -> &RenderCell {
        &self.cells[row * self.cols + col]
    }

    /// Text of a row, without trailing spaces or wide-char spacers.
    pub fn row_text(&self, row: usize) -> String {
        let cells = &self.cells[row * self.cols..(row + 1) * self.cols];
        let text: String = cells
            .iter()
            .filter(|c| !c.flags.contains(Flags::WIDE_CHAR_SPACER))
            .map(|c| c.c)
            .collect();
        text.trim_end().to_owned()
    }
}

fn resolve_color(color: Color, theme: &Theme, overrides: &Colors) -> Rgb {
    let from_vte = |c: VteRgb| Rgb::new(c.r, c.g, c.b);
    match color {
        Color::Spec(rgb) => from_vte(rgb),
        Color::Indexed(i) => overrides[i as usize].map(from_vte).unwrap_or_else(|| theme.indexed(i)),
        Color::Named(named) => {
            if let Some(c) = overrides[named as usize] {
                return from_vte(c);
            }
            let c = &theme.colors;
            match named {
                NamedColor::Foreground | NamedColor::BrightForeground => c.foreground,
                NamedColor::Background => c.background,
                NamedColor::Cursor => c.cursor,
                NamedColor::DimForeground => c.foreground.scale(0.66),
                n if (n as usize) < 16 => theme.ansi(n as usize),
                // Dim variants follow Background..Cursor: DimBlack..DimWhite.
                n => theme.ansi(n as usize - NamedColor::DimBlack as usize).scale(0.66),
            }
        }
    }
}

/// Bold text in one of the 8 base colors is drawn with its bright variant.
fn bold_as_bright(color: Color) -> Color {
    match color {
        Color::Named(n) if (n as usize) < 8 => {
            let bright = [
                NamedColor::BrightBlack,
                NamedColor::BrightRed,
                NamedColor::BrightGreen,
                NamedColor::BrightYellow,
                NamedColor::BrightBlue,
                NamedColor::BrightMagenta,
                NamedColor::BrightCyan,
                NamedColor::BrightWhite,
            ];
            Color::Named(bright[n as usize])
        }
        Color::Named(NamedColor::Foreground) => Color::Named(NamedColor::BrightForeground),
        other => other,
    }
}

fn snapshot(term: &Term<Listener>, theme: &Theme) -> GridSnapshot {
    let content = term.renderable_content();
    let (cols, rows) = (term.columns(), term.screen_lines());
    let offset = content.display_offset as i32;
    let overrides = content.colors;
    let background = resolve_color(Color::Named(NamedColor::Background), theme, overrides);
    let blank = RenderCell { c: ' ', zerowidth: None, fg: theme.colors.foreground, bg: background, flags: Flags::empty() };
    let mut cells = vec![blank; cols * rows];
    let sel_bg = theme.colors.selection_background;
    let sel_fg = theme.colors.selection_foreground;

    for indexed in content.display_iter {
        let row = indexed.point.line.0 + offset;
        let col = indexed.point.column.0;
        if row < 0 || row as usize >= rows || col >= cols {
            continue;
        }
        let cell = indexed.cell;
        let flags = cell.flags;
        let fg_color = if flags.contains(Flags::BOLD) { bold_as_bright(cell.fg) } else { cell.fg };
        let mut fg = resolve_color(fg_color, theme, overrides);
        let mut bg = resolve_color(cell.bg, theme, overrides);
        if flags.contains(Flags::DIM) {
            fg = fg.scale(0.66);
        }
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if flags.contains(Flags::HIDDEN) {
            fg = bg;
        }
        if content.selection.is_some_and(|s| s.contains(indexed.point)) {
            bg = sel_bg;
            fg = sel_fg.unwrap_or(fg);
        }
        cells[row as usize * cols + col] = RenderCell {
            c: cell.c,
            zerowidth: cell.zerowidth().filter(|z| !z.is_empty()).map(Box::from),
            fg,
            bg,
            flags,
        };
    }

    let cursor = {
        let point = content.cursor.point;
        let row = point.line.0 + offset;
        let shape = match content.cursor.shape {
            VteCursorShape::Block => Some(SnapshotCursorShape::Block),
            VteCursorShape::HollowBlock => Some(SnapshotCursorShape::HollowBlock),
            VteCursorShape::Underline => Some(SnapshotCursorShape::Underline),
            VteCursorShape::Beam => Some(SnapshotCursorShape::Beam),
            VteCursorShape::Hidden => None,
        };
        shape.filter(|_| row >= 0 && (row as usize) < rows && point.column.0 < cols).map(|shape| {
            let idx = row as usize * cols + point.column.0;
            CursorSnapshot {
                row: row as usize,
                col: point.column.0,
                shape,
                color: resolve_color(Color::Named(NamedColor::Cursor), theme, overrides),
                text_color: theme.cursor_text(),
                wide: cells[idx].flags.contains(Flags::WIDE_CHAR),
            }
        })
    };

    GridSnapshot { cols, rows, cells, cursor, display_offset: content.display_offset, background }
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// What kind of program a session runs, for icons and budgets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionKind {
    /// `[[agents]]` profile name, `None` for a plain shell.
    pub profile: Option<String>,
    /// Icon key (`claude`, `codex`, `gemini`, `shell`).
    pub icon: String,
    /// Default title (profile name or shell name, e.g. `zsh`).
    pub label: String,
}

pub struct Session {
    pub id: SessionId,
    pub kind: SessionKind,
    pub cwd: PathBuf,
    pub project: Project,
    /// User-assigned name (rename from the sidebar).
    pub custom_title: Option<String>,
    pub created_at: SystemTime,
    started: Instant,
    state: Arc<Mutex<TermState>>,
    shared: Arc<Shared>,
    pty: PtyHandle,
    size: (u16, u16),
}

/// Parameters for a new session.
#[derive(Clone, Debug, Default)]
pub struct SpawnRequest {
    pub profile: Option<AgentProfile>,
    pub cwd: Option<PathBuf>,
    pub cols: u16,
    pub rows: u16,
}

impl Session {
    pub fn spawn(id: SessionId, config: &AppConfig, theme: &Theme, req: SpawnRequest, wake: Wake) -> Result<Self, String> {
        let cwd = req
            .cwd
            .or_else(|| config.terminal.working_directory.as_deref().map(expand_tilde))
            .filter(|p| p.is_dir())
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        let shell = match &config.terminal.shell {
            Some(s) => (s.program.clone(), s.args.clone()),
            None => default_shell(),
        };
        let shell_name = program_name(&shell.0);
        let kind = match &req.profile {
            Some(p) => SessionKind { profile: Some(p.name.clone()), icon: p.icon.clone(), label: p.name.clone() },
            None => SessionKind { profile: None, icon: "shell".into(), label: shell_name },
        };
        let (program, args) = build_command(req.profile.as_ref(), shell);
        let (cols, rows) = (req.cols.max(2), req.rows.max(1));

        let mut env: Vec<(String, String)> = config.terminal.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        env.push(("TERMY_SESSION_ID".into(), id.to_string()));

        let shared = Arc::new(Shared {
            pty_input: OnceLock::new(),
            title: Mutex::new(None),
            clipboard_request: Mutex::new(None),
            palette: Mutex::new(QueryPalette::from_theme(theme)),
            window_size: Mutex::new(WindowSize { num_lines: rows, num_cols: cols, cell_width: 0, cell_height: 0 }),
            exit: Mutex::new(None),
            attention: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            last_output_ms: AtomicU64::new(0),
            epoch: Instant::now(),
            wake,
        });
        let state = Arc::new(Mutex::new(TermState::new(term_config(config), cols, rows, Listener(shared.clone()))));

        let opts = SpawnOptions { program, args, cwd: Some(cwd.clone()), env, cols, rows };
        let (out_state, out_shared) = (state.clone(), shared.clone());
        let exit_shared = shared.clone();
        let pty = PtyHandle::spawn(
            &opts,
            move |bytes| {
                out_state.lock().advance(bytes);
                let ms = out_shared.epoch.elapsed().as_millis() as u64;
                out_shared.last_output_ms.store(ms.max(1), Ordering::Release);
                out_shared.mark_dirty();
            },
            move |info| {
                *exit_shared.exit.lock() = Some(info);
                exit_shared.attention.store(true, Ordering::Release);
                exit_shared.mark_dirty();
            },
        )?;
        let _ = shared.pty_input.set(pty.input_sender());

        Ok(Self {
            id,
            project: detect_project(&cwd),
            cwd,
            kind,
            custom_title: None,
            created_at: SystemTime::now(),
            started: Instant::now(),
            state,
            shared,
            pty,
            size: (cols, rows),
        })
    }

    /// Sends input (already encoded) to the program.
    pub fn write(&self, bytes: impl Into<Vec<u8>>) {
        self.pty.write(bytes);
    }

    /// Pastes text, using bracketed paste when the program asked for it.
    pub fn paste(&self, text: &str) {
        let bracketed = self.state.lock().term.mode().contains(TermMode::BRACKETED_PASTE);
        if bracketed {
            // Strip any embedded end marker so pasted text can't escape the bracket.
            let clean = text.replace("\x1b[201~", "");
            self.write(format!("\x1b[200~{clean}\x1b[201~"));
        } else {
            self.write(text.replace("\r\n", "\r").replace('\n', "\r"));
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16, cell_width: u16, cell_height: u16) {
        let (cols, rows) = (cols.max(2), rows.max(1));
        *self.shared.window_size.lock() = WindowSize { num_lines: rows, num_cols: cols, cell_width, cell_height };
        if (cols, rows) == self.size {
            return;
        }
        self.size = (cols, rows);
        self.state.lock().term.resize(GridSize { cols: cols as usize, rows: rows as usize });
        self.pty.resize(cols, rows);
    }

    pub fn size(&self) -> (u16, u16) {
        self.size
    }

    /// Scrolls the viewport through the scrollback (positive = up).
    pub fn scroll(&self, lines: i32) {
        self.state.lock().term.scroll_display(Scroll::Delta(lines));
        self.shared.dirty.store(true, Ordering::Release);
    }

    pub fn scroll_to_bottom(&self) {
        self.state.lock().term.scroll_display(Scroll::Bottom);
        self.shared.dirty.store(true, Ordering::Release);
    }

    /// Viewport is scrolled up (show "Jump to bottom").
    pub fn is_scrolled(&self) -> bool {
        self.state.lock().term.grid().display_offset() > 0
    }

    /// Theme switch: affects rendering and answers to color queries.
    pub fn set_theme(&self, theme: &Theme) {
        *self.shared.palette.lock() = QueryPalette::from_theme(theme);
        self.shared.dirty.store(true, Ordering::Release);
    }

    pub fn apply_config(&self, config: &AppConfig) {
        self.state.lock().term.set_options(term_config(config));
        self.shared.dirty.store(true, Ordering::Release);
    }

    pub fn snapshot(&self, theme: &Theme) -> GridSnapshot {
        let mut state = self.state.lock();
        state.flush_expired_sync();
        snapshot(&state.term, theme)
    }

    /// Read access to the emulator (selection, search, …).
    pub fn with_term<R>(&self, f: impl FnOnce(&mut Term<Listener>) -> R) -> R {
        f(&mut self.state.lock().term)
    }

    /// Returns and clears the "needs redraw" flag.
    pub fn take_dirty(&self) -> bool {
        self.shared.dirty.swap(false, Ordering::AcqRel)
    }

    /// OSC 0/2 title, cleaned of spinner glyphs.
    pub fn task_title(&self) -> Option<String> {
        self.shared.title.lock().as_deref().map(clean_title).filter(|t| !t.is_empty())
    }

    /// Agent recognized from the terminal title when the session runs a plain
    /// shell (e.g. the user typed `claude`): Claude Code titles start with
    /// `✳` when idle and a braille spinner while working.
    pub fn detected_icon(&self) -> Option<&'static str> {
        if self.kind.profile.is_some() {
            return None;
        }
        let title = self.shared.title.lock();
        let first = title.as_deref()?.trim_start().chars().next()?;
        (first == '✳' || ('\u{2801}'..='\u{28FF}').contains(&first)).then_some("claude")
    }

    pub fn title(&self) -> String {
        self.custom_title.clone().unwrap_or_else(|| self.kind.label.clone())
    }

    pub fn exit_info(&self) -> Option<ExitInfo> {
        *self.shared.exit.lock()
    }

    pub fn activity(&self, now: Instant, timeout: Duration) -> ActivityState {
        if self.exit_info().is_some() {
            return ActivityState::Done;
        }
        let last = self.shared.last_output_ms.load(Ordering::Acquire);
        let since_start = now.saturating_duration_since(self.shared.epoch).as_millis() as u64;
        if last != 0 && since_start.saturating_sub(last) < timeout.as_millis() as u64 {
            ActivityState::Working
        } else {
            ActivityState::Waiting
        }
    }

    /// Bell rang or the process exited since the user last looked.
    pub fn needs_attention(&self) -> bool {
        self.shared.attention.load(Ordering::Acquire)
    }

    pub fn clear_attention(&self) {
        self.shared.attention.store(false, Ordering::Release);
    }

    /// PID of the process running in the PTY (root of the session's tree).
    pub fn pid(&self) -> Option<u32> {
        self.pty.pid()
    }

    /// Text an app asked to put on the clipboard (OSC 52).
    pub fn take_clipboard_request(&self) -> Option<String> {
        self.shared.clipboard_request.lock().take()
    }

    pub fn summary(&self, now: Instant, timeout: Duration) -> SessionSummary {
        let state = self.activity(now, timeout);
        SessionSummary {
            id: self.id,
            title: self.title(),
            subtitle: self.task_title().unwrap_or_else(|| state.label().to_owned()),
            icon: self.detected_icon().map(str::to_owned).unwrap_or_else(|| self.kind.icon.clone()),
            profile: self.kind.profile.clone(),
            project_name: self.project.name.clone(),
            project_root: self.project.root.clone(),
            cwd: self.cwd.clone(),
            state,
            attention: self.needs_attention(),
            elapsed: now.saturating_duration_since(self.started),
            started_unix_ms: unix_ms(self.created_at),
            exit_code: self.exit_info().map(|e| e.code),
        }
    }
}

// ---------------------------------------------------------------------------
// Sidebar model
// ---------------------------------------------------------------------------

/// Plain data for one sidebar card.
#[derive(Clone, Debug, PartialEq)]
pub struct SessionSummary {
    pub id: SessionId,
    pub title: String,
    pub subtitle: String,
    pub icon: String,
    pub profile: Option<String>,
    pub project_name: String,
    pub project_root: PathBuf,
    pub cwd: PathBuf,
    pub state: ActivityState,
    pub attention: bool,
    pub elapsed: Duration,
    pub started_unix_ms: u64,
    pub exit_code: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SessionGroup {
    pub name: String,
    pub root: PathBuf,
    pub sessions: Vec<SessionSummary>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FilterCounts {
    pub all: usize,
    pub waiting: usize,
    pub working: usize,
}

pub fn filter_counts(summaries: &[SessionSummary]) -> FilterCounts {
    FilterCounts {
        all: summaries.len(),
        waiting: summaries.iter().filter(|s| s.state == ActivityState::Waiting).count(),
        working: summaries.iter().filter(|s| s.state == ActivityState::Working).count(),
    }
}

/// Filters by state and search query, then groups by project root, keeping
/// the order in which projects and sessions first appear.
pub fn group_sessions(summaries: &[SessionSummary], filter: SessionFilter, query: &str) -> Vec<SessionGroup> {
    let query = query.trim().to_lowercase();
    let mut groups: Vec<SessionGroup> = Vec::new();
    for s in summaries {
        let state_ok = match filter {
            SessionFilter::All => true,
            SessionFilter::Waiting => s.state == ActivityState::Waiting,
            SessionFilter::Working => s.state == ActivityState::Working,
        };
        let query_ok = query.is_empty()
            || [&s.title, &s.subtitle, &s.project_name, s.profile.as_deref().unwrap_or("")]
                .iter()
                .any(|field| field.to_lowercase().contains(&query));
        if !(state_ok && query_ok) {
            continue;
        }
        match groups.iter_mut().find(|g| g.root == s.project_root) {
            Some(g) => g.sessions.push(s.clone()),
            None => groups.push(SessionGroup {
                name: s.project_name.clone(),
                root: s.project_root.clone(),
                sessions: vec![s.clone()],
            }),
        }
    }
    groups
}

// ---------------------------------------------------------------------------
// Manager & archive
// ---------------------------------------------------------------------------

/// A closed session, persisted in `archive.json` and restorable.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ArchivedSession {
    pub title: String,
    pub task: Option<String>,
    pub profile: Option<String>,
    pub icon: String,
    pub cwd: PathBuf,
    pub project: String,
    pub started_unix_ms: u64,
    pub closed_unix_ms: u64,
}

const MAX_ARCHIVED: usize = 200;

pub struct SessionManager {
    sessions: Vec<Session>,
    archived: Vec<ArchivedSession>,
    next_id: SessionId,
    wake: Wake,
    archive_path: Option<PathBuf>,
}

impl SessionManager {
    pub fn new(wake: Wake, archive_path: Option<PathBuf>) -> Self {
        let archived = archive_path
            .as_deref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self { sessions: Vec::new(), archived, next_id: 1, wake, archive_path }
    }

    pub fn spawn(&mut self, config: &AppConfig, theme: &Theme, req: SpawnRequest) -> Result<SessionId, String> {
        let id = self.next_id;
        let session = Session::spawn(id, config, theme, req, self.wake.clone())?;
        self.next_id += 1;
        self.sessions.push(session);
        Ok(id)
    }

    pub fn get(&self, id: SessionId) -> Option<&Session> {
        self.sessions.iter().find(|s| s.id == id)
    }

    pub fn get_mut(&mut self, id: SessionId) -> Option<&mut Session> {
        self.sessions.iter_mut().find(|s| s.id == id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Session> {
        self.sessions.iter()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Kills the session's process and moves it to the archive.
    pub fn close(&mut self, id: SessionId) -> Option<ArchivedSession> {
        let index = self.sessions.iter().position(|s| s.id == id)?;
        let session = self.sessions.remove(index);
        let entry = ArchivedSession {
            title: session.title(),
            task: session.task_title(),
            profile: session.kind.profile.clone(),
            icon: session.kind.icon.clone(),
            cwd: session.cwd.clone(),
            project: session.project.name.clone(),
            started_unix_ms: unix_ms(session.created_at),
            closed_unix_ms: unix_ms(SystemTime::now()),
        };
        drop(session); // PtyHandle::drop kills the child
        self.archived.insert(0, entry.clone());
        self.archived.truncate(MAX_ARCHIVED);
        self.persist_archive();
        Some(entry)
    }

    pub fn archived(&self) -> &[ArchivedSession] {
        &self.archived
    }

    /// Respawns an archived session (same profile and directory).
    pub fn restore(&mut self, index: usize, config: &AppConfig, theme: &Theme, cols: u16, rows: u16) -> Result<SessionId, String> {
        let entry = self.archived.get(index).cloned().ok_or("no such archived session")?;
        let profile = entry.profile.as_deref().and_then(|name| config.agent(name)).cloned();
        let id = self.spawn(config, theme, SpawnRequest { profile, cwd: Some(entry.cwd.clone()), cols, rows })?;
        if let Some(s) = self.get_mut(id) {
            s.custom_title = (entry.title != s.kind.label).then_some(entry.title);
        }
        self.archived.remove(index);
        self.persist_archive();
        Ok(id)
    }

    pub fn remove_archived(&mut self, index: usize) {
        if index < self.archived.len() {
            self.archived.remove(index);
            self.persist_archive();
        }
    }

    fn persist_archive(&self) {
        let Some(path) = &self.archive_path else { return };
        match serde_json::to_string_pretty(&self.archived) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    log::warn!("could not write {}: {e}", path.display());
                }
            }
            Err(e) => log::warn!("could not serialize archive: {e}"),
        }
    }

    pub fn summaries(&self, now: Instant, activity_timeout: Duration) -> Vec<SessionSummary> {
        self.sessions.iter().map(|s| s.summary(now, activity_timeout)).collect()
    }

    pub fn set_theme(&self, theme: &Theme) {
        self.sessions.iter().for_each(|s| s.set_theme(theme));
    }

    pub fn apply_config(&self, config: &AppConfig) {
        self.sessions.iter().for_each(|s| s.apply_config(config));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ThemeRegistry;

    fn noop_wake() -> Wake {
        Arc::new(|| {})
    }

    fn headless(cols: u16, rows: u16) -> (TermState, Arc<Shared>) {
        let shared = Arc::new(Shared {
            pty_input: OnceLock::new(),
            title: Mutex::new(None),
            clipboard_request: Mutex::new(None),
            palette: Mutex::new(QueryPalette::from_theme(ThemeRegistry::builtin().resolve("dark-oled"))),
            window_size: Mutex::new(WindowSize { num_lines: rows, num_cols: cols, cell_width: 8, cell_height: 16 }),
            exit: Mutex::new(None),
            attention: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            last_output_ms: AtomicU64::new(0),
            epoch: Instant::now(),
            wake: noop_wake(),
        });
        let state = TermState::new(term_config(&AppConfig::with_defaults()), cols, rows, Listener(shared.clone()));
        (state, shared)
    }

    fn theme() -> Theme {
        ThemeRegistry::builtin().resolve("dark-oled").clone()
    }

    #[test]
    fn vt_colors_are_resolved_with_the_theme() {
        let (mut st, _) = headless(20, 3);
        let t = theme();
        st.advance(b"\x1b[31mR\x1b[0m\x1b[1;32mG\x1b[0m\x1b[38;5;196mX\x1b[38;2;1;2;3mT\x1b[0m\x1b[7mI\x1b[0m");
        let snap = snapshot(&st.term, &t);
        assert_eq!(snap.row_text(0), "RGXTI");
        assert_eq!(snap.cell(0, 0).fg, t.colors.normal.red);
        assert_eq!(snap.cell(0, 1).fg, t.colors.bright.green); // bold → bright
        assert_eq!(snap.cell(0, 2).fg, Rgb::new(255, 0, 0)); // 256-color cube
        assert_eq!(snap.cell(0, 3).fg, Rgb::new(1, 2, 3)); // truecolor
        assert_eq!(snap.cell(0, 4).bg, t.colors.foreground); // inverse
        assert_eq!(snap.cell(0, 4).fg, t.colors.background);
        assert_eq!(snap.background, t.colors.background);
        let cursor = snap.cursor.unwrap();
        assert_eq!((cursor.row, cursor.col), (0, 5));
    }

    #[test]
    fn wide_chars_and_combining_marks() {
        let (mut st, _) = headless(10, 2);
        st.advance("日本e\u{301}".as_bytes());
        let snap = snapshot(&st.term, &theme());
        assert!(snap.cell(0, 0).flags.contains(Flags::WIDE_CHAR));
        assert!(snap.cell(0, 1).flags.contains(Flags::WIDE_CHAR_SPACER));
        assert_eq!(snap.cell(0, 4).c, 'e');
        assert_eq!(snap.cell(0, 4).zerowidth.as_deref(), Some(&['\u{301}'][..]));
        assert_eq!(snap.row_text(0), "日本e");
    }

    #[test]
    fn scrollback_and_viewport() {
        let (mut st, _) = headless(10, 3);
        for i in 0..10 {
            st.advance(format!("line{i}\r\n").as_bytes());
        }
        let t = theme();
        let snap = snapshot(&st.term, &t);
        assert_eq!(snap.row_text(0), "line8");
        assert_eq!(snap.row_text(1), "line9");
        st.term.scroll_display(Scroll::Delta(3));
        let snap = snapshot(&st.term, &t);
        assert_eq!(snap.display_offset, 3);
        assert_eq!(snap.row_text(0), "line5");
        assert!(snap.cursor.is_none() || snap.cursor.unwrap().row < 3);
    }

    #[cfg(unix)]
    #[test]
    fn claude_code_is_detected_from_its_title_in_a_shell() {
        let t = theme();
        let mut manager = SessionManager::new(noop_wake(), None);
        let config = sh_config("printf '\\033]0;\\342\\234\\263 Fix logos\\007'; read _");
        let id = manager.spawn(&config, &t, SpawnRequest { cols: 40, rows: 5, ..Default::default() }).unwrap();
        let s = manager.get(id).unwrap();
        assert!(wait_until(|| s.detected_icon() == Some("claude")));
        assert_eq!(s.summary(Instant::now(), Duration::from_secs(1)).icon, "claude");
    }

    #[test]
    fn osc_title_bell_and_osc52_copy() {
        let (mut st, shared) = headless(20, 2);
        st.advance(b"\x1b]0;\xe2\x9c\xb3 Fix logos\x07"); // "✳ Fix logos"
        assert_eq!(shared.title.lock().as_deref(), Some("✳ Fix logos"));
        assert_eq!(clean_title("✳ Fix logos"), "Fix logos");
        assert!(!shared.attention.load(Ordering::Acquire));
        st.advance(b"\x07");
        assert!(shared.attention.load(Ordering::Acquire));
        st.advance(b"\x1b]52;c;aGVsbG8=\x07"); // base64("hello")
        assert_eq!(shared.clipboard_request.lock().as_deref(), Some("hello"));
    }

    #[test]
    fn terminal_queries_are_answered_through_the_pty() {
        let (mut st, shared) = headless(20, 2);
        let (tx, rx) = crossbeam_channel::unbounded();
        shared.pty_input.set(tx).unwrap();
        st.advance(b"\x1b[6n"); // cursor position report
        assert_eq!(rx.try_recv().unwrap(), b"\x1b[1;1R");
        st.advance(b"\x1b]11;?\x07"); // background color query
        let reply = String::from_utf8(rx.try_recv().unwrap()).unwrap();
        assert!(reply.starts_with("\x1b]11;rgb:0000/0000/0000"), "{reply:?}");
    }

    #[test]
    fn synchronized_updates_are_flushed() {
        let (mut st, _) = headless(20, 2);
        st.advance(b"\x1b[?2026hhidden");
        assert_eq!(snapshot(&st.term, &theme()).row_text(0), "");
        st.advance(b"\x1b[?2026l");
        assert_eq!(snapshot(&st.term, &theme()).row_text(0), "hidden");
    }

    #[test]
    fn project_detection_finds_git_root() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("midterm");
        let nested = repo.join("src/renderer");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir(repo.join(".git")).unwrap();
        let p = detect_project(&nested);
        assert_eq!(p.name, "midterm");
        assert_eq!(p.root, repo);
        // Worktrees use a `.git` file.
        let wt = dir.path().join("wt");
        std::fs::create_dir(&wt).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: ../x").unwrap();
        assert_eq!(detect_project(&wt).name, "wt");
        // No repo → the directory itself.
        let plain = dir.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        assert_eq!(detect_project(&plain).root, plain);
    }

    #[test]
    fn agent_commands_go_through_login_shell() {
        let profile = crate::config::default_agents().remove(0);
        let (prog, args) = build_command(Some(&profile), ("/bin/zsh".into(), vec!["-l".into()]));
        if cfg!(windows) {
            assert_eq!(prog, "claude");
        } else {
            assert_eq!(prog, "/bin/zsh");
            assert_eq!(args, ["-l", "-c", r#"exec "$0" "$@""#, "claude"]);
            let (_, fish) = build_command(Some(&profile), ("/usr/bin/fish".into(), vec![]));
            assert_eq!(fish[2], "exec $argv");
            let (direct, _) = build_command(Some(&profile), ("/usr/bin/xonsh".into(), vec![]));
            assert_eq!(direct, "claude");
        }
        let shell = build_command(None, ("pwsh.exe".into(), vec!["-NoLogo".into()]));
        assert_eq!(shell, ("pwsh.exe".to_owned(), vec!["-NoLogo".to_owned()]));
        assert_eq!(program_name("C:\\bin\\codex.exe"), "codex");
    }

    fn summary(id: SessionId, project: &str, state: ActivityState, title: &str) -> SessionSummary {
        SessionSummary {
            id,
            title: title.into(),
            subtitle: state.label().into(),
            icon: "claude".into(),
            profile: Some("Claude Code".into()),
            project_name: project.into(),
            project_root: PathBuf::from(format!("/dev/{project}")),
            cwd: PathBuf::from(format!("/dev/{project}")),
            state,
            attention: false,
            elapsed: Duration::from_secs(60),
            started_unix_ms: 0,
            exit_code: None,
        }
    }

    #[test]
    fn grouping_filters_and_search() {
        use ActivityState::*;
        let all = vec![
            summary(1, "bid-model-detector", Done, "Logos que no se muestran"),
            summary(2, "midterm", Working, "Claude Code"),
            summary(3, "midterm", Working, "midterm"),
            summary(4, "midulive", Waiting, "Claude Code"),
        ];
        let groups = group_sessions(&all, SessionFilter::All, "");
        let names: Vec<_> = groups.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(names, ["bid-model-detector", "midterm", "midulive"]);
        assert_eq!(groups[1].sessions.len(), 2);

        let working = group_sessions(&all, SessionFilter::Working, "");
        assert_eq!(working.len(), 1);
        assert_eq!(working[0].sessions.len(), 2);

        let searched = group_sessions(&all, SessionFilter::All, "LOGOS");
        assert_eq!(searched.len(), 1);
        assert_eq!(searched[0].sessions[0].id, 1);
        assert_eq!(group_sessions(&all, SessionFilter::All, "midul")[0].name, "midulive");

        assert_eq!(filter_counts(&all), FilterCounts { all: 4, waiting: 1, working: 2 });
    }

    #[test]
    fn elapsed_format() {
        assert_eq!(format_elapsed(Duration::from_secs(27)), "27s");
        assert_eq!(format_elapsed(Duration::from_secs(17 * 60 + 5)), "17m");
        assert_eq!(format_elapsed(Duration::from_secs(3 * 3600)), "3h");
        assert_eq!(format_elapsed(Duration::from_secs(2 * 86_400)), "2d");
    }

    // --- Real PTY -----------------------------------------------------------

    #[cfg(unix)]
    fn sh_config(script: &str) -> AppConfig {
        let mut config = AppConfig::with_defaults();
        config.terminal.shell = Some(crate::config::ShellConfig {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
        });
        config
    }

    #[cfg(unix)]
    fn wait_until(mut f: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    #[cfg(unix)]
    #[test]
    fn session_lifecycle_with_real_pty() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().canonicalize().unwrap();
        let t = theme();
        let (wake_tx, wake_rx) = crossbeam_channel::unbounded();
        let wake: Wake = Arc::new(move || {
            let _ = wake_tx.send(());
        });
        let mut manager = SessionManager::new(wake, Some(cwd.join("archive.json")));
        let config = sh_config("printf '\\033]2;\\342\\234\\263 Building\\007\\033[34mblue\\033[0m'; read _; exit 0");
        let id = manager
            .spawn(&config, &t, SpawnRequest { cwd: Some(cwd.clone()), cols: 40, rows: 5, ..Default::default() })
            .unwrap();
        let session = manager.get(id).unwrap();
        assert_eq!(session.kind.label, "sh");
        assert_eq!(session.project.root, cwd);

        assert!(wake_rx.recv_timeout(Duration::from_secs(5)).is_ok(), "output must wake the UI");
        assert!(wait_until(|| session.snapshot(&t).row_text(0) == "blue"));
        assert!(session.take_dirty());
        assert_eq!(session.snapshot(&t).cell(0, 0).fg, t.colors.normal.blue);
        assert_eq!(session.task_title().as_deref(), Some("Building"));

        let timeout = Duration::from_millis(300);
        assert_eq!(session.activity(Instant::now(), timeout), ActivityState::Working);
        std::thread::sleep(Duration::from_millis(350));
        assert_eq!(session.activity(Instant::now(), timeout), ActivityState::Waiting);

        session.write("\n");
        assert!(wait_until(|| session.exit_info().is_some()));
        assert_eq!(session.activity(Instant::now(), timeout), ActivityState::Done);
        assert!(session.needs_attention());
        let summary = session.summary(Instant::now(), timeout);
        assert_eq!(summary.subtitle, "Building");

        // Close → archive on disk → restore.
        let archived = manager.close(id).unwrap();
        assert_eq!(archived.task.as_deref(), Some("Building"));
        assert!(manager.is_empty());
        let reloaded = SessionManager::new(Arc::new(|| {}), Some(cwd.join("archive.json")));
        assert_eq!(reloaded.archived(), manager.archived());
        let restored = manager.restore(0, &config, &t, 40, 5).unwrap();
        assert_eq!(manager.get(restored).unwrap().cwd, cwd);
        assert!(manager.archived().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn resize_reaches_both_term_and_pty() {
        let t = theme();
        let mut manager = SessionManager::new(noop_wake(), None);
        let config = sh_config("read _; stty size; read _");
        let id = manager.spawn(&config, &t, SpawnRequest { cols: 40, rows: 5, ..Default::default() }).unwrap();
        let session = manager.get_mut(id).unwrap();
        session.resize(100, 30, 8, 16);
        assert_eq!(session.snapshot(&t).cols, 100);
        session.write("\n");
        assert!(wait_until(|| (0..30).any(|r| session.snapshot(&t).row_text(r) == "30 100")));
    }

    #[cfg(unix)]
    #[test]
    fn paste_uses_bracketed_mode_when_requested() {
        let t = theme();
        let mut manager = SessionManager::new(noop_wake(), None);
        // Enable bracketed paste, then echo raw input bytes back as hex.
        let config = sh_config("printf '\\033[?2004h'; stty raw -echo; dd bs=1 count=14 2>/dev/null | od -An -c | tr -s ' '; sleep 1");
        let id = manager.spawn(&config, &t, SpawnRequest { cols: 80, rows: 5, ..Default::default() }).unwrap();
        let session = manager.get(id).unwrap();
        assert!(wait_until(|| session.with_term(|term| term.mode().contains(TermMode::BRACKETED_PASTE))));
        session.paste("hi\x1b[201~");
        assert!(wait_until(|| (0..5).any(|r| session.snapshot(&t).row_text(r).contains("[ 2 0 0 ~ h i 033 [ 2 0 1 ~"))));
    }
}
