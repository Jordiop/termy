//! Shell-integration prompt marks (OSC 133).
//!
//! `alacritty_terminal` has no stable line numbers (once the scrollback is
//! full, lines drop off the top with no counter to tell), so marks can't be
//! kept as line indices. Instead, while a prompt or command line is being
//! printed, the cursor template carries an internal hyperlink (`termy:prompt`
//! / `termy:command`, id = prompt number): the marks live in the cells
//! themselves and move with them through scrolling, reflow and trimming.

use std::collections::BTreeMap;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Row, Scroll};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Hyperlink};
use alacritty_terminal::term::{Term, TermMode};

use crate::osc::OscEvent;

const PROMPT_URI: &str = "termy:prompt";
const COMMAND_URI: &str = "termy:command";
/// Exit codes kept (one per command); older ones lost their lines long ago.
const MAX_EXITS: usize = 1024;

#[derive(Default)]
pub struct PromptMarks {
    /// Number of the current prompt (1-based; 0 = none yet).
    seq: u64,
    exits: BTreeMap<u64, i32>,
}

impl PromptMarks {
    pub fn apply<T>(&mut self, term: &mut Term<T>, event: &OscEvent) {
        // Full-screen apps on the alternate screen have no prompts.
        if term.mode().contains(TermMode::ALT_SCREEN) {
            return;
        }
        let uri = match event {
            OscEvent::PromptStart => {
                self.seq += 1;
                Some(PROMPT_URI)
            }
            OscEvent::CommandStart => Some(COMMAND_URI),
            OscEvent::OutputStart => None,
            OscEvent::CommandEnd(code) => {
                if let Some(code) = code {
                    self.exits.insert(self.seq, *code);
                    if self.exits.len() > MAX_EXITS {
                        self.exits.pop_first();
                    }
                }
                None
            }
            OscEvent::Cwd(_) | OscEvent::Notify(_) => return,
        };
        let link = uri.map(|uri| Hyperlink::new(Some(self.seq), uri.to_owned()));
        term.grid_mut().cursor.template.set_hyperlink(link);
    }

    /// Exit code of the command run from prompt `seq`, once it finished.
    pub fn exit_code(&self, seq: u64) -> Option<i32> {
        self.exits.get(&seq).copied()
    }
}

/// The prompt number a cell was tagged with, and whether it is part of the
/// prompt itself or the command line typed after it.
fn cell_mark(cell: &Cell) -> Option<(u64, bool)> {
    let link = cell.hyperlink()?;
    let is_prompt = match link.uri() {
        PROMPT_URI => true,
        COMMAND_URI => false,
        _ => return None,
    };
    Some((link.id().parse().ok()?, is_prompt))
}

/// Prompt number of the first prompt cell in `row`.
pub fn row_prompt(row: &Row<Cell>) -> Option<u64> {
    row[..].iter().find_map(|c| cell_mark(c).filter(|m| m.1).map(|m| m.0))
}

fn row_has(row: &Row<Cell>, seq: u64) -> bool {
    row[..].iter().any(|c| cell_mark(c).is_some_and(|m| m.0 == seq))
}

/// First line of each prompt in the scrollback and screen, top to bottom.
fn prompt_starts<T>(term: &Term<T>) -> Vec<(Line, u64)> {
    let grid = term.grid();
    let mut starts = Vec::new();
    let mut above = None;
    for l in grid.topmost_line().0..=grid.bottommost_line().0 {
        let seq = row_prompt(&grid[Line(l)]);
        if let Some(s) = seq.filter(|_| seq != above) {
            starts.push((Line(l), s));
        }
        above = seq;
    }
    starts
}

/// Scrolls so the previous (`up`) or next prompt is at the top of the view.
/// Returns whether the view moved.
pub fn jump<T: EventListener>(term: &mut Term<T>, up: bool) -> bool {
    let offset = term.grid().display_offset() as i32;
    let top = Line(-offset);
    let starts = prompt_starts(term);
    let target = if up {
        starts.iter().rev().find(|(l, _)| *l < top)
    } else {
        starts.iter().find(|(l, _)| *l > top)
    };
    // Prompts on the bottom screen can't reach the top: that's offset 0.
    match target.map(|(line, _)| (-line.0).max(0)) {
        Some(new_offset) if new_offset != offset => {
            term.scroll_display(Scroll::Delta(new_offset - offset));
            true
        }
        _ if !up && offset != 0 => {
            term.scroll_display(Scroll::Bottom);
            true
        }
        _ => false,
    }
}

/// Output of the last finished command: the lines after its prompt and
/// command line, up to the next prompt.
pub fn last_output<T>(term: &Term<T>) -> Option<String> {
    let starts = prompt_starts(term);
    let [.., (prev_line, prev_seq), (next_line, _)] = starts[..] else { return None };
    let grid = term.grid();
    let last_tagged = (prev_line.0..next_line.0).rev().find(|&l| row_has(&grid[Line(l)], prev_seq))?;
    let (first, last) = (Line(last_tagged + 1), Line(next_line.0 - 1));
    if first > last {
        return None;
    }
    let text = term.bounds_to_string(Point::new(first, Column(0)), Point::new(last, grid.last_column()));
    let text = text.trim_end();
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::Processor;

    use super::*;
    use crate::osc::OscScanner;

    struct Size(usize, usize);

    impl Dimensions for Size {
        fn total_lines(&self) -> usize {
            self.1
        }
        fn screen_lines(&self) -> usize {
            self.1
        }
        fn columns(&self) -> usize {
            self.0
        }
    }

    struct Shell {
        term: Term<VoidListener>,
        parser: Processor,
        scanner: OscScanner,
        marks: PromptMarks,
    }

    impl Shell {
        fn new(rows: usize, scrollback: usize) -> Self {
            let config = Config { scrolling_history: scrollback, ..Config::default() };
            Self {
                term: Term::new(config, &Size(20, rows), VoidListener),
                parser: Processor::new(),
                scanner: OscScanner::default(),
                marks: PromptMarks::default(),
            }
        }

        fn feed(&mut self, bytes: &[u8]) {
            let mut start = 0;
            for (end, event) in self.scanner.feed(bytes) {
                self.parser.advance(&mut self.term, &bytes[start..end]);
                self.marks.apply(&mut self.term, &event);
                start = end;
            }
            self.parser.advance(&mut self.term, &bytes[start..]);
        }

        /// One prompt, a command, its output and exit code (what zsh sends).
        fn run(&mut self, command: &str, output: &str, code: i32) {
            let out = output.replace('\n', "\r\n");
            self.feed(format!("\x1b]133;A\x07$ \x1b]133;B\x07{command}\r\n\x1b]133;C\x07{out}\x1b]133;D;{code}\x07").as_bytes());
        }

        fn prompt(&mut self) {
            self.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        }

        fn top_text(&self) -> String {
            let grid = self.term.grid();
            let line = Line(-(grid.display_offset() as i32));
            grid[line][..].iter().map(|c| c.c).collect::<String>().trim_end().to_owned()
        }
    }

    #[test]
    fn prompts_are_found_and_exit_codes_kept() {
        let mut sh = Shell::new(10, 100);
        sh.run("true", "", 0);
        sh.run("false", "oops\n", 1);
        sh.prompt();
        let starts = prompt_starts(&sh.term);
        assert_eq!(starts.iter().map(|s| s.1).collect::<Vec<_>>(), [1, 2, 3]);
        assert_eq!(sh.marks.exit_code(1), Some(0));
        assert_eq!(sh.marks.exit_code(2), Some(1));
        assert_eq!(sh.marks.exit_code(3), None);
    }

    #[test]
    fn last_output_is_between_the_command_and_the_next_prompt() {
        let mut sh = Shell::new(10, 100);
        sh.run("ls", "a.txt\nb.txt\n", 0);
        sh.prompt();
        assert_eq!(last_output(&sh.term).as_deref(), Some("a.txt\nb.txt"));
        // A command with no output.
        sh.feed(b"true\r\n\x1b]133;C\x07\x1b]133;D;0\x07");
        sh.prompt();
        assert_eq!(last_output(&sh.term), None);
    }

    #[test]
    fn jumps_survive_a_full_scrollback() {
        let mut sh = Shell::new(5, 20);
        for i in 0..30 {
            sh.run(&format!("echo {i}"), &format!("{i}\n{i}\n"), 0);
        }
        sh.prompt();
        // History is full and has dropped lines: marks still line up. The view
        // shows "$ echo 29", so the first jump goes to the prompt above it.
        assert!(jump(&mut sh.term, true));
        assert_eq!(sh.top_text(), "$ echo 28");
        assert!(jump(&mut sh.term, true));
        assert_eq!(sh.top_text(), "$ echo 27");
        assert!(jump(&mut sh.term, false));
        assert_eq!(sh.top_text(), "$ echo 28");
        assert!(jump(&mut sh.term, false));
        assert_eq!(sh.term.grid().display_offset(), 0);
        assert!(!jump(&mut sh.term, false));
    }

    #[test]
    fn plain_output_is_never_tagged() {
        let mut sh = Shell::new(5, 20);
        sh.feed(b"no integration here\r\n");
        assert!(prompt_starts(&sh.term).is_empty());
        assert!(!jump(&mut sh.term, true));
        assert_eq!(last_output(&sh.term), None);
    }
}
