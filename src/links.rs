//! Clickable links: OSC 8 hyperlinks, URLs and `path:line:col` in the text.
//! Detection reuses alacritty's regex search (it follows wrapped lines and
//! wide characters); a path only counts when it exists on disk.

use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::process::Command;

use alacritty_terminal::index::{Boundary, Direction, Point};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::search::{RegexIter, RegexSearch};

use crate::config::expand_tilde;

/// No trailing punctuation, so "see https://x.com/a." stops before the dot.
const URL: &str = r#"(?:https?://|mailto:|file:)[^\x00-\x1F\x7F-\x9F<>"\s{}^`\\]*[^\x00-\x1F\x7F-\x9F<>"\s{}^`\\.,;:!?')\]]"#;
const PATH: &str = r"(?:~|\.{1,2})?/?(?:[\w.@+-]+/)*[\w.@+-]*[\w@+-](?::\d+){0,2}";

/// Files the system opener would run instead of show.
const EXECUTABLE_EXTENSIONS: &[&str] = &[
    "app", "command", "tool", "sh", "zsh", "bash", "fish", "scpt", "applescript", "workflow", "terminal", "jar", "pkg",
    "mpkg", "exe", "bat", "cmd", "com", "ps1", "msi", "lnk", "vbs", "desktop", "appimage", "run",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Url(String),
    File { path: PathBuf, line: Option<u32>, col: Option<u32> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// First and last cell (inclusive), in grid coordinates.
    pub start: Point,
    pub end: Point,
    pub target: Target,
    /// Set by the program (OSC 8): the text may not show where it leads.
    pub uri: Option<String>,
}

pub struct LinkDetector {
    url: RegexSearch,
    path: RegexSearch,
}

impl LinkDetector {
    pub fn new() -> Self {
        Self {
            url: RegexSearch::new(URL).expect("URL regex is valid"),
            path: RegexSearch::new(PATH).expect("path regex is valid"),
        }
    }

    /// The link under `point`, with relative paths resolved against `cwd`.
    pub fn link_at<T>(&mut self, term: &Term<T>, point: Point, cwd: &Path) -> Option<Link> {
        if let Some(link) = hyperlink_at(term, point, cwd) {
            return Some(link);
        }
        if let Some(m) = match_at(term, &mut self.url, point) {
            let target = parse_uri(&term.bounds_to_string(*m.start(), *m.end()), cwd)?;
            return Some(Link { start: *m.start(), end: *m.end(), target, uri: None });
        }
        let m = match_at(term, &mut self.path, point)?;
        let target = parse_path(&term.bounds_to_string(*m.start(), *m.end()), cwd)?;
        Some(Link { start: *m.start(), end: *m.end(), target, uri: None })
    }
}

fn match_at<T>(term: &Term<T>, regex: &mut RegexSearch, point: Point) -> Option<RangeInclusive<Point>> {
    let (start, end) = (term.line_search_left(point), term.line_search_right(point));
    RegexIter::new(start, end, Direction::Right, term, regex)
        .take_while(|m| *m.start() <= point)
        .find(|m| m.contains(&point))
}

/// An OSC 8 link: the run of cells around `point` carrying the same link.
/// termy's own prompt marks (`termy:` URIs) aren't links.
fn hyperlink_at<T>(term: &Term<T>, point: Point, cwd: &Path) -> Option<Link> {
    let grid = term.grid();
    let link = grid[point].hyperlink().filter(|l| !l.uri().starts_with("termy:"))?;
    let target = parse_uri(link.uri(), cwd)?;
    let (first, last) = (term.line_search_left(point), term.line_search_right(point));
    let (mut start, mut end) = (point, point);
    while start > first {
        let prev = start.sub(grid, Boundary::None, 1);
        if grid[prev].hyperlink().as_ref() != Some(&link) {
            break;
        }
        start = prev;
    }
    while end < last {
        let next = end.add(grid, Boundary::None, 1);
        if grid[next].hyperlink().as_ref() != Some(&link) {
            break;
        }
        end = next;
    }
    Some(Link { start, end, target, uri: Some(link.uri().to_owned()) })
}

fn parse_uri(uri: &str, cwd: &Path) -> Option<Target> {
    if uri.starts_with("file:") {
        let path = crate::osc::parse_file_url(uri)?;
        return parse_path(&path.to_string_lossy(), cwd);
    }
    (uri.starts_with("http://") || uri.starts_with("https://") || uri.starts_with("mailto:"))
        .then(|| Target::Url(uri.to_owned()))
}

/// `src/app.rs:12:5` → the file, line 12, column 5, if the file exists.
fn parse_path(text: &str, cwd: &Path) -> Option<Target> {
    let mut path = text;
    let mut numbers = Vec::new();
    while numbers.len() < 2
        && let Some((rest, n)) = path.rsplit_once(':')
        && let Ok(n) = n.parse::<u32>()
    {
        numbers.insert(0, n);
        path = rest;
    }
    let path = expand_tilde(path);
    let path = if path.is_absolute() { path } else { cwd.join(path) };
    path.exists().then(|| Target::File { path, line: numbers.first().copied(), col: numbers.get(1).copied() })
}

fn is_executable(path: &Path) -> bool {
    let by_extension = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXECUTABLE_EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = path.metadata().ok().filter(|m| m.is_file()).map(|m| m.permissions().mode());
        by_extension || mode.is_some_and(|m| m & 0o111 != 0)
    }
    #[cfg(not(unix))]
    by_extension
}

/// Command that opens `target`: files go to `editor` (`code -g {file}:{line}:{col}`)
/// when set, everything else to the system opener.
pub fn open_command(target: &Target, editor: Option<&str>) -> Result<(String, Vec<String>), String> {
    let (what, file) = match target {
        Target::Url(url) => (url.clone(), None),
        Target::File { path, line, col } => (path.to_string_lossy().into_owned(), Some((line, col))),
    };
    if let (Some((line, col)), Some(editor)) = (file, editor.filter(|e| !e.trim().is_empty())) {
        let (line, col) = (line.unwrap_or(1).to_string(), col.unwrap_or(1).to_string());
        let mut parts = editor.split_whitespace();
        let program = parts.next().expect("editor is not blank").to_owned();
        let mut args: Vec<String> =
            parts.map(|a| a.replace("{file}", &what).replace("{line}", &line).replace("{col}", &col)).collect();
        if !editor.contains("{file}") {
            args.push(what);
        }
        return Ok((program, args));
    }
    if let Target::File { path, .. } = target
        && is_executable(path)
    {
        return Err(format!("Not opening {}: it is an executable", path.display()));
    }
    Ok(if cfg!(target_os = "macos") {
        ("open".into(), vec![what])
    } else if cfg!(windows) {
        ("explorer".into(), vec![what])
    } else {
        ("xdg-open".into(), vec![what])
    })
}

/// Runs `open_command` without blocking; editors go through the login shell
/// so they're found on the user's `PATH`.
pub fn open(target: &Target, editor: Option<&str>) -> Result<(), String> {
    let (program, args) = open_command(target, editor)?;
    let (program, args) = match (target, editor) {
        (Target::File { .. }, Some(_)) => crate::session::login_shell_command(&program, &args).unwrap_or((program, args)),
        _ => (program, args),
    };
    let mut child = Command::new(&program).args(&args).spawn().map_err(|e| format!("Could not run {program}: {e}"))?;
    // Reap it so it doesn't linger as a zombie.
    std::thread::spawn(move || child.wait());
    Ok(())
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::Processor;

    use super::*;

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

    fn term(cols: usize, text: &str) -> Term<VoidListener> {
        let mut term = Term::new(Config::default(), &Size(cols, 4), VoidListener);
        Processor::<alacritty_terminal::vte::ansi::StdSyncHandler>::new().advance(&mut term, text.as_bytes());
        term
    }

    fn at(line: i32, col: usize) -> Point {
        Point::new(Line(line), Column(col))
    }

    fn link(text: &str, point: Point, cwd: &Path) -> Option<Link> {
        LinkDetector::new().link_at(&term(40, text), point, cwd)
    }

    #[test]
    fn urls_stop_before_trailing_punctuation() {
        let l = link("see https://example.com/a?b=1. ok", at(0, 10), Path::new("/")).unwrap();
        assert_eq!(l.target, Target::Url("https://example.com/a?b=1".into()));
        assert_eq!((l.start, l.end), (at(0, 4), at(0, 28)));
        assert!(link("(https://x.org)", at(0, 5), Path::new("/")).is_some_and(|l| l.end == at(0, 13)));
        assert_eq!(link("see https://x.org now", at(0, 1), Path::new("/")), None);
    }

    #[test]
    fn urls_follow_wrapped_lines() {
        let l = LinkDetector::new().link_at(&term(10, "https://example.com/x"), at(1, 3), Path::new("/")).unwrap();
        assert_eq!(l.target, Target::Url("https://example.com/x".into()));
        assert_eq!((l.start, l.end), (at(0, 0), at(2, 0)));
    }

    #[test]
    fn existing_paths_with_line_and_column() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/app.rs"), "").unwrap();
        let l = link("error at src/app.rs:12:5: oops", at(0, 12), dir.path()).unwrap();
        assert_eq!(l.target, Target::File { path: dir.path().join("src/app.rs"), line: Some(12), col: Some(5) });
        assert_eq!((l.start, l.end), (at(0, 9), at(0, 23)));
        // Words that aren't files on disk are left alone.
        assert_eq!(link("error at src/app.rs:12:5", at(0, 2), dir.path()), None);
        assert_eq!(link("see src/missing.rs", at(0, 6), dir.path()), None);
    }

    #[test]
    fn osc8_links_and_prompt_marks() {
        let t = term(40, "\x1b]8;;https://example.com\x07click me\x1b]8;;\x07 after");
        let l = LinkDetector::new().link_at(&t, at(0, 2), Path::new("/")).unwrap();
        assert_eq!(l.target, Target::Url("https://example.com".into()));
        assert_eq!((l.start, l.end, l.uri.as_deref()), (at(0, 0), at(0, 7), Some("https://example.com")));
        assert_eq!(LinkDetector::new().link_at(&t, at(0, 10), Path::new("/")), None);
        let t = term(40, "\x1b]8;id=1;termy:prompt\x07$ \x1b]8;;\x07");
        assert_eq!(LinkDetector::new().link_at(&t, at(0, 0), Path::new("/")), None);
        // Only web and mail schemes open.
        let t = term(40, "\x1b]8;;javascript:alert(1)\x07x\x1b]8;;\x07");
        assert_eq!(LinkDetector::new().link_at(&t, at(0, 0), Path::new("/")), None);
    }

    #[test]
    fn executables_need_an_editor() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("run.command");
        std::fs::write(&script, "").unwrap();
        let target = Target::File { path: script.clone(), line: None, col: None };
        assert!(open_command(&target, None).is_err());
        let (program, args) = open_command(&target, Some("code -g {file}:{line}:{col}")).unwrap();
        assert_eq!((program.as_str(), args), ("code", vec!["-g".into(), format!("{}:1:1", script.display())]));
        let (_, args) = open_command(&target, Some("subl")).unwrap();
        assert_eq!(args, [script.to_string_lossy()]);
    }
}
