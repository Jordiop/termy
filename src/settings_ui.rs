//! Settings modal (Cmd+, / Ctrl+,): Appearance & Themes, Typography & Cursor,
//! Terminal & Shell, AI Agents & Token Monitor, Keybindings.
//!
//! It edits a *draft* `AppConfig`; every change is applied live by the app
//! (theme, font, opacity…) and nothing is written until "Save to
//! config.toml", which goes through the comment-preserving `ConfigFile::save`.
//! Shortcuts are recorded by pressing them (see `App::on_key`), so an invalid
//! combo can't be typed.

use std::collections::HashMap;

use egui::{Color32, CornerRadius, RichText, Stroke, StrokeKind, Vec2};

use crate::ai_monitor::{WeeklyReset, format_tokens};
use crate::chrome::{c32, paint_icon};
use crate::config::{
    Action, AgentProfile, AppConfig, CursorShape, Decorations, JsonlSource, KeyCombo, MetricsSource, ShellConfig,
    TabStyle, TokenBudget,
};
use crate::theme::{Rgb, Theme, ThemeRegistry, UiColors};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SettingsTab {
    #[default]
    Appearance,
    Typography,
    Terminal,
    Agents,
    Keybindings,
}

impl SettingsTab {
    const ALL: [SettingsTab; 5] = [Self::Appearance, Self::Typography, Self::Terminal, Self::Agents, Self::Keybindings];

    fn label(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance & Themes",
            Self::Typography => "Typography & Cursor",
            Self::Terminal => "Terminal & Shell",
            Self::Agents => "AI Agents & Tokens",
            Self::Keybindings => "Keybindings",
        }
    }
}

#[derive(Default)]
pub struct SettingsState {
    pub open: bool,
    pub tab: SettingsTab,
    /// Live-applied configuration being edited.
    pub draft: AppConfig,
    /// What's on disk, to show "unsaved changes".
    pub saved: AppConfig,
    /// Action waiting for a key press to bind.
    pub recording: Option<Action>,
    /// Feedback line: (message, is_error).
    pub status: Option<(String, bool)>,
    /// Text buffers for list-like fields (args, fallbacks).
    shell_args: String,
}

impl SettingsState {
    pub fn open_with(&mut self, config: &AppConfig, saved: &AppConfig) {
        self.open = true;
        self.draft = config.clone();
        self.saved = saved.clone();
        self.recording = None;
        self.status = None;
        self.shell_args = config.terminal.shell.as_ref().map(|s| s.args.join(" ")).unwrap_or_default();
    }

    /// Binds the recorded combo to the pending action. Returns whether a
    /// recording was in progress.
    pub fn record(&mut self, combo: Option<KeyCombo>) -> bool {
        let Some(action) = self.recording.take() else { return false };
        if let Some(combo) = combo {
            self.draft.keybindings.0.insert(action, combo.to_config_string());
        }
        true
    }

    pub fn has_unsaved_changes(&self) -> bool {
        self.draft != self.saved
    }
}

pub struct SettingsInput<'a> {
    pub colors: &'a UiColors,
    pub themes: &'a ThemeRegistry,
    pub font_families: &'a [String],
    /// Billable tokens this week per agent name (from the AI monitor).
    pub weekly_usage: &'a HashMap<String, u64>,
}

#[derive(Default, Debug)]
pub struct SettingsOutput {
    /// The draft changed this frame (apply it live).
    pub changed: bool,
    pub save: bool,
    pub revert: bool,
    pub close: bool,
}

pub fn show(ctx: &egui::Context, state: &mut SettingsState, input: &SettingsInput<'_>) -> SettingsOutput {
    let mut out = SettingsOutput::default();
    let before = state.draft.clone();
    let colors = input.colors;
    let screen = ctx.content_rect();
    // Fits small windows: never taller/wider than the window minus a margin.
    let size = Vec2::new((screen.width() - 64.0).clamp(320.0, 820.0), (screen.height() - 64.0).clamp(240.0, 600.0));

    let modal = egui::Modal::new(egui::Id::unique("termy-settings"))
        .backdrop_color(Color32::from_black_alpha(140))
        .frame(
            egui::Frame::new()
                .fill(c32(colors.chrome_background))
                .stroke(Stroke::new(1.0, c32(colors.border)))
                .corner_radius(CornerRadius::same(10))
                .inner_margin(egui::Margin::same(16)),
        )
        .show(ctx, |ui| {
            ui.set_width(size.x);
            ui.set_max_width(size.x);
            ui.set_height(size.y);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Settings").size(16.0).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("✕").on_hover_text("Close (Esc)").clicked() {
                        out.close = true;
                    }
                });
            });
            ui.add_space(8.0);
            let body_h = size.y - 80.0;
            ui.horizontal_top(|ui| {
                let top_down = egui::Layout::top_down(egui::Align::Min);
                ui.allocate_ui_with_layout(Vec2::new(170.0, body_h), top_down, |ui| {
                    ui.set_width(170.0);
                    for tab in SettingsTab::ALL {
                        let selected = state.tab == tab;
                        let text = RichText::new(tab.label()).size(12.5).color(if selected { c32(colors.text_primary) } else { c32(colors.text_dim) });
                        if ui.add_sized([170.0, 28.0], egui::Button::selectable(selected, text)).clicked() {
                            state.tab = tab;
                            state.recording = None;
                        }
                    }
                });
                ui.separator();
                let content_w = ui.available_width();
                ui.allocate_ui_with_layout(Vec2::new(content_w, body_h), top_down, |ui| {
                    egui::ScrollArea::vertical().max_height(body_h).auto_shrink([false, false]).show(ui, |ui| {
                        ui.set_width(content_w - 14.0);
                        match state.tab {
                            SettingsTab::Appearance => appearance(ui, state, input),
                            SettingsTab::Typography => typography(ui, state, input),
                            SettingsTab::Terminal => terminal(ui, state),
                            SettingsTab::Agents => agents(ui, state, input),
                            SettingsTab::Keybindings => keybindings(ui, state, colors),
                        }
                    });
                });
            });
            ui.separator();
            ui.horizontal(|ui| {
                match &state.status {
                    Some((msg, true)) => ui.label(RichText::new(msg).color(c32(colors.diff_delete_background.mix(Rgb::new(255, 120, 120), 0.8)))),
                    Some((msg, false)) => ui.label(RichText::new(msg).color(c32(colors.status_done))),
                    None if state.has_unsaved_changes() => ui.label(RichText::new("● Unsaved changes (applied live)").color(c32(colors.status_waiting))),
                    None => ui.label(RichText::new("All changes saved").color(c32(colors.text_dim))),
                };
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let save = egui::Button::new(RichText::new("Save to config.toml").strong()).fill(c32(colors.accent).gamma_multiply(0.45));
                    if ui.add_enabled(state.has_unsaved_changes(), save).clicked() {
                        out.save = true;
                    }
                    if ui.add_enabled(state.has_unsaved_changes(), egui::Button::new("Revert")).on_hover_text("Reload config.toml from disk").clicked() {
                        out.revert = true;
                    }
                });
            });
        });
    if modal.should_close() && state.recording.is_none() {
        out.close = true;
    }
    out.changed = state.draft != before;
    if out.changed {
        state.status = None;
    }
    out
}

// ---------------------------------------------------------------------------
// Tabs
// ---------------------------------------------------------------------------

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(6.0);
    ui.label(RichText::new(title).size(13.5).strong());
    ui.add_space(4.0);
}

fn grid(ui: &mut egui::Ui, id: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Grid::new(id).num_columns(2).spacing([16.0, 8.0]).min_col_width(150.0).show(ui, add);
}

fn appearance(ui: &mut egui::Ui, state: &mut SettingsState, input: &SettingsInput<'_>) {
    let draft = &mut state.draft;
    section(ui, "Theme");
    let card = Vec2::new(176.0, 92.0);
    let current = input.themes.resolve(&draft.appearance.theme).id.clone();
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(10.0);
        for theme in input.themes.themes() {
            if theme_card(ui, theme, theme.id == current, card, input.colors).clicked() {
                draft.appearance.theme = theme.id.clone();
            }
        }
    });
    for e in &input.themes.errors {
        ui.label(RichText::new(format!("⚠ {}: {}", e.path.display(), e.message)).size(11.0).color(c32(input.colors.status_waiting)));
    }
    ui.label(RichText::new("Add your own in ~/.config/termy/themes/<name>.toml — they appear here instantly.").size(11.0).color(c32(input.colors.text_dim)));

    section(ui, "Window");
    grid(ui, "appearance-window", |ui| {
        ui.label("Background opacity");
        ui.add(egui::Slider::new(&mut draft.window.opacity, 0.3..=1.0).step_by(0.01).fixed_decimals(2));
        ui.end_row();
        ui.label("Padding (horizontal)");
        ui.add(egui::Slider::new(&mut draft.window.padding_x, 0.0..=32.0).step_by(1.0).suffix(" pt"));
        ui.end_row();
        ui.label("Padding (vertical)");
        ui.add(egui::Slider::new(&mut draft.window.padding_y, 0.0..=32.0).step_by(1.0).suffix(" pt"));
        ui.end_row();
        ui.label("Sidebar");
        ui.checkbox(&mut draft.window.sidebar_visible, "Visible at startup");
        ui.end_row();
        ui.label("Sidebar width");
        ui.add(egui::Slider::new(&mut draft.window.sidebar_width, 160.0..=380.0).step_by(1.0).suffix(" pt"));
        ui.end_row();
        ui.label("Pane footer");
        ui.checkbox(&mut draft.window.pane_footer, "Agent status, model and token budget");
        ui.end_row();
        ui.label("Tab style");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut draft.window.tab_style, TabStyle::Flat, "Flat");
            ui.selectable_value(&mut draft.window.tab_style, TabStyle::Pill, "Pill");
        });
        ui.end_row();
        ui.label("Decorations");
        ui.horizontal(|ui| {
            ui.selectable_value(&mut draft.window.decorations, Decorations::Transparent, "Unified");
            ui.selectable_value(&mut draft.window.decorations, Decorations::Full, "Native");
            ui.selectable_value(&mut draft.window.decorations, Decorations::None, "None");
            ui.label(RichText::new("(restart)").size(11.0).color(c32(input.colors.text_dim)));
        });
        ui.end_row();
    });

    section(ui, "Resource usage");
    ui.label(
        RichText::new("CPU, RAM and disk I/O of each tab's process tree, shown small in the pane footer.")
            .size(11.0)
            .color(c32(input.colors.text_dim)),
    );
    let footer_on = draft.window.pane_footer;
    grid(ui, "appearance-resources", |ui| {
        ui.label("Show");
        ui.add_enabled(footer_on, egui::Checkbox::new(&mut draft.resources.enabled, "Resource usage in the footer"));
        ui.end_row();
        let on = footer_on && draft.resources.enabled;
        ui.label("Metrics");
        ui.horizontal(|ui| {
            ui.add_enabled(on, egui::Checkbox::new(&mut draft.resources.show_cpu, "CPU"));
            ui.add_enabled(on, egui::Checkbox::new(&mut draft.resources.show_memory, "RAM"));
            ui.add_enabled(on, egui::Checkbox::new(&mut draft.resources.show_disk, "Disk I/O"));
        });
        ui.end_row();
        ui.label("Refresh every");
        ui.add_enabled(on, egui::Slider::new(&mut draft.resources.interval_ms, 500..=10_000).step_by(250.0).suffix(" ms"));
        ui.end_row();
    });
    if !footer_on {
        ui.label(RichText::new("Enable the pane footer above to show resource usage.").size(11.0).color(c32(input.colors.text_dim)));
    }
}

/// Palette preview: background, a prompt line and the 8 ANSI swatches.
fn theme_card(ui: &mut egui::Ui, theme: &Theme, selected: bool, size: Vec2, colors: &UiColors) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let p = ui.painter();
    let c = &theme.colors;
    let preview = egui::Rect::from_min_size(rect.min, Vec2::new(size.x, size.y - 22.0));
    p.rect_filled(preview, CornerRadius::same(6), c32(c.background));
    let font = egui::FontId::monospace(11.0);
    p.text(preview.min + Vec2::new(8.0, 8.0), egui::Align2::LEFT_TOP, "❯", font.clone(), c32(c.normal.green));
    p.text(preview.min + Vec2::new(22.0, 8.0), egui::Align2::LEFT_TOP, "cargo run", font, c32(c.foreground));
    for (i, swatch) in c.normal.as_array().iter().chain(c.bright.as_array().iter()).enumerate() {
        let (row, col) = (i / 8, i % 8);
        let center = preview.min + Vec2::new(14.0 + col as f32 * 20.0, 36.0 + row as f32 * 16.0);
        p.circle_filled(center, 5.5, c32(*swatch));
    }
    let stroke = if selected {
        Stroke::new(2.0, c32(colors.accent))
    } else if resp.hovered() {
        Stroke::new(1.0, c32(colors.text_dim))
    } else {
        Stroke::new(1.0, c32(colors.border))
    };
    p.rect_stroke(preview, CornerRadius::same(6), stroke, StrokeKind::Outside);
    let name_color = if selected { colors.text_primary } else { colors.text_dim };
    p.text(egui::Pos2::new(rect.left() + 2.0, rect.bottom() - 9.0), egui::Align2::LEFT_CENTER, &theme.name, egui::FontId::proportional(12.0), c32(name_color));
    resp
}

fn typography(ui: &mut egui::Ui, state: &mut SettingsState, input: &SettingsInput<'_>) {
    let draft = &mut state.draft;
    section(ui, "Font");
    grid(ui, "typography-font", |ui| {
        ui.label("Family");
        egui::ComboBox::from_id_salt("font-family")
            .width(240.0)
            .selected_text(&draft.font.family)
            .show_ui(ui, |ui| {
                let mut families: Vec<String> = input.font_families.to_vec();
                if !families.contains(&draft.font.family) {
                    families.insert(0, draft.font.family.clone());
                }
                for family in families {
                    let label = family.clone();
                    ui.selectable_value(&mut draft.font.family, family, label);
                }
            });
        ui.end_row();
        ui.label("Size");
        ui.add(egui::Slider::new(&mut draft.font.size, 8.0..=32.0).step_by(0.5).suffix(" pt"));
        ui.end_row();
        ui.label("Line height");
        ui.add(egui::Slider::new(&mut draft.font.line_height, 1.0..=2.0).step_by(0.05));
        ui.end_row();
        ui.label("Ligatures");
        ui.checkbox(&mut draft.font.ligatures, "Enable (=> != -> …)");
        ui.end_row();
    });

    section(ui, "Cursor");
    grid(ui, "typography-cursor", |ui| {
        ui.label("Shape");
        ui.horizontal(|ui| {
            for (shape, label) in [(CursorShape::Block, "█ Block"), (CursorShape::Beam, "▏ Beam"), (CursorShape::Underline, "▁ Underline")] {
                ui.selectable_value(&mut draft.cursor.shape, shape, label);
            }
        });
        ui.end_row();
        ui.label("Blink");
        ui.checkbox(&mut draft.cursor.blink, "Blinking cursor");
        ui.end_row();
        ui.label("Blink interval");
        ui.add_enabled(draft.cursor.blink, egui::Slider::new(&mut draft.cursor.blink_interval_ms, 200..=1500).suffix(" ms"));
        ui.end_row();
    });
}

fn terminal(ui: &mut egui::Ui, state: &mut SettingsState) {
    let SettingsState { draft, shell_args, .. } = state;
    section(ui, "Shell");
    grid(ui, "terminal-shell", |ui| {
        ui.label("Program");
        ui.vertical(|ui| {
            let mut custom = draft.terminal.shell.is_some();
            if ui.checkbox(&mut custom, "Custom shell (otherwise the OS default)").changed() {
                draft.terminal.shell = custom.then(|| {
                    let (program, args) = crate::pty::default_shell();
                    *shell_args = args.join(" ");
                    ShellConfig { program, args }
                });
            }
            if let Some(shell) = &mut draft.terminal.shell {
                ui.add(egui::TextEdit::singleline(&mut shell.program).hint_text("/bin/zsh, pwsh.exe…").desired_width(260.0));
                if ui.add(egui::TextEdit::singleline(shell_args).hint_text("arguments, e.g. -l").desired_width(260.0)).changed() {
                    shell.args = shell_args.split_whitespace().map(str::to_owned).collect();
                }
            }
        });
        ui.end_row();
        ui.label("Working directory");
        let mut dir = draft.terminal.working_directory.clone().unwrap_or_default();
        if ui.add(egui::TextEdit::singleline(&mut dir).hint_text("~ (home)").desired_width(260.0)).changed() {
            draft.terminal.working_directory = (!dir.trim().is_empty()).then_some(dir);
        }
        ui.end_row();
    });

    section(ui, "Behavior");
    grid(ui, "terminal-behavior", |ui| {
        ui.label("Scrollback lines");
        ui.add(egui::DragValue::new(&mut draft.terminal.scrollback).range(0..=1_000_000).speed(100));
        ui.end_row();
        ui.label("Scroll speed");
        ui.add(egui::Slider::new(&mut draft.terminal.scroll_multiplier, 1.0..=10.0).step_by(0.5).suffix(" lines"));
        ui.end_row();
        ui.label("Working → Waiting after");
        ui.add(egui::Slider::new(&mut draft.terminal.activity_timeout_ms, 300..=10_000).suffix(" ms"));
        ui.end_row();
        if cfg!(target_os = "macos") {
            ui.label("Option key");
            ui.checkbox(&mut draft.terminal.option_as_alt, "Send Option as Alt/Meta (disables ñ, ´, € composition)");
            ui.end_row();
        }
    });

    section(ui, "Environment variables");
    let mut remove = None;
    let mut entries: Vec<(String, String)> = draft.terminal.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let mut changed = false;
    for (i, (key, value)) in entries.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            changed |= ui.add(egui::TextEdit::singleline(key).desired_width(140.0)).changed();
            ui.label("=");
            changed |= ui.add(egui::TextEdit::singleline(value).desired_width(220.0)).changed();
            if ui.small_button("✕").clicked() {
                remove = Some(i);
            }
        });
    }
    if let Some(i) = remove {
        entries.remove(i);
        changed = true;
    }
    if ui.button("+ Add variable").clicked() {
        let mut n = 1;
        while entries.iter().any(|(k, _)| *k == format!("VAR_{n}")) {
            n += 1;
        }
        entries.push((format!("VAR_{n}"), String::new()));
        changed = true;
    }
    if changed {
        draft.terminal.env = entries.into_iter().filter(|(k, _)| !k.trim().is_empty()).collect();
    }
}

fn metrics_kind(source: &MetricsSource) -> &'static str {
    match source {
        MetricsSource::None => "none",
        MetricsSource::ClaudeCode { .. } => "Claude Code logs",
        MetricsSource::Codex { .. } => "Codex logs",
        MetricsSource::GeminiCli { .. } => "Gemini CLI logs",
        MetricsSource::Jsonl(_) => "Custom JSONL",
        MetricsSource::Command { .. } => "Quota command",
    }
}

fn metrics_templates() -> Vec<MetricsSource> {
    vec![
        MetricsSource::None,
        MetricsSource::ClaudeCode { path: "~/.claude/projects".into() },
        MetricsSource::Codex { path: "~/.codex/sessions".into() },
        MetricsSource::GeminiCli { path: "~/.gemini/tmp".into() },
        MetricsSource::Jsonl(Box::new(JsonlSource {
            path: "~/logs".into(),
            extension: "jsonl".into(),
            input_tokens: "/usage/input_tokens".into(),
            output_tokens: "/usage/output_tokens".into(),
            cache_read_tokens: None,
            cache_creation_tokens: None,
            session_id: None,
            timestamp: None,
            cwd: None,
            model: None,
            dedupe_key: None,
            cumulative: false,
        })),
        MetricsSource::Command { program: "my-quota".into(), args: vec!["--json".into()], interval_secs: 300 },
    ]
}

fn optional_text(ui: &mut egui::Ui, value: &mut Option<String>, hint: &str) {
    let mut text = value.clone().unwrap_or_default();
    if ui.add(egui::TextEdit::singleline(&mut text).hint_text(hint).desired_width(220.0)).changed() {
        *value = (!text.trim().is_empty()).then_some(text);
    }
}

fn optional_tokens(ui: &mut egui::Ui, value: &mut Option<u64>, default: u64) {
    ui.horizontal(|ui| {
        let mut on = value.is_some();
        if ui.checkbox(&mut on, "").changed() {
            *value = on.then_some(default);
        }
        if let Some(v) = value {
            ui.add(egui::DragValue::new(v).range(1_000..=u64::MAX / 2).speed(10_000.0));
            ui.label(format_tokens(*v));
        } else {
            ui.label("no limit");
        }
    });
}

fn agents(ui: &mut egui::Ui, state: &mut SettingsState, input: &SettingsInput<'_>) {
    let draft = &mut state.draft;
    let colors = input.colors;
    section(ui, "Token monitor");
    ui.label(
        RichText::new("100% local: reads the agents' own session logs on this machine. No network, no telemetry.")
            .size(11.0)
            .color(c32(colors.text_dim)),
    );
    grid(ui, "monitor", |ui| {
        ui.label("Enabled");
        ui.checkbox(&mut draft.monitor.enabled, "Watch agent logs for token usage");
        ui.end_row();
        ui.label("History scanned at startup");
        ui.add(egui::Slider::new(&mut draft.monitor.scan_days, 1..=31).suffix(" days"));
        ui.end_row();
    });

    section(ui, "Agent profiles");
    let mut remove = None;
    for (i, agent) in draft.agents.iter_mut().enumerate() {
        let used = input.weekly_usage.get(&agent.name).copied().unwrap_or(0);
        let header = format!("{}   ·   {} tokens this week", agent.name, format_tokens(used));
        egui::CollapsingHeader::new(header).id_salt(("agent", i)).default_open(false).show(ui, |ui| {
            agent_editor(ui, i, agent, colors);
            ui.add_space(4.0);
            if ui.button(RichText::new("Remove profile").color(c32(colors.text_dim))).clicked() {
                remove = Some(i);
            }
        });
    }
    if let Some(i) = remove {
        draft.agents.remove(i);
    }
    ui.add_space(6.0);
    if ui.button("+ Add agent profile").clicked() {
        draft.agents.push(AgentProfile {
            name: format!("Agent {}", draft.agents.len() + 1),
            command: String::new(),
            args: vec![],
            icon: "shell".into(),
            color: None,
            metrics: MetricsSource::None,
            budget: TokenBudget::default(),
        });
    }
}

fn agent_editor(ui: &mut egui::Ui, i: usize, agent: &mut AgentProfile, colors: &UiColors) {
    grid(ui, &format!("agent-{i}"), |ui| {
        ui.label("Name");
        ui.text_edit_singleline(&mut agent.name);
        ui.end_row();
        ui.label("Command");
        ui.add(egui::TextEdit::singleline(&mut agent.command).hint_text("claude").desired_width(220.0));
        ui.end_row();
        ui.label("Arguments");
        let mut args = agent.args.join(" ");
        if ui.add(egui::TextEdit::singleline(&mut args).hint_text("--model opus").desired_width(220.0)).changed() {
            agent.args = args.split_whitespace().map(str::to_owned).collect();
        }
        ui.end_row();
        ui.label("Icon & color");
        ui.horizontal(|ui| {
            for icon in ["claude", "codex", "gemini", "shell"] {
                let (rect, resp) = ui.allocate_exact_size(Vec2::splat(26.0), egui::Sense::click());
                if agent.icon == icon {
                    ui.painter().rect_filled(rect, CornerRadius::same(5), c32(colors.pill_selected_background));
                }
                let color = agent.color.map(c32).unwrap_or(c32(colors.text_primary));
                paint_icon(ui.painter(), rect.center(), 7.0, icon, color);
                if resp.clicked() {
                    agent.icon = icon.into();
                }
            }
            let mut rgb = agent.color.map(|c| [c.r, c.g, c.b]).unwrap_or([200, 200, 200]);
            if ui.color_edit_button_srgb(&mut rgb).changed() {
                agent.color = Some(Rgb::new(rgb[0], rgb[1], rgb[2]));
            }
        });
        ui.end_row();

        ui.label("Token source");
        egui::ComboBox::from_id_salt(("metrics", i)).selected_text(metrics_kind(&agent.metrics)).show_ui(ui, |ui| {
            for template in metrics_templates() {
                let selected = metrics_kind(&template) == metrics_kind(&agent.metrics);
                if ui.selectable_label(selected, metrics_kind(&template)).clicked() && !selected {
                    agent.metrics = template;
                }
            }
        });
        ui.end_row();
        match &mut agent.metrics {
            MetricsSource::None => {}
            MetricsSource::ClaudeCode { path } | MetricsSource::Codex { path } | MetricsSource::GeminiCli { path } => {
                ui.label("Log directory");
                ui.add(egui::TextEdit::singleline(path).desired_width(260.0));
                ui.end_row();
            }
            MetricsSource::Jsonl(spec) => {
                ui.label("Log directory");
                ui.add(egui::TextEdit::singleline(&mut spec.path).desired_width(260.0));
                ui.end_row();
                ui.label("File extension");
                ui.add(egui::TextEdit::singleline(&mut spec.extension).desired_width(80.0));
                ui.end_row();
                ui.label("Input tokens (JSON pointer)");
                ui.add(egui::TextEdit::singleline(&mut spec.input_tokens).desired_width(220.0));
                ui.end_row();
                ui.label("Output tokens");
                ui.add(egui::TextEdit::singleline(&mut spec.output_tokens).desired_width(220.0));
                ui.end_row();
                for (label, field, hint) in [
                    ("Cache read tokens", &mut spec.cache_read_tokens, "/usage/cache_read"),
                    ("Cache write tokens", &mut spec.cache_creation_tokens, "/usage/cache_creation"),
                    ("Session id", &mut spec.session_id, "/session_id"),
                    ("Timestamp", &mut spec.timestamp, "/timestamp"),
                    ("Working directory", &mut spec.cwd, "/cwd"),
                    ("Model", &mut spec.model, "/model"),
                    ("Dedupe key", &mut spec.dedupe_key, "/request_id"),
                ] {
                    ui.label(label);
                    optional_text(ui, field, hint);
                    ui.end_row();
                }
                ui.label("Values are running totals");
                ui.checkbox(&mut spec.cumulative, "");
                ui.end_row();
            }
            MetricsSource::Command { program, args, interval_secs } => {
                ui.label("Program");
                ui.add(egui::TextEdit::singleline(program).desired_width(220.0));
                ui.end_row();
                ui.label("Arguments");
                let mut text = args.join(" ");
                if ui.add(egui::TextEdit::singleline(&mut text).desired_width(220.0)).changed() {
                    *args = text.split_whitespace().map(str::to_owned).collect();
                }
                ui.end_row();
                ui.label("Every");
                ui.add(egui::DragValue::new(interval_secs).range(5..=86_400).suffix(" s"));
                ui.end_row();
            }
        }

        ui.label("Session budget");
        optional_tokens(ui, &mut agent.budget.session_tokens, 1_000_000);
        ui.end_row();
        ui.label("Weekly budget");
        optional_tokens(ui, &mut agent.budget.weekly_tokens, 10_000_000);
        ui.end_row();
        ui.label("Weekly reset (UTC)");
        ui.horizontal(|ui| {
            optional_text(ui, &mut agent.budget.weekly_reset, "mon 09:00 (empty = rolling 7 days)");
            if let Some(reset) = &agent.budget.weekly_reset
                && let Err(e) = WeeklyReset::parse(reset)
            {
                ui.label(RichText::new(format!("⚠ {e}")).size(11.0).color(c32(colors.status_waiting)));
            }
        });
        ui.end_row();
        ui.label("Cache reads");
        ui.checkbox(&mut agent.budget.include_cache_reads, "Count towards budgets");
        ui.end_row();
    });
}

fn keybindings(ui: &mut egui::Ui, state: &mut SettingsState, colors: &UiColors) {
    section(ui, "Keyboard shortcuts");
    ui.label(RichText::new("Click a shortcut and press the new combination. Esc cancels.").size(11.0).color(c32(colors.text_dim)));
    ui.add_space(4.0);

    let defaults = crate::config::Keybindings::default();
    let mut counts: HashMap<KeyCombo, usize> = HashMap::new();
    for action in Action::ALL {
        if let Some(combo) = state.draft.keybindings.combo(action) {
            *counts.entry(combo).or_default() += 1;
        }
    }
    egui::Grid::new("keybindings").num_columns(3).spacing([16.0, 6.0]).show(ui, |ui| {
        for action in Action::ALL {
            ui.label(action.label());
            let combo = state.draft.keybindings.combo(action);
            let recording = state.recording == Some(action);
            let text = if recording {
                RichText::new("Press keys…").color(c32(colors.status_waiting))
            } else {
                match &combo {
                    Some(c) if counts.get(c).copied().unwrap_or(0) > 1 => RichText::new(format!("{}  ⚠ duplicate", c.display())).color(c32(colors.status_waiting)),
                    Some(c) => RichText::new(c.display()).monospace(),
                    None => RichText::new("—").color(c32(colors.text_dim)),
                }
            };
            if ui.add_sized([180.0, 24.0], egui::Button::selectable(recording, text)).clicked() {
                state.recording = if recording { None } else { Some(action) };
            }
            ui.horizontal(|ui| {
                let is_default = state.draft.keybindings.0.get(&action) == defaults.0.get(&action);
                if ui.add_enabled(!is_default, egui::Button::new("Reset").small()).clicked() {
                    state.draft.keybindings.0.insert(action, defaults.0[&action].clone());
                }
                if ui.add_enabled(combo.is_some(), egui::Button::new("Clear").small()).clicked() {
                    state.draft.keybindings.0.insert(action, String::new());
                }
            });
            ui.end_row();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_binds_the_pressed_combo() {
        let mut state = SettingsState::default();
        state.open_with(&AppConfig::with_defaults(), &AppConfig::with_defaults());
        assert!(!state.has_unsaved_changes());
        assert!(!state.record(None), "nothing was being recorded");

        state.recording = Some(Action::ToggleSidebar);
        let combo = KeyCombo::parse("ctrl+alt+s").unwrap();
        assert!(state.record(Some(combo.clone())));
        assert_eq!(state.draft.keybindings.combo(Action::ToggleSidebar), Some(combo));
        assert!(state.has_unsaved_changes());

        // Esc → cancel without changes.
        state.recording = Some(Action::Copy);
        let before = state.draft.clone();
        assert!(state.record(None));
        assert_eq!(state.draft, before);
    }

    #[test]
    fn metrics_templates_cover_every_kind_and_roundtrip() {
        let kinds: Vec<&str> = metrics_templates().iter().map(metrics_kind).collect();
        assert_eq!(kinds.len(), 6);
        let mut config = AppConfig::with_defaults();
        for (i, template) in metrics_templates().into_iter().enumerate() {
            config.agents[i % 3].metrics = template;
            let text = config.to_toml().unwrap();
            assert_eq!(AppConfig::parse(&text).unwrap(), config);
        }
    }
}
