//! TrueColor palettes: 16 ANSI colors + UI chrome (sidebar, subtle borders,
//! `Working`/`Waiting`/`Done` badges, diff colors).
//!
//! Built-in themes live in `assets/themes/*.toml` and are embedded at compile
//! time; user themes are loaded from `~/.config/termy/themes/*.toml` using the
//! exact same format, and override built-ins with the same id (file stem).

use std::fmt;
use std::path::{Path, PathBuf};

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};

/// A 24-bit color, (de)serialized as `"#rrggbb"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        let hex = s.strip_prefix('#').unwrap_or(s);
        if hex.len() != 6 || !hex.is_ascii() {
            return None;
        }
        let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
        Some(Self::new(channel(0)?, channel(2)?, channel(4)?))
    }

    /// Linear interpolation in sRGB space: `t = 0` is `self`, `t = 1` is `other`.
    pub fn mix(self, other: Rgb, t: f32) -> Rgb {
        let t = t.clamp(0.0, 1.0);
        let lerp = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Rgb::new(lerp(self.r, other.r), lerp(self.g, other.g), lerp(self.b, other.b))
    }

    /// Scales brightness (used for SGR "dim").
    pub fn scale(self, factor: f32) -> Rgb {
        self.mix(Rgb::new(0, 0, 0), 1.0 - factor)
    }

    /// Relative luminance (0..1), used to tell light themes from dark ones.
    pub fn luminance(self) -> f32 {
        (0.2126 * self.r as f32 + 0.7152 * self.g as f32 + 0.0722 * self.b as f32) / 255.0
    }
}

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

impl Serialize for Rgb {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Rgb {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Rgb::from_hex(&s).ok_or_else(|| de::Error::custom(format!("invalid color {s:?}, expected \"#rrggbb\"")))
    }
}

/// The 8 colors of one ANSI intensity (normal or bright).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnsiColors {
    pub black: Rgb,
    pub red: Rgb,
    pub green: Rgb,
    pub yellow: Rgb,
    pub blue: Rgb,
    pub magenta: Rgb,
    pub cyan: Rgb,
    pub white: Rgb,
}

impl AnsiColors {
    pub fn as_array(&self) -> [Rgb; 8] {
        [self.black, self.red, self.green, self.yellow, self.blue, self.magenta, self.cyan, self.white]
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TerminalColors {
    pub foreground: Rgb,
    pub background: Rgb,
    pub cursor: Rgb,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_text: Option<Rgb>,
    pub selection_background: Rgb,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_foreground: Option<Rgb>,
    pub normal: AnsiColors,
    pub bright: AnsiColors,
}

/// Optional chrome overrides as written in a theme file. Anything missing is
/// derived from the terminal palette (see [`Theme::ui`]).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiColorOverrides {
    pub chrome_background: Option<Rgb>,
    pub input_background: Option<Rgb>,
    pub button_background: Option<Rgb>,
    pub pill_selected_background: Option<Rgb>,
    pub border: Option<Rgb>,
    pub text_primary: Option<Rgb>,
    pub text_dim: Option<Rgb>,
    pub accent: Option<Rgb>,
    pub status_working: Option<Rgb>,
    pub status_waiting: Option<Rgb>,
    pub status_done: Option<Rgb>,
    pub diff_add_background: Option<Rgb>,
    pub diff_delete_background: Option<Rgb>,
}

/// Fully resolved chrome palette consumed by the egui UI.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UiColors {
    pub chrome_background: Rgb,
    pub input_background: Rgb,
    pub button_background: Rgb,
    pub pill_selected_background: Rgb,
    pub border: Rgb,
    pub text_primary: Rgb,
    pub text_dim: Rgb,
    pub accent: Rgb,
    pub status_working: Rgb,
    pub status_waiting: Rgb,
    pub status_done: Rgb,
    pub diff_add_background: Rgb,
    pub diff_delete_background: Rgb,
}

/// On-disk theme format (`themes/*.toml`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Theme {
    pub name: String,
    pub colors: TerminalColors,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<UiColorOverrides>,
    /// Lookup key: the file stem (`catppuccin-mocha`). Not part of the file.
    #[serde(skip)]
    pub id: String,
}

impl Theme {
    pub fn parse(id: &str, source: &str) -> Result<Self, toml::de::Error> {
        let mut theme: Theme = toml::from_str(source)?;
        theme.id = id.to_owned();
        Ok(theme)
    }

    pub fn is_light(&self) -> bool {
        self.colors.background.luminance() > 0.5
    }

    /// ANSI color 0..=15.
    pub fn ansi(&self, index: usize) -> Rgb {
        match index {
            0..=7 => self.colors.normal.as_array()[index],
            8..=15 => self.colors.bright.as_array()[index - 8],
            _ => self.colors.foreground,
        }
    }

    /// Any xterm-256 color: 16 ANSI + 6×6×6 cube + 24-step gray ramp.
    pub fn indexed(&self, index: u8) -> Rgb {
        match index {
            0..=15 => self.ansi(index as usize),
            16..=231 => {
                let i = index - 16;
                let level = |v: u8| if v == 0 { 0 } else { v * 40 + 55 };
                Rgb::new(level(i / 36), level((i / 6) % 6), level(i % 6))
            }
            232..=255 => {
                let v = (index - 232) * 10 + 8;
                Rgb::new(v, v, v)
            }
        }
    }

    pub fn cursor_text(&self) -> Rgb {
        self.colors.cursor_text.unwrap_or(self.colors.background)
    }

    /// Chrome palette: explicit `[ui]` values, otherwise derived from the
    /// terminal colors so any imported terminal theme looks coherent.
    pub fn ui(&self) -> UiColors {
        let c = &self.colors;
        let (bg, fg) = (c.background, c.foreground);
        let o = self.ui.clone().unwrap_or_default();
        UiColors {
            chrome_background: o.chrome_background.unwrap_or(bg.mix(fg, 0.05)),
            input_background: o.input_background.unwrap_or(bg.mix(fg, 0.03)),
            button_background: o.button_background.unwrap_or(bg.mix(fg, 0.09)),
            pill_selected_background: o.pill_selected_background.unwrap_or(bg.mix(fg, 0.13)),
            border: o.border.unwrap_or(bg.mix(fg, 0.10)),
            text_primary: o.text_primary.unwrap_or(bg.mix(fg, 0.80)),
            text_dim: o.text_dim.unwrap_or(bg.mix(fg, 0.45)),
            accent: o.accent.unwrap_or(c.bright.blue),
            status_working: o.status_working.unwrap_or(bg.mix(fg, 0.45)),
            status_waiting: o.status_waiting.unwrap_or(c.normal.yellow),
            status_done: o.status_done.unwrap_or(c.normal.green),
            diff_add_background: o.diff_add_background.unwrap_or(bg.mix(c.normal.green, 0.18)),
            diff_delete_background: o.diff_delete_background.unwrap_or(bg.mix(c.normal.red, 0.18)),
        }
    }
}

/// `(id, toml source)` of every theme shipped in the binary.
const BUILTIN_THEMES: &[(&str, &str)] = &[
    ("dark-oled", include_str!("../assets/themes/dark-oled.toml")),
    ("ghostty-dark", include_str!("../assets/themes/ghostty-dark.toml")),
    ("catppuccin-mocha", include_str!("../assets/themes/catppuccin-mocha.toml")),
    ("tokyo-night", include_str!("../assets/themes/tokyo-night.toml")),
    ("gruvbox-dark", include_str!("../assets/themes/gruvbox-dark.toml")),
    ("solarized-light", include_str!("../assets/themes/solarized-light.toml")),
];

pub const DEFAULT_THEME_ID: &str = "dark-oled";

/// A user theme that failed to load (surfaced in the settings UI).
#[derive(Debug, Clone)]
pub struct ThemeLoadError {
    pub path: PathBuf,
    pub message: String,
}

/// All available themes: built-ins first, then user themes (which replace a
/// built-in with the same id).
#[derive(Clone, Debug)]
pub struct ThemeRegistry {
    themes: Vec<Theme>,
    pub errors: Vec<ThemeLoadError>,
}

impl ThemeRegistry {
    pub fn builtin() -> Self {
        let themes = BUILTIN_THEMES
            .iter()
            .map(|(id, src)| Theme::parse(id, src).unwrap_or_else(|e| panic!("built-in theme {id} is invalid: {e}")))
            .collect();
        Self { themes, errors: Vec::new() }
    }

    /// Built-ins plus every `*.toml` in `dir` (missing dir is not an error).
    pub fn load(dir: &Path) -> Self {
        let mut registry = Self::builtin();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return registry;
        };
        let mut paths: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
            .collect();
        paths.sort();
        for path in paths {
            let id = path.file_stem().map(|s| slug(&s.to_string_lossy())).unwrap_or_default();
            let result = std::fs::read_to_string(&path)
                .map_err(|e| e.to_string())
                .and_then(|src| Theme::parse(&id, &src).map_err(|e| e.to_string()));
            match result {
                Ok(theme) => registry.insert(theme),
                Err(message) => registry.errors.push(ThemeLoadError { path, message }),
            }
        }
        registry
    }

    fn insert(&mut self, theme: Theme) {
        match self.themes.iter_mut().find(|t| t.id == theme.id) {
            Some(existing) => *existing = theme,
            None => self.themes.push(theme),
        }
    }

    pub fn themes(&self) -> &[Theme] {
        &self.themes
    }

    /// Finds a theme by id or display name, ignoring case and separators
    /// (`"Catppuccin Mocha"`, `"catppuccin_mocha"` and `"catppuccin-mocha"`
    /// all match).
    pub fn get(&self, name: &str) -> Option<&Theme> {
        let key = slug(name);
        self.themes.iter().find(|t| t.id == key || slug(&t.name) == key)
    }

    /// Like [`get`](Self::get) but never fails: falls back to the default theme.
    pub fn resolve(&self, name: &str) -> &Theme {
        self.get(name)
            .or_else(|| self.get(DEFAULT_THEME_ID))
            .unwrap_or(&self.themes[0])
    }
}

/// `"Tokyo Night"` → `"tokyo-night"`.
pub fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.trim().chars() {
        if ch.is_alphanumeric() {
            out.extend(ch.to_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb_hex_roundtrip() {
        let c = Rgb::from_hex("#d97757").unwrap();
        assert_eq!(c, Rgb::new(0xd9, 0x77, 0x57));
        assert_eq!(c.to_string(), "#d97757");
        assert!(Rgb::from_hex("#12345").is_none());
        assert!(Rgb::from_hex("#zzzzzz").is_none());
    }

    #[test]
    fn all_builtins_parse_and_are_unique() {
        let reg = ThemeRegistry::builtin();
        assert_eq!(reg.themes().len(), BUILTIN_THEMES.len());
        for (id, _) in BUILTIN_THEMES {
            assert!(reg.get(id).is_some(), "missing {id}");
        }
        assert!(reg.get("solarized-light").unwrap().is_light());
        assert!(!reg.get("dark-oled").unwrap().is_light());
    }

    #[test]
    fn lookup_is_case_and_separator_insensitive() {
        let reg = ThemeRegistry::builtin();
        for q in ["Catppuccin Mocha", "catppuccin_mocha", "CATPPUCCIN-MOCHA"] {
            assert_eq!(reg.get(q).unwrap().id, "catppuccin-mocha");
        }
        assert_eq!(reg.resolve("does-not-exist").id, DEFAULT_THEME_ID);
    }

    #[test]
    fn dark_oled_uses_sampled_chrome_palette() {
        let ui = ThemeRegistry::builtin().resolve("dark-oled").ui();
        assert_eq!(ui.chrome_background, Rgb::from_hex("#0d0d0d").unwrap());
        assert_eq!(ui.diff_delete_background, Rgb::from_hex("#350400").unwrap());
    }

    #[test]
    fn ui_colors_are_derived_when_missing() {
        let theme = ThemeRegistry::builtin().resolve("tokyo-night").clone();
        assert!(theme.ui.is_none());
        let ui = theme.ui();
        assert_eq!(ui.accent, theme.colors.bright.blue);
        assert_ne!(ui.chrome_background, theme.colors.background);
    }

    #[test]
    fn indexed_palette() {
        let theme = ThemeRegistry::builtin().resolve("gruvbox-dark").clone();
        assert_eq!(theme.indexed(1), theme.colors.normal.red);
        assert_eq!(theme.indexed(9), theme.colors.bright.red);
        assert_eq!(theme.indexed(16), Rgb::new(0, 0, 0));
        assert_eq!(theme.indexed(231), Rgb::new(255, 255, 255));
        assert_eq!(theme.indexed(232), Rgb::new(8, 8, 8));
        assert_eq!(theme.indexed(196), Rgb::new(255, 0, 0));
    }

    #[test]
    fn user_themes_load_override_and_report_errors() {
        let dir = tempfile::tempdir().unwrap();
        let custom = ThemeRegistry::builtin().resolve("tokyo-night").clone();
        let mut renamed = custom.clone();
        renamed.name = "My Theme".into();
        std::fs::write(dir.path().join("my-theme.toml"), toml::to_string(&renamed).unwrap()).unwrap();
        // Same id as a built-in → replaces it.
        let mut override_oled = custom;
        override_oled.name = "OLED Custom".into();
        std::fs::write(dir.path().join("dark-oled.toml"), toml::to_string(&override_oled).unwrap()).unwrap();
        std::fs::write(dir.path().join("broken.toml"), "name = 1").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "ignored").unwrap();

        let reg = ThemeRegistry::load(dir.path());
        assert_eq!(reg.get("my theme").unwrap().id, "my-theme");
        assert_eq!(reg.get("dark-oled").unwrap().name, "OLED Custom");
        assert_eq!(reg.themes().len(), BUILTIN_THEMES.len() + 1);
        assert_eq!(reg.errors.len(), 1);
        assert!(reg.errors[0].path.ends_with("broken.toml"));
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("Tokyo Night"), "tokyo-night");
        assert_eq!(slug("  Dark   OLED! "), "dark-oled");
    }
}
