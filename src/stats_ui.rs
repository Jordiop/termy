//! The "Agent usage" dashboard tab: KPI cards, tokens by day, an hour-of-week
//! heatmap, and model / agent tables. Everything is painted with egui's
//! painter from a precomputed [`UsageStats`]; this module never touches the
//! usage store itself.

use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};

use crate::ai_monitor::format_tokens;
use crate::chrome::{c32, paint_icon};
use crate::config::AgentProfile;
use crate::theme::{Rgb, UiColors};
use crate::usage_stats::{
    Kpi, Provider, Range, TOP_MODELS, UsageStats, WEEKDAYS, format_count, format_day, format_hours,
};

/// Dashboard state that persists across frames.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StatsState {
    pub range: Range,
    /// Only this agent's usage (clicked in the agents table).
    pub agent: Option<String>,
}

pub struct StatsInput<'a> {
    pub colors: &'a UiColors,
    pub background: Rgb,
    pub stats: &'a UsageStats,
    pub agents: &'a [AgentProfile],
    /// Older logs are still being read for the selected range.
    pub loading: bool,
    pub monitor_enabled: bool,
}

const MARGIN: f32 = 22.0;
const GAP: f32 = 14.0;
const SECTION_GAP: f32 = 28.0;
const HEADER_H: f32 = 44.0;
const CARD_H: f32 = 112.0;
const CHART_H: f32 = 236.0;
const ROW_H: f32 = 23.0;
/// Two columns from this content width up.
const WIDE: f32 = 820.0;

fn provider_color(p: Provider, colors: &UiColors) -> Color32 {
    match p {
        Provider::Anthropic => Color32::from_rgb(0xD9, 0x77, 0x57),
        Provider::Google => Color32::from_rgb(0x4C, 0x8B, 0xF5),
        Provider::OpenAi => Color32::from_rgb(0x10, 0xA3, 0x7F),
        Provider::XAi => Color32::from_rgb(0xA0, 0xA4, 0xAB),
        Provider::Other => c32(colors.text_dim),
    }
}

/// Text, numbers and shades used throughout, derived from the theme.
struct Palette {
    text: Color32,
    dim: Color32,
    card: Color32,
    border: Color32,
    input_bar: Color32,
    output_bar: Color32,
    up: (Color32, Color32),
    down: (Color32, Color32),
    selected: Color32,
    heat_low: Rgb,
    heat_high: Rgb,
}

impl Palette {
    fn new(colors: &UiColors, background: Rgb) -> Self {
        Self {
            text: c32(colors.text_primary),
            dim: c32(colors.text_dim),
            card: c32(background.mix(colors.button_background, 0.55)),
            border: c32(colors.border),
            input_bar: c32(colors.text_dim.mix(background, 0.25)),
            // Fixed blue: theme accents are often orange, the Anthropic color.
            output_bar: Color32::from_rgb(0x3D, 0x9B, 0xFF),
            up: (c32(colors.diff_add_background), c32(colors.status_done)),
            down: (c32(colors.diff_delete_background), c32(colors.diff_delete_background.mix(Rgb::new(255, 90, 90), 0.85))),
            selected: c32(colors.pill_selected_background),
            heat_low: background.mix(colors.button_background, 0.8),
            heat_high: colors.text_primary,
        }
    }
}

fn font(size: f32) -> FontId {
    FontId::proportional(size)
}

/// Paints the dashboard into `rect` (the pane's content area).
pub fn draw(ui: &mut egui::Ui, rect: Rect, state: &mut StatsState, input: &StatsInput<'_>) {
    let pal = Palette::new(input.colors, input.background);
    ui.painter().rect_filled(rect, CornerRadius::ZERO, c32(input.background));
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).id_salt("usage-dashboard"));
    child.set_clip_rect(rect);
    egui::ScrollArea::vertical().id_salt("usage-scroll").auto_shrink([false, false]).show(&mut child, |ui| {
        let width = ui.available_width();
        let w = (width - 2.0 * MARGIN).max(280.0);
        let height = content_height(w, input.stats);
        let (area, _) = ui.allocate_exact_size(Vec2::new(width, height), Sense::hover());
        let mut y = area.top() + MARGIN;
        let left = area.left() + MARGIN;

        header(ui, Rect::from_min_size(Pos2::new(left, y), Vec2::new(w, HEADER_H)), state, input, &pal);
        y += HEADER_H + GAP;

        let s = input.stats;
        if !has_data(s) {
            empty(ui, Rect::from_min_size(Pos2::new(left, y), Vec2::new(w, 160.0)), state, input, &pal);
            return;
        }

        // KPI cards: 4 across, or 2×2 when narrow.
        let per_row = if w >= WIDE { 4 } else { 2 };
        let card_w = (w - GAP * (per_row as f32 - 1.0)) / per_row as f32;
        let cards = [
            ("INPUT TOKENS", format_tokens(s.input.value), format!("{:.0}% from cache", s.cache_share * 100.0), &s.input),
            ("OUTPUT TOKENS", format_tokens(s.output.value), format!("{} per turn", format_tokens(s.output_per_turn)), &s.output),
            ("TASKS", format_count(s.tasks.value), format!("{} turns", format_count(s.turns)), &s.tasks),
            ("AGENT TIME", format_hours(s.agent_time.value), format!("up to {} at once", s.max_concurrent), &s.agent_time),
        ];
        for (i, (label, value, sub, kpi)) in cards.into_iter().enumerate() {
            let (row, col) = (i / per_row, i % per_row);
            let min = Pos2::new(left + col as f32 * (card_w + GAP), y + row as f32 * (CARD_H + GAP));
            kpi_card(ui.painter(), Rect::from_min_size(min, Vec2::new(card_w, CARD_H)), label, &value, &sub, kpi, &pal);
        }
        y += kpi_rows(w) as f32 * (CARD_H + GAP) - GAP + SECTION_GAP;

        // Tokens by day | when they work.
        if w >= WIDE {
            let chart_w = (w - SECTION_GAP) * 0.55;
            tokens_by_day(ui, Rect::from_min_size(Pos2::new(left, y), Vec2::new(chart_w, CHART_H)), s, &pal);
            let heat_left = left + chart_w + SECTION_GAP;
            heatmap(ui, Rect::from_min_size(Pos2::new(heat_left, y), Vec2::new(w - chart_w - SECTION_GAP, CHART_H)), s, &pal);
            y += CHART_H + SECTION_GAP;
        } else {
            tokens_by_day(ui, Rect::from_min_size(Pos2::new(left, y), Vec2::new(w, CHART_H)), s, &pal);
            y += CHART_H + SECTION_GAP;
            heatmap(ui, Rect::from_min_size(Pos2::new(left, y), Vec2::new(w, CHART_H)), s, &pal);
            y += CHART_H + SECTION_GAP;
        }

        // Models | agents.
        if w >= WIDE {
            let col_w = (w - SECTION_GAP) / 2.0;
            models(ui, Rect::from_min_size(Pos2::new(left, y), Vec2::new(col_w, models_height(s))), s, input, &pal);
            agents(ui, Rect::from_min_size(Pos2::new(left + col_w + SECTION_GAP, y), Vec2::new(col_w, agents_height(s))), state, input, &pal);
        } else {
            models(ui, Rect::from_min_size(Pos2::new(left, y), Vec2::new(w, models_height(s))), s, input, &pal);
            y += models_height(s) + SECTION_GAP;
            agents(ui, Rect::from_min_size(Pos2::new(left, y), Vec2::new(w, agents_height(s))), state, input, &pal);
        }
    });
}

fn has_data(s: &UsageStats) -> bool {
    !s.agents.is_empty()
}

fn kpi_rows(w: f32) -> usize {
    if w >= WIDE { 1 } else { 2 }
}

fn shown_models(s: &UsageStats) -> usize {
    s.models.len().min(TOP_MODELS) + usize::from(s.models.len() > TOP_MODELS)
}

fn models_height(s: &UsageStats) -> f32 {
    74.0 + (shown_models(s) + 1) as f32 * ROW_H
}

fn agents_height(s: &UsageStats) -> f32 {
    30.0 + (s.agents.len() + 1) as f32 * ROW_H
}

fn content_height(w: f32, s: &UsageStats) -> f32 {
    let mut h = 2.0 * MARGIN + HEADER_H + GAP;
    if !has_data(s) {
        return h + 160.0;
    }
    h += kpi_rows(w) as f32 * (CARD_H + GAP) - GAP + SECTION_GAP;
    if w >= WIDE {
        h += CHART_H + SECTION_GAP + models_height(s).max(agents_height(s));
    } else {
        h += 2.0 * (CHART_H + SECTION_GAP) + models_height(s) + SECTION_GAP + agents_height(s);
    }
    h
}

fn header(ui: &mut egui::Ui, rect: Rect, state: &mut StatsState, input: &StatsInput<'_>, pal: &Palette) {
    let p = ui.painter();
    p.text(rect.left_top(), Align2::LEFT_TOP, "Agent usage", font(17.0), pal.text);
    let s = input.stats;
    let mut sub = format!("{} – today", format_day(s.first_day));
    if let Some(agent) = &state.agent {
        sub.push_str(&format!("  ·  {agent} only"));
    }
    if input.loading {
        sub.push_str("  ·  loading older logs…");
    }
    p.text(Pos2::new(rect.left(), rect.top() + 24.0), Align2::LEFT_TOP, sub, font(11.5), pal.dim);

    // Range selector (segmented), right-aligned.
    let seg_h = 26.0;
    let widths: Vec<f32> = Range::ALL.iter().map(|r| p.layout_no_wrap(r.label().into(), font(11.5), pal.text).size().x + 22.0).collect();
    let total: f32 = widths.iter().sum::<f32>() + 6.0;
    let bar = Rect::from_min_size(Pos2::new(rect.right() - total, rect.top()), Vec2::new(total, seg_h + 6.0));
    p.rect_filled(bar, CornerRadius::same(8), pal.card);
    let mut x = bar.left() + 3.0;
    for (range, w) in Range::ALL.into_iter().zip(widths) {
        let r = Rect::from_min_size(Pos2::new(x, bar.top() + 3.0), Vec2::new(w, seg_h));
        x += w;
        let resp = ui.interact(r, ui.scope_id().with(("range", range)), Sense::click());
        let selected = state.range == range;
        let p = ui.painter();
        if selected {
            p.rect_filled(r, CornerRadius::same(6), pal.selected);
        } else if resp.hovered() {
            p.rect_stroke(r, CornerRadius::same(6), Stroke::new(1.0, pal.border), StrokeKind::Inside);
        }
        p.text(r.center(), Align2::CENTER_CENTER, range.label(), font(11.5), if selected { pal.text } else { pal.dim });
        if resp.clicked() {
            state.range = range;
        }
    }
}

fn empty(ui: &mut egui::Ui, rect: Rect, state: &mut StatsState, input: &StatsInput<'_>, pal: &Palette) {
    let p = ui.painter();
    let msg = if !input.monitor_enabled {
        "The token monitor is off (Settings → AI Agents & Tokens)"
    } else if input.loading {
        "Reading agent logs…"
    } else if state.agent.is_some() {
        "No usage for this agent in this range"
    } else {
        "No agent usage in this range"
    };
    p.text(rect.center(), Align2::CENTER_CENTER, msg, font(13.0), pal.dim);
    if state.agent.is_some() {
        let button = Rect::from_center_size(rect.center() + Vec2::new(0.0, 30.0), Vec2::new(140.0, 26.0));
        let resp = ui.interact(button, ui.scope_id().with("clear-agent"), Sense::click());
        ui.painter().rect_filled(button, CornerRadius::same(6), if resp.hovered() { pal.selected } else { pal.card });
        ui.painter().text(button.center(), Align2::CENTER_CENTER, "Show all agents", font(11.5), pal.text);
        if resp.clicked() {
            state.agent = None;
        }
    }
}

fn kpi_card(p: &egui::Painter, rect: Rect, label: &str, value: &str, sub: &str, kpi: &Kpi, pal: &Palette) {
    p.rect(rect, CornerRadius::same(8), pal.card, Stroke::new(1.0, pal.border), StrokeKind::Inside);
    let inner = rect.shrink(14.0);
    p.text(inner.left_top(), Align2::LEFT_TOP, label, font(10.5), pal.dim);
    if let Some(trend) = kpi.trend_percent() {
        let text = format!("{}{:.0}%", if trend >= 0.0 { "+" } else { "" }, trend.clamp(-999.0, 999.0));
        let (fill, color) = if trend >= 0.0 { pal.up } else { pal.down };
        let galley = p.layout_no_wrap(text, font(10.5), color);
        let chip = Rect::from_min_size(Pos2::new(inner.right() - galley.size().x - 12.0, inner.top() - 2.0), galley.size() + Vec2::new(12.0, 4.0));
        p.rect_filled(chip, CornerRadius::same(4), fill);
        p.galley(chip.min + Vec2::new(6.0, 2.0), galley, color);
    }
    p.text(Pos2::new(inner.left(), inner.top() + 16.0), Align2::LEFT_TOP, value, font(25.0), pal.text);
    p.text(Pos2::new(inner.left(), inner.top() + 47.0), Align2::LEFT_TOP, sub, font(11.0), pal.dim);
    let spark = Rect::from_min_max(Pos2::new(inner.left(), inner.bottom() - 18.0), inner.right_bottom());
    sparkline(p, spark, &kpi.daily, pal.input_bar);
}

/// Sums `values` into at most `max` buckets, keeping their order.
fn bucket(values: &[u64], max: usize) -> Vec<u64> {
    if values.len() <= max || max == 0 {
        return values.to_vec();
    }
    let size = values.len().div_ceil(max);
    values.chunks(size).map(|c| c.iter().sum()).collect()
}

fn sparkline(p: &egui::Painter, rect: Rect, values: &[u64], color: Color32) {
    let values = bucket(values, (rect.width() / 4.0) as usize);
    let max = values.iter().copied().max().unwrap_or(0).max(1);
    let step = rect.width() / values.len().max(1) as f32;
    let bar_w = (step * 0.7).max(1.0);
    for (i, v) in values.iter().enumerate() {
        let h = if *v == 0 { 1.0 } else { (*v as f32 / max as f32 * rect.height()).max(2.0) };
        let x = rect.left() + i as f32 * step;
        p.rect_filled(Rect::from_min_max(Pos2::new(x, rect.bottom() - h), Pos2::new(x + bar_w, rect.bottom())), CornerRadius::ZERO, color);
    }
}

fn section_title(p: &egui::Painter, rect: Rect, title: &str, right: &str, pal: &Palette) {
    p.text(rect.left_top(), Align2::LEFT_TOP, title, font(10.5), pal.dim);
    p.text(rect.right_top(), Align2::RIGHT_TOP, right, font(10.5), pal.dim);
}

fn tokens_by_day(ui: &mut egui::Ui, rect: Rect, s: &UsageStats, pal: &Palette) {
    let p = ui.painter();
    let per_day = format!("{} of {} days with agents · {}/day", s.active_days, s.days.len(), format_tokens(s.tokens_per_day));
    section_title(p, rect, "TOKENS BY DAY", &per_day, pal);

    let label_w = 52.0;
    let plot = Rect::from_min_max(Pos2::new(rect.left() + label_w, rect.top() + 26.0), Pos2::new(rect.right(), rect.bottom() - 22.0));
    let axis = plot.top() + plot.height() * 0.66;
    let max_in = s.days.iter().map(|d| d.input).max().unwrap_or(0).max(1);
    let max_out = s.days.iter().map(|d| d.output).max().unwrap_or(0).max(1);
    p.text(Pos2::new(rect.left(), plot.top()), Align2::LEFT_TOP, format_tokens(max_in), font(11.5), pal.text);
    p.text(Pos2::new(rect.left(), plot.top() + 15.0), Align2::LEFT_TOP, "input", font(10.0), pal.dim);
    p.text(Pos2::new(rect.left(), axis + 4.0), Align2::LEFT_TOP, format_tokens(max_out), font(11.5), pal.text);
    p.text(Pos2::new(rect.left(), axis + 19.0), Align2::LEFT_TOP, "output", font(10.0), pal.output_bar);

    let n = s.days.len().max(1);
    let step = plot.width() / n as f32;
    let bar_w = (step * 0.72).max(1.0);
    let hover = ui.interact(plot, ui.scope_id().with("tokens-by-day"), Sense::hover());
    let hovered = hover.hover_pos().map(|pos| (((pos.x - plot.left()) / step) as usize).min(n - 1));
    let p = ui.painter();
    for (i, d) in s.days.iter().enumerate() {
        let x = plot.left() + i as f32 * step + (step - bar_w) / 2.0;
        let up = d.input as f32 / max_in as f32 * (axis - plot.top());
        let down = d.output as f32 / max_out as f32 * (plot.bottom() - axis);
        let dim = hovered.is_some_and(|h| h != i);
        let (ci, co) = if dim { (pal.input_bar.gamma_multiply(0.45), pal.output_bar.gamma_multiply(0.45)) } else { (pal.input_bar, pal.output_bar) };
        if d.input > 0 {
            p.rect_filled(Rect::from_min_max(Pos2::new(x, axis - up.max(1.0)), Pos2::new(x + bar_w, axis)), CornerRadius::ZERO, ci);
        }
        if d.output > 0 {
            p.rect_filled(Rect::from_min_max(Pos2::new(x, axis + 1.0), Pos2::new(x + bar_w, axis + 1.0 + down.max(1.0))), CornerRadius::ZERO, co);
        }
    }
    p.line_segment([Pos2::new(plot.left(), axis + 0.5), Pos2::new(plot.right(), axis + 0.5)], Stroke::new(1.0, pal.border));

    // X labels: first, middle, today.
    let y = plot.bottom() + 6.0;
    if let (Some(first), Some(mid)) = (s.days.first(), s.days.get(n / 2)) {
        p.text(Pos2::new(plot.left(), y), Align2::LEFT_TOP, format_day(first.local_day), font(10.5), pal.dim);
        if n >= 5 {
            p.text(Pos2::new(plot.left() + (n / 2) as f32 * step + step / 2.0, y), Align2::CENTER_TOP, format_day(mid.local_day), font(10.5), pal.dim);
        }
    }
    p.text(Pos2::new(plot.right(), y), Align2::RIGHT_TOP, "today", font(10.5), pal.dim);

    if let Some(d) = hovered.and_then(|i| s.days.get(i)) {
        hover.on_hover_text_at_pointer(format!(
            "{}\ninput {}\noutput {}",
            format_day(d.local_day),
            format_tokens(d.input),
            format_tokens(d.output)
        ));
    }
}

fn heatmap(ui: &mut egui::Ui, rect: Rect, s: &UsageStats, pal: &Palette) {
    let peak = s.peak.map(|(d, h)| format!("most active {} {h} h", WEEKDAYS[d])).unwrap_or_default();
    section_title(ui.painter(), rect, "WHEN THEY WORK", &peak, pal);

    let label_w = 16.0;
    let grid_top = rect.top() + 26.0;
    let gap = 2.0;
    let cell = ((rect.width() - label_w) / 24.0 - gap).min((rect.height() - 26.0 - 44.0) / 7.0 - gap).max(4.0);
    let grid = Rect::from_min_size(Pos2::new(rect.left() + label_w, grid_top), Vec2::new(24.0 * (cell + gap), 7.0 * (cell + gap)));
    let max = s.heat.iter().flatten().copied().max().unwrap_or(0).max(1);
    let shade = |v: u64| {
        let t = if v == 0 { 0.0 } else { 0.15 + 0.85 * (v as f32 / max as f32).powf(0.6) };
        c32(pal.heat_low.mix(pal.heat_high, t))
    };
    let hover = ui.interact(grid, ui.scope_id().with("heatmap"), Sense::hover());
    let hovered = hover.hover_pos().map(|pos| {
        let col = (((pos.x - grid.left()) / (cell + gap)) as usize).min(23);
        let row = (((pos.y - grid.top()) / (cell + gap)) as usize).min(6);
        (row, col)
    });
    let p = ui.painter();
    for (d, row) in s.heat.iter().enumerate() {
        let y = grid.top() + d as f32 * (cell + gap);
        p.text(Pos2::new(rect.left(), y + cell / 2.0), Align2::LEFT_CENTER, &WEEKDAYS[d][..1], font(10.0), pal.dim);
        for (h, v) in row.iter().enumerate() {
            let r = Rect::from_min_size(Pos2::new(grid.left() + h as f32 * (cell + gap), y), Vec2::splat(cell));
            p.rect_filled(r, CornerRadius::same(2), shade(*v));
            if hovered == Some((d, h)) {
                p.rect_stroke(r, CornerRadius::same(2), Stroke::new(1.0, pal.text), StrokeKind::Outside);
            }
        }
    }
    for h in [0, 6, 12, 18] {
        p.text(Pos2::new(grid.left() + h as f32 * (cell + gap), grid.bottom() + 5.0), Align2::LEFT_TOP, h.to_string(), font(10.5), pal.dim);
    }
    // Legend.
    let y = grid.bottom() + 26.0;
    let mut x = grid.right();
    let more = p.text(Pos2::new(x, y), Align2::RIGHT_TOP, "more", font(10.0), pal.dim);
    x = more.left() - 6.0;
    for t in [1.0, 0.7, 0.45, 0.2] {
        let r = Rect::from_min_size(Pos2::new(x - 10.0, y + 1.0), Vec2::splat(10.0));
        p.rect_filled(r, CornerRadius::same(2), c32(pal.heat_low.mix(pal.heat_high, t)));
        x -= 13.0;
    }
    p.text(Pos2::new(x - 3.0, y), Align2::RIGHT_TOP, "less", font(10.0), pal.dim);

    if let Some((d, h)) = hovered {
        let turns = s.heat[d][h];
        hover.on_hover_text_at_pointer(format!("{} {h:02}:00 · {} turn{}", WEEKDAYS[d], format_count(turns), if turns == 1 { "" } else { "s" }));
    }
}

/// Right-aligned numeric columns: (text, right edge offset from `rect.right()`).
fn columns(p: &egui::Painter, y: f32, right: f32, cells: &[(&str, f32)], color: Color32) {
    for (text, offset) in cells {
        p.text(Pos2::new(right - offset, y), Align2::RIGHT_CENTER, *text, font(11.5), color);
    }
}

fn models(ui: &mut egui::Ui, rect: Rect, s: &UsageStats, input: &StatsInput<'_>, pal: &Palette) {
    let p = ui.painter();
    let count = s.models.len();
    section_title(p, rect, "MODELS", &format!("{count} model{}", if count == 1 { "" } else { "s" }), pal);

    // Stacked share by provider.
    let total: u64 = s.providers.iter().map(|(_, v)| v).sum::<u64>().max(1);
    let bar = Rect::from_min_size(Pos2::new(rect.left(), rect.top() + 22.0), Vec2::new(rect.width(), 6.0));
    let mut x = bar.left();
    for (i, (provider, v)) in s.providers.iter().enumerate() {
        let w = bar.width() * *v as f32 / total as f32;
        let seg = Rect::from_min_size(Pos2::new(x, bar.top()), Vec2::new((w - 2.0).max(1.5), bar.height()));
        let radius = if i == 0 || i + 1 == s.providers.len() { CornerRadius::same(3) } else { CornerRadius::ZERO };
        p.rect_filled(seg, radius, provider_color(*provider, input.colors));
        x += w;
    }
    let mut x = rect.left();
    let legend_y = bar.bottom() + 12.0;
    for (provider, v) in &s.providers {
        let color = provider_color(*provider, input.colors);
        p.circle_filled(Pos2::new(x + 4.0, legend_y), 3.5, color);
        let pct = *v as f64 / total as f64 * 100.0;
        let pct = if pct < 0.5 { "<1%".to_owned() } else { format!("{pct:.0}%") };
        let r = p.text(Pos2::new(x + 12.0, legend_y), Align2::LEFT_CENTER, format!("{} {pct}", provider.label()), font(11.0), pal.dim);
        x = r.right() + 14.0;
    }

    // Table.
    let right = rect.right();
    let (c_tasks, c_out, c_in) = (0.0, 52.0, 120.0);
    let mut y = rect.top() + 60.0;
    columns(p, y, right, &[("input", c_in), ("output", c_out), ("tasks", c_tasks)], pal.dim);
    let name_w = (rect.width() - 190.0).max(80.0) * 0.55;
    let bar_left = rect.left() + name_w + 8.0;
    let bar_max = (right - c_in - 60.0 - bar_left).max(10.0);
    let max = s.models.first().map(|m| m.input + m.output).unwrap_or(0).max(1);
    let mut rows: Vec<(String, Option<Provider>, u64, u64, u64)> =
        s.models.iter().take(TOP_MODELS).map(|m| (display_model(&m.name), Some(m.provider), m.input, m.output, m.tasks)).collect();
    if count > TOP_MODELS {
        let rest = &s.models[TOP_MODELS..];
        rows.push((
            format!("{} more", rest.len()),
            None,
            rest.iter().map(|m| m.input).sum(),
            rest.iter().map(|m| m.output).sum(),
            rest.iter().map(|m| m.tasks).sum(),
        ));
    }
    for (name, provider, input_t, output_t, tasks) in rows {
        y += ROW_H;
        let color = provider.map_or(pal.dim, |pr| provider_color(pr, input.colors));
        let name_left = if provider.is_some() {
            p.circle_filled(Pos2::new(rect.left() + 4.0, y), 3.5, color);
            rect.left() + 14.0
        } else {
            rect.left() + 14.0
        };
        p.with_clip_rect(Rect::from_min_max(Pos2::new(name_left, y - ROW_H / 2.0), Pos2::new(bar_left - 6.0, y + ROW_H / 2.0)))
            .text(Pos2::new(name_left, y), Align2::LEFT_CENTER, &name, font(12.0), if provider.is_some() { pal.text } else { pal.dim });
        let w = (bar_max * (input_t + output_t) as f32 / max as f32).max(2.0);
        p.rect_filled(Rect::from_min_size(Pos2::new(bar_left, y - 2.5), Vec2::new(w.min(bar_max), 5.0)), CornerRadius::same(2), color);
        columns(p, y, right, &[(&format_tokens(input_t), c_in), (&format_tokens(output_t), c_out), (&format_count(tasks), c_tasks)], pal.text);
    }
}

/// `claude-opus-5-5` → `Opus 5.5`, `claude-sonnet-4-5-20250929` → `Sonnet 4.5`.
/// Other ids are shown as logged.
fn display_model(id: &str) -> String {
    let Some(rest) = id.strip_prefix("claude-") else { return id.to_owned() };
    let mut parts = rest.split('-').filter(|p| !(p.len() == 8 && p.bytes().all(|b| b.is_ascii_digit())));
    let Some(family) = parts.next().filter(|f| f.chars().all(|c| c.is_ascii_alphabetic())) else { return id.to_owned() };
    let version: Vec<&str> = parts.collect();
    if version.is_empty() || !version.iter().all(|v| v.bytes().all(|b| b.is_ascii_digit())) {
        return id.to_owned();
    }
    let mut name = family[..1].to_ascii_uppercase() + &family[1..];
    name.push(' ');
    name.push_str(&version.join("."));
    name
}

fn agent_look(name: &str, input: &StatsInput<'_>) -> (String, Color32) {
    if let Some(a) = input.agents.iter().find(|a| a.name == name) {
        return (a.icon.clone(), a.color.map(c32).unwrap_or_else(|| c32(input.colors.text_dim)));
    }
    let lower = name.to_lowercase();
    let icon = ["claude", "codex", "gemini"].into_iter().find(|k| lower.contains(k)).unwrap_or("shell");
    (icon.to_owned(), c32(input.colors.text_dim))
}

fn agents(ui: &mut egui::Ui, rect: Rect, state: &mut StatsState, input: &StatsInput<'_>, pal: &Palette) {
    let s = input.stats;
    section_title(ui.painter(), rect, "AGENTS", "click one to see only its usage", pal);
    let right = rect.right();
    let (c_time, c_out, c_in) = (0.0, 64.0, 132.0);
    let mut y = rect.top() + 30.0;
    columns(ui.painter(), y, right, &[("input", c_in), ("output", c_out), ("time", c_time)], pal.dim);
    let name_w = (rect.width() - 200.0).max(90.0) * 0.5;
    let bar_left = rect.left() + name_w + 8.0;
    let bar_max = (right - c_in - 60.0 - bar_left).max(10.0);
    let max = s.agents.first().map(|a| a.input + a.output).unwrap_or(0).max(1);
    for a in &s.agents {
        y += ROW_H;
        let row = Rect::from_min_max(Pos2::new(rect.left() - 6.0, y - ROW_H / 2.0 + 1.0), Pos2::new(right + 6.0, y + ROW_H / 2.0 - 1.0));
        let resp = ui.interact(row, ui.scope_id().with(("agent", &a.name)), Sense::click());
        let selected = state.agent.as_deref() == Some(a.name.as_str());
        let faded = state.agent.is_some() && !selected;
        let p = ui.painter();
        if selected {
            p.rect_filled(row, CornerRadius::same(5), pal.selected);
        } else if resp.hovered() {
            p.rect_filled(row, CornerRadius::same(5), pal.card);
        }
        let (icon, color) = agent_look(&a.name, input);
        let text = if faded { pal.dim } else { pal.text };
        paint_icon(p, Pos2::new(rect.left() + 6.0, y), 6.0, &icon, if faded { pal.dim } else { color });
        p.with_clip_rect(Rect::from_min_max(Pos2::new(rect.left(), y - ROW_H / 2.0), Pos2::new(bar_left - 6.0, y + ROW_H / 2.0)))
            .text(Pos2::new(rect.left() + 18.0, y), Align2::LEFT_CENTER, &a.name, font(12.0), text);
        let w = bar_max * (a.input + a.output) as f32 / max as f32;
        if w >= 3.0 {
            p.rect_filled(Rect::from_min_size(Pos2::new(bar_left, y - 2.5), Vec2::new(w, 5.0)), CornerRadius::same(2), pal.input_bar);
        } else {
            p.circle_filled(Pos2::new(bar_left + 2.5, y), 2.5, pal.input_bar);
        }
        let dash = |v: u64| if v == 0 { "—".to_owned() } else { format_tokens(v) };
        columns(p, y, right, &[(&dash(a.input), c_in), (&dash(a.output), c_out), (&format_hours(a.active_ms), c_time)], text);
        if resp.clicked() {
            state.agent = if selected { None } else { Some(a.name.clone()) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_keep_totals() {
        assert_eq!(bucket(&[1, 2, 3], 10), vec![1, 2, 3]);
        let v: Vec<u64> = (1..=10).collect();
        let b = bucket(&v, 4);
        assert!(b.len() <= 4);
        assert_eq!(b.iter().sum::<u64>(), 55);
        assert_eq!(bucket(&v, 0), v);
    }

    #[test]
    fn model_display_names() {
        assert_eq!(display_model("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(display_model("claude-sonnet-4-5-20250929"), "Sonnet 4.5");
        assert_eq!(display_model("claude-fable-5-2"), "Fable 5.2");
        assert_eq!(display_model("gpt-6.2-astra"), "gpt-6.2-astra");
        assert_eq!(display_model("claude-3-5-sonnet-20241022"), "claude-3-5-sonnet-20241022");
        assert_eq!(display_model("claude-custom"), "claude-custom");
    }
}
