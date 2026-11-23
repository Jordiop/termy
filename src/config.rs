//! `AppConfig`: clean TOML in `~/.config/termy/config.toml` (generated with
//! defaults if missing), hot-reload via `notify`, format-preserving `save()`
//! and `[[agents]]` profiles (name, command, icon/color, metrics source,
//! token budgets).

use std::collections::BTreeMap;
use std::fmt;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};

use notify::{EventKind, RecursiveMode, Watcher};
use serde::{Deserialize, Deserializer, Serialize};

use crate::theme::{DEFAULT_THEME_ID, Rgb};

pub const APP_NAME: &str = "termy";

// ---------------------------------------------------------------------------
// Locations
// ---------------------------------------------------------------------------

/// Every file termy reads or writes. Same layout on every OS:
/// `~/.config/termy/` (or `$XDG_CONFIG_HOME/termy`, or `$TERMY_CONFIG_DIR`).
#[derive(Clone, Debug)]
pub struct ConfigPaths {
    pub dir: PathBuf,
    pub config_file: PathBuf,
    pub themes_dir: PathBuf,
    /// Archived (closed) sessions, recoverable from the sidebar.
    pub archive_file: PathBuf,
}

impl ConfigPaths {
    pub fn at(dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        Self {
            config_file: dir.join("config.toml"),
            themes_dir: dir.join("themes"),
            archive_file: dir.join("archive.json"),
            dir,
        }
    }

    pub fn default_location() -> Self {
        if let Some(dir) = std::env::var_os("TERMY_CONFIG_DIR") {
            return Self::at(dir);
        }
        let xdg = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty());
        let base = match xdg {
            Some(xdg) if !cfg!(windows) => PathBuf::from(xdg),
            _ => dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".config"),
        };
        Self::at(base.join(APP_NAME))
    }
}

/// Expands a leading `~` / `~/` to the home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    let home = || dirs::home_dir().unwrap_or_default();
    if path == "~" {
        home()
    } else if let Some(rest) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        home().join(rest)
    } else {
        PathBuf::from(path)
    }
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AppConfig {
    pub window: WindowConfig,
    pub font: FontConfig,
    pub cursor: CursorConfig,
    pub terminal: TerminalConfig,
    pub appearance: AppearanceConfig,
    pub monitor: MonitorConfig,
    pub resources: ResourcesConfig,
    pub keybindings: Keybindings,
    /// `[[agents]]`: AI CLI profiles. Must stay last (array of tables).
    pub agents: Vec<AgentProfile>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decorations {
    /// Native title bar and borders.
    Full,
    /// Native window controls over our own chrome (macOS-style unified bar).
    Transparent,
    /// No decorations at all.
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TabStyle {
    /// Square tabs, accent line on top of the active one.
    Flat,
    /// Browser-style: rounded top corners, the active tab joins the terminal.
    Rounded,
    /// Floating rounded pills.
    Pill,
    /// Text only, accent underline under the active tab.
    Underline,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowConfig {
    /// Background opacity, 0.0 (transparent) – 1.0 (opaque).
    pub opacity: f64,
    pub padding_x: f64,
    pub padding_y: f64,
    pub decorations: Decorations,
    pub columns: u16,
    pub rows: u16,
    pub tab_style: TabStyle,
    /// Corner radius of `rounded` / `pill` tabs, in points.
    pub tab_radius: f64,
    /// Widest a tab grows to, in points (tabs shrink to fit).
    pub tab_max_width: f64,
    pub sidebar_visible: bool,
    pub sidebar_width: f64,
    /// Per-pane status footer (agent state, model, token budget).
    pub pane_footer: bool,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            opacity: 1.0,
            padding_x: 8.0,
            padding_y: 6.0,
            decorations: if cfg!(target_os = "macos") { Decorations::Transparent } else { Decorations::Full },
            columns: 120,
            rows: 34,
            tab_style: TabStyle::Rounded,
            tab_radius: 8.0,
            tab_max_width: 190.0,
            sidebar_visible: true,
            sidebar_width: 220.0,
            pane_footer: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FontConfig {
    pub family: String,
    /// Tried in order for glyphs missing from `family` (Nerd Font icons, emoji…).
    pub fallbacks: Vec<String>,
    pub size: f64,
    /// Multiplier over the font's natural line height.
    pub line_height: f64,
    pub ligatures: bool,
}

impl Default for FontConfig {
    fn default() -> Self {
        let (family, emoji) = if cfg!(target_os = "macos") {
            ("Menlo", "Apple Color Emoji")
        } else if cfg!(windows) {
            ("Cascadia Mono", "Segoe UI Emoji")
        } else {
            ("DejaVu Sans Mono", "Noto Color Emoji")
        };
        Self {
            family: family.into(),
            fallbacks: vec!["Symbols Nerd Font Mono".into(), emoji.into()],
            size: 13.0,
            line_height: 1.2,
            ligatures: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CursorShape {
    #[default]
    Block,
    Beam,
    Underline,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CursorConfig {
    pub shape: CursorShape,
    pub blink: bool,
    pub blink_interval_ms: u64,
}

impl Default for CursorConfig {
    fn default() -> Self {
        Self { shape: CursorShape::Block, blink: true, blink_interval_ms: 530 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ShellConfig {
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalConfig {
    /// `None` → OS default (`$SHELL`/zsh, pwsh/powershell/cmd).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<ShellConfig>,
    /// Directory for new sessions. `None` → home directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    pub scrollback: usize,
    /// Lines per mouse-wheel notch.
    pub scroll_multiplier: f64,
    /// Output within this window marks a session as `Working`.
    pub activity_timeout_ms: u64,
    /// macOS: send Option+key as Meta (ESC prefix) instead of composing
    /// characters (ñ, ´, €…). Always on for Windows/Linux Alt.
    pub option_as_alt: bool,
    pub env: BTreeMap<String, String>,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            shell: None,
            working_directory: None,
            scrollback: 10_000,
            scroll_multiplier: 3.0,
            activity_timeout_ms: 1_500,
            option_as_alt: false,
            env: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceConfig {
    pub theme: String,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self { theme: DEFAULT_THEME_ID.into() }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitorConfig {
    /// Watch the agents' local logs for token usage. Never touches the network.
    pub enabled: bool,
    /// How many days of logs to scan at startup (≥ 7 for weekly budgets).
    pub scan_days: u32,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self { enabled: true, scan_days: 8 }
    }
}

/// `[resources]`: per-tab CPU / RAM / disk usage in the pane footer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourcesConfig {
    pub enabled: bool,
    pub show_cpu: bool,
    pub show_memory: bool,
    pub show_disk: bool,
    /// How often the process table is sampled.
    pub interval_ms: u64,
}

impl Default for ResourcesConfig {
    fn default() -> Self {
        Self { enabled: true, show_cpu: true, show_memory: true, show_disk: true, interval_ms: 2_000 }
    }
}

impl ResourcesConfig {
    /// Sampling is pointless if nothing would be shown.
    pub fn active(&self, pane_footer: bool) -> bool {
        pane_footer && self.enabled && (self.show_cpu || self.show_memory || self.show_disk)
    }
}

// ---------------------------------------------------------------------------
// Agents
// ---------------------------------------------------------------------------

/// `[[agents]]` — an AI CLI that can be launched as a session.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentProfile {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Icon key: `claude`, `gemini`, `codex`, `shell`, …
    #[serde(default = "default_icon")]
    pub icon: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Rgb>,
    #[serde(default)]
    pub metrics: MetricsSource,
    #[serde(default)]
    pub budget: TokenBudget,
}

fn default_icon() -> String {
    "shell".into()
}

/// Where token usage for an agent comes from. Always local.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MetricsSource {
    #[default]
    None,
    /// `~/.claude/projects/**/*.jsonl`
    ClaudeCode { path: String },
    /// `~/.codex/sessions/**/rollout-*.jsonl`
    Codex { path: String },
    /// `~/.gemini/tmp/**/chats/*.json`
    GeminiCli { path: String },
    /// Any JSONL log, fields addressed with JSON pointers (`/usage/input_tokens`).
    Jsonl(Box<JsonlSource>),
    /// A local command printing `{"used": N, "limit": N, "resets_at": "…"}`.
    Command {
        program: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default = "default_command_interval")]
        interval_secs: u64,
    },
}

fn default_command_interval() -> u64 {
    300
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JsonlSource {
    pub path: String,
    #[serde(default = "default_jsonl_ext")]
    pub extension: String,
    pub input_tokens: String,
    pub output_tokens: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_tokens: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_tokens: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Lines sharing this key are the same API call (deduplicated).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedupe_key: Option<String>,
    /// Values are running totals for the session instead of per-call deltas.
    #[serde(default)]
    pub cumulative: bool,
}

fn default_jsonl_ext() -> String {
    "jsonl".into()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct TokenBudget {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekly_tokens: Option<u64>,
    /// Fixed weekly reset in UTC, e.g. `"mon 09:00"`. Unset → rolling 7 days.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekly_reset: Option<String>,
    /// Count cache reads towards budgets (they are cheap and huge on Claude).
    pub include_cache_reads: bool,
}

pub fn default_agents() -> Vec<AgentProfile> {
    vec![
        AgentProfile {
            name: "Claude Code".into(),
            command: "claude".into(),
            args: vec![],
            icon: "claude".into(),
            color: Rgb::from_hex("#d97757"),
            metrics: MetricsSource::ClaudeCode { path: "~/.claude/projects".into() },
            budget: TokenBudget::default(),
        },
        AgentProfile {
            name: "Codex".into(),
            command: "codex".into(),
            args: vec![],
            icon: "codex".into(),
            color: Rgb::from_hex("#10a37f"),
            metrics: MetricsSource::Codex { path: "~/.codex/sessions".into() },
            budget: TokenBudget::default(),
        },
        AgentProfile {
            name: "Gemini CLI".into(),
            command: "gemini".into(),
            args: vec![],
            icon: "gemini".into(),
            color: Rgb::from_hex("#4796e3"),
            metrics: MetricsSource::GeminiCli { path: "~/.gemini/tmp".into() },
            budget: TokenBudget::default(),
        },
    ]
}

// ---------------------------------------------------------------------------
// Keybindings
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Copy,
    Paste,
    NewSession,
    CloseTab,
    NextTab,
    PrevTab,
    SplitVertical,
    SplitHorizontal,
    FocusNextPane,
    ToggleSidebar,
    SearchSessions,
    ToggleSettings,
    OpenUsage,
    IncreaseFont,
    DecreaseFont,
    ResetFont,
    ReloadConfig,
}

impl Action {
    pub const ALL: [Action; 17] = [
        Action::Copy,
        Action::Paste,
        Action::NewSession,
        Action::CloseTab,
        Action::NextTab,
        Action::PrevTab,
        Action::SplitVertical,
        Action::SplitHorizontal,
        Action::FocusNextPane,
        Action::ToggleSidebar,
        Action::SearchSessions,
        Action::ToggleSettings,
        Action::OpenUsage,
        Action::IncreaseFont,
        Action::DecreaseFont,
        Action::ResetFont,
        Action::ReloadConfig,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Action::Copy => "Copy",
            Action::Paste => "Paste",
            Action::NewSession => "New session",
            Action::CloseTab => "Close tab",
            Action::NextTab => "Next tab",
            Action::PrevTab => "Previous tab",
            Action::SplitVertical => "Split vertically",
            Action::SplitHorizontal => "Split horizontally",
            Action::FocusNextPane => "Focus next pane",
            Action::ToggleSidebar => "Toggle sidebar",
            Action::SearchSessions => "Search sessions",
            Action::ToggleSettings => "Settings",
            Action::OpenUsage => "Usage dashboard",
            Action::IncreaseFont => "Increase font size",
            Action::DecreaseFont => "Decrease font size",
            Action::ResetFont => "Reset font size",
            Action::ReloadConfig => "Reload config",
        }
    }

    /// Defaults. On Windows/Linux, letter shortcuts use Ctrl+Shift so plain
    /// Ctrl+letter keeps reaching the shell (Ctrl+C, Ctrl+B, Ctrl+T, …).
    fn default_combo(self) -> &'static str {
        let mac = cfg!(target_os = "macos");
        match self {
            Action::Copy => if mac { "cmd+c" } else { "ctrl+shift+c" },
            Action::Paste => if mac { "cmd+v" } else { "ctrl+shift+v" },
            Action::NewSession => if mac { "cmd+t" } else { "ctrl+shift+t" },
            Action::CloseTab => if mac { "cmd+w" } else { "ctrl+shift+w" },
            Action::NextTab => if mac { "cmd+shift+bracketright" } else { "ctrl+tab" },
            Action::PrevTab => if mac { "cmd+shift+bracketleft" } else { "ctrl+shift+tab" },
            Action::SplitVertical => if mac { "cmd+d" } else { "ctrl+shift+d" },
            Action::SplitHorizontal => if mac { "cmd+shift+d" } else { "ctrl+shift+e" },
            Action::FocusNextPane => if mac { "cmd+bracketright" } else { "ctrl+shift+bracketright" },
            Action::ToggleSidebar => if mac { "cmd+b" } else { "ctrl+shift+b" },
            Action::SearchSessions => if mac { "cmd+k" } else { "ctrl+shift+k" },
            Action::ToggleSettings => if mac { "cmd+comma" } else { "ctrl+comma" },
            Action::OpenUsage => if mac { "cmd+shift+u" } else { "ctrl+shift+u" },
            Action::IncreaseFont => if mac { "cmd+equal" } else { "ctrl+equal" },
            Action::DecreaseFont => if mac { "cmd+minus" } else { "ctrl+minus" },
            Action::ResetFont => if mac { "cmd+0" } else { "ctrl+0" },
            Action::ReloadConfig => if mac { "cmd+shift+r" } else { "ctrl+shift+r" },
        }
    }
}

/// `[keybindings]`: `action = "combo"`. Missing actions keep their default;
/// an empty string disables the binding.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(transparent)]
pub struct Keybindings(pub BTreeMap<Action, String>);

impl Default for Keybindings {
    fn default() -> Self {
        Self(Action::ALL.iter().map(|a| (*a, a.default_combo().to_owned())).collect())
    }
}

impl<'de> Deserialize<'de> for Keybindings {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let user = BTreeMap::<Action, String>::deserialize(deserializer)?;
        if let Some((action, combo)) = user.iter().find(|(_, c)| !c.is_empty() && KeyCombo::parse(c).is_err()) {
            let err = KeyCombo::parse(combo).unwrap_err();
            return Err(serde::de::Error::custom(format!("keybinding `{action:?}`: {err}")));
        }
        let mut map = Keybindings::default().0;
        map.extend(user);
        Ok(Self(map))
    }
}

impl Keybindings {
    pub fn combo(&self, action: Action) -> Option<KeyCombo> {
        self.0.get(&action).filter(|c| !c.is_empty()).and_then(|c| KeyCombo::parse(c).ok())
    }

    /// Reverse lookup used by input routing.
    pub fn action_for(&self, pressed: &KeyCombo) -> Option<Action> {
        Action::ALL.into_iter().find(|a| self.combo(*a).as_ref() == Some(pressed))
    }
}

/// A parsed shortcut. `key` is a normalized lowercase name (`"t"`, `"comma"`,
/// `"tab"`, `"f5"`, …).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct KeyCombo {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Cmd on macOS, Windows/Super key elsewhere.
    pub logo: bool,
    pub key: String,
}

impl KeyCombo {
    pub fn parse(s: &str) -> Result<Self, String> {
        let mut combo = KeyCombo::default();
        let parts: Vec<String> = s.split('+').map(|p| p.trim().to_lowercase()).collect();
        let (key, mods) = parts.split_last().ok_or("empty shortcut")?;
        for m in mods {
            match m.as_str() {
                "ctrl" | "control" => combo.ctrl = true,
                "alt" | "option" | "opt" => combo.alt = true,
                "shift" => combo.shift = true,
                "cmd" | "command" | "super" | "meta" | "win" | "logo" => combo.logo = true,
                // Platform primary modifier.
                "mod" | "primary" => {
                    if cfg!(target_os = "macos") {
                        combo.logo = true
                    } else {
                        combo.ctrl = true
                    }
                }
                other => return Err(format!("unknown modifier `{other}` in `{s}`")),
            }
        }
        combo.key = normalize_key(key).ok_or_else(|| format!("missing key in `{s}`"))?;
        Ok(combo)
    }

    /// Canonical `config.toml` spelling (`"cmd+shift+t"`, `"ctrl+comma"`).
    pub fn to_config_string(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.ctrl {
            parts.push("ctrl");
        }
        if self.alt {
            parts.push("alt");
        }
        if self.shift {
            parts.push("shift");
        }
        if self.logo {
            parts.push(if cfg!(target_os = "macos") { "cmd" } else { "super" });
        }
        parts.push(&self.key);
        parts.join("+")
    }

    /// Human label: `⌘⇧T` on macOS, `Ctrl+Shift+T` elsewhere.
    pub fn display(&self) -> String {
        let key = match self.key.as_str() {
            "comma" => ",".into(),
            "equal" => "=".into(),
            "minus" => "-".into(),
            "bracketleft" => "[".into(),
            "bracketright" => "]".into(),
            "slash" => "/".into(),
            k if k.chars().count() == 1 => k.to_uppercase(),
            k => {
                let mut c = k.chars();
                c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
            }
        };
        if cfg!(target_os = "macos") {
            let mut s = String::new();
            for (on, sym) in [(self.ctrl, "⌃"), (self.alt, "⌥"), (self.shift, "⇧"), (self.logo, "⌘")] {
                if on {
                    s.push_str(sym);
                }
            }
            s + &key
        } else {
            let mut parts = Vec::new();
            for (on, name) in [(self.ctrl, "Ctrl"), (self.alt, "Alt"), (self.shift, "Shift"), (self.logo, "Super")] {
                if on {
                    parts.push(name.to_owned());
                }
            }
            parts.push(key);
            parts.join("+")
        }
    }
}

fn normalize_key(key: &str) -> Option<String> {
    let k = match key {
        "" => return None,
        "," => "comma",
        "=" | "plus" => "equal",
        "-" => "minus",
        "[" => "bracketleft",
        "]" => "bracketright",
        "/" => "slash",
        "esc" => "escape",
        "return" => "enter",
        "ctrl" | "control" | "alt" | "shift" | "cmd" | "super" | "mod" => return None,
        other => other,
    };
    Some(k.to_owned())
}

// ---------------------------------------------------------------------------
// Load / save / hot-reload
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum ConfigError {
    Io(PathBuf, std::io::Error),
    Parse(PathBuf, toml::de::Error),
    Serialize(toml::ser::Error),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(path, e) => write!(f, "{}: {e}", path.display()),
            ConfigError::Parse(path, e) => write!(f, "{}: {e}", path.display()),
            ConfigError::Serialize(e) => write!(f, "could not serialize config: {e}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl AppConfig {
    pub fn parse(source: &str) -> Result<Self, toml::de::Error> {
        let mut config: AppConfig = toml::from_str(source)?;
        // `agents` absent from the file → built-in profiles; `agents = []` → none.
        if !source_has_key(source, "agents") {
            config.agents = default_agents();
        }
        Ok(config)
    }

    pub fn with_defaults() -> Self {
        Self { agents: default_agents(), ..Default::default() }
    }

    pub fn to_toml(&self) -> Result<String, ConfigError> {
        toml::to_string_pretty(self).map_err(ConfigError::Serialize)
    }

    pub fn agent(&self, name: &str) -> Option<&AgentProfile> {
        self.agents.iter().find(|a| a.name.eq_ignore_ascii_case(name))
    }
}

fn source_has_key(source: &str, key: &str) -> bool {
    source
        .parse::<toml_edit::DocumentMut>()
        .map(|doc| doc.contains_key(key))
        .unwrap_or(false)
}

const HEADER: &str = "\
# termy configuration — changes are applied live (hot-reload).
# Edit here or from the settings panel (Cmd+, / Ctrl+,). Comments you add are preserved.
# Themes: built-in ids (dark-oled, ghostty-dark, catppuccin-mocha, tokyo-night,
# gruvbox-dark, solarized-light) or any ~/.config/termy/themes/<id>.toml.

";

/// Outcome of [`ConfigFile::reload`].
#[derive(Debug)]
pub enum Reload {
    /// Content identical to what we last loaded/saved (e.g. our own write).
    Unchanged,
    Changed(Box<AppConfig>),
    Error(ConfigError),
}

/// Owns `config.toml` on disk and remembers what it last read/wrote, so
/// watcher events caused by our own `save()` are ignored.
#[derive(Debug)]
pub struct ConfigFile {
    pub paths: ConfigPaths,
    last_hash: Option<u64>,
}

impl ConfigFile {
    /// Loads the config, creating the directory tree and a default file if
    /// needed. A broken file yields the defaults plus the error to display.
    pub fn open(paths: ConfigPaths) -> (Self, AppConfig, Option<ConfigError>) {
        let mut file = Self { paths, last_hash: None };
        let _ = std::fs::create_dir_all(&file.paths.themes_dir);
        if !file.paths.config_file.exists() {
            let config = AppConfig::with_defaults();
            let err = file.save(&config).err();
            return (file, config, err);
        }
        match file.reload() {
            Reload::Changed(config) => (file, *config, None),
            Reload::Error(err) => (file, AppConfig::with_defaults(), Some(err)),
            Reload::Unchanged => unreachable!("first load always differs"),
        }
    }

    pub fn reload(&mut self) -> Reload {
        let path = &self.paths.config_file;
        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => return Reload::Error(ConfigError::Io(path.clone(), e)),
        };
        let hash = hash_str(&source);
        if self.last_hash == Some(hash) {
            return Reload::Unchanged;
        }
        match AppConfig::parse(&source) {
            Ok(config) => {
                self.last_hash = Some(hash);
                Reload::Changed(Box::new(config))
            }
            // Don't record the hash: re-saving the same broken text should
            // still report the error.
            Err(e) => Reload::Error(ConfigError::Parse(path.clone(), e)),
        }
    }

    /// Reads and parses the file regardless of what was loaded before
    /// ("Revert" in the settings panel).
    pub fn load_from_disk(&mut self) -> Result<AppConfig, ConfigError> {
        self.last_hash = None;
        match self.reload() {
            Reload::Changed(config) => Ok(*config),
            Reload::Error(e) => Err(e),
            Reload::Unchanged => unreachable!("hash was reset"),
        }
    }

    /// Persists `config`, keeping the user's comments, ordering and
    /// formatting for everything that didn't change.
    pub fn save(&mut self, config: &AppConfig) -> Result<(), ConfigError> {
        let path = &self.paths.config_file;
        let fresh = config.to_toml()?;
        let output = match std::fs::read_to_string(path) {
            Ok(existing) => merge_preserving(&existing, &fresh).unwrap_or_else(|| format!("{HEADER}{fresh}")),
            Err(_) => format!("{HEADER}{fresh}"),
        };
        write_atomic(path, &output).map_err(|e| ConfigError::Io(path.clone(), e))?;
        self.last_hash = Some(hash_str(&output));
        Ok(())
    }
}

fn hash_str(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Write to a temp file and rename, so a crash never leaves a half-written
/// config. Symlinks (dotfile managers) are resolved and written through.
fn write_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = target.with_extension("toml.tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, &target)
}

/// Applies the values of `new_src` onto the document `old_src`, touching only
/// what changed. Returns `None` if `old_src` isn't valid TOML.
fn merge_preserving(old_src: &str, new_src: &str) -> Option<String> {
    let mut old: toml_edit::DocumentMut = old_src.parse().ok()?;
    let new: toml_edit::DocumentMut = new_src.parse().ok()?;
    merge_table(old.as_table_mut(), new.as_table());
    Some(old.to_string())
}

fn merge_table(old: &mut toml_edit::Table, new: &toml_edit::Table) {
    let stale: Vec<String> = old.iter().map(|(k, _)| k.to_owned()).filter(|k| !new.contains_key(k)).collect();
    for key in stale {
        old.remove(&key);
    }
    for (key, new_item) in new.iter() {
        match old.get_mut(key) {
            Some(old_item) => merge_item(old_item, new_item),
            None => {
                old.insert(key, new_item.clone());
            }
        }
    }
}

fn merge_item(old: &mut toml_edit::Item, new: &toml_edit::Item) {
    use toml_edit::Item;
    match (old, new) {
        (Item::Table(o), Item::Table(n)) => merge_table(o, n),
        (Item::Value(o), Item::Value(n)) => {
            if !semantically_equal(&Item::Value(o.clone()), new) {
                let decor = o.decor().clone();
                *o = n.clone();
                *o.decor_mut() = decor;
            }
        }
        (o, n) => {
            if !semantically_equal(o, n) {
                *o = n.clone();
            }
        }
    }
}

/// Compares two items by value, ignoring formatting and representation
/// (inline table vs. `[table]`, quoting style, comments…).
fn semantically_equal(a: &toml_edit::Item, b: &toml_edit::Item) -> bool {
    let to_value = |item: &toml_edit::Item| {
        let mut doc = toml_edit::DocumentMut::new();
        doc.insert("v", item.clone());
        toml::from_str::<toml::Table>(&doc.to_string()).ok()
    };
    match (to_value(a), to_value(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// What changed on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchEvent {
    Config,
    Themes,
}

/// Watches `config.toml` and `themes/` and invokes the callback from the
/// notify thread (main wraps it in an `EventLoopProxy`). The directory is
/// watched rather than the file because editors save via rename.
pub struct ConfigWatcher {
    _watcher: notify::RecommendedWatcher,
}

impl ConfigWatcher {
    pub fn start(paths: &ConfigPaths, on_event: impl Fn(WatchEvent) + Send + 'static) -> notify::Result<Self> {
        let config_file_name = paths.config_file.file_name().map(|n| n.to_os_string());
        let themes_dir = paths.themes_dir.clone();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            if matches!(event.kind, EventKind::Access(_) | EventKind::Other) {
                return;
            }
            let mut config = false;
            let mut themes = false;
            for path in &event.paths {
                if path.starts_with(&themes_dir) {
                    themes |= path.extension().is_some_and(|e| e == "toml") || path == &themes_dir;
                } else if path.file_name().map(|n| n.to_os_string()) == config_file_name {
                    config = true;
                }
            }
            if config {
                on_event(WatchEvent::Config);
            }
            if themes {
                on_event(WatchEvent::Themes);
            }
        })?;
        std::fs::create_dir_all(&paths.themes_dir).map_err(notify::Error::io)?;
        watcher.watch(&paths.dir, RecursiveMode::NonRecursive)?;
        watcher.watch(&paths.themes_dir, RecursiveMode::Recursive)?;
        Ok(Self { _watcher: watcher })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn default_config_roundtrips() {
        let config = AppConfig::with_defaults();
        let text = config.to_toml().unwrap();
        assert_eq!(AppConfig::parse(&text).unwrap(), config);
        assert!(text.contains("[[agents]]"));
        assert!(text.contains("theme = \"dark-oled\""));
    }

    #[test]
    fn partial_file_falls_back_to_defaults() {
        let config = AppConfig::parse(
            r##"
            [window]
            opacity = 0.85
            [font]
            size = 15.5
            [cursor]
            shape = "beam"
            [appearance]
            theme = "tokyo-night"
            [keybindings]
            toggle_sidebar = "ctrl+alt+s"
            new_session = ""
            "##,
        )
        .unwrap();
        assert_eq!(config.window.opacity, 0.85);
        assert_eq!(config.window.padding_x, WindowConfig::default().padding_x);
        assert_eq!(config.font.size, 15.5);
        assert_eq!(config.cursor.shape, CursorShape::Beam);
        assert_eq!(config.appearance.theme, "tokyo-night");
        assert_eq!(config.terminal.scrollback, 10_000);
        assert_eq!(config.resources, ResourcesConfig::default());
        // Override, untouched default, disabled.
        let sidebar = config.keybindings.combo(Action::ToggleSidebar).unwrap();
        assert!(sidebar.ctrl && sidebar.alt && sidebar.key == "s");
        assert!(config.keybindings.combo(Action::ToggleSettings).is_some());
        assert!(config.keybindings.combo(Action::NewSession).is_none());
        // No [[agents]] in the file → built-in profiles.
        assert_eq!(config.agents.len(), 3);
    }

    #[test]
    fn agents_and_metrics_sources_parse() {
        let config = AppConfig::parse(
            r##"
            [[agents]]
            name = "Claude Code"
            command = "claude"
            args = ["--model", "opus"]
            icon = "claude"
            color = "#d97757"
            metrics = { kind = "claude_code", path = "~/.claude/projects" }
            budget = { weekly_tokens = 5000000, weekly_reset = "mon 09:00" }

            [[agents]]
            name = "Local LLM"
            command = "llm-cli"
            [agents.metrics]
            kind = "jsonl"
            path = "~/llm/logs"
            input_tokens = "/usage/prompt"
            output_tokens = "/usage/completion"
            cumulative = true

            [[agents]]
            name = "Quota"
            command = "q"
            metrics = { kind = "command", program = "quota", args = ["--json"] }
            "##,
        )
        .unwrap();
        assert_eq!(config.agents.len(), 3);
        let claude = config.agent("claude code").unwrap();
        assert_eq!(claude.args, ["--model", "opus"]);
        assert_eq!(claude.color, Rgb::from_hex("#d97757"));
        assert_eq!(claude.budget.weekly_tokens, Some(5_000_000));
        assert!(matches!(&claude.metrics, MetricsSource::ClaudeCode { path } if path == "~/.claude/projects"));
        match &config.agents[1].metrics {
            MetricsSource::Jsonl(src) => {
                assert_eq!(src.input_tokens, "/usage/prompt");
                assert_eq!(src.extension, "jsonl");
                assert!(src.cumulative);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(config.agents[1].icon, "shell");
        assert!(matches!(config.agents[2].metrics, MetricsSource::Command { interval_secs: 300, .. }));

        // And it survives a roundtrip.
        assert_eq!(AppConfig::parse(&config.to_toml().unwrap()).unwrap(), config);
    }

    #[test]
    fn resources_section_parses_and_gates_sampling() {
        let config = AppConfig::parse("[resources]\nshow_disk = false\ninterval_ms = 5000").unwrap();
        assert!(config.resources.enabled && config.resources.show_cpu && !config.resources.show_disk);
        assert_eq!(config.resources.interval_ms, 5000);
        assert!(config.resources.active(true));
        assert!(!config.resources.active(false), "no footer → nothing to show");
        let hidden = ResourcesConfig { show_cpu: false, show_memory: false, show_disk: false, ..Default::default() };
        assert!(!hidden.active(true));
    }

    #[test]
    fn explicit_empty_agents_is_respected() {
        assert!(AppConfig::parse("agents = []").unwrap().agents.is_empty());
    }

    #[test]
    fn invalid_values_are_rejected() {
        assert!(AppConfig::parse("[cursor]\nshape = \"triangle\"").is_err());
        assert!(AppConfig::parse("[keybindings]\ncopy = \"hyper+c\"").is_err());
        assert!(AppConfig::parse("[keybindings]\nnot_an_action = \"ctrl+c\"").is_err());
        assert!(AppConfig::parse("[window]\nopacity = \"high\"").is_err());
    }

    #[test]
    fn key_combo_parsing() {
        let c = KeyCombo::parse("Ctrl+Shift+,").unwrap();
        assert_eq!(c, KeyCombo { ctrl: true, shift: true, key: "comma".into(), ..Default::default() });
        assert_eq!(KeyCombo::parse("cmd+plus").unwrap().key, "equal");
        let label = KeyCombo::parse("ctrl+shift+comma").unwrap().display();
        assert_eq!(label, if cfg!(target_os = "macos") { "⌃⇧," } else { "Ctrl+Shift+," });
        let cmd_t = KeyCombo::parse("cmd+t").unwrap().display();
        assert_eq!(cmd_t, if cfg!(target_os = "macos") { "⌘T" } else { "Super+T" });
        assert!(KeyCombo::parse("ctrl+").is_err());
        assert!(KeyCombo::parse("ctrl+shift").is_err());
        let primary = KeyCombo::parse("mod+b").unwrap();
        assert_eq!(primary.logo, cfg!(target_os = "macos"));
        assert_eq!(primary.ctrl, !cfg!(target_os = "macos"));
    }

    #[test]
    fn combos_roundtrip_through_config_strings() {
        for (_, combo) in Keybindings::default().0 {
            let parsed = KeyCombo::parse(&combo).unwrap();
            assert_eq!(KeyCombo::parse(&parsed.to_config_string()).unwrap(), parsed);
        }
        assert_eq!(KeyCombo::parse("Shift+Ctrl+K").unwrap().to_config_string(), "ctrl+shift+k");
    }

    #[test]
    fn every_default_binding_is_valid_and_unique() {
        let kb = Keybindings::default();
        let combos: Vec<KeyCombo> = Action::ALL.iter().map(|a| kb.combo(*a).expect("default parses")).collect();
        for (i, c) in combos.iter().enumerate() {
            assert_eq!(kb.action_for(c), Some(Action::ALL[i]), "duplicate binding {c:?}");
        }
    }

    #[test]
    fn open_creates_default_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::at(dir.path().join("termy"));
        let (_, config, err) = ConfigFile::open(paths.clone());
        assert!(err.is_none());
        assert_eq!(config, AppConfig::with_defaults());
        assert!(paths.themes_dir.is_dir());
        let text = std::fs::read_to_string(&paths.config_file).unwrap();
        assert!(text.starts_with("# termy configuration"));
    }

    #[test]
    fn broken_file_yields_defaults_and_error() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::at(dir.path());
        std::fs::write(&paths.config_file, "[window\nopacity = ").unwrap();
        let (_, config, err) = ConfigFile::open(paths);
        assert_eq!(config, AppConfig::with_defaults());
        assert!(matches!(err, Some(ConfigError::Parse(..))));
    }

    #[test]
    fn save_preserves_comments_and_layout() {
        let dir = tempfile::tempdir().unwrap();
        let paths = ConfigPaths::at(dir.path());
        let original = "\
# my personal setup
[appearance]
theme = \"gruvbox-dark\" # warm colors

[font]
# I like it big
size = 16.0
family = \"Iosevka\"
";
        std::fs::write(&paths.config_file, original).unwrap();
        let (mut file, mut config, err) = ConfigFile::open(paths.clone());
        assert!(err.is_none());
        config.font.size = 18.0;
        config.appearance.theme = "tokyo-night".into();
        file.save(&config).unwrap();

        let saved = std::fs::read_to_string(&paths.config_file).unwrap();
        assert!(saved.starts_with("# my personal setup\n[appearance]\n"));
        assert!(saved.contains("theme = \"tokyo-night\" # warm colors"));
        assert!(saved.contains("# I like it big\nsize = 18.0"));
        assert!(saved.contains("family = \"Iosevka\""));
        // Everything else got materialized and still parses to the same config.
        assert_eq!(AppConfig::parse(&saved).unwrap(), config);
    }

    #[test]
    fn reload_ignores_own_writes_and_detects_external_edits() {
        let dir = tempfile::tempdir().unwrap();
        let (mut file, mut config, _) = ConfigFile::open(ConfigPaths::at(dir.path()));
        assert!(matches!(file.reload(), Reload::Unchanged));

        config.window.opacity = 0.5;
        file.save(&config).unwrap();
        assert!(matches!(file.reload(), Reload::Unchanged));

        let text = std::fs::read_to_string(&file.paths.config_file).unwrap();
        std::fs::write(&file.paths.config_file, text.replace("opacity = 0.5", "opacity = 0.75")).unwrap();
        match file.reload() {
            Reload::Changed(c) => assert_eq!(c.window.opacity, 0.75),
            other => panic!("expected change, got {other:?}"),
        }

        std::fs::write(&file.paths.config_file, "window = 3").unwrap();
        assert!(matches!(file.reload(), Reload::Error(_)));
    }

    #[test]
    fn watcher_reports_config_and_theme_changes() {
        let dir = tempfile::tempdir().unwrap();
        // Canonical path: on macOS the temp dir lives behind the /var → /private/var symlink.
        let paths = ConfigPaths::at(dir.path().canonicalize().unwrap());
        let (mut file, config, _) = ConfigFile::open(paths.clone());
        let (tx, rx) = mpsc::channel();
        let _watcher = ConfigWatcher::start(&paths, move |e| {
            let _ = tx.send(e);
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(200));

        let mut changed = config.clone();
        changed.font.size = 20.0;
        file.save(&changed).unwrap();
        std::fs::write(paths.themes_dir.join("mine.toml"), "name = \"x\"").unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let (mut saw_config, mut saw_themes) = (false, false);
        while !(saw_config && saw_themes) && std::time::Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(WatchEvent::Config) => saw_config = true,
                Ok(WatchEvent::Themes) => saw_themes = true,
                Err(_) => {}
            }
        }
        assert!(saw_config, "config change not observed");
        assert!(saw_themes, "theme change not observed");
    }

    #[test]
    fn tilde_expansion() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde("~/.claude/projects"), home.join(".claude/projects"));
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
    }
}
