//! egui chrome (see `docs/DESIGN.md`): collapsible sidebar (new session,
//! search, All/Waiting/Working pills, sessions grouped by project with token
//! counters, agent launcher, Archived), one tab bar per pane, a live status /
//! token footer per pane, the empty state and "Jump to bottom". Terminals are
//! drawn by `renderer.rs` inside the rectangles this module reports back.
//!
//! The UI never mutates app state directly: it returns [`UiAction`]s that
//! `app.rs` applies after the frame.

use std::collections::HashMap;

use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};

use crate::ai_monitor::{Gauge, format_reset, format_tokens};
use crate::config::{AgentProfile, ResourcesConfig, TabStyle};
use crate::resources::{ResourceUsage, format_bytes, format_cpu};
use crate::session::{
    ActivityState, ArchivedSession, SessionFilter, SessionId, SessionSummary, USAGE_TAB, filter_counts, format_elapsed,
    group_sessions,
};
use crate::theme::{Rgb, UiColors};
use crate::workspace::{PaneId, Rect as WsRect, Workspace};

pub const TAB_BAR_HEIGHT: f32 = 30.0;
pub const FOOTER_HEIGHT: f32 = 22.0;
const PANE_GAP: f32 = 1.0;

pub fn c32(c: Rgb) -> Color32 {
    Color32::from_rgb(c.r, c.g, c.b)
}

#[derive(Clone, Debug, PartialEq)]
pub enum UiAction {
    NewSession { profile: Option<String> },
    FocusSession(SessionId),
    ActivateTab(PaneId, usize),
    CloseSession(SessionId),
    NewTab(PaneId),
    ToggleSidebar,
    JumpToBottom(SessionId),
    DragWindow,
    OpenSettings,
    OpenUsage,
    RestoreArchived(usize),
    RemoveArchived(usize),
    /// New name for a session; empty restores the automatic name.
    RenameSession(SessionId, String),
}

/// Sidebar UI state that persists across frames.
#[derive(Default)]
pub struct ChromeState {
    pub filter: SessionFilter,
    pub search: String,
    /// Set by the "search sessions" shortcut; consumed on the next frame.
    pub focus_search: bool,
    pub archived_open: bool,
    /// Session whose name is being edited inline (tab or sidebar card).
    pub renaming: Option<Renaming>,
}

pub struct Renaming {
    pub id: SessionId,
    pub text: String,
    /// Focus the text field on the next frame.
    pub focus: bool,
}

impl ChromeState {
    fn start_rename(&mut self, id: SessionId, current: String) {
        if id != USAGE_TAB {
            self.renaming = Some(Renaming { id, text: current, focus: true });
        }
    }
}

/// Token usage of the agent running in a session, from the local monitor.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageView {
    pub agent: String,
    pub model: Option<String>,
    pub session_tokens: u64,
    pub session_budget: Option<Gauge>,
    pub weekly: Option<Gauge>,
    pub weekly_used: u64,
}

pub struct ChromeInput<'a> {
    pub colors: &'a UiColors,
    pub terminal_background: Rgb,
    pub summaries: &'a [SessionSummary],
    pub usage: &'a HashMap<SessionId, UsageView>,
    /// Per-tab CPU / RAM / disk (empty when disabled).
    pub resources: &'a HashMap<SessionId, ResourceUsage>,
    pub resource_display: &'a ResourcesConfig,
    pub archived: &'a [ArchivedSession],
    pub workspace: &'a Workspace,
    pub agents: &'a [AgentProfile],
    pub sidebar_visible: bool,
    pub sidebar_width: f32,
    pub pane_footer: bool,
    pub tab_style: TabStyle,
    pub tab_radius: f32,
    pub tab_max_width: f32,
    /// Space reserved at the top-left for native window controls (macOS).
    pub traffic_lights: Option<Vec2>,
    /// Sessions whose viewport is scrolled up.
    pub scrolled: &'a [SessionId],
    pub new_session_shortcut: String,
    pub search_shortcut: String,
    pub error: Option<&'a str>,
}

/// Where a pane's terminal goes (egui points).
#[derive(Clone, Copy, Debug)]
pub struct PaneView {
    pub pane: PaneId,
    pub session: Option<SessionId>,
    pub terminal: Rect,
}

#[derive(Default)]
pub struct ChromeOutput {
    pub actions: Vec<UiAction>,
    pub panes: Vec<PaneView>,
}

/// Applies the theme's chrome palette and the terminal font to egui.
pub fn apply_style(ctx: &egui::Context, colors: &UiColors, light: bool, font: Option<Vec<u8>>) {
    let mut visuals = if light { egui::Visuals::light() } else { egui::Visuals::dark() };
    visuals.panel_fill = c32(colors.chrome_background);
    visuals.window_fill = c32(colors.chrome_background);
    visuals.extreme_bg_color = c32(colors.input_background);
    visuals.faint_bg_color = c32(colors.button_background);
    visuals.override_text_color = Some(c32(colors.text_primary));
    visuals.window_stroke = Stroke::new(1.0, c32(colors.border));
    visuals.selection.bg_fill = c32(colors.accent).gamma_multiply(0.35);
    visuals.selection.stroke = Stroke::new(1.0, c32(colors.accent));
    for w in [&mut visuals.widgets.inactive, &mut visuals.widgets.noninteractive] {
        w.bg_fill = c32(colors.button_background);
        w.weak_bg_fill = c32(colors.button_background);
        w.bg_stroke = Stroke::NONE;
        w.fg_stroke = Stroke::new(1.0, c32(colors.text_primary));
    }
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, c32(colors.border));
    visuals.widgets.hovered.weak_bg_fill = c32(colors.pill_selected_background);
    visuals.widgets.hovered.bg_fill = c32(colors.pill_selected_background);
    visuals.widgets.hovered.bg_stroke = Stroke::NONE;
    visuals.widgets.active.weak_bg_fill = c32(colors.pill_selected_background);
    ctx.set_visuals(visuals);

    let mut fonts = egui::FontDefinitions::default();
    if let Some(bytes) = font {
        fonts.font_data.insert("terminal".into(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts.families.entry(family).or_default().insert(0, "terminal".into());
        }
    }
    ctx.set_fonts(fonts);
}

/// Small vector icons, so nothing depends on an icon font being installed.
pub fn paint_icon(painter: &egui::Painter, center: Pos2, radius: f32, icon: &str, color: Color32) {
    use std::f32::consts::TAU;
    match icon {
        "claude" => {
            // Starburst.
            for i in 0..8 {
                let dir = Vec2::angled(i as f32 * TAU / 8.0);
                painter.line_segment([center + dir * radius * 0.15, center + dir * radius], Stroke::new(radius * 0.32, color));
            }
        }
        "gemini" => {
            // Four-pointed sparkle.
            let pts: Vec<Pos2> = (0..8)
                .map(|i| {
                    let r = if i % 2 == 0 { radius } else { radius * 0.28 };
                    center + Vec2::angled(i as f32 * TAU / 8.0 - TAU / 4.0) * r
                })
                .collect();
            painter.add(egui::Shape::convex_polygon(pts, color, Stroke::NONE));
        }
        "codex" => {
            let pts: Vec<Pos2> = (0..6).map(|i| center + Vec2::angled(i as f32 * TAU / 6.0 + TAU / 12.0) * radius * 0.9).collect();
            painter.add(egui::Shape::closed_line(pts, Stroke::new(radius * 0.25, color)));
            painter.circle_filled(center, radius * 0.25, color);
        }
        "new" => {
            let r = Rect::from_center_size(center, Vec2::splat(radius * 2.0));
            let stroke = Stroke::new(1.2, color);
            painter.rect_stroke(r, CornerRadius::same(3), stroke, StrokeKind::Inside);
            painter.line_segment([center - Vec2::new(radius * 0.5, 0.0), center + Vec2::new(radius * 0.5, 0.0)], stroke);
            painter.line_segment([center - Vec2::new(0.0, radius * 0.5), center + Vec2::new(0.0, radius * 0.5)], stroke);
        }
        "search" => {
            let stroke = Stroke::new(1.3, color);
            painter.circle_stroke(center - Vec2::splat(radius * 0.2), radius * 0.6, stroke);
            painter.line_segment([center + Vec2::splat(radius * 0.25), center + Vec2::splat(radius * 0.85)], stroke);
        }
        "clock" => {
            let stroke = Stroke::new(1.2, color);
            painter.circle_stroke(center, radius * 0.8, stroke);
            painter.line_segment([center, center - Vec2::new(0.0, radius * 0.5)], stroke);
            painter.line_segment([center, center + Vec2::new(radius * 0.4, 0.0)], stroke);
        }
        "spinner" => {
            for i in 0..8 {
                let dir = Vec2::angled(i as f32 * TAU / 8.0);
                let c = color.gamma_multiply(0.25 + 0.75 * (i as f32 / 7.0));
                painter.circle_filled(center + dir * radius * 0.7, radius * 0.16, c);
            }
        }
        "archive" => {
            let stroke = Stroke::new(1.2, color);
            let lid = Rect::from_center_size(center - Vec2::new(0.0, radius * 0.5), Vec2::new(radius * 2.0, radius * 0.5));
            let body = Rect::from_min_max(Pos2::new(lid.left() + 1.5, lid.bottom()), Pos2::new(lid.right() - 1.5, center.y + radius * 0.85));
            painter.rect_stroke(lid, CornerRadius::same(1), stroke, StrokeKind::Inside);
            painter.rect_stroke(body, CornerRadius::same(1), stroke, StrokeKind::Inside);
            painter.line_segment([Pos2::new(center.x - radius * 0.3, body.top() + 3.0), Pos2::new(center.x + radius * 0.3, body.top() + 3.0)], stroke);
        }
        "close" => {
            // Drawn, not "✕": the terminal font egui uses may not have it.
            let k = radius * 0.55;
            let stroke = Stroke::new(1.3, color);
            painter.line_segment([center + Vec2::new(-k, -k), center + Vec2::new(k, k)], stroke);
            painter.line_segment([center + Vec2::new(k, -k), center + Vec2::new(-k, k)], stroke);
        }
        "chart" => {
            // Three rising bars.
            let w = radius * 0.42;
            for (i, h) in [0.55, 1.0, 1.45].into_iter().enumerate() {
                let x = center.x - radius * 0.85 + i as f32 * (w + radius * 0.22);
                let bar = Rect::from_min_max(Pos2::new(x, center.y + radius * 0.8 - radius * h), Pos2::new(x + w, center.y + radius * 0.8));
                painter.rect_filled(bar, CornerRadius::same(1), color);
            }
        }
        "gear" => {
            let stroke = Stroke::new(1.3, color);
            for i in 0..8 {
                let dir = Vec2::angled(i as f32 * TAU / 8.0);
                painter.line_segment([center + dir * radius * 0.55, center + dir * radius * 0.95], stroke);
            }
            painter.circle_stroke(center, radius * 0.55, stroke);
            painter.circle_filled(center, radius * 0.18, color);
        }
        _ => {
            // Shell prompt: ">_".
            let s = radius * 0.55;
            let stroke = Stroke::new(radius * 0.22, color);
            painter.line_segment([center + Vec2::new(-s, -s), center], stroke);
            painter.line_segment([center, center + Vec2::new(-s, s)], stroke);
            painter.line_segment([center + Vec2::new(s * 0.3, s), center + Vec2::new(s * 1.3, s)], stroke);
        }
    }
}

fn agent_color(agents: &[AgentProfile], profile: Option<&str>, icon: &str, colors: &UiColors) -> Color32 {
    profile
        .and_then(|p| agents.iter().find(|a| a.name == p))
        .or_else(|| agents.iter().find(|a| a.icon == icon))
        .and_then(|a| a.color)
        .map(c32)
        .unwrap_or_else(|| c32(colors.text_dim))
}

fn sidebar_toggle(ui: &mut egui::Ui, rect: Rect, colors: &UiColors, out: &mut ChromeOutput) {
    let resp = ui.interact(rect, ui.scope_id().with("sidebar-toggle"), Sense::click());
    let color = if resp.hovered() { c32(colors.text_primary) } else { c32(colors.text_dim) };
    let r = rect.shrink(4.0);
    let p = ui.painter();
    p.rect_stroke(r, CornerRadius::same(3), Stroke::new(1.2, color), StrokeKind::Inside);
    p.line_segment([Pos2::new(r.left() + r.width() * 0.38, r.top()), Pos2::new(r.left() + r.width() * 0.38, r.bottom())], Stroke::new(1.2, color));
    if resp.clicked() {
        out.actions.push(UiAction::ToggleSidebar);
    }
}

pub fn draw(root: &mut egui::Ui, state: &mut ChromeState, input: &ChromeInput<'_>) -> ChromeOutput {
    let mut out = ChromeOutput::default();
    let colors = input.colors;

    if input.sidebar_visible {
        egui::Panel::left("sidebar")
            .exact_size(input.sidebar_width)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(c32(colors.chrome_background)).inner_margin(egui::Margin::symmetric(8, 0)))
            .show(root, |ui| sidebar(ui, state, input, &mut out));
    }

    egui::CentralPanel::no_frame().show(root, |ui| {
        let area = ui.max_rect();
        if input.workspace.is_empty() {
            empty_state(ui, area, input, &mut out);
            return;
        }
        let ws_area = WsRect::new(area.left(), area.top(), area.width(), area.height());
        let order = input.workspace.pane_order();
        for (pane_id, r) in input.workspace.layout(ws_area, PANE_GAP) {
            let rect = Rect::from_min_size(Pos2::new(r.x, r.y), Vec2::new(r.w, r.h));
            // The top-left pane shares its row with the window controls when
            // the sidebar is hidden.
            let is_first = order.first() == Some(&pane_id) && rect.left() <= area.left() + 0.5;
            let inset = if !input.sidebar_visible && is_first && rect.top() <= area.top() + 0.5 {
                input.traffic_lights.map(|v| v.x).unwrap_or(0.0) + 30.0
            } else {
                0.0
            };
            let tabs = Rect::from_min_size(rect.min, Vec2::new(rect.width(), TAB_BAR_HEIGHT));
            tab_bar(ui, state, pane_id, tabs, inset, input, &mut out);
            if inset > 0.0 {
                let x = input.traffic_lights.map(|v| v.x).unwrap_or(0.0) + 2.0;
                sidebar_toggle(ui, Rect::from_min_size(Pos2::new(rect.left() + x, rect.top() + 4.0), Vec2::splat(22.0)), colors, &mut out);
            }
            let session = input.workspace.pane(pane_id).and_then(|p| p.active_session());
            let terminal_tab = session.is_some_and(|s| s != USAGE_TAB);
            let footer_h = if input.pane_footer && terminal_tab { FOOTER_HEIGHT } else { 0.0 };
            let terminal = Rect::from_min_max(Pos2::new(rect.left(), tabs.bottom()), Pos2::new(rect.right(), rect.bottom() - footer_h));
            if let Some(sid) = session.filter(|_| terminal_tab) {
                if footer_h > 0.0 {
                    let footer_rect = Rect::from_min_max(Pos2::new(rect.left(), terminal.bottom()), rect.max);
                    footer(ui, footer_rect, sid, input);
                }
                if input.scrolled.contains(&sid) {
                    jump_to_bottom(ui, terminal, sid, colors, &mut out);
                }
            }
            out.panes.push(PaneView { pane: pane_id, session, terminal });
        }
    });
    out
}

fn sidebar(ui: &mut egui::Ui, state: &mut ChromeState, input: &ChromeInput<'_>, out: &mut ChromeOutput) {
    let colors = input.colors;
    let full = ui.max_rect();

    // Header row: window controls + sidebar toggle; empty space drags.
    let header_h = if input.traffic_lights.is_some() { 38.0 } else { 34.0 };
    let toggle_x = input.traffic_lights.map(|v| v.x - 8.0).unwrap_or(0.0);
    let header = ui.interact(Rect::from_min_size(full.min, Vec2::new(full.width(), header_h)), ui.scope_id().with("sidebar-drag"), Sense::click_and_drag());
    if header.drag_started() {
        out.actions.push(UiAction::DragWindow);
    }
    sidebar_toggle(ui, Rect::from_min_size(Pos2::new(full.left() + toggle_x, full.top() + 8.0), Vec2::splat(22.0)), colors, out);
    ui.add_space(header_h);

    // + New session.
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 30.0), Sense::click());
    let fill = if resp.hovered() { colors.pill_selected_background } else { colors.button_background };
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(6), c32(fill));
    paint_icon(p, Pos2::new(rect.left() + 14.0, rect.center().y), 6.0, "new", c32(colors.text_primary));
    p.text(Pos2::new(rect.left() + 28.0, rect.center().y), Align2::LEFT_CENTER, "New session", FontId::proportional(12.5), c32(colors.text_primary));
    p.text(Pos2::new(rect.right() - 10.0, rect.center().y), Align2::RIGHT_CENTER, &input.new_session_shortcut, FontId::proportional(11.0), c32(colors.text_dim));
    if resp.clicked() {
        out.actions.push(UiAction::NewSession { profile: None });
    }
    ui.add_space(6.0);

    // Search.
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 28.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(6), c32(colors.input_background));
    paint_icon(ui.painter(), Pos2::new(rect.left() + 13.0, rect.center().y), 6.0, "search", c32(colors.text_dim));
    let edit_rect = Rect::from_min_max(Pos2::new(rect.left() + 24.0, rect.top()), Pos2::new(rect.right() - 34.0, rect.bottom()));
    let edit = egui::TextEdit::singleline(&mut state.search)
        .hint_text(egui::RichText::new("Search agents").color(c32(colors.text_dim)))
        .frame(egui::Frame::NONE)
        .font(FontId::proportional(12.0))
        .align(Align2::LEFT_CENTER)
        .desired_width(edit_rect.width());
    let search = ui.put(edit_rect, edit);
    if std::mem::take(&mut state.focus_search) {
        search.request_focus();
    }
    if search.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        state.search.clear();
        search.surrender_focus();
    }
    ui.painter().text(Pos2::new(rect.right() - 8.0, rect.center().y), Align2::RIGHT_CENTER, &input.search_shortcut, FontId::proportional(10.5), c32(colors.text_dim));
    ui.add_space(6.0);

    // Filter pills.
    let counts = filter_counts(input.summaries);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(2.0, 4.0);
        for (filter, label, count, icon) in [
            (SessionFilter::All, "All", Some(counts.all), None),
            (SessionFilter::Waiting, "Waiting", (counts.waiting > 0).then_some(counts.waiting), Some("clock")),
            (SessionFilter::Working, "Working", (counts.working > 0).then_some(counts.working), Some("spinner")),
        ] {
            let text = match count {
                Some(n) => format!("{label} {n}"),
                None => label.to_owned(),
            };
            let font = FontId::proportional(10.5);
            let galley = ui.painter().layout_no_wrap(text, font, Color32::WHITE);
            let width = galley.size().x + if icon.is_some() { 26.0 } else { 14.0 };
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 24.0), Sense::click());
            let selected = state.filter == filter;
            let p = ui.painter();
            if selected {
                p.rect_filled(rect, CornerRadius::same(6), c32(colors.pill_selected_background));
            } else if resp.hovered() {
                p.rect_filled(rect, CornerRadius::same(6), c32(colors.button_background));
            }
            let color = c32(if selected { colors.text_primary } else { colors.text_dim });
            let mut x = rect.left() + 7.0;
            if let Some(icon) = icon {
                paint_icon(p, Pos2::new(x + 4.0, rect.center().y), 4.5, icon, color);
                x += 12.0;
            }
            p.galley(Pos2::new(x, rect.center().y - galley.size().y / 2.0), galley, color);
            if resp.clicked() {
                state.filter = filter;
            }
        }
    });
    ui.add_space(6.0);

    // Footer: error banner, agent launcher, archived + settings.
    let footer_h = 76.0 + if input.error.is_some() { 48.0 } else { 0.0 };
    let list_h = (ui.available_height() - footer_h).max(0.0);
    let focused = input.workspace.focused_session();

    egui::ScrollArea::vertical().max_height(list_h).auto_shrink([false, false]).show(ui, |ui| {
        if state.archived_open {
            archived_list(ui, input, out);
            return;
        }
        let groups = group_sessions(input.summaries, state.filter, &state.search);
        if groups.is_empty() {
            ui.add_space(8.0);
            let msg = if input.summaries.is_empty() { "No sessions yet" } else { "No matching sessions" };
            ui.label(egui::RichText::new(msg).color(c32(colors.text_dim)).size(12.0));
        }
        for group in groups {
            ui.add_space(6.0);
            ui.label(egui::RichText::new(&group.name).color(c32(colors.text_dim)).size(11.5));
            ui.add_space(2.0);
            for s in &group.sessions {
                session_card(ui, state, s, focused == Some(s.id), input, out);
            }
        }
    });

    let bottom = Rect::from_min_max(Pos2::new(full.left(), full.bottom() - footer_h), full.max);
    let mut ui = ui.new_child(egui::UiBuilder::new().max_rect(bottom));
    if let Some(error) = input.error {
        egui::Frame::new()
            .fill(c32(colors.diff_delete_background))
            .corner_radius(CornerRadius::same(6))
            .inner_margin(egui::Margin::same(6))
            .show(&mut ui, |ui| {
                ui.label(egui::RichText::new(format!("config.toml: {error}")).size(10.5).color(c32(colors.text_primary)));
            });
        ui.add_space(4.0);
    }
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let launchers = input.agents.iter().map(|a| (Some(a.name.clone()), a.icon.as_str(), a.color.map(c32), a.name.as_str()));
        let shell = std::iter::once((None, "shell", None, "Shell"));
        for (profile, icon, color, name) in launchers.chain(shell) {
            let (rect, resp) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
            if resp.hovered() {
                ui.painter().rect_filled(rect, CornerRadius::same(6), c32(colors.button_background));
            }
            paint_icon(ui.painter(), rect.center(), 7.0, icon, color.unwrap_or(c32(colors.text_dim)));
            if resp.on_hover_text(format!("New {name} session")).clicked() {
                out.actions.push(UiAction::NewSession { profile });
            }
        }
    });
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        // Room for the usage and settings buttons.
        let w = ui.available_width() - 2.0 * (28.0 + ui.spacing().item_spacing.x);
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 28.0), Sense::click());
        if state.archived_open || resp.hovered() {
            ui.painter().rect_filled(rect, CornerRadius::same(6), c32(colors.button_background));
        }
        let color = if state.archived_open { colors.text_primary } else { colors.text_dim };
        paint_icon(ui.painter(), Pos2::new(rect.left() + 12.0, rect.center().y), 6.0, "archive", c32(color));
        ui.painter().text(Pos2::new(rect.left() + 26.0, rect.center().y), Align2::LEFT_CENTER, "Archived", FontId::proportional(12.0), c32(color));
        ui.painter().text(Pos2::new(rect.right() - 8.0, rect.center().y), Align2::RIGHT_CENTER, input.archived.len().to_string(), FontId::proportional(11.0), c32(colors.text_dim));
        if resp.clicked() {
            state.archived_open = !state.archived_open;
        }
        let (chart, chart_resp) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
        if chart_resp.hovered() {
            ui.painter().rect_filled(chart, CornerRadius::same(6), c32(colors.button_background));
        }
        paint_icon(ui.painter(), chart.center(), 7.0, "chart", c32(colors.text_dim));
        if chart_resp.on_hover_text("Usage dashboard").clicked() {
            out.actions.push(UiAction::OpenUsage);
        }
        let (gear, gear_resp) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::click());
        if gear_resp.hovered() {
            ui.painter().rect_filled(gear, CornerRadius::same(6), c32(colors.button_background));
        }
        paint_icon(ui.painter(), gear.center(), 7.0, "gear", c32(colors.text_dim));
        if gear_resp.on_hover_text("Settings").clicked() {
            out.actions.push(UiAction::OpenSettings);
        }
    });
}

fn archived_list(ui: &mut egui::Ui, input: &ChromeInput<'_>, out: &mut ChromeOutput) {
    let colors = input.colors;
    ui.add_space(6.0);
    ui.label(egui::RichText::new("Archived — click to reopen").color(c32(colors.text_dim)).size(11.5));
    if input.archived.is_empty() {
        ui.label(egui::RichText::new("Closed sessions appear here").color(c32(colors.text_dim)).size(12.0));
    }
    let now_ms = crate::ai_monitor::now_ms();
    for (i, entry) in input.archived.iter().enumerate() {
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 40.0), Sense::click());
        let p = ui.painter();
        if resp.hovered() {
            p.rect_filled(rect, CornerRadius::same(6), c32(colors.button_background));
        }
        let color = agent_color(input.agents, entry.profile.as_deref(), &entry.icon, colors);
        paint_icon(p, Pos2::new(rect.left() + 12.0, rect.top() + 13.0), 6.5, &entry.icon, color);
        let left = rect.left() + 26.0;
        let ago = format_elapsed(std::time::Duration::from_millis(now_ms.saturating_sub(entry.closed_unix_ms)));
        p.with_clip_rect(Rect::from_min_max(rect.min, Pos2::new(rect.right() - 52.0, rect.bottom())))
            .text(Pos2::new(left, rect.top() + 13.0), Align2::LEFT_CENTER, entry.task.as_deref().unwrap_or(&entry.title), FontId::proportional(12.5), c32(colors.text_primary));
        p.text(Pos2::new(rect.right() - 24.0, rect.top() + 13.0), Align2::RIGHT_CENTER, ago, FontId::proportional(11.0), c32(colors.text_dim));
        p.with_clip_rect(rect).text(Pos2::new(left, rect.top() + 28.0), Align2::LEFT_CENTER, &entry.project, FontId::proportional(11.0), c32(colors.text_dim));
        let remove = Rect::from_center_size(Pos2::new(rect.right() - 10.0, rect.top() + 13.0), Vec2::splat(16.0));
        let remove_resp = ui.interact(remove, ui.scope_id().with(("rm-archived", i)), Sense::click());
        if resp.hovered() || remove_resp.hovered() {
            paint_icon(ui.painter(), remove.center(), 7.0, "close", c32(colors.text_dim));
        }
        if remove_resp.clicked() {
            out.actions.push(UiAction::RemoveArchived(i));
        } else if resp.clicked() {
            out.actions.push(UiAction::RestoreArchived(i));
        }
    }
}

fn session_card(ui: &mut egui::Ui, state: &mut ChromeState, s: &SessionSummary, selected: bool, input: &ChromeInput<'_>, out: &mut ChromeOutput) {
    let colors = input.colors;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 40.0), Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, CornerRadius::same(6), c32(colors.pill_selected_background));
    } else if resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(6), c32(colors.button_background));
    }
    let icon_center = Pos2::new(rect.left() + 12.0, rect.top() + 13.0);
    let color = agent_color(input.agents, s.profile.as_deref(), &s.icon, colors);
    paint_icon(p, icon_center, 6.5, &s.icon, color);
    if s.attention && !selected {
        p.circle_filled(icon_center + Vec2::new(6.0, -6.0), 3.0, c32(colors.status_done));
    }

    let text_left = rect.left() + 26.0;
    let elapsed = format_elapsed(s.elapsed);
    let title_rect = Rect::from_min_max(Pos2::new(text_left, rect.top() + 4.0), Pos2::new(rect.right() - 34.0, rect.top() + 20.0));
    if !rename_field(ui, state, s.id, title_rect, 12.5, colors, out) {
        ui.painter().with_clip_rect(title_rect).text(Pos2::new(text_left, rect.top() + 13.0), Align2::LEFT_CENTER, &s.title, FontId::proportional(12.5), c32(colors.text_primary));
    }
    let p = ui.painter();
    p.text(Pos2::new(rect.right() - 6.0, rect.top() + 13.0), Align2::RIGHT_CENTER, elapsed, FontId::proportional(11.0), c32(colors.text_dim));

    // Tokens used (or left in the session budget) on the second line.
    let tokens = input.usage.get(&s.id).map(|u| match u.session_budget {
        Some(g) => format!("{} left", format_tokens(g.remaining())),
        None => format_tokens(u.session_tokens),
    });
    let tokens_w = match tokens {
        Some(t) => {
            let r = p.text(Pos2::new(rect.right() - 6.0, rect.top() + 28.0), Align2::RIGHT_CENTER, t, FontId::proportional(10.5), c32(colors.text_dim));
            r.width() + 8.0
        }
        None => 0.0,
    };

    let sub_color = match s.state {
        ActivityState::Done => colors.status_done,
        ActivityState::Waiting if s.attention => colors.status_waiting,
        _ => colors.text_dim,
    };
    let sub_rect = Rect::from_min_max(Pos2::new(text_left, rect.top() + 20.0), Pos2::new(rect.right() - 6.0 - tokens_w, rect.bottom()));
    p.with_clip_rect(sub_rect).text(Pos2::new(text_left, rect.top() + 28.0), Align2::LEFT_CENTER, &s.subtitle, FontId::proportional(11.0), c32(sub_color));

    if resp.double_clicked() {
        state.start_rename(s.id, s.title.clone());
    } else if resp.clicked() {
        out.actions.push(UiAction::FocusSession(s.id));
    }
    resp.context_menu(|ui| session_menu(ui, state, s.id, s.title.clone(), s.renamed, out));
}

/// Right-click menu shared by tabs and sidebar cards.
fn session_menu(ui: &mut egui::Ui, state: &mut ChromeState, id: SessionId, current: String, renamed: bool, out: &mut ChromeOutput) {
    if id != USAGE_TAB {
        if ui.button("Rename…").clicked() {
            state.start_rename(id, current);
            ui.close();
        }
        if renamed && ui.button("Reset name").clicked() {
            out.actions.push(UiAction::RenameSession(id, String::new()));
            ui.close();
        }
        ui.separator();
    }
    if ui.button("Close").clicked() {
        out.actions.push(UiAction::CloseSession(id));
        ui.close();
    }
}

/// Draws the inline name editor when `id` is being renamed. Enter or
/// clicking away saves, Escape cancels. Returns whether it was drawn.
fn rename_field(ui: &mut egui::Ui, state: &mut ChromeState, id: SessionId, rect: Rect, size: f32, colors: &UiColors, out: &mut ChromeOutput) -> bool {
    let Some(r) = state.renaming.as_mut().filter(|r| r.id == id) else { return false };
    ui.painter().rect_filled(rect.expand2(Vec2::new(3.0, 1.0)), CornerRadius::same(4), c32(colors.input_background));
    let edit = egui::TextEdit::singleline(&mut r.text)
        .id(ui.scope_id().with(("rename", id)))
        .frame(egui::Frame::NONE)
        .margin(egui::Margin::ZERO)
        .font(FontId::proportional(size))
        .text_color(c32(colors.text_primary))
        .desired_width(rect.width());
    let resp = ui.put(rect, edit);
    if std::mem::take(&mut r.focus) {
        resp.request_focus();
        if let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), resp.id) {
            let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(r.text.chars().count()));
            st.cursor.set_char_range(Some(all));
            st.store(ui.ctx(), resp.id);
        }
    } else if resp.lost_focus() || !resp.has_focus() {
        let cancelled = ui.input(|i| i.key_pressed(egui::Key::Escape));
        if !cancelled {
            out.actions.push(UiAction::RenameSession(id, r.text.clone()));
        }
        state.renaming = None;
    }
    true
}

fn tab_bar(ui: &mut egui::Ui, state: &mut ChromeState, pane_id: PaneId, rect: Rect, inset: f32, input: &ChromeInput<'_>, out: &mut ChromeOutput) {
    let colors = input.colors;
    let Some(pane) = input.workspace.pane(pane_id) else { return };
    let focused_pane = input.workspace.focused_pane() == Some(pane_id);
    ui.painter().rect_filled(rect, CornerRadius::ZERO, c32(colors.chrome_background));

    // Empty space drags the window (unified title bar).
    let bg = ui.interact(rect, ui.scope_id().with(("tabbar", pane_id)), Sense::click_and_drag());
    if bg.drag_started() {
        out.actions.push(UiAction::DragWindow);
    }

    let style = input.tab_style;
    let radius = input.tab_radius.round().clamp(0.0, 14.0) as u8;
    let accent = if focused_pane { colors.accent } else { colors.text_dim };
    let plus_w = 28.0;
    let avail = (rect.width() - inset - plus_w).max(0.0);
    let tab_w = (avail / pane.tabs.len().max(1) as f32).clamp(60.0, input.tab_max_width.max(60.0));
    // Rounded tabs start lower, so their content sits a bit lower too.
    let dy = if style == TabStyle::Rounded { 2.5 } else { 0.0 };
    let mut x = rect.left() + inset;
    for (index, sid) in pane.tabs.iter().enumerate() {
        let tab = Rect::from_min_size(Pos2::new(x, rect.top()), Vec2::new(tab_w, rect.height()));
        x += tab_w;
        let summary = input.summaries.iter().find(|s| s.id == *sid);
        let active = index == pane.active;
        let resp = ui.interact(tab, ui.scope_id().with(("tab", pane_id, *sid)), Sense::click());
        let hovered = resp.hovered();
        let p = ui.painter();

        match style {
            TabStyle::Flat => {
                if active {
                    p.rect_filled(tab, CornerRadius::ZERO, c32(input.terminal_background));
                    if focused_pane {
                        p.line_segment([tab.left_top(), tab.right_top()], Stroke::new(1.5, c32(colors.accent)));
                    }
                } else if hovered {
                    p.rect_filled(tab, CornerRadius::ZERO, c32(colors.button_background));
                }
            }
            TabStyle::Rounded => {
                // Joins the terminal below: only the top corners are rounded.
                let shape = Rect::from_min_max(Pos2::new(tab.left() + 2.0, tab.top() + 5.0), Pos2::new(tab.right() - 2.0, tab.bottom()));
                if active {
                    let corners = CornerRadius { nw: radius, ne: radius, sw: 0, se: 0 };
                    p.rect_filled(shape, corners, c32(input.terminal_background));
                    // Outline the top and sides; the bottom edge hangs below
                    // the bar (clipped) so the tab flows into the terminal.
                    let outline = Rect::from_min_max(shape.min, Pos2::new(shape.right(), shape.bottom() + 2.0));
                    p.with_clip_rect(rect).rect_stroke(outline, corners, Stroke::new(1.0, c32(colors.border)), StrokeKind::Inside);
                    if focused_pane {
                        let r = radius as f32;
                        let top = shape.top() + 0.5;
                        p.line_segment([Pos2::new(shape.left() + r, top), Pos2::new(shape.right() - r, top)], Stroke::new(1.5, c32(colors.accent)));
                    }
                } else if hovered {
                    let hover = Rect::from_min_max(shape.min, Pos2::new(shape.right(), shape.bottom() - 4.0));
                    p.rect_filled(hover, CornerRadius::same(radius), c32(colors.button_background));
                } else if index + 1 < pane.tabs.len() && index + 1 != pane.active {
                    // Thin divider between two inactive tabs.
                    let dx = tab.right();
                    p.line_segment([Pos2::new(dx, tab.top() + 11.0), Pos2::new(dx, tab.bottom() - 7.0)], Stroke::new(1.0, c32(colors.border)));
                }
            }
            TabStyle::Pill => {
                let shape = tab.shrink2(Vec2::new(2.0, 4.0));
                if active {
                    p.rect_filled(shape, CornerRadius::same(radius), c32(colors.pill_selected_background));
                } else if hovered {
                    p.rect_filled(shape, CornerRadius::same(radius), c32(colors.button_background));
                }
            }
            TabStyle::Underline => {
                let y = tab.bottom() - 1.5;
                let line = [Pos2::new(tab.left() + 8.0, y), Pos2::new(tab.right() - 8.0, y)];
                if active {
                    p.line_segment(line, Stroke::new(2.0, c32(accent)));
                } else if hovered {
                    p.line_segment(line, Stroke::new(1.0, c32(colors.border)));
                }
            }
        }
        let cy = tab.center().y + dy;

        let (title, icon, profile, attention, renamed) = match summary {
            Some(s) => (s.tab_label(), s.icon.as_str(), s.profile.as_deref(), s.attention, s.renamed),
            None if *sid == USAGE_TAB => ("Usage".into(), "chart", None, false, false),
            None => ("…".into(), "shell", None, false, false),
        };
        let color = if *sid == USAGE_TAB { c32(colors.accent) } else { agent_color(input.agents, profile, icon, colors) };
        let icon_pos = Pos2::new(tab.left() + 14.0, cy);
        paint_icon(p, icon_pos, 5.5, icon, color);
        if attention && !active {
            p.circle_filled(icon_pos + Vec2::new(5.0, -5.0), 2.5, c32(colors.status_done));
        }
        let text_color = if active || (hovered && style == TabStyle::Underline) { colors.text_primary } else { colors.text_dim };
        let text_rect = Rect::from_min_max(Pos2::new(tab.left() + 26.0, cy - 8.0), Pos2::new(tab.right() - 22.0, cy + 8.0));
        let editing = rename_field(ui, state, *sid, text_rect, 12.0, colors, out);
        if !editing {
            ui.painter().with_clip_rect(text_rect).text(Pos2::new(text_rect.left(), cy), Align2::LEFT_CENTER, &title, FontId::proportional(12.0), c32(text_color));
        }

        // Close button on hover / active.
        let close = Rect::from_center_size(Pos2::new(tab.right() - 12.0, cy), Vec2::splat(16.0));
        let close_resp = ui.interact(close, ui.scope_id().with(("close", *sid)), Sense::click());
        if !editing && (active || hovered || close_resp.hovered()) {
            let c = if close_resp.hovered() { colors.text_primary } else { colors.text_dim };
            let k = 3.5;
            let p = ui.painter();
            if close_resp.hovered() {
                p.rect_filled(close, CornerRadius::same(4), c32(colors.button_background));
            }
            p.line_segment([close.center() + Vec2::new(-k, -k), close.center() + Vec2::new(k, k)], Stroke::new(1.2, c32(c)));
            p.line_segment([close.center() + Vec2::new(k, -k), close.center() + Vec2::new(-k, k)], Stroke::new(1.2, c32(c)));
        }
        if close_resp.clicked() {
            out.actions.push(UiAction::CloseSession(*sid));
        } else if resp.double_clicked() {
            state.start_rename(*sid, title.clone());
        } else if resp.clicked() {
            out.actions.push(UiAction::ActivateTab(pane_id, index));
        }
        resp.context_menu(|ui| session_menu(ui, state, *sid, title.clone(), renamed, out));
    }

    let plus = Rect::from_min_size(Pos2::new(x, rect.top()), Vec2::new(plus_w, rect.height()));
    let resp = ui.interact(plus, ui.scope_id().with(("plus", pane_id)), Sense::click());
    let c = if resp.hovered() { colors.text_primary } else { colors.text_dim };
    let plus_center = plus.center() + Vec2::new(0.0, dy);
    if resp.hovered() {
        ui.painter().rect_filled(Rect::from_center_size(plus_center, Vec2::splat(20.0)), CornerRadius::same(radius.min(10)), c32(colors.button_background));
    }
    ui.painter().text(plus_center, Align2::CENTER_CENTER, "+", FontId::proportional(15.0), c32(c));
    if resp.on_hover_text("New tab").clicked() {
        out.actions.push(UiAction::NewTab(pane_id));
    }
}

/// `● Working · claude-opus-4-5 · 139.5k tokens        week ━━━── 15% ↻ 5d 11h`
fn footer(ui: &mut egui::Ui, rect: Rect, sid: SessionId, input: &ChromeInput<'_>) {
    let colors = input.colors;
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::ZERO, c32(input.terminal_background));
    p.line_segment([rect.left_top(), rect.right_top()], Stroke::new(1.0, c32(colors.border)));
    let Some(s) = input.summaries.iter().find(|s| s.id == sid) else { return };
    let font = FontId::proportional(11.0);
    let y = rect.center().y;
    let usage = input.usage.get(&sid);

    // Right side first (budget gauge, weekly preferred), so the left side can
    // be clipped before it on narrow panes.
    let mut right_edge = rect.right();
    let gauge = usage.and_then(|u| u.weekly.map(|g| ("week", g)).or(u.session_budget.map(|g| ("session", g))));
    if let Some((label, g)) = gauge {
        let mut right = rect.right() - 10.0;
        if let Some(reset) = g.resets_in_ms {
            let r = p.text(Pos2::new(right, y), Align2::RIGHT_CENTER, format!("↻ {}", format_reset(reset)), font.clone(), c32(colors.text_dim));
            right = r.left() - 8.0;
        }
        let pct_color = match g.percent_used() {
            0..=69 => colors.text_primary,
            70..=89 => colors.status_waiting,
            _ => colors.diff_delete_background.mix(Rgb::new(255, 90, 90), 0.85),
        };
        let r = p.text(Pos2::new(right, y), Align2::RIGHT_CENTER, format!("{}%", g.percent_used()), font.clone(), c32(pct_color));
        let bar = Rect::from_min_max(Pos2::new(r.left() - 68.0, y - 1.5), Pos2::new(r.left() - 8.0, y + 1.5));
        p.rect_filled(bar, CornerRadius::same(1), c32(colors.border));
        p.rect_filled(Rect::from_min_size(bar.min, Vec2::new(bar.width() * g.fraction_used(), bar.height())), CornerRadius::same(1), c32(pct_color));
        let label_rect = p.text(Pos2::new(bar.left() - 8.0, y), Align2::RIGHT_CENTER, label, font.clone(), c32(colors.text_dim));
        right_edge = label_rect.left();
        let hover = Rect::from_min_max(Pos2::new(label_rect.left(), rect.top()), rect.max);
        ui.interact(hover, ui.scope_id().with(("gauge", sid)), Sense::hover())
            .on_hover_text(format!("{} of {} tokens · {} left", format_tokens(g.used), format_tokens(g.limit), format_tokens(g.remaining())));
    } else if let Some(u) = usage.filter(|u| u.weekly_used > 0) {
        let r = p.text(Pos2::new(rect.right() - 10.0, y), Align2::RIGHT_CENTER, format!("{} this week", format_tokens(u.weekly_used)), font.clone(), c32(colors.text_dim));
        right_edge = r.left();
    }

    // Resource usage, small and dim, left of the budget.
    if let Some(r) = input.resources.get(&sid) {
        let cfg = input.resource_display;
        let mut parts = Vec::new();
        if cfg.show_cpu {
            parts.push(format!("CPU {}", format_cpu(r.cpu_percent)));
        }
        if cfg.show_memory {
            parts.push(format!("RAM {}", format_bytes(r.memory_bytes)));
        }
        if cfg.show_disk {
            parts.push(format!("IO {}/s", format_bytes(r.disk_read_per_sec + r.disk_write_per_sec)));
        }
        if !parts.is_empty() {
            let x = if right_edge < rect.right() { right_edge - 14.0 } else { rect.right() - 10.0 };
            let small = FontId::proportional(10.5);
            let text_rect = p.text(Pos2::new(x, y), Align2::RIGHT_CENTER, parts.join("  ·  "), small, c32(colors.text_dim));
            right_edge = text_rect.left();
            let hover = Rect::from_min_max(Pos2::new(text_rect.left(), rect.top()), Pos2::new(text_rect.right(), rect.bottom()));
            ui.interact(hover, ui.scope_id().with(("resources", sid)), Sense::hover()).on_hover_text(format!(
                "{} process{}\nCPU {} of one core\nRAM {}\nDisk read {}/s · write {}/s",
                r.processes,
                if r.processes == 1 { "" } else { "es" },
                format_cpu(r.cpu_percent),
                format_bytes(r.memory_bytes),
                format_bytes(r.disk_read_per_sec),
                format_bytes(r.disk_write_per_sec),
            ));
        }
    }

    // Left side: state · model · tokens.
    let left = p.with_clip_rect(Rect::from_min_max(rect.min, Pos2::new(right_edge - 12.0, rect.bottom())));
    // Text starts after the state dot.
    let mut x = rect.left() + 22.0;
    let mut text = |t: &str, color: Rgb| {
        let r = left.text(Pos2::new(x, y), Align2::LEFT_CENTER, t, font.clone(), c32(color));
        x = r.right() + 8.0;
    };
    let (state_color, label) = match (s.state, s.exit_code) {
        (ActivityState::Done, Some(code)) if code != 0 => (colors.status_waiting, format!("Exited {code}")),
        (ActivityState::Done, _) => (colors.status_done, "Done".to_owned()),
        (ActivityState::Working, _) => (colors.accent, "Working".to_owned()),
        (ActivityState::Waiting, _) => (if s.attention { colors.status_waiting } else { colors.text_dim }, "Waiting".to_owned()),
    };
    left.circle_filled(Pos2::new(rect.left() + 13.0, y), 3.0, c32(state_color));
    text(&label, state_color);
    if let Some(u) = usage {
        text("·", colors.text_dim);
        text(u.model.as_deref().unwrap_or(&u.agent), colors.text_dim);
        text("·", colors.text_dim);
        text(&format!("{} tokens", format_tokens(u.session_tokens)), colors.text_dim);
    }
}

fn jump_to_bottom(ui: &mut egui::Ui, terminal: Rect, sid: SessionId, colors: &UiColors, out: &mut ChromeOutput) {
    let size = Vec2::new(150.0, 24.0);
    let rect = Rect::from_center_size(Pos2::new(terminal.center().x, terminal.bottom() - 26.0), size);
    let resp = ui.interact(rect, ui.scope_id().with(("jump", sid)), Sense::click());
    let fill = if resp.hovered() { colors.pill_selected_background } else { colors.button_background };
    let p = ui.painter();
    p.rect(rect, CornerRadius::same(12), c32(fill), Stroke::new(1.0, c32(colors.border)), StrokeKind::Inside);
    p.text(rect.center(), Align2::CENTER_CENTER, "Jump to bottom ↓", FontId::proportional(11.5), c32(colors.text_primary));
    if resp.clicked() {
        out.actions.push(UiAction::JumpToBottom(sid));
    }
}

fn empty_state(ui: &mut egui::Ui, area: Rect, input: &ChromeInput<'_>, out: &mut ChromeOutput) {
    let colors = input.colors;
    let bg = ui.interact(Rect::from_min_size(area.min, Vec2::new(area.width(), TAB_BAR_HEIGHT)), ui.scope_id().with("empty-drag"), Sense::click_and_drag());
    if bg.drag_started() {
        out.actions.push(UiAction::DragWindow);
    }
    if !input.sidebar_visible {
        let x = input.traffic_lights.map(|v| v.x).unwrap_or(0.0) + 2.0;
        sidebar_toggle(ui, Rect::from_min_size(Pos2::new(area.left() + x, area.top() + 4.0), Vec2::splat(22.0)), colors, out);
    }
    let center = area.center();
    ui.painter().text(center - Vec2::new(0.0, 30.0), Align2::CENTER_CENTER, "No open sessions", FontId::proportional(14.0), c32(colors.text_dim));
    let button = Rect::from_center_size(center + Vec2::new(0.0, 6.0), Vec2::new(180.0, 30.0));
    let resp = ui.interact(button, ui.scope_id().with("empty-new"), Sense::click());
    let fill = if resp.hovered() { colors.pill_selected_background } else { colors.button_background };
    ui.painter().rect_filled(button, CornerRadius::same(6), c32(fill));
    ui.painter().text(button.center(), Align2::CENTER_CENTER, format!("New session  {}", input.new_session_shortcut), FontId::proportional(12.5), c32(colors.text_primary));
    if resp.clicked() {
        out.actions.push(UiAction::NewSession { profile: None });
    }
}

impl SessionSummary {
    /// Tab label: a name the user gave, else the task an agent reported
    /// (OSC title), else the session name.
    fn tab_label(&self) -> String {
        if !self.renamed && self.subtitle != self.state.label() { self.subtitle.clone() } else { self.title.clone() }
    }
}
