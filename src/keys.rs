//! Named keys for `wmux key`.
//!
//! `wmux send` types text, which cannot express a function key, an arrow, or an
//! Alt chord. `wmux key` takes tmux-style names instead (`F1`, `Alt-F10`,
//! `C-x`, `PgDn`) and the server delivers each one in whatever form the
//! session's backend understands:
//!
//! * a ConPTY session receives the xterm escape sequence a terminal would send
//!   ([`Key::to_vt`]);
//! * a legacy console session receives a real `KEY_EVENT_RECORD`, built from
//!   the virtual-key code ([`Key::vk`]), because a program reading
//!   `ReadConsoleInput` sees key records, not bytes.

use std::fmt;

/// Modifier bits. The values match the xterm modifier parameter minus one,
/// which makes [`Key::to_vt`] a single addition.
pub const SHIFT: u8 = 1;
pub const ALT: u8 = 2;
pub const CTRL: u8 = 4;

/// A key that has a name rather than a character.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Named {
    Enter,
    Esc,
    Tab,
    Space,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    /// F1 through F24.
    F(u8),
}

/// What the key is, before modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
    Char(char),
    Named(Named),
}

/// One keypress: a key plus any held modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub code: KeyCode,
    pub mods: u8,
}

impl Key {
    pub fn char(c: char) -> Key {
        Key {
            code: KeyCode::Char(c),
            mods: 0,
        }
    }

    pub fn named(n: Named) -> Key {
        Key {
            code: KeyCode::Named(n),
            mods: 0,
        }
    }

    /// Parses a key name such as `F1`, `Alt-F10`, `C-x`, `S-Tab` or `q`.
    ///
    /// Modifier prefixes are `Ctrl-`/`C-`, `Alt-`/`M-` and `Shift-`/`S-`, in
    /// any order and any case. Anything that is a single character after the
    /// prefixes is taken literally, so `-`, `?` and `\` are all valid keys.
    pub fn parse(spec: &str) -> Result<Key, String> {
        let mut rest = spec;
        let mut mods = 0u8;
        // A lone trailing character is the key itself even if it looks like a
        // modifier letter: `C` is the letter C, `C-` is Ctrl.
        while let Some((head, tail)) = rest.split_once('-') {
            if tail.is_empty() {
                break;
            }
            let bit = match head.to_ascii_lowercase().as_str() {
                "c" | "ctrl" | "control" => CTRL,
                "m" | "alt" | "meta" => ALT,
                "s" | "shift" => SHIFT,
                _ => break,
            };
            mods |= bit;
            rest = tail;
        }

        let mut chars = rest.chars();
        if let (Some(c), None) = (chars.next(), chars.clone().next()) {
            return Ok(Key {
                code: KeyCode::Char(c),
                mods,
            });
        }

        let named = match rest.to_ascii_lowercase().as_str() {
            "enter" | "return" | "cr" => Named::Enter,
            "esc" | "escape" => Named::Esc,
            "tab" => Named::Tab,
            "space" => Named::Space,
            "bspace" | "backspace" | "back" | "bs" => Named::Backspace,
            "up" => Named::Up,
            "down" => Named::Down,
            "left" => Named::Left,
            "right" => Named::Right,
            "home" => Named::Home,
            "end" => Named::End,
            "pgup" | "pageup" | "ppage" => Named::PageUp,
            "pgdn" | "pagedown" | "npage" => Named::PageDown,
            "ins" | "insert" | "ic" => Named::Insert,
            "del" | "delete" | "dc" => Named::Delete,
            other => match other.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                Some(n @ 1..=24) => Named::F(n),
                _ => return Err(format!("unknown key {spec:?}")),
            },
        };
        Ok(Key {
            code: KeyCode::Named(named),
            mods,
        })
    }

    /// Converts typed text into keypresses, for backends that need key
    /// records rather than bytes. Control characters become the key a user
    /// would press to produce them.
    pub fn from_text(text: &str) -> Vec<Key> {
        let mut keys = Vec::with_capacity(text.len());
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            let key = match c {
                '\r' => {
                    // A CRLF pair is one Enter, not two.
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    Key::named(Named::Enter)
                }
                '\n' => Key::named(Named::Enter),
                '\t' => Key::named(Named::Tab),
                '\x1b' => Key::named(Named::Esc),
                '\x08' | '\x7f' => Key::named(Named::Backspace),
                c if (c as u32) < 0x20 => Key {
                    code: KeyCode::Char((b'@' + c as u8).to_ascii_lowercase() as char),
                    mods: CTRL,
                },
                c => Key::char(c),
            };
            keys.push(key);
        }
        keys
    }

    /// The bytes an xterm-compatible terminal sends for this key.
    pub fn to_vt(&self) -> Vec<u8> {
        let m = self.mods;
        // xterm's modifier parameter: 1 + shift(1) + alt(2) + ctrl(4).
        let param = 1 + m;
        let csi = |final_byte: char| -> Vec<u8> {
            if m == 0 {
                format!("\x1b[{final_byte}").into_bytes()
            } else {
                format!("\x1b[1;{param}{final_byte}").into_bytes()
            }
        };
        let tilde = |n: u8| -> Vec<u8> {
            if m == 0 {
                format!("\x1b[{n}~").into_bytes()
            } else {
                format!("\x1b[{n};{param}~").into_bytes()
            }
        };
        let ss3 = |final_byte: char| -> Vec<u8> {
            if m == 0 {
                format!("\x1bO{final_byte}").into_bytes()
            } else {
                format!("\x1b[1;{param}{final_byte}").into_bytes()
            }
        };
        // Alt on a plain byte is sent as an ESC prefix.
        let alt_prefix = |bytes: Vec<u8>| -> Vec<u8> {
            if m & ALT != 0 {
                let mut v = vec![0x1b];
                v.extend(bytes);
                v
            } else {
                bytes
            }
        };

        match self.code {
            KeyCode::Char(c) => {
                if m & CTRL != 0 {
                    if let Some(b) = ctrl_byte(c) {
                        return alt_prefix(vec![b]);
                    }
                }
                let mut buf = [0u8; 4];
                alt_prefix(c.encode_utf8(&mut buf).as_bytes().to_vec())
            }
            KeyCode::Named(n) => match n {
                Named::Enter => alt_prefix(vec![b'\r']),
                Named::Esc => alt_prefix(vec![0x1b]),
                Named::Tab if m & SHIFT != 0 => b"\x1b[Z".to_vec(),
                Named::Tab => alt_prefix(vec![b'\t']),
                Named::Space if m & CTRL != 0 => alt_prefix(vec![0]),
                Named::Space => alt_prefix(vec![b' ']),
                Named::Backspace => alt_prefix(vec![0x7f]),
                Named::Up => csi('A'),
                Named::Down => csi('B'),
                Named::Right => csi('C'),
                Named::Left => csi('D'),
                Named::Home => csi('H'),
                Named::End => csi('F'),
                Named::Insert => tilde(2),
                Named::Delete => tilde(3),
                Named::PageUp => tilde(5),
                Named::PageDown => tilde(6),
                Named::F(1) => ss3('P'),
                Named::F(2) => ss3('Q'),
                Named::F(3) => ss3('R'),
                Named::F(4) => ss3('S'),
                Named::F(n) => tilde(match n {
                    5 => 15,
                    6 => 17,
                    7 => 18,
                    8 => 19,
                    9 => 20,
                    10 => 21,
                    11 => 23,
                    12 => 24,
                    13 => 25,
                    14 => 26,
                    15 => 28,
                    16 => 29,
                    17 => 31,
                    18 => 32,
                    19 => 33,
                    20 => 34,
                    // F21-F24 have no common xterm encoding; reuse F20's
                    // range rather than sending nothing.
                    n => 34 + (n - 20),
                }),
            },
        }
    }

    /// The Windows virtual-key code for a named key, or `None` for a
    /// character, whose code depends on the keyboard layout.
    pub fn vk(&self) -> Option<u16> {
        let KeyCode::Named(n) = self.code else {
            return None;
        };
        Some(match n {
            Named::Enter => 0x0D,
            Named::Esc => 0x1B,
            Named::Tab => 0x09,
            Named::Space => 0x20,
            Named::Backspace => 0x08,
            Named::PageUp => 0x21,
            Named::PageDown => 0x22,
            Named::End => 0x23,
            Named::Home => 0x24,
            Named::Left => 0x25,
            Named::Up => 0x26,
            Named::Right => 0x27,
            Named::Down => 0x28,
            Named::Insert => 0x2D,
            Named::Delete => 0x2E,
            Named::F(n) => 0x6F + n as u16,
        })
    }

    /// Whether the console flags this key as ENHANCED_KEY: the navigation
    /// cluster that is separate from the numeric keypad.
    pub fn is_enhanced(&self) -> bool {
        matches!(
            self.code,
            KeyCode::Named(
                Named::Up
                    | Named::Down
                    | Named::Left
                    | Named::Right
                    | Named::Home
                    | Named::End
                    | Named::PageUp
                    | Named::PageDown
                    | Named::Insert
                    | Named::Delete
            )
        )
    }

    /// The character a console reports in `UnicodeChar` for this key, the
    /// same value a real keypress would produce.
    pub fn console_char(&self) -> u16 {
        match self.code {
            KeyCode::Char(c) => {
                if self.mods & CTRL != 0 {
                    return ctrl_byte(c).map_or(0, u16::from);
                }
                let mut buf = [0u16; 2];
                // Characters outside the BMP cannot be expressed in a single
                // key record; the high surrogate is the best a console can do.
                c.encode_utf16(&mut buf)[0]
            }
            KeyCode::Named(n) => match n {
                Named::Enter => 0x0D,
                Named::Esc => 0x1B,
                Named::Tab => 0x09,
                Named::Space => 0x20,
                Named::Backspace => 0x08,
                _ => 0,
            },
        }
    }

    /// Wire encoding: kind u8, value u32, mods u8.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let (kind, value): (u8, u32) = match self.code {
            KeyCode::Char(c) => (0, c as u32),
            KeyCode::Named(n) => (1, named_to_wire(n)),
        };
        out.push(kind);
        out.extend_from_slice(&value.to_le_bytes());
        out.push(self.mods);
    }

    pub const WIRE_LEN: usize = 6;

    pub fn decode(b: &[u8]) -> Option<Key> {
        if b.len() < Self::WIRE_LEN {
            return None;
        }
        let value = u32::from_le_bytes([b[1], b[2], b[3], b[4]]);
        let code = match b[0] {
            0 => KeyCode::Char(char::from_u32(value)?),
            1 => KeyCode::Named(named_from_wire(value)?),
            _ => return None,
        };
        Some(Key { code, mods: b[5] })
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.mods & CTRL != 0 {
            f.write_str("Ctrl-")?;
        }
        if self.mods & ALT != 0 {
            f.write_str("Alt-")?;
        }
        if self.mods & SHIFT != 0 {
            f.write_str("Shift-")?;
        }
        match self.code {
            KeyCode::Char(c) => write!(f, "{c}"),
            KeyCode::Named(Named::F(n)) => write!(f, "F{n}"),
            KeyCode::Named(n) => write!(f, "{n:?}"),
        }
    }
}

/// The control byte for Ctrl plus a character, as a terminal sends it.
fn ctrl_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '@' | ' ' => Some(0),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        _ => None,
    }
}

fn named_to_wire(n: Named) -> u32 {
    match n {
        Named::Enter => 1,
        Named::Esc => 2,
        Named::Tab => 3,
        Named::Space => 4,
        Named::Backspace => 5,
        Named::Up => 6,
        Named::Down => 7,
        Named::Left => 8,
        Named::Right => 9,
        Named::Home => 10,
        Named::End => 11,
        Named::PageUp => 12,
        Named::PageDown => 13,
        Named::Insert => 14,
        Named::Delete => 15,
        Named::F(n) => 100 + n as u32,
    }
}

fn named_from_wire(v: u32) -> Option<Named> {
    Some(match v {
        1 => Named::Enter,
        2 => Named::Esc,
        3 => Named::Tab,
        4 => Named::Space,
        5 => Named::Backspace,
        6 => Named::Up,
        7 => Named::Down,
        8 => Named::Left,
        9 => Named::Right,
        10 => Named::Home,
        11 => Named::End,
        12 => Named::PageUp,
        13 => Named::PageDown,
        14 => Named::Insert,
        15 => Named::Delete,
        101..=124 => Named::F((v - 100) as u8),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: &str) -> Key {
        Key::parse(s).unwrap()
    }

    #[test]
    fn parses_named_keys_case_insensitively() {
        assert_eq!(k("F1"), Key::named(Named::F(1)));
        assert_eq!(k("f12"), Key::named(Named::F(12)));
        assert_eq!(k("ENTER"), Key::named(Named::Enter));
        assert_eq!(k("PgDn"), Key::named(Named::PageDown));
        assert_eq!(k("NPage"), Key::named(Named::PageDown));
        assert_eq!(k("BSpace"), Key::named(Named::Backspace));
    }

    #[test]
    fn parses_modifier_prefixes_in_both_styles() {
        assert_eq!(
            k("Alt-F10"),
            Key {
                code: KeyCode::Named(Named::F(10)),
                mods: ALT
            }
        );
        assert_eq!(
            k("C-x"),
            Key {
                code: KeyCode::Char('x'),
                mods: CTRL
            }
        );
        assert_eq!(
            k("ctrl-shift-up"),
            Key {
                code: KeyCode::Named(Named::Up),
                mods: CTRL | SHIFT
            }
        );
        assert_eq!(
            k("M-S-Tab"),
            Key {
                code: KeyCode::Named(Named::Tab),
                mods: ALT | SHIFT
            }
        );
    }

    #[test]
    fn single_characters_are_literal_even_when_they_look_like_modifiers() {
        assert_eq!(k("C"), Key::char('C'));
        assert_eq!(k("-"), Key::char('-'));
        assert_eq!(
            k("Alt--"),
            Key {
                code: KeyCode::Char('-'),
                mods: ALT
            }
        );
        assert_eq!(k("\\"), Key::char('\\'));
    }

    #[test]
    fn rejects_unknown_names() {
        assert!(Key::parse("Hyper-q").is_err());
        assert!(Key::parse("F25").is_err());
        assert!(Key::parse("F0").is_err());
        assert!(Key::parse("Banana").is_err());
        assert!(Key::parse("").is_err());
    }

    #[test]
    fn vt_encodings_match_xterm() {
        assert_eq!(k("F1").to_vt(), b"\x1bOP");
        assert_eq!(k("F5").to_vt(), b"\x1b[15~");
        assert_eq!(k("F12").to_vt(), b"\x1b[24~");
        assert_eq!(k("Up").to_vt(), b"\x1b[A");
        assert_eq!(k("C-Up").to_vt(), b"\x1b[1;5A");
        assert_eq!(k("S-F1").to_vt(), b"\x1b[1;2P");
        assert_eq!(k("Alt-F10").to_vt(), b"\x1b[21;3~");
        assert_eq!(k("PgDn").to_vt(), b"\x1b[6~");
        assert_eq!(k("S-Tab").to_vt(), b"\x1b[Z");
        assert_eq!(k("Enter").to_vt(), b"\r");
        assert_eq!(k("C-c").to_vt(), b"\x03");
        assert_eq!(k("M-x").to_vt(), b"\x1bx");
        assert_eq!(k("é").to_vt(), "é".as_bytes());
    }

    #[test]
    fn virtual_key_codes_match_winuser() {
        assert_eq!(k("F1").vk(), Some(0x70));
        assert_eq!(k("F10").vk(), Some(0x79));
        assert_eq!(k("Down").vk(), Some(0x28));
        assert_eq!(k("Enter").vk(), Some(0x0D));
        assert_eq!(k("q").vk(), None);
        assert!(k("Down").is_enhanced());
        assert!(!k("F1").is_enhanced());
    }

    #[test]
    fn console_chars_match_a_real_keypress() {
        assert_eq!(k("Enter").console_char(), 0x0D);
        assert_eq!(k("C-c").console_char(), 0x03);
        assert_eq!(k("q").console_char(), u16::from(b'q'));
        assert_eq!(k("F1").console_char(), 0);
    }

    #[test]
    fn text_becomes_keys() {
        assert_eq!(
            Key::from_text("hi\r\n"),
            vec![Key::char('h'), Key::char('i'), Key::named(Named::Enter)]
        );
        assert_eq!(
            Key::from_text("\x03\x1b\t"),
            vec![
                Key {
                    code: KeyCode::Char('c'),
                    mods: CTRL
                },
                Key::named(Named::Esc),
                Key::named(Named::Tab),
            ]
        );
    }

    #[test]
    fn wire_roundtrip() {
        for spec in ["F1", "Alt-F10", "C-x", "é", "S-Tab", "F24", "PgUp"] {
            let key = k(spec);
            let mut buf = Vec::new();
            key.encode(&mut buf);
            assert_eq!(buf.len(), Key::WIRE_LEN);
            assert_eq!(Key::decode(&buf), Some(key), "{spec}");
        }
        assert_eq!(Key::decode(&[9, 0, 0, 0, 0, 0]), None);
        assert_eq!(Key::decode(&[0, 0, 0]), None);
    }
}
