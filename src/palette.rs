//! Command palette: every action, new agent sessions, open and archived
//! sessions and themes, filtered with a fuzzy query. Recently run commands
//! lead the list while the query is empty.

use std::cmp::Reverse;

use egui::{Align2, Color32, CornerRadius, FontId, Id, Key, Modal, Modifiers, Pos2, Sense, Vec2};

use crate::chrome::c32;
use crate::config::Action;
use crate::session::SessionId;
use crate::theme::UiColors;

const MAX_RECENT: usize = 20;
const ROW_HEIGHT: f32 = 26.0;
const VISIBLE_ROWS: f32 = 12.0;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Action(Action),
    /// New session with this agent profile (`None` = shell).
    NewSession(Option<String>),
    Focus(SessionId),
    /// Index into the archive.
    Restore(usize),
    Theme(String),
}

pub struct Item {
    pub command: Command,
    pub label: String,
    /// Shortcut, project, "current"… drawn on the right.
    pub hint: String,
}

#[derive(Default)]
pub struct PaletteState {
    pub open: bool,
    query: String,
    selected: usize,
    /// Most recent first; only commands that mean the same thing later.
    recent: Vec<Command>,
}

impl PaletteState {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        self.query.clear();
        self.selected = 0;
    }

    fn remember(&mut self, command: &Command) {
        if matches!(command, Command::Focus(_) | Command::Restore(_)) {
            return;
        }
        self.recent.retain(|c| c != command);
        self.recent.insert(0, command.clone());
        self.recent.truncate(MAX_RECENT);
    }
}

/// Fuzzy match: every query character appears in order (case-insensitive).
/// Higher is better: runs of consecutive characters and word starts score,
/// skipped characters cost.
fn score(query: &str, text: &str) -> Option<i32> {
    let text: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let (mut score, mut pos, mut prev) = (0, 0, None);
    for q in query.chars().flat_map(char::to_lowercase).filter(|c| !c.is_whitespace()) {
        let i = pos + text[pos..].iter().position(|&c| c == q)?;
        score += if prev.is_some_and(|p: usize| p + 1 == i) {
            8
        } else if i == 0 || !text[i - 1].is_alphanumeric() {
            6
        } else {
            1
        };
        score -= (i - pos).min(4) as i32;
        prev = Some(i);
        pos = i + 1;
    }
    Some(score)
}

/// Items matching `query`, best first; ties go to the most recent, then to
/// the original order.
fn rank<'a>(items: &'a [Item], query: &str, recent: &[Command]) -> Vec<&'a Item> {
    let mut ranked: Vec<_> = items
        .iter()
        .enumerate()
        .filter_map(|(i, item)| {
            let recency = recent.iter().position(|c| *c == item.command).unwrap_or(usize::MAX);
            Some((Reverse(score(query, &item.label)?), recency, i, item))
        })
        .collect();
    ranked.sort_by_key(|&(s, r, i, _)| (s, r, i));
    ranked.into_iter().map(|t| t.3).collect()
}

/// Draws the palette; returns the command picked this frame.
pub fn show(ctx: &egui::Context, state: &mut PaletteState, items: &[Item], colors: &UiColors) -> Option<Command> {
    // Arrows and Enter drive the list, not the text field.
    let (up, down, enter) = ctx.input_mut(|i| {
        (
            i.consume_key(Modifiers::NONE, Key::ArrowUp),
            i.consume_key(Modifiers::NONE, Key::ArrowDown),
            i.consume_key(Modifiers::NONE, Key::Enter),
        )
    });
    let id = Id::unique("command-palette");
    let area = Modal::default_area(id).anchor(Align2::CENTER_TOP, Vec2::new(0.0, 60.0));
    let frame = egui::Frame::popup(&ctx.global_style()).fill(c32(colors.chrome_background)).inner_margin(8);
    let mut picked = None;
    let modal = Modal::new(id).area(area).frame(frame).backdrop_color(Color32::from_black_alpha(60)).show(ctx, |ui| {
        ui.set_width(520.0_f32.min(ctx.content_rect().width() - 40.0));
        let edit = egui::TextEdit::singleline(&mut state.query)
            .hint_text("Type a command")
            .font(FontId::proportional(14.0))
            .desired_width(f32::INFINITY);
        let field = ui.add(edit);
        field.request_focus();
        if field.changed() {
            state.selected = 0;
        }
        let ranked = rank(items, &state.query, &state.recent);
        let last = ranked.len().saturating_sub(1);
        state.selected = if down { state.selected + 1 } else { state.selected.saturating_sub(up as usize) }.min(last);
        if enter {
            picked = ranked.get(state.selected).map(|item| item.command.clone());
        }
        ui.add_space(6.0);
        if ranked.is_empty() {
            ui.label(egui::RichText::new("No matching commands").color(c32(colors.text_dim)));
            return;
        }
        egui::ScrollArea::vertical().max_height(ROW_HEIGHT * VISIBLE_ROWS).show(ui, |ui| {
            for (i, item) in ranked.iter().enumerate() {
                let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), ROW_HEIGHT), Sense::click());
                let p = ui.painter();
                if i == state.selected {
                    p.rect_filled(rect, CornerRadius::same(6), c32(colors.pill_selected_background));
                    if up || down {
                        resp.scroll_to_me(None);
                    }
                } else if resp.hovered() {
                    p.rect_filled(rect, CornerRadius::same(6), c32(colors.button_background));
                }
                let hint = p.text(Pos2::new(rect.right() - 8.0, rect.center().y), Align2::RIGHT_CENTER, &item.hint, FontId::proportional(11.0), c32(colors.text_dim));
                let label_clip = rect.with_max_x(hint.left() - 8.0);
                p.with_clip_rect(label_clip).text(Pos2::new(rect.left() + 8.0, rect.center().y), Align2::LEFT_CENTER, &item.label, FontId::proportional(13.0), c32(colors.text_primary));
                if resp.clicked() {
                    picked = Some(item.command.clone());
                }
            }
        });
    });
    if picked.is_some() || modal.should_close() {
        state.open = false;
    }
    if let Some(command) = &picked {
        state.remember(command);
    }
    picked
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(label: &str, command: Command) -> Item {
        Item { command, label: label.into(), hint: String::new() }
    }

    #[test]
    fn fuzzy_score_needs_every_character_in_order() {
        assert!(score("spl", "Split vertically").is_some());
        assert!(score("SPLIT V", "Split vertically").is_some());
        assert!(score("vs", "Split vertically").is_none());
        assert_eq!(score("", "anything"), Some(0));
        // Consecutive and word-start matches beat scattered ones.
        assert!(score("sv", "Split vertically") > score("sv", "Close a saved view"));
        assert!(score("font", "Increase font size") > score("font", "Focus next tab"));
    }

    #[test]
    fn ranking_puts_best_matches_then_recent_commands_first() {
        let items = [
            item("New session", Command::Action(Action::NewSession)),
            item("Split vertically", Command::Action(Action::SplitVertical)),
            item("Theme: Dracula", Command::Theme("dracula".into())),
        ];
        let labels = |ranked: Vec<&Item>| ranked.iter().map(|i| i.label.clone()).collect::<Vec<_>>();
        assert_eq!(labels(rank(&items, "drac", &[])), ["Theme: Dracula"]);

        let mut state = PaletteState::default();
        state.remember(&Command::Theme("dracula".into()));
        state.remember(&Command::Focus(3));
        assert_eq!(state.recent, [Command::Theme("dracula".into())]);
        assert_eq!(labels(rank(&items, "", &state.recent)), ["Theme: Dracula", "New session", "Split vertically"]);
        assert_eq!(labels(rank(&items, "split", &state.recent)), ["Split vertically"]);
    }
}
