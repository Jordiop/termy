//! Keyboard and mouse translation.
//!
//! * Resolves `winit` key events against `[keybindings]` ([`combo_for`]).
//! * Encodes everything else as xterm-compatible VT/ANSI sequences for the
//!   focused session's PTY ([`encode_key`]), honoring application cursor
//!   mode, modifiers and Option-as-Alt.
//! * Encodes mouse wheel / button reports (SGR 1006 and legacy X10).
//!
//! Routing (egui vs. terminal) lives in `app.rs`; this module is pure and
//! unit-tested.

use winit::keyboard::{Key, ModifiersState, NamedKey};

use crate::config::KeyCombo;

/// Modifier bits as used in xterm `CSI 1;<m>X` sequences (value is `1 + bits`).
fn xterm_modifier(mods: ModifiersState) -> u8 {
    1 + (mods.shift_key() as u8) + 2 * (mods.alt_key() as u8) + 4 * (mods.control_key() as u8)
}

/// Normalized key name shared with `KeyCombo` (`"t"`, `"comma"`, `"tab"`…).
pub fn key_name(key: &Key) -> Option<String> {
    let name = match key {
        Key::Character(s) => match s.as_str() {
            "," => "comma".into(),
            "=" | "+" => "equal".into(),
            "-" => "minus".into(),
            "[" => "bracketleft".into(),
            "]" => "bracketright".into(),
            "/" => "slash".into(),
            other => other.to_lowercase(),
        },
        Key::Named(named) => match named {
            NamedKey::Enter => "enter".into(),
            NamedKey::Tab => "tab".into(),
            NamedKey::Space => "space".into(),
            NamedKey::Backspace => "backspace".into(),
            NamedKey::Escape => "escape".into(),
            NamedKey::Delete => "delete".into(),
            NamedKey::Insert => "insert".into(),
            NamedKey::Home => "home".into(),
            NamedKey::End => "end".into(),
            NamedKey::PageUp => "pageup".into(),
            NamedKey::PageDown => "pagedown".into(),
            NamedKey::ArrowUp => "up".into(),
            NamedKey::ArrowDown => "down".into(),
            NamedKey::ArrowLeft => "left".into(),
            NamedKey::ArrowRight => "right".into(),
            other => {
                let f = function_key_number(other)?;
                format!("f{f}")
            }
        },
        _ => return None,
    };
    Some(name)
}

/// The shortcut a key press represents. `key` must be the key *without*
/// modifiers applied (so Shift+] is still `bracketright`).
pub fn combo_for(key: &Key, mods: ModifiersState) -> Option<KeyCombo> {
    Some(KeyCombo {
        ctrl: mods.control_key(),
        alt: mods.alt_key(),
        shift: mods.shift_key(),
        logo: mods.super_key(),
        key: key_name(key)?,
    })
}

fn function_key_number(key: &NamedKey) -> Option<u8> {
    use NamedKey::*;
    Some(match key {
        F1 => 1,
        F2 => 2,
        F3 => 3,
        F4 => 4,
        F5 => 5,
        F6 => 6,
        F7 => 7,
        F8 => 8,
        F9 => 9,
        F10 => 10,
        F11 => 11,
        F12 => 12,
        _ => return None,
    })
}

/// Terminal modes that change how keys are encoded.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyModes {
    /// DECCKM: arrows send `ESC O A` instead of `ESC [ A`.
    pub app_cursor: bool,
    /// Treat Option/Alt as Meta (ESC prefix). Always on outside macOS.
    pub option_as_alt: bool,
}

/// Encodes a key press for the PTY.
///
/// * `key`: logical key *without* modifiers (`key_without_modifiers`).
/// * `text`: text the key produced with modifiers (IME-aware), if any.
pub fn encode_key(key: &Key, text: Option<&str>, mods: ModifiersState, modes: KeyModes) -> Option<Vec<u8>> {
    let m = xterm_modifier(mods);
    let has_mods = m > 1;
    let alt = mods.alt_key() && modes.option_as_alt;
    let esc = |mut bytes: Vec<u8>| {
        if alt {
            bytes.insert(0, 0x1b);
        }
        bytes
    };

    if let Key::Named(named) = key {
        let csi_letter = |letter: char| -> Vec<u8> {
            if has_mods {
                format!("\x1b[1;{m}{letter}").into_bytes()
            } else if modes.app_cursor {
                format!("\x1bO{letter}").into_bytes()
            } else {
                format!("\x1b[{letter}").into_bytes()
            }
        };
        let csi_tilde = |code: u8| -> Vec<u8> {
            if has_mods { format!("\x1b[{code};{m}~") } else { format!("\x1b[{code}~") }.into_bytes()
        };
        let bytes = match named {
            // Shift+Enter → ESC CR: the "insert newline" convention used by
            // Claude Code, Codex and most readline-style prompts.
            NamedKey::Enter if mods.shift_key() => b"\x1b\r".to_vec(),
            NamedKey::Enter => esc(b"\r".to_vec()),
            NamedKey::Tab if mods.shift_key() => b"\x1b[Z".to_vec(),
            NamedKey::Tab => esc(b"\t".to_vec()),
            NamedKey::Backspace if mods.control_key() => esc(vec![0x08]),
            NamedKey::Backspace => esc(vec![0x7f]),
            NamedKey::Escape => b"\x1b".to_vec(),
            NamedKey::Space if mods.control_key() => esc(vec![0]),
            NamedKey::Space => esc(b" ".to_vec()),
            NamedKey::ArrowUp => csi_letter('A'),
            NamedKey::ArrowDown => csi_letter('B'),
            NamedKey::ArrowRight => csi_letter('C'),
            NamedKey::ArrowLeft => csi_letter('D'),
            NamedKey::Home => csi_letter('H'),
            NamedKey::End => csi_letter('F'),
            NamedKey::Insert => csi_tilde(2),
            NamedKey::Delete => csi_tilde(3),
            NamedKey::PageUp => csi_tilde(5),
            NamedKey::PageDown => csi_tilde(6),
            other => match function_key_number(other)? {
                n @ 1..=4 => {
                    let letter = (b'P' + n - 1) as char;
                    if has_mods { format!("\x1b[1;{m}{letter}") } else { format!("\x1bO{letter}") }.into_bytes()
                }
                n => csi_tilde([15, 17, 18, 19, 20, 21, 23, 24][(n - 5) as usize]),
            },
        };
        return Some(bytes);
    }

    let Key::Character(base) = key else {
        // Dead keys, IME, unidentified: whatever text was produced.
        return text.filter(|t| !t.is_empty()).map(|t| t.as_bytes().to_vec());
    };

    if mods.control_key()
        && let Some(byte) = base.chars().next().and_then(control_byte)
    {
        return Some(esc(vec![byte]));
    }
    if alt {
        // Meta: ESC + the character (with Shift applied, without Option's
        // composition on macOS).
        let ch = if mods.shift_key() { base.to_uppercase() } else { base.to_string() };
        return Some(esc(ch.into_bytes()));
    }
    text.filter(|t| !t.is_empty()).map(|t| t.as_bytes().to_vec())
}

/// Ctrl+<key> → C0 control byte.
fn control_byte(c: char) -> Option<u8> {
    let c = c.to_ascii_lowercase();
    Some(match c {
        'a'..='z' => c as u8 & 0x1f,
        '@' | ' ' | '2' => 0x00,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '-' | '/' | '7' => 0x1f,
        '8' | '?' => 0x7f,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Mouse
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseEventKind {
    Press,
    Release,
    Drag,
}

/// Encodes a mouse report for an app that enabled mouse tracking.
/// `col`/`row` are 0-based cell coordinates.
pub fn mouse_report(
    button: MouseButton,
    kind: MouseEventKind,
    col: usize,
    row: usize,
    mods: ModifiersState,
    sgr: bool,
) -> Vec<u8> {
    let mut code: u32 = match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
        MouseButton::WheelUp => 64,
        MouseButton::WheelDown => 65,
    };
    if kind == MouseEventKind::Drag {
        code += 32;
    }
    code += 4 * mods.shift_key() as u32 + 8 * mods.alt_key() as u32 + 16 * mods.control_key() as u32;
    if sgr {
        let suffix = if kind == MouseEventKind::Release { 'm' } else { 'M' };
        format!("\x1b[<{code};{};{}{suffix}", col + 1, row + 1).into_bytes()
    } else {
        // X10/normal: release is button 3, coordinates offset by 32 (max 223).
        if kind == MouseEventKind::Release {
            code = 3 + (code & !3);
        }
        let clamp = |v: usize| (v.min(222) + 33) as u8;
        vec![0x1b, b'[', b'M', (code + 32) as u8, clamp(col), clamp(row)]
    }
}

/// What a wheel movement should do in the focused terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WheelAction {
    /// Move the viewport through the scrollback (positive = up).
    Scrollback(i32),
    /// Send bytes to the app (alternate-screen apps without mouse support
    /// get arrow keys, like xterm's alternateScroll).
    Send(Vec<u8>),
}

pub fn wheel_action(lines: i32, alt_screen: bool, alternate_scroll: bool, app_cursor: bool) -> WheelAction {
    if alt_screen && alternate_scroll {
        let letter = if lines > 0 { 'A' } else { 'B' };
        let seq = if app_cursor { format!("\x1bO{letter}") } else { format!("\x1b[{letter}") };
        WheelAction::Send(seq.repeat(lines.unsigned_abs() as usize).into_bytes())
    } else {
        WheelAction::Scrollback(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::SmolStr;

    fn ch(s: &str) -> Key {
        Key::Character(SmolStr::new(s))
    }

    const NONE: ModifiersState = ModifiersState::empty();
    const CTRL: ModifiersState = ModifiersState::CONTROL;
    const SHIFT: ModifiersState = ModifiersState::SHIFT;
    const ALT: ModifiersState = ModifiersState::ALT;
    const ALT_ON: KeyModes = KeyModes { app_cursor: false, option_as_alt: true };

    fn enc(key: Key, text: Option<&str>, mods: ModifiersState) -> Vec<u8> {
        encode_key(&key, text, mods, ALT_ON).unwrap()
    }

    #[test]
    fn plain_text_and_control_keys() {
        assert_eq!(enc(ch("a"), Some("a"), NONE), b"a");
        assert_eq!(enc(ch("a"), Some("A"), SHIFT), b"A");
        assert_eq!(enc(ch("ñ"), Some("ñ"), NONE), "ñ".as_bytes());
        assert_eq!(enc(ch("c"), None, CTRL), [0x03]);
        assert_eq!(enc(ch("["), None, CTRL), [0x1b]);
        assert_eq!(enc(Key::Named(NamedKey::Space), Some(" "), CTRL), [0x00]);
        assert_eq!(enc(Key::Named(NamedKey::Enter), Some("\r"), NONE), b"\r");
        assert_eq!(enc(Key::Named(NamedKey::Backspace), None, NONE), [0x7f]);
        assert_eq!(enc(Key::Named(NamedKey::Tab), None, SHIFT), b"\x1b[Z");
        assert_eq!(enc(Key::Named(NamedKey::Enter), None, SHIFT), b"\x1b\r");
    }

    #[test]
    fn alt_is_meta_only_when_enabled() {
        assert_eq!(enc(ch("b"), Some("b"), ALT), b"\x1bb");
        assert_eq!(enc(ch("b"), Some("B"), ALT | SHIFT), b"\x1bB");
        assert_eq!(enc(ch("c"), None, ALT | CTRL), [0x1b, 0x03]);
        // macOS default: Option composes characters (∫ for Option+B).
        let mac = KeyModes { app_cursor: false, option_as_alt: false };
        assert_eq!(encode_key(&ch("b"), Some("∫"), ALT, mac).unwrap(), "∫".as_bytes());
    }

    #[test]
    fn cursor_keys_follow_decckm_and_modifiers() {
        let up = Key::Named(NamedKey::ArrowUp);
        assert_eq!(enc(up.clone(), None, NONE), b"\x1b[A");
        let app = KeyModes { app_cursor: true, option_as_alt: true };
        assert_eq!(encode_key(&up, None, NONE, app).unwrap(), b"\x1bOA");
        assert_eq!(enc(Key::Named(NamedKey::ArrowLeft), None, CTRL), b"\x1b[1;5D");
        assert_eq!(enc(Key::Named(NamedKey::ArrowRight), None, ALT | SHIFT), b"\x1b[1;4C");
        assert_eq!(enc(Key::Named(NamedKey::Home), None, NONE), b"\x1b[H");
    }

    #[test]
    fn editing_and_function_keys() {
        assert_eq!(enc(Key::Named(NamedKey::Delete), None, NONE), b"\x1b[3~");
        assert_eq!(enc(Key::Named(NamedKey::PageUp), None, SHIFT), b"\x1b[5;2~");
        assert_eq!(enc(Key::Named(NamedKey::F1), None, NONE), b"\x1bOP");
        assert_eq!(enc(Key::Named(NamedKey::F4), None, CTRL), b"\x1b[1;5S");
        assert_eq!(enc(Key::Named(NamedKey::F5), None, NONE), b"\x1b[15~");
        assert_eq!(enc(Key::Named(NamedKey::F12), None, NONE), b"\x1b[24~");
        assert!(encode_key(&Key::Named(NamedKey::CapsLock), None, NONE, ALT_ON).is_none());
    }

    #[test]
    fn shortcut_resolution_matches_config_syntax() {
        let combo = combo_for(&ch(","), ModifiersState::SUPER).unwrap();
        assert_eq!(combo, KeyCombo::parse("cmd+comma").unwrap());
        let combo = combo_for(&ch("T"), CTRL | SHIFT).unwrap();
        assert_eq!(combo, KeyCombo::parse("ctrl+shift+t").unwrap());
        let combo = combo_for(&Key::Named(NamedKey::Tab), CTRL).unwrap();
        assert_eq!(combo, KeyCombo::parse("ctrl+tab").unwrap());
        assert_eq!(combo_for(&ch("]"), ModifiersState::SUPER | SHIFT).unwrap(), KeyCombo::parse("cmd+shift+bracketright").unwrap());
    }

    #[test]
    fn mouse_reports() {
        assert_eq!(mouse_report(MouseButton::Left, MouseEventKind::Press, 0, 0, NONE, true), b"\x1b[<0;1;1M");
        assert_eq!(mouse_report(MouseButton::Left, MouseEventKind::Release, 9, 4, NONE, true), b"\x1b[<0;10;5m");
        assert_eq!(mouse_report(MouseButton::WheelUp, MouseEventKind::Press, 2, 3, CTRL, true), b"\x1b[<80;3;4M");
        assert_eq!(mouse_report(MouseButton::Left, MouseEventKind::Drag, 1, 1, NONE, true), b"\x1b[<32;2;2M");
        assert_eq!(mouse_report(MouseButton::Left, MouseEventKind::Press, 0, 0, NONE, false), [0x1b, b'[', b'M', 32, 33, 33]);
        assert_eq!(mouse_report(MouseButton::Right, MouseEventKind::Release, 0, 0, NONE, false)[3], 35);
    }

    #[test]
    fn wheel_in_alternate_screen_sends_arrows() {
        assert_eq!(wheel_action(3, false, true, false), WheelAction::Scrollback(3));
        assert_eq!(wheel_action(2, true, true, false), WheelAction::Send(b"\x1b[A\x1b[A".to_vec()));
        assert_eq!(wheel_action(-1, true, true, true), WheelAction::Send(b"\x1bOB".to_vec()));
        assert_eq!(wheel_action(-1, true, false, false), WheelAction::Scrollback(-1));
    }
}
