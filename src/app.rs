//! `winit` `ApplicationHandler`: owns the window, renderer, sessions and
//! workspace; routes input between egui and the terminals; applies config /
//! theme hot-reloads. Background threads only ever talk to it through
//! `EventLoopProxy<UserEvent>`, so the UI thread never blocks on I/O.

use std::sync::Arc;
use std::time::{Duration, Instant};

use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;
use parking_lot::Mutex;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, Ime, KeyEvent, MouseButton as WinitButton, MouseScrollDelta, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{Window, WindowId};

use crate::Wake;
use crate::ai_monitor::AiMonitor;
use std::collections::HashMap;

use crate::ai_monitor::now_ms;
use crate::chrome::{self, ChromeInput, ChromeOutput, ChromeState, UiAction, UsageView};
use crate::settings_ui::{self, SettingsInput, SettingsOutput, SettingsState};
use crate::config::{
    Action, AppConfig, ConfigFile, ConfigPaths, ConfigWatcher, Decorations, FontConfig, Reload, SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH,
    WatchEvent,
};
use crate::input::{self, KeyModes, MouseButton, MouseEventKind, WheelAction};
use crate::links::{self, Link, LinkDetector};
use crate::renderer::{CursorDraw, EguiFrame, PaneFrame, Renderer};
use crate::resources::ResourceMonitor;
use crate::session::{GridSnapshot, SessionId, SessionManager, SpawnRequest, USAGE_TAB};
use crate::stats_ui::{self, StatsInput, StatsState};
use crate::theme::{Theme, ThemeRegistry};
use crate::usage_stats::UsageStats;
use crate::workspace::{PaneId, Rect, SplitDir, Workspace};

#[derive(Debug, Clone, Copy)]
pub enum UserEvent {
    /// A session produced output / exited, or the AI monitor has new data.
    Wake,
    ConfigChanged,
    ThemesChanged,
}

struct Gfx {
    window: Arc<Window>,
    renderer: Renderer,
    egui_ctx: egui::Context,
    egui_state: egui_winit::State,
}

/// Last frame's pane geometry, in physical pixels, for mouse hit-testing.
#[derive(Clone, Copy, Debug)]
struct PaneHit {
    pane: PaneId,
    session: Option<SessionId>,
    area: Rect,
    origin: [f32; 2],
    cols: usize,
    rows: usize,
}

#[derive(Default)]
struct MouseState {
    pos: PhysicalPosition<f64>,
    /// Session being selected with the left button.
    selecting: Option<SessionId>,
    /// Session receiving mouse reports while a button is held.
    reporting: Option<(SessionId, MouseButton)>,
    last_click: Option<(Instant, (usize, usize), u8)>,
    wheel_accum: f64,
}

pub struct App {
    config_file: ConfigFile,
    config: AppConfig,
    /// Last config.toml load/parse error (cleared by a good reload).
    config_error: Option<String>,
    /// Last error from an action (a session that failed to start, …). Shown
    /// in the sidebar until dismissed or replaced.
    error: Option<String>,
    themes: ThemeRegistry,
    theme: Theme,
    sessions: SessionManager,
    workspace: Workspace,
    monitor: AiMonitor,
    resources: ResourceMonitor,
    _watcher: Option<ConfigWatcher>,
    gfx: Option<Gfx>,
    chrome: ChromeState,
    settings: SettingsState,
    /// Usage dashboard tab: view state, and its last computed stats keyed by
    /// (store generation, view state, minute).
    stats: StatsState,
    usage_cache: Option<((u64, StatsState, u64), Arc<UsageStats>)>,
    local_tz: jiff::tz::TimeZone,
    /// Installed monospace fonts (computed on first settings open).
    font_families: Option<Vec<String>>,
    /// Debounced AI monitor reconfiguration (typing a log path rescans).
    monitor_reconfigure_at: Option<Instant>,
    sidebar_visible: bool,
    font_delta: f64,
    modifiers: ModifiersState,
    window_focused: bool,
    panes: Vec<PaneHit>,
    /// Last frame's pane-divider grab areas (physical px).
    dividers: Vec<Rect>,
    mouse: MouseState,
    links: LinkDetector,
    /// Link under the mouse while Cmd/Ctrl is held (underlined, clickable).
    hover_link: Option<(SessionId, Link)>,
    blink_epoch: Instant,
    clipboard: Option<arboard::Clipboard>,
    egui_repaint_at: Option<Instant>,
    /// Development aid: `TERMY_CAPTURE=<file.bmp>` saves a frame after
    /// `TERMY_CAPTURE_DELAY_MS` (default 2000) and quits;
    /// `TERMY_DEMO_INPUT` is typed into the first session.
    capture: Option<(Instant, std::path::PathBuf)>,
}

impl App {
    pub fn new(proxy: EventLoopProxy<UserEvent>) -> Self {
        let paths = ConfigPaths::default_location();
        let (config_file, config, error) = ConfigFile::open(paths.clone());
        let themes = ThemeRegistry::load(&paths.themes_dir);
        let theme = themes.resolve(&config.appearance.theme).clone();

        // `EventLoopProxy` isn't `Sync` on every platform; `Wake` must be.
        let wake_proxy = Mutex::new(proxy.clone());
        let wake: Wake = Arc::new(move || {
            let _ = wake_proxy.lock().send_event(UserEvent::Wake);
        });
        let watch_proxy = Mutex::new(proxy);
        let watcher = ConfigWatcher::start(&paths, move |event| {
            let event = match event {
                WatchEvent::Config => UserEvent::ConfigChanged,
                WatchEvent::Themes => UserEvent::ThemesChanged,
            };
            let _ = watch_proxy.lock().send_event(event);
        })
        .map_err(|e| log::warn!("config hot-reload unavailable: {e}"))
        .ok();

        Self {
            sessions: SessionManager::new(wake.clone(), Some(paths.archive_file.clone())),
            resources: ResourceMonitor::start(
                config.resources.active(config.window.pane_footer),
                Duration::from_millis(config.resources.interval_ms),
                wake.clone(),
            ),
            monitor: AiMonitor::start(&config, wake),
            config_error: error.map(|e| e.to_string()),
            error: None,
            sidebar_visible: config.window.sidebar_visible,
            config_file,
            config,
            themes,
            theme,
            workspace: Workspace::new(),
            _watcher: watcher,
            gfx: None,
            chrome: ChromeState::default(),
            settings: SettingsState::default(),
            stats: StatsState::default(),
            usage_cache: None,
            local_tz: jiff::tz::TimeZone::system(),
            font_families: None,
            monitor_reconfigure_at: None,
            font_delta: 0.0,
            modifiers: ModifiersState::empty(),
            window_focused: true,
            panes: Vec::new(),
            dividers: Vec::new(),
            mouse: MouseState::default(),
            links: LinkDetector::new(),
            hover_link: None,
            blink_epoch: Instant::now(),
            clipboard: None,
            egui_repaint_at: None,
            capture: std::env::var_os("TERMY_CAPTURE").map(|p| {
                let delay = std::env::var("TERMY_CAPTURE_DELAY_MS").ok().and_then(|d| d.parse().ok()).unwrap_or(2000);
                (Instant::now() + Duration::from_millis(delay), p.into())
            }),
        }
    }

    fn effective_font(&self) -> FontConfig {
        let mut font = self.config.font.clone();
        font.size = (font.size + self.font_delta).clamp(6.0, 72.0);
        font
    }

    fn activity_timeout(&self) -> Duration {
        Duration::from_millis(self.config.terminal.activity_timeout_ms)
    }

    fn request_redraw(&self) {
        if let Some(g) = &self.gfx {
            g.window.request_redraw();
        }
    }

    fn traffic_lights(&self) -> Option<egui::Vec2> {
        (cfg!(target_os = "macos") && self.config.window.decorations == Decorations::Transparent)
            .then(|| egui::Vec2::new(78.0, 28.0))
    }

    fn restyle(&mut self) {
        if let Some(g) = &self.gfx {
            let ui = self.theme.ui();
            chrome::apply_style(&g.egui_ctx, &ui, self.theme.is_light(), g.renderer.font_bytes());
            style_title_bar(&g.window, &ui, self.theme.is_light());
        }
    }

    fn clipboard(&mut self) -> Option<&mut arboard::Clipboard> {
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().map_err(|e| log::warn!("clipboard: {e}")).ok();
        }
        self.clipboard.as_mut()
    }

    fn paste_into_focused(&mut self, text: &str) {
        if let Some(s) = self.focused() {
            s.with_term(|t| t.selection = None);
            s.scroll_to_bottom();
            s.clear_attention();
            s.paste(text);
            self.blink_epoch = Instant::now();
        }
        self.request_redraw();
    }

    // --- Window creation ----------------------------------------------------

    fn create_window(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let w = &self.config.window;
        let font = self.effective_font();
        let (cw, ch) = (font.size * 0.62, font.size * font.line_height);
        let width = if w.sidebar_visible { w.sidebar_width.clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH) } else { 0.0 } + w.columns as f64 * cw + 2.0 * w.padding_x;
        let height = chrome::TAB_BAR_HEIGHT as f64 + w.rows as f64 * ch + 2.0 * w.padding_y;
        let transparent = w.opacity < 1.0;

        let attrs = Window::default_attributes()
            .with_title("termy")
            .with_inner_size(LogicalSize::new(width, height))
            .with_min_inner_size(LogicalSize::new(480.0, 320.0))
            .with_transparent(transparent)
            .with_decorations(w.decorations != Decorations::None);
        // macOS takes the icon from the .app bundle; elsewhere the window sets it.
        #[cfg(not(target_os = "macos"))]
        let attrs = attrs.with_window_icon(window_icon());
        #[cfg(target_os = "windows")]
        let attrs = {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs.with_taskbar_icon(window_icon())
        };
        #[cfg(target_os = "macos")]
        let attrs = if w.decorations == Decorations::Transparent {
            use winit::platform::macos::WindowAttributesExtMacOS;
            attrs.with_titlebar_transparent(true).with_fullsize_content_view(true).with_title_hidden(true)
        } else {
            attrs
        };
        let window = Arc::new(event_loop.create_window(attrs).map_err(|e| format!("window: {e}"))?);
        window.set_ime_allowed(true);

        let renderer = Renderer::new(window.clone(), Box::new(event_loop.owned_display_handle()), &font, transparent)?;
        if transparent && !renderer.transparency_supported() {
            log::warn!("window opacity < 1 is not supported by this GPU/compositor");
        }
        let egui_ctx = egui::Context::default();
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            None,
            Some(8192),
        );
        self.gfx = Some(Gfx { window, renderer, egui_ctx, egui_state });
        self.restyle();
        Ok(())
    }

    // --- Sessions -----------------------------------------------------------

    fn spawn(&mut self, profile: Option<String>, split: Option<SplitDir>) {
        let profile = match profile {
            Some(name) => match self.config.agent(&name) {
                Some(p) => Some(p.clone()),
                None => {
                    self.error = Some(format!("Unknown agent profile {name:?}"));
                    return;
                }
            },
            None => None,
        };
        // New sessions start where the focused one is.
        let cwd = self.workspace.focused_session().and_then(|id| self.sessions.get(id)).map(|s| s.current_dir());
        let (cols, rows) = self
            .workspace
            .focused_session()
            .and_then(|id| self.sessions.get(id))
            .map(|s| s.size())
            .unwrap_or((self.config.window.columns, self.config.window.rows));
        match self.sessions.spawn(&self.config, &self.theme, SpawnRequest { profile, cwd, cols, rows }) {
            Ok(id) => {
                match split {
                    Some(dir) => {
                        self.workspace.split(dir, id);
                    }
                    None => self.workspace.add_session(id),
                }
                self.blink_epoch = Instant::now();
            }
            Err(e) => self.error = Some(e),
        }
        self.request_redraw();
    }

    fn close_session(&mut self, id: SessionId) {
        self.workspace.remove_session(id);
        if id != USAGE_TAB {
            self.sessions.close(id);
        }
        self.request_redraw();
    }

    /// Focuses the usage dashboard tab, opening it in the focused pane.
    fn open_usage(&mut self) {
        if !self.workspace.focus_session(USAGE_TAB) {
            self.workspace.add_session(USAGE_TAB);
        }
        self.monitor.load_history(self.stats.range.history_needed());
        self.request_redraw();
    }

    /// Stats for the dashboard when its tab is on screen. Recomputed only
    /// when the usage data, the view or the minute changes.
    fn usage_stats(&mut self) -> Option<Arc<UsageStats>> {
        if !self.workspace.visible_sessions().contains(&USAGE_TAB) {
            return None;
        }
        let now = now_ms();
        let store = self.monitor.store().read();
        let key = (store.generation(), self.stats.clone(), now / 60_000);
        if let Some((k, stats)) = &self.usage_cache
            && *k == key
        {
            return Some(stats.clone());
        }
        let tz = &self.local_tz;
        let offset = |ms: u64| jiff::Timestamp::from_millisecond(ms as i64).map_or(0, |t| tz.to_offset(t).seconds() as i64);
        let stats = Arc::new(UsageStats::compute(&store, self.stats.range, self.stats.agent.as_deref(), now, offset));
        drop(store);
        self.usage_cache = Some((key, stats.clone()));
        Some(stats)
    }

    fn focused(&self) -> Option<&crate::session::Session> {
        self.workspace.focused_session().and_then(|id| self.sessions.get(id))
    }

    // --- Config & themes ----------------------------------------------------

    fn reload_config(&mut self) {
        match self.config_file.reload() {
            Reload::Unchanged => {}
            Reload::Changed(config) => {
                // External edit: the panel follows it unless it has its own
                // unsaved changes.
                if self.settings.open && !self.settings.has_unsaved_changes() {
                    self.settings.draft = (*config).clone();
                }
                self.settings.saved = (*config).clone();
                self.apply_config(*config);
            }
            Reload::Error(e) => {
                log::warn!("{e}");
                self.config_error = Some(e.to_string());
            }
        }
        self.request_redraw();
    }

    fn apply_config(&mut self, new: AppConfig) {
        let old = std::mem::replace(&mut self.config, new);
        self.config_error = None;
        if old.window.sidebar_visible != self.config.window.sidebar_visible {
            self.sidebar_visible = self.config.window.sidebar_visible;
        }
        self.theme = self.themes.resolve(&self.config.appearance.theme).clone();
        self.sessions.set_theme(&self.theme);
        self.sessions.apply_config(&self.config);
        if old.resources != self.config.resources || old.window.pane_footer != self.config.window.pane_footer {
            let r = &self.config.resources;
            self.resources.configure(r.active(self.config.window.pane_footer), Duration::from_millis(r.interval_ms));
        }
        if old.agents != self.config.agents || old.monitor != self.config.monitor {
            self.monitor_reconfigure_at = Some(Instant::now() + Duration::from_millis(800));
        }
        let font = self.effective_font();
        if let Some(g) = &mut self.gfx {
            if old.font != self.config.font {
                let scale = g.window.scale_factor() as f32;
                g.renderer.set_font(&font, scale);
            }
            let transparent = self.config.window.opacity < 1.0;
            g.window.set_transparent(transparent);
            g.renderer.set_transparent(transparent);
        }
        self.restyle();
    }

    fn reload_themes(&mut self) {
        self.themes = ThemeRegistry::load(&self.config_file.paths.themes_dir);
        for e in &self.themes.errors {
            log::warn!("theme {}: {}", e.path.display(), e.message);
        }
        self.theme = self.themes.resolve(&self.config.appearance.theme).clone();
        self.sessions.set_theme(&self.theme);
        self.restyle();
        self.request_redraw();
    }

    fn set_font_delta(&mut self, delta: f64) {
        self.font_delta = delta;
        let font = self.effective_font();
        if let Some(g) = &mut self.gfx {
            g.renderer.set_font(&font, g.window.scale_factor() as f32);
        }
        self.request_redraw();
    }

    // --- Actions --------------------------------------------------------------

    fn perform(&mut self, action: Action) {
        match action {
            Action::Copy => {
                let text = self.focused().and_then(|s| s.with_term(|t| t.selection_to_string()));
                if let Some(text) = text.filter(|t| !t.is_empty())
                    && let Some(cb) = self.clipboard()
                {
                    let _ = cb.set_text(text);
                }
            }
            Action::Paste => {
                let text = self.clipboard().and_then(crate::paste::clipboard_text);
                if let Some(text) = text {
                    self.paste_into_focused(&text);
                }
            }
            Action::NewSession => self.spawn(None, None),
            Action::CloseTab => {
                if let Some(id) = self.workspace.focused_session() {
                    self.close_session(id);
                }
            }
            Action::NextTab => self.workspace.cycle_tab(1),
            Action::PrevTab => self.workspace.cycle_tab(-1),
            Action::SplitVertical => self.spawn(None, Some(SplitDir::Vertical)),
            Action::SplitHorizontal => self.spawn(None, Some(SplitDir::Horizontal)),
            Action::FocusNextPane => self.workspace.focus_next_pane(),
            Action::ToggleSidebar => self.sidebar_visible = !self.sidebar_visible,
            // Milestone 4 focuses the sidebar search box.
            Action::SearchSessions => {
                self.sidebar_visible = true;
                self.chrome.focus_search = true;
            }
            // Milestone 4: settings panel.
            Action::ToggleSettings => self.toggle_settings(),
            Action::OpenUsage => self.open_usage(),
            Action::IncreaseFont => self.set_font_delta(self.font_delta + 1.0),
            Action::DecreaseFont => self.set_font_delta(self.font_delta - 1.0),
            Action::ResetFont => self.set_font_delta(0.0),
            Action::ReloadConfig => self.reload_config(),
            Action::PrevPrompt | Action::NextPrompt => {
                if let Some(s) = self.focused() {
                    s.jump_to_prompt(action == Action::PrevPrompt);
                }
            }
            Action::CopyLastOutput => {
                let text = self.focused().and_then(|s| s.last_command_output());
                if let Some(text) = text
                    && let Some(cb) = self.clipboard()
                {
                    let _ = cb.set_text(text);
                }
            }
        }
        self.request_redraw();
    }

    fn apply_ui_action(&mut self, action: UiAction) {
        match action {
            UiAction::NewSession { profile } => self.spawn(profile, None),
            UiAction::FocusSession(id) => {
                if !self.workspace.focus_session(id) {
                    self.workspace.add_session(id);
                }
                if let Some(s) = self.sessions.get(id) {
                    s.clear_attention();
                }
            }
            UiAction::ActivateTab(pane, index) => self.workspace.activate_tab(pane, index),
            UiAction::CloseSession(id) => self.close_session(id),
            UiAction::NewTab(pane) => {
                self.workspace.focus_pane(pane);
                self.spawn(None, None);
            }
            UiAction::ToggleSidebar => self.sidebar_visible = !self.sidebar_visible,
            UiAction::ResizeSidebar { width, done } => self.resize_sidebar(width as f64, done),
            UiAction::JumpToBottom(id) => {
                if let Some(s) = self.sessions.get(id) {
                    s.scroll_to_bottom();
                }
            }
            UiAction::OpenSettings => {
                if !self.settings.open {
                    self.toggle_settings();
                }
            }
            UiAction::OpenUsage => self.open_usage(),
            UiAction::RestoreArchived(index) => {
                let (cols, rows) = self.focused().map(|s| s.size()).unwrap_or((self.config.window.columns, self.config.window.rows));
                match self.sessions.restore(index, &self.config, &self.theme, cols, rows) {
                    Ok(id) => {
                        self.workspace.add_session(id);
                        self.chrome.archived_open = false;
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            UiAction::RemoveArchived(index) => self.sessions.remove_archived(index),
            UiAction::RenameSession(id, name) => {
                if let Some(s) = self.sessions.get_mut(id) {
                    let name = name.trim();
                    s.custom_title = (!name.is_empty()).then(|| name.to_owned());
                }
            }
            UiAction::DragWindow => {
                if let Some(g) = &self.gfx {
                    let _ = g.window.drag_window();
                }
            }
            UiAction::MoveTab(id, target) => {
                if self.workspace.move_tab(id, target)
                    && let Some(s) = self.sessions.get(id)
                {
                    s.clear_attention();
                }
            }
            UiAction::SplitPane(pane, dir) => {
                self.workspace.focus_pane(pane);
                self.spawn(None, Some(dir));
            }
            UiAction::ResizeSplit { path, index, delta, min } => self.workspace.resize_split(&path, index, delta, min),
            UiAction::EqualizeSplits => self.workspace.equalize(),
            UiAction::DismissError => {
                self.error = None;
                self.config_error = None;
            }
        }
        self.request_redraw();
    }

    // --- Input ----------------------------------------------------------------

    fn toggle_settings(&mut self) {
        if self.settings.open {
            self.settings.open = false;
            self.settings.recording = None;
        } else {
            let saved = match self.config_file.load_from_disk() {
                Ok(c) => c,
                Err(_) => self.config.clone(),
            };
            self.settings.open_with(&self.config, &saved);
            if self.font_families.is_none() {
                self.font_families = self.gfx.as_ref().map(|g| g.renderer.monospace_families());
            }
        }
        self.request_redraw();
    }

    /// While settings are open the width only goes to the draft (saved with
    /// it); otherwise it's written to config.toml on release.
    fn resize_sidebar(&mut self, width: f64, done: bool) {
        let width = width.round().clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
        self.config.window.sidebar_width = width;
        if self.settings.open {
            self.settings.draft.window.sidebar_width = width;
        } else if done && let Err(e) = self.config_file.save(&self.config) {
            self.error = Some(e.to_string());
        }
    }

    /// Applies the settings draft live.
    fn on_settings_changed(&mut self) {
        let draft = self.settings.draft.clone();
        if draft != self.config {
            self.apply_config(draft);
        }
        self.request_redraw();
    }

    fn handle_settings(&mut self, out: SettingsOutput) {
        if out.changed {
            self.on_settings_changed();
        }
        if out.save {
            match self.config_file.save(&self.config) {
                Ok(()) => {
                    self.settings.saved = self.config.clone();
                    self.settings.status = Some((format!("Saved to {}", self.config_file.paths.config_file.display()), false));
                }
                Err(e) => self.settings.status = Some((e.to_string(), true)),
            }
        }
        if out.revert {
            match self.config_file.load_from_disk() {
                Ok(config) => {
                    self.settings.draft = config.clone();
                    self.settings.saved = config.clone();
                    self.apply_config(config);
                    self.settings.status = Some(("Reverted to config.toml".into(), false));
                }
                Err(e) => self.settings.status = Some((e.to_string(), true)),
            }
        }
        if out.close {
            self.settings.open = false;
            self.settings.recording = None;
        }
        self.request_redraw();
    }

    /// Token usage for each session running a known agent.
    fn usage_views(&self, summaries: &[crate::session::SessionSummary]) -> HashMap<SessionId, UsageView> {
        let store = self.monitor.store().read();
        let now = now_ms();
        let mut views = HashMap::new();
        for s in summaries {
            let agent = match &s.profile {
                Some(name) => self.config.agent(name),
                // A plain shell where the user launched an agent by hand.
                None => self.config.agents.iter().find(|a| a.icon == s.icon && s.icon != "shell"),
            };
            let Some(agent) = agent else { continue };
            let session = store.latest_session(&agent.name, &s.cwd, s.started_unix_ms.saturating_sub(5_000));
            let report = store.budget(agent, session, now);
            views.insert(s.id, UsageView {
                agent: agent.name.clone(),
                model: session.and_then(|u| u.model.clone()),
                session_tokens: session.map(|u| u.usage.billable(agent.budget.include_cache_reads)).unwrap_or(0),
                session_budget: report.session,
                weekly: report.weekly,
                weekly_used: report.weekly_used,
            });
        }
        views
    }

    fn weekly_usage(&self) -> HashMap<String, u64> {
        let store = self.monitor.store().read();
        let now = now_ms();
        self.config.agents.iter().map(|a| (a.name.clone(), store.budget(a, None, now).weekly_used)).collect()
    }

    fn egui_wants_keyboard(&self) -> bool {
        self.gfx.as_ref().is_some_and(|g| g.egui_ctx.egui_wants_keyboard_input())
    }

    /// Returns true if the key was consumed by a shortcut or the terminal.
    fn on_key(&mut self, event: &KeyEvent) -> bool {
        if event.state != ElementState::Pressed {
            return false;
        }
        let key = event.key_without_modifiers();
        if self.settings.open {
            if self.settings.recording.is_some() {
                // Modifier-only presses don't finish a recording.
                if matches!(&key, Key::Named(n) if matches!(n, winit::keyboard::NamedKey::Shift
                    | winit::keyboard::NamedKey::Control | winit::keyboard::NamedKey::Alt
                    | winit::keyboard::NamedKey::Super | winit::keyboard::NamedKey::Meta))
                {
                    return true;
                }
                let combo = (key != Key::Named(winit::keyboard::NamedKey::Escape)).then(|| input::combo_for(&key, self.modifiers)).flatten();
                self.settings.record(combo);
                self.on_settings_changed();
                return true;
            }
            let toggles = input::combo_for(&key, self.modifiers)
                .is_some_and(|c| self.config.keybindings.action_for(&c) == Some(Action::ToggleSettings));
            if toggles {
                self.toggle_settings();
                return true;
            }
            return false; // everything else belongs to the panel
        }
        if let Some(combo) = input::combo_for(&key, self.modifiers)
            && let Some(action) = self.config.keybindings.action_for(&combo)
        {
            self.perform(action);
            return true;
        }
        if self.egui_wants_keyboard() {
            return false;
        }
        // Unbound Cmd/Super shortcuts never reach the shell as text.
        if self.modifiers.super_key() {
            return true;
        }
        // Ctrl+V (unbound) with an image or files on the clipboard pastes
        // their path, as Windows Terminal does; with text it stays ^V.
        let plain_ctrl = self.modifiers.control_key() && !self.modifiers.shift_key() && !self.modifiers.alt_key();
        if plain_ctrl
            && matches!(&key, Key::Character(c) if c.eq_ignore_ascii_case("v"))
            && self.focused().is_some()
            && let Some(text) = self.clipboard().and_then(crate::paste::clipboard_non_text)
        {
            self.paste_into_focused(&text);
            return true;
        }
        let Some(session) = self.focused() else { return false };
        let mode = session.with_term(|t| *t.mode());
        let modes = KeyModes {
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            option_as_alt: !cfg!(target_os = "macos") || self.config.terminal.option_as_alt,
        };
        let key = if matches!(event.logical_key, Key::Named(_)) { &event.logical_key } else { &key };
        if let Some(bytes) = input::encode_key(key, event.text.as_deref(), self.modifiers, modes) {
            session.with_term(|t| t.selection = None);
            session.scroll_to_bottom();
            session.clear_attention();
            session.write(bytes);
            self.blink_epoch = Instant::now();
            self.request_redraw();
        }
        true
    }

    fn hit(&self, x: f64, y: f64) -> Option<PaneHit> {
        if self.dividers.iter().any(|d| d.contains(x as f32, y as f32)) {
            return None; // resizing panes, not clicking a terminal
        }
        self.panes.iter().copied().find(|p| p.area.contains(x as f32, y as f32))
    }

    /// Cell under the pointer, clamped to the grid, plus the cell side.
    fn grid_point(&self, hit: &PaneHit, x: f64, y: f64) -> (usize, usize, Side) {
        let Some(g) = &self.gfx else { return (0, 0, Side::Left) };
        let m = g.renderer.cell_metrics();
        let fx = ((x as f32 - hit.origin[0]) / m.width).max(0.0);
        let fy = ((y as f32 - hit.origin[1]) / m.height).max(0.0);
        let col = (fx as usize).min(hit.cols.saturating_sub(1));
        let row = (fy as usize).min(hit.rows.saturating_sub(1));
        let side = if fx.fract() < 0.5 { Side::Left } else { Side::Right };
        (row, col, side)
    }

    fn mouse_mode(&self, id: SessionId) -> Option<(bool, bool)> {
        let mode = self.sessions.get(id)?.with_term(|t| *t.mode());
        mode.intersects(TermMode::MOUSE_MODE).then(|| (mode.contains(TermMode::SGR_MOUSE), mode.contains(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION)))
    }

    fn on_mouse_button(&mut self, state: ElementState, button: WinitButton) -> bool {
        if self.settings.open {
            return false;
        }
        let (x, y) = (self.mouse.pos.x, self.mouse.pos.y);
        let button = match button {
            WinitButton::Left => MouseButton::Left,
            WinitButton::Middle => MouseButton::Middle,
            WinitButton::Right => MouseButton::Right,
            _ => return false,
        };
        if state == ElementState::Released {
            self.mouse.selecting = None;
            if let Some((id, b)) = self.mouse.reporting.take()
                && let Some((sgr, _)) = self.mouse_mode(id)
                && let Some(hit) = self.panes.iter().copied().find(|p| p.session == Some(id))
            {
                let (row, col, _) = self.grid_point(&hit, x, y);
                let bytes = input::mouse_report(b, MouseEventKind::Release, col, row, self.modifiers, sgr);
                if let Some(s) = self.sessions.get(id) {
                    s.write(bytes);
                }
            }
            return false;
        }
        let Some(hit) = self.hit(x, y) else { return false };
        self.workspace.focus_pane(hit.pane);
        let Some(id) = hit.session.filter(|id| *id != USAGE_TAB) else { return true };
        let (row, col, side) = self.grid_point(&hit, x, y);
        if let Some(s) = self.sessions.get(id) {
            s.clear_attention();
        }
        if button == MouseButton::Left
            && let Some((_, link)) = self.link_under_mouse()
        {
            if let Err(e) = links::open(&link.target, self.config.links.editor.as_deref()) {
                self.error = Some(e);
            }
            return true;
        }

        // Apps with mouse tracking get the click (Shift forces selection).
        if let Some((sgr, _)) = self.mouse_mode(id).filter(|_| !self.modifiers.shift_key()) {
            let bytes = input::mouse_report(button, MouseEventKind::Press, col, row, self.modifiers, sgr);
            if let Some(s) = self.sessions.get(id) {
                s.write(bytes);
            }
            self.mouse.reporting = Some((id, button));
            return true;
        }
        if button != MouseButton::Left {
            return true;
        }
        let now = Instant::now();
        let clicks = match self.mouse.last_click {
            Some((t, pos, n)) if pos == (row, col) && now.duration_since(t) < Duration::from_millis(400) => n % 3 + 1,
            _ => 1,
        };
        self.mouse.last_click = Some((now, (row, col), clicks));
        let ty = match clicks {
            2 => SelectionType::Semantic,
            3 => SelectionType::Lines,
            _ => SelectionType::Simple,
        };
        if let Some(s) = self.sessions.get(id) {
            s.with_term(|t| {
                let point = Point::new(Line(row as i32 - t.grid().display_offset() as i32), Column(col));
                let mut selection = Selection::new(ty, point, side);
                if ty != SelectionType::Simple {
                    selection.update(point, side);
                }
                t.selection = Some(selection);
            });
        }
        self.mouse.selecting = Some(id);
        self.request_redraw();
        true
    }

    /// Cmd (macOS) or Ctrl held: links under the mouse are clickable.
    fn link_modifier(&self) -> bool {
        if cfg!(target_os = "macos") { self.modifiers.super_key() } else { self.modifiers.control_key() }
    }

    fn link_under_mouse(&mut self) -> Option<(SessionId, Link)> {
        if self.settings.open || !self.link_modifier() {
            return None;
        }
        let (x, y) = (self.mouse.pos.x, self.mouse.pos.y);
        let hit = self.hit(x, y)?;
        let id = hit.session.filter(|id| *id != USAGE_TAB)?;
        let (row, col, _) = self.grid_point(&hit, x, y);
        let session = self.sessions.get(id)?;
        let cwd = session.current_dir();
        let detector = &mut self.links;
        let link = session.with_term(|t| {
            let point = Point::new(Line(row as i32 - t.grid().display_offset() as i32), Column(col));
            detector.link_at(t, point, &cwd)
        });
        link.map(|l| (id, l))
    }

    fn update_hover_link(&mut self) {
        let link = self.link_under_mouse();
        if link != self.hover_link {
            self.hover_link = link;
            self.request_redraw();
        }
    }

    fn on_cursor_moved(&mut self, pos: PhysicalPosition<f64>) {
        self.mouse.pos = pos;
        if self.mouse.selecting.is_none() {
            self.update_hover_link();
        }
        if let Some(id) = self.mouse.selecting
            && let Some(hit) = self.panes.iter().copied().find(|p| p.session == Some(id))
        {
            let (row, col, side) = self.grid_point(&hit, pos.x, pos.y);
            if let Some(s) = self.sessions.get(id) {
                s.with_term(|t| {
                    let point = Point::new(Line(row as i32 - t.grid().display_offset() as i32), Column(col));
                    if let Some(sel) = t.selection.as_mut() {
                        sel.update(point, side);
                    }
                });
            }
            self.request_redraw();
        } else if let Some((id, button)) = self.mouse.reporting
            && let Some((sgr, true)) = self.mouse_mode(id)
            && let Some(hit) = self.panes.iter().copied().find(|p| p.session == Some(id))
        {
            let (row, col, _) = self.grid_point(&hit, pos.x, pos.y);
            if let Some(s) = self.sessions.get(id) {
                s.write(input::mouse_report(button, MouseEventKind::Drag, col, row, self.modifiers, sgr));
            }
        }
    }

    fn on_wheel(&mut self, delta: MouseScrollDelta) -> bool {
        if self.settings.open {
            return false;
        }
        // The text under the mouse moves: found again on the next mouse move.
        self.hover_link = None;
        let (x, y) = (self.mouse.pos.x, self.mouse.pos.y);
        let Some(hit) = self.hit(x, y) else { return false };
        let Some(id) = hit.session else { return true };
        if id == USAGE_TAB {
            return false; // the dashboard scrolls itself
        }
        let cell_h = self.gfx.as_ref().map(|g| g.renderer.cell_metrics().height as f64).unwrap_or(16.0);
        let lines = match delta {
            MouseScrollDelta::LineDelta(_, dy) => dy as f64 * self.config.terminal.scroll_multiplier,
            MouseScrollDelta::PixelDelta(p) => p.y / cell_h,
        };
        self.mouse.wheel_accum += lines;
        let whole = self.mouse.wheel_accum.trunc() as i32;
        if whole == 0 {
            return true;
        }
        self.mouse.wheel_accum -= whole as f64;

        if let Some((sgr, _)) = self.mouse_mode(id) {
            let (row, col, _) = self.grid_point(&hit, x, y);
            let button = if whole > 0 { MouseButton::WheelUp } else { MouseButton::WheelDown };
            let report = input::mouse_report(button, MouseEventKind::Press, col, row, self.modifiers, sgr);
            if let Some(s) = self.sessions.get(id) {
                s.write(report.repeat(whole.unsigned_abs() as usize));
            }
            return true;
        }
        let Some(session) = self.sessions.get(id) else { return true };
        let mode = session.with_term(|t| *t.mode());
        match input::wheel_action(
            whole,
            mode.contains(TermMode::ALT_SCREEN),
            mode.contains(TermMode::ALTERNATE_SCROLL),
            mode.contains(TermMode::APP_CURSOR),
        ) {
            WheelAction::Scrollback(n) => session.scroll(n),
            WheelAction::Send(bytes) => session.write(bytes),
        }
        self.request_redraw();
        true
    }

    // --- Frame ----------------------------------------------------------------

    fn blink_on(&self) -> bool {
        if !self.config.cursor.blink {
            return true;
        }
        let interval = self.config.cursor.blink_interval_ms.max(100) as u128;
        (self.blink_epoch.elapsed().as_millis() / interval).is_multiple_of(2)
    }

    fn redraw(&mut self) {
        let now = Instant::now();
        let timeout = self.activity_timeout();
        let blink_on = self.blink_on();
        let new_session_shortcut = self.config.keybindings.combo(Action::NewSession).map(|c| c.display()).unwrap_or_default();
        let search_shortcut = self.config.keybindings.combo(Action::SearchSessions).map(|c| c.display()).unwrap_or_default();
        let traffic_lights = self.traffic_lights();
        if self.monitor_reconfigure_at.is_some_and(|t| now >= t) {
            self.monitor_reconfigure_at = None;
            self.monitor.reconfigure(&self.config);
        }
        let mut summaries = self.sessions.summaries(now, timeout);
        let usage = self.usage_views(&summaries);
        self.resources.set_roots(self.sessions.iter().filter_map(|s| Some((s.id, s.pid()?))).collect());
        let resource_usage = if self.config.resources.active(self.config.window.pane_footer) {
            self.resources.snapshot()
        } else {
            HashMap::new()
        };
        // A shell where an agent was detected is named after the agent.
        for s in &mut summaries {
            if s.profile.is_none()
                && let Some(u) = usage.get(&s.id)
                && self.sessions.get(s.id).is_some_and(|session| session.custom_title.is_none())
            {
                s.title = u.agent.clone();
            }
        }
        let weekly_usage = if self.settings.open { self.weekly_usage() } else { HashMap::new() };
        let mut settings_out = SettingsOutput::default();
        let usage_stats = self.usage_stats();
        let usage_loading = usage_stats.is_some() && self.monitor.store().read().is_loading();
        let stats_before = self.stats.clone();

        // OSC 52 copies requested by apps.
        let mut copies = Vec::new();
        for s in self.sessions.iter() {
            s.take_dirty();
            copies.extend(s.take_clipboard_request());
        }
        for text in copies {
            if let Some(cb) = self.clipboard() {
                let _ = cb.set_text(text);
            }
        }

        let banner = self.error.clone().or_else(|| self.config_error.as_ref().map(|e| format!("config.toml: {e}")));
        let Self { gfx, chrome, sessions, workspace, config, theme, themes, sidebar_visible, settings, font_families, stats, hover_link, .. } = self;
        let Some(g) = gfx.as_mut() else { return };
        let scrolled: Vec<SessionId> = workspace.visible_sessions().into_iter().filter(|id| sessions.get(*id).is_some_and(|s| s.is_scrolled())).collect();
        let ui_colors = theme.ui();
        let input = ChromeInput {
            colors: &ui_colors,
            terminal_background: theme.colors.background,
            summaries: &summaries,
            usage: &usage,
            resources: &resource_usage,
            resource_display: &config.resources,
            archived: sessions.archived(),
            workspace,
            agents: &config.agents,
            sidebar_visible: *sidebar_visible,
            sidebar_width: config.window.sidebar_width.clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH) as f32,
            sidebar_actions: config.window.sidebar_actions,
            pane_footer: config.window.pane_footer,
            tab_style: config.window.tab_style,
            tab_radius: config.window.tab_radius.clamp(0.0, 14.0) as f32,
            tab_max_width: config.window.tab_max_width.clamp(80.0, 400.0) as f32,
            traffic_lights,
            scrolled: &scrolled,
            new_session_shortcut,
            search_shortcut,
            error: banner.as_deref(),
        };
        let empty: Vec<String> = Vec::new();
        let settings_input = SettingsInput {
            colors: &ui_colors,
            themes,
            font_families: font_families.as_deref().unwrap_or(&empty),
            weekly_usage: &weekly_usage,
        };

        let raw = g.egui_state.take_egui_input(&g.window);
        let mut out = ChromeOutput::default();
        let full = g.egui_ctx.run_ui(raw, |ui| {
            out = chrome::draw(ui, chrome, &input);
            if let Some(usage) = &usage_stats {
                let stats_input = StatsInput {
                    colors: &ui_colors,
                    background: theme.colors.background,
                    stats: usage,
                    agents: &config.agents,
                    loading: usage_loading,
                    monitor_enabled: config.monitor.enabled,
                };
                for view in out.panes.iter().filter(|v| v.session == Some(USAGE_TAB)) {
                    stats_ui::draw(ui, view.terminal, stats, &stats_input);
                }
            }
            if settings.open {
                settings_out = settings_ui::show(ui.ctx(), settings, &settings_input);
            }
            if let Some((_, link)) = hover_link.as_ref() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                // OSC 8 text may hide where it leads: show it.
                if let (Some(uri), Some(pos)) = (&link.uri, ui.ctx().pointer_hover_pos()) {
                    let uri: String = uri.chars().take(200).collect();
                    egui::Area::new(egui::Id::unique("link-uri"))
                        .order(egui::Order::Tooltip)
                        .fixed_pos(pos + egui::vec2(12.0, 16.0))
                        .show(ui.ctx(), |ui| egui::Frame::popup(ui.style()).show(ui, |ui| ui.label(uri)));
                }
            }
        });
        g.egui_state.handle_platform_output(&g.window, full.platform_output);
        let ppp = full.pixels_per_point;
        let primitives = g.egui_ctx.tessellate(full.shapes, ppp);
        self.egui_repaint_at = full
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.repaint_delay)
            .filter(|d| *d < Duration::from_secs(1))
            .map(|d| now + d);

        // Terminal geometry (physical px) + snapshots.
        let m = g.renderer.cell_metrics();
        let (pad_x, pad_y) = (config.window.padding_x as f32 * ppp, config.window.padding_y as f32 * ppp);
        let focused_pane = workspace.focused_pane();
        let mut hits = Vec::new();
        let mut snapshots: Vec<(PaneHit, CursorDraw, GridSnapshot, Vec<LinkSpan>)> = Vec::new();
        for view in &out.panes {
            let t = view.terminal;
            let area = Rect::new(t.left() * ppp, t.top() * ppp, t.width() * ppp, t.height() * ppp);
            let origin = [(area.x + pad_x).round(), (area.y + pad_y).round()];
            let cols = (((area.w - 2.0 * pad_x) / m.width).floor() as i64).max(2) as usize;
            let rows = (((area.h - 2.0 * pad_y) / m.height).floor() as i64).max(1) as usize;
            let hit = PaneHit { pane: view.pane, session: view.session, area, origin, cols, rows };
            hits.push(hit);
            let Some(session) = view.session.and_then(|id| sessions.get_mut(id)) else { continue };
            session.resize(cols as u16, rows as u16, m.width as u16, m.height as u16);
            let cursor = if focused_pane == Some(view.pane) && self.window_focused {
                if blink_on { CursorDraw::Focused } else { CursorDraw::Hidden }
            } else {
                CursorDraw::Unfocused
            };
            let snap = session.snapshot(theme);
            let link = match hover_link.as_ref() {
                Some((id, link)) if Some(*id) == view.session => link_spans(link, &snap),
                _ => Vec::new(),
            };
            snapshots.push((hit, cursor, snap, link));
        }
        let frames: Vec<PaneFrame<'_>> = snapshots
            .iter()
            .map(|(hit, cursor, snap, link)| PaneFrame {
                session: hit.session.expect("snapshot has a session"),
                area: [hit.area.x, hit.area.y, hit.area.w, hit.area.h],
                origin: hit.origin,
                snapshot: snap,
                cursor: *cursor,
                link,
            })
            .collect();
        let egui_frame = EguiFrame { primitives, textures: full.textures_delta, pixels_per_point: ppp };
        let mut quit_after = false;
        if let Some((at, path)) = &self.capture
            && now >= *at
        {
            g.renderer.capture_next_frame(path.clone());
            quit_after = true;
        }
        if let Err(e) = g.renderer.render(ui_colors.chrome_background, config.window.opacity as f32, &frames, egui_frame) {
            log::error!("render: {e}");
        }
        drop(frames);
        self.panes = hits;
        self.dividers = out.dividers.iter().map(|r| Rect::new(r.left() * ppp, r.top() * ppp, r.width() * ppp, r.height() * ppp)).collect();
        if quit_after {
            self.capture = None;
            std::process::exit(0);
        }

        for action in out.actions {
            self.apply_ui_action(action);
        }
        if self.stats != stats_before {
            // This frame drew stats for the old view; show the new one.
            if self.stats.range != stats_before.range {
                self.monitor.load_history(self.stats.range.history_needed());
            }
            self.request_redraw();
        }
        if self.settings.open || settings_out.close {
            self.handle_settings(settings_out);
        }
    }

    /// Next instant something time-based must be redrawn.
    fn next_deadline(&self) -> Option<Instant> {
        let mut next: Option<Instant> = self.egui_repaint_at.or(self.capture.as_ref().map(|(t, _)| *t)).or(self.monitor_reconfigure_at);
        let mut consider = |t: Instant| next = Some(next.map_or(t, |n| n.min(t)));
        if self.config.cursor.blink && self.window_focused && self.focused().is_some() {
            let interval = self.config.cursor.blink_interval_ms.max(100);
            let elapsed = self.blink_epoch.elapsed().as_millis() as u64;
            consider(self.blink_epoch + Duration::from_millis((elapsed / interval + 1) * interval));
        }
        // Elapsed times and Working → Waiting transitions in the sidebar.
        if !self.sessions.is_empty() {
            consider(Instant::now() + Duration::from_millis(1000));
        }
        next
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.gfx.is_some() {
            return;
        }
        if let Err(e) = self.create_window(event_loop) {
            log::error!("{e}");
            event_loop.exit();
            return;
        }
        if self.sessions.is_empty() {
            self.spawn(None, None);
            if let Ok(text) = std::env::var("TERMY_DEMO_INPUT")
                && let Some(s) = self.focused()
            {
                s.write(text.replace("\\n", "\r"));
            }
            // e.g. TERMY_DEMO_ACTIONS="split_vertical,toggle_sidebar"
            if let Ok(actions) = std::env::var("TERMY_DEMO_ACTIONS") {
                use serde::Deserialize;
                for name in actions.split(',').map(str::trim).filter(|n| !n.is_empty()) {
                    match Action::deserialize(serde::de::value::StrDeserializer::<serde::de::value::Error>::new(name)) {
                        Ok(action) => self.perform(action),
                        Err(e) => log::warn!("TERMY_DEMO_ACTIONS: {e}"),
                    }
                }
            }
        }
    }

    fn new_events(&mut self, _: &ActiveEventLoop, cause: StartCause) {
        if matches!(cause, StartCause::ResumeTimeReached { .. }) {
            self.request_redraw();
        }
    }

    fn user_event(&mut self, _: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Wake => self.request_redraw(),
            UserEvent::ConfigChanged => self.reload_config(),
            UserEvent::ThemesChanged => self.reload_themes(),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        // Keyboard goes to egui only when it wants it (search box, settings),
        // otherwise Tab etc. would move egui focus instead of reaching the shell.
        let to_egui = match &event {
            WindowEvent::KeyboardInput { event: key, is_synthetic: false, .. } => !self.on_key(key),
            WindowEvent::KeyboardInput { .. } => false,
            WindowEvent::Ime(Ime::Commit(text)) if !self.egui_wants_keyboard() => {
                if let Some(s) = self.focused() {
                    s.write(text.clone());
                }
                false
            }
            WindowEvent::MouseInput { state, button, .. } => {
                self.on_mouse_button(*state, *button);
                true
            }
            WindowEvent::MouseWheel { delta, .. } => !self.on_wheel(*delta),
            WindowEvent::CursorMoved { position, .. } => {
                self.on_cursor_moved(*position);
                true
            }
            // egui-winit answers `repaint: true` to RedrawRequested, which would
            // request another redraw from every redraw: an endless loop at the
            // display's refresh rate even when idle.
            WindowEvent::RedrawRequested => false,
            _ => true,
        };
        if to_egui && let Some(g) = &mut self.gfx {
            let response = g.egui_state.on_window_event(&g.window, &event);
            if response.repaint {
                g.window.request_redraw();
            }
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(g) = &mut self.gfx {
                    g.renderer.resize(size.width, size.height);
                }
                self.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let font = self.effective_font();
                if let Some(g) = &mut self.gfx {
                    g.renderer.set_font(&font, scale_factor as f32);
                }
                self.request_redraw();
            }
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
                self.update_hover_link();
            }
            WindowEvent::Focused(focused) => {
                self.window_focused = focused;
                self.request_redraw();
            }
            WindowEvent::RedrawRequested => self.redraw(),
            // Files dropped on the window paste their (quoted) paths.
            WindowEvent::DroppedFile(path) if !self.settings.open => {
                let text = crate::paste::quote_path(&path);
                self.paste_into_focused(&format!("{text} "));
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(match self.next_deadline() {
            Some(t) => ControlFlow::WaitUntil(t),
            None => ControlFlow::Wait,
        });
    }
}

/// Makes the native title bar (when there is one) follow the theme. Every OS
/// gets light/dark: GTK and Wayland decorations and macOS's native bar honour
/// it. Windows 11 also takes exact colors: the chrome background, text and
/// border (older Windows ignores them and keeps the light/dark hint).
/// One visible row of a link: (row, first col, last col).
type LinkSpan = (usize, usize, usize);

fn link_spans(link: &Link, snap: &GridSnapshot) -> Vec<LinkSpan> {
    let offset = snap.display_offset as i32;
    (link.start.line.0..=link.end.line.0)
        .filter_map(|line| {
            let row = usize::try_from(line + offset).ok().filter(|r| *r < snap.rows)?;
            let first = if line == link.start.line.0 { link.start.column.0 } else { 0 };
            let last = if line == link.end.line.0 { link.end.column.0 } else { snap.cols - 1 };
            Some((row, first, last.min(snap.cols - 1)))
        })
        .collect()
}

fn style_title_bar(window: &Window, ui: &crate::theme::UiColors, light: bool) {
    window.set_theme(Some(if light { winit::window::Theme::Light } else { winit::window::Theme::Dark }));
    #[cfg(target_os = "windows")]
    {
        use winit::platform::windows::{Color, WindowExtWindows};
        let c = |c: crate::theme::Rgb| Color::from_rgb(c.r, c.g, c.b);
        window.set_title_background_color(Some(c(ui.chrome_background)));
        window.set_title_text_color(c(ui.text_primary));
        window.set_border_color(Some(c(ui.border)));
    }
    #[cfg(not(target_os = "windows"))]
    let _ = ui;
}

/// The app icon for the title bar, taskbar and Alt+Tab (Windows / Linux).
#[cfg(not(target_os = "macos"))]
fn window_icon() -> Option<winit::window::Icon> {
    const PNG: &[u8] = include_bytes!("../assets/windows/icon-256.png");
    let mut decoder = png::Decoder::new(std::io::Cursor::new(PNG));
    decoder.set_transformations(png::Transformations::normalize_to_color8() | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().ok()?;
    let mut rgba = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut rgba).ok()?;
    if info.color_type != png::ColorType::Rgba {
        return None;
    }
    rgba.truncate(info.buffer_size());
    winit::window::Icon::from_rgba(rgba, info.width, info.height).ok()
}

#[cfg(all(test, not(target_os = "macos")))]
mod icon_tests {
    #[test]
    fn window_icon_decodes() {
        assert!(super::window_icon().is_some());
    }
}
