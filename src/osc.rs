//! Picks out the OSC sequences `vte` drops (133 prompt marks, 7 working
//! directory, 9 / 777 notifications) from raw PTY output. It only reads the bytes: `vte` still parses
//! all of them, so the caller feeds the parser up to each event's offset and
//! then applies the event, at the exact point in the stream where it occurred.

use std::path::PathBuf;

/// OSC payloads longer than this (OSC 52 clipboard data, …) are skipped.
const MAX_PAYLOAD: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OscEvent {
    /// `133;A`: the prompt starts.
    PromptStart,
    /// `133;B`: the prompt ends and the command line starts.
    CommandStart,
    /// `133;C`: the command runs; its output starts.
    OutputStart,
    /// `133;D[;exit]`: the command finished.
    CommandEnd(Option<i32>),
    /// `7;file://host/path`: the shell's working directory.
    Cwd(PathBuf),
    /// `9;message` (iTerm2) or `777;notify;title;body`: the program wants the user.
    Notify(String),
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    Osc,
    /// ESC seen inside an OSC: `\` completes the ST terminator.
    OscEscape,
}

#[derive(Default)]
pub struct OscScanner {
    state: State,
    payload: Vec<u8>,
    overflow: bool,
}

impl OscScanner {
    /// Scans `bytes` and returns each event with the offset just past its
    /// terminator. State carries over, so sequences may span calls.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<(usize, OscEvent)> {
        let mut events = Vec::new();
        for (i, &b) in bytes.iter().enumerate() {
            self.state = match (self.state, b) {
                (State::Ground, 0x1b) => State::Escape,
                (State::Ground, _) => State::Ground,
                (State::Escape, b']') => {
                    self.payload.clear();
                    self.overflow = false;
                    State::Osc
                }
                (State::Escape, 0x1b) => State::Escape,
                (State::Escape, _) => State::Ground,
                (State::Osc, 0x07) => {
                    events.extend(self.finish().map(|e| (i + 1, e)));
                    State::Ground
                }
                (State::Osc, 0x1b) => State::OscEscape,
                // CAN / SUB abort the sequence.
                (State::Osc, 0x18 | 0x1a) => State::Ground,
                (State::Osc, _) => {
                    if self.payload.len() < MAX_PAYLOAD {
                        self.payload.push(b);
                    } else {
                        self.overflow = true;
                    }
                    State::Osc
                }
                (State::OscEscape, b'\\') => {
                    events.extend(self.finish().map(|e| (i + 1, e)));
                    State::Ground
                }
                // Any other ESC ends the OSC (as in vte) and starts a new escape.
                (State::OscEscape, b']') => {
                    self.payload.clear();
                    self.overflow = false;
                    State::Osc
                }
                (State::OscEscape, _) => State::Ground,
            };
        }
        events
    }

    fn finish(&mut self) -> Option<OscEvent> {
        if self.overflow {
            return None;
        }
        parse(&self.payload)
    }
}

fn parse(payload: &[u8]) -> Option<OscEvent> {
    let text = std::str::from_utf8(payload).ok()?;
    let (code, rest) = text.split_once(';').unwrap_or((text, ""));
    match code {
        "133" => {
            let mut fields = rest.split(';');
            match fields.next()? {
                "A" => Some(OscEvent::PromptStart),
                "B" => Some(OscEvent::CommandStart),
                "C" => Some(OscEvent::OutputStart),
                "D" => Some(OscEvent::CommandEnd(fields.next().and_then(|s| s.parse().ok()))),
                _ => None,
            }
        }
        "7" => parse_file_url(rest).map(OscEvent::Cwd),
        // ConEmu uses `9;<number>;…` (progress, tab titles…), not notifications.
        "9" if !rest.split(';').next()?.bytes().all(|b| b.is_ascii_digit()) => Some(OscEvent::Notify(rest.to_owned())),
        "777" => {
            let mut fields = rest.splitn(3, ';');
            if fields.next()? != "notify" {
                return None;
            }
            let (title, body) = (fields.next().unwrap_or(""), fields.next().unwrap_or(""));
            Some(OscEvent::Notify(if body.is_empty() { title } else { body }.to_owned()))
        }
        _ => None,
    }
}

/// `file://host/some%20dir` → `/some dir`. The host is ignored: the shell runs
/// on this machine (ssh sessions report remote paths that won't exist here,
/// and callers check that).
fn parse_file_url(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let path = &rest[rest.find('/')?..];
    let bytes = path.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = path.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let path = String::from_utf8(out).ok()?;
    // Windows shells report `/C:/Users/…`.
    let path = match path.as_bytes() {
        [b'/', drive, b':', ..] if cfg!(windows) && drive.is_ascii_alphabetic() => path[1..].to_owned(),
        _ => path,
    };
    Some(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> Vec<OscEvent> {
        let mut scanner = OscScanner::default();
        chunks.iter().flat_map(|c| scanner.feed(c)).map(|(_, e)| e).collect()
    }

    #[test]
    fn prompt_marks_with_both_terminators() {
        let events = scan(&[b"\x1b]133;A\x07$ \x1b]133;B\x1b\\ls\r\n\x1b]133;C\x07out\x1b]133;D;2\x07"]);
        assert_eq!(
            events,
            [OscEvent::PromptStart, OscEvent::CommandStart, OscEvent::OutputStart, OscEvent::CommandEnd(Some(2))]
        );
        assert_eq!(scan(&[b"\x1b]133;D\x07"]), [OscEvent::CommandEnd(None)]);
    }

    #[test]
    fn offsets_point_past_the_terminator() {
        let mut scanner = OscScanner::default();
        let bytes = b"ab\x1b]133;A\x07cd";
        assert_eq!(scanner.feed(bytes), [(10, OscEvent::PromptStart)]);
        assert_eq!(&bytes[10..], b"cd");
    }

    #[test]
    fn sequences_split_across_reads() {
        assert_eq!(scan(&[b"\x1b", b"]13", b"3;A\x1b", b"\\"]), [OscEvent::PromptStart]);
    }

    #[test]
    fn working_directory_is_percent_decoded() {
        let events = scan(&[b"\x1b]7;file://mac.local/Users/me/my%20dir\x07"]);
        assert_eq!(events, [OscEvent::Cwd(PathBuf::from("/Users/me/my dir"))]);
        assert_eq!(scan(&[b"\x1b]7;file:///tmp\x07"]), [OscEvent::Cwd(PathBuf::from("/tmp"))]);
        assert!(scan(&[b"\x1b]7;http://x/y\x07"]).is_empty());
    }

    #[test]
    fn notifications() {
        assert_eq!(scan(&[b"\x1b]9;Build done\x07"]), [OscEvent::Notify("Build done".into())]);
        assert_eq!(scan(&[b"\x1b]777;notify;Claude Code;Needs approval\x1b\\"]), [OscEvent::Notify("Needs approval".into())]);
        assert_eq!(scan(&[b"\x1b]777;notify;Only title\x07"]), [OscEvent::Notify("Only title".into())]);
        // ConEmu progress and other 777 commands are not notifications.
        assert!(scan(&[b"\x1b]9;4;1;50\x07\x1b]9;4\x07\x1b]777;precmd\x07"]).is_empty());
    }

    #[test]
    fn other_sequences_are_ignored() {
        assert!(scan(&[b"\x1b]0;title\x07\x1b[31mred\x1b]8;;http://x\x07"]).is_empty());
        // Aborted by CAN, oversized payloads, and an OSC cut short by another escape.
        assert!(scan(&[b"\x1b]133;A\x18"]).is_empty());
        let mut big = b"\x1b]52;c;".to_vec();
        big.extend(std::iter::repeat_n(b'x', MAX_PAYLOAD + 10));
        big.push(0x07);
        assert!(scan(&[&big]).is_empty());
        assert_eq!(scan(&[b"\x1b]133;A\x1b[0m\x1b]133;B\x07"]), [OscEvent::CommandStart]);
    }
}
