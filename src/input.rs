//! Console input decoding.
//!
//! Reading raw bytes is not enough to recognise a key. Windows Terminal
//! negotiates **win32-input-mode**, in which every keystroke arrives as an
//! escape sequence carrying the full key record rather than as a plain byte:
//!
//! ```text
//! ESC [ Vk ; Sc ; Uc ; Kd ; Cs ; Rc _
//!       │    │    │    │    │    └─ repeat count
//!       │    │    │    │    └────── control key state
//!       │    │    │    └─────────── 1 = key down, 0 = key up
//!       │    │    └──────────────── unicode character
//!       │    └───────────────────── scan code
//!       └────────────────────────── virtual key code
//! ```
//!
//! Ctrl-B therefore arrives as `ESC[66;48;2;1;40;1_` — virtual key `0x42`,
//! character `0x02` — and never as a bare `0x02`. A detector scanning the byte
//! stream for `0x02` sees nothing and passes the whole sequence through to the
//! session, where the shell acts on it. That was the detach bug.
//!
//! This module turns a byte stream into discrete key presses, each carrying
//! both the character it represents and the original bytes, so the caller can
//! decide per key whether to act on it or forward it untouched.

/// One key press decoded from the input stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    /// The character this key produced, if it maps to a single byte.
    ///
    /// `None` for key releases, cursor keys, focus events, and anything else
    /// that is not a plain character. Such keys are never treated as a prefix.
    pub ch: Option<u8>,
    /// The exact bytes that produced this key, for forwarding verbatim.
    pub raw: Vec<u8>,
}

impl Key {
    fn plain(byte: u8) -> Key {
        Key {
            ch: Some(byte),
            raw: vec![byte],
        }
    }

    fn opaque(raw: &[u8]) -> Key {
        Key {
            ch: None,
            raw: raw.to_vec(),
        }
    }
}

/// Incremental decoder for console input.
///
/// Holds partial escape sequences between reads, because a keystroke can be
/// split across two `ReadFile` calls.
#[derive(Debug, Default)]
pub struct InputParser {
    pending: Vec<u8>,
}

/// Terminates a win32-input-mode sequence.
const WIN32_INPUT_FINAL: u8 = b'_';

impl InputParser {
    pub fn new() -> InputParser {
        InputParser::default()
    }

    /// Decodes as many complete keys as the buffered bytes allow.
    ///
    /// Anything incomplete is retained for the next call.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        self.pending.extend_from_slice(bytes);
        let mut keys = Vec::new();

        loop {
            if self.pending.is_empty() {
                break;
            }
            if self.pending[0] != 0x1b {
                let byte = self.pending.remove(0);
                keys.push(Key::plain(byte));
                continue;
            }
            // An escape at the very end may be the start of a sequence.
            if self.pending.len() < 2 {
                break;
            }
            if self.pending[1] != b'[' {
                // ESC followed by something else, e.g. Alt-<key>. Two bytes,
                // opaque: never a prefix, always forwarded.
                let raw: Vec<u8> = self.pending.drain(..2).collect();
                keys.push(Key::opaque(&raw));
                continue;
            }
            // CSI: scan for the final byte.
            match self.pending[2..]
                .iter()
                .position(|b| (0x40..=0x7e).contains(b))
            {
                Some(offset) => {
                    let end = 2 + offset;
                    let raw: Vec<u8> = self.pending.drain(..=end).collect();
                    keys.push(decode_csi(&raw));
                }
                // Incomplete sequence; wait for the rest.
                None => break,
            }
        }
        keys
    }
}

/// Turns a complete CSI sequence into a key.
fn decode_csi(raw: &[u8]) -> Key {
    let final_byte = *raw.last().expect("a CSI sequence has a final byte");
    if final_byte != WIN32_INPUT_FINAL {
        // Cursor keys, focus events, and friends. Forward, never match.
        return Key::opaque(raw);
    }

    // Parameters sit between "ESC[" and the trailing '_'.
    let body = &raw[2..raw.len() - 1];
    let params: Vec<u32> = String::from_utf8_lossy(body)
        .split(';')
        .map(|field| field.trim().parse::<u32>().unwrap_or(0))
        .collect();

    let unicode = params.get(2).copied().unwrap_or(0);
    let key_down = params.get(3).copied().unwrap_or(0) == 1;

    // Only key presses count, and only those that carry a character. A key
    // release with the same character must not re-trigger the prefix.
    let ch = if key_down && (1..=0xff).contains(&unicode) {
        Some(unicode as u8)
    } else {
        None
    };

    Key {
        ch,
        raw: raw.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(bytes: &[u8]) -> Vec<Key> {
        InputParser::new().feed(bytes)
    }

    #[test]
    fn plain_bytes_become_plain_keys() {
        let keys = parse(b"hi");
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0], Key::plain(b'h'));
        assert_eq!(keys[1], Key::plain(b'i'));
    }

    #[test]
    fn a_bare_control_byte_is_a_character() {
        assert_eq!(parse(&[0x02]), vec![Key::plain(0x02)]);
    }

    #[test]
    fn win32_input_mode_ctrl_b_decodes_to_0x02() {
        // The exact sequence Windows Terminal sends for Ctrl-B, captured from
        // a real session: VK_B (66), scan 48, char 2, key down, Ctrl held.
        let raw = b"\x1b[66;48;2;1;40;1_";
        let keys = parse(raw);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].ch, Some(0x02), "Ctrl-B must decode to 0x02");
        assert_eq!(keys[0].raw, raw.to_vec(), "raw bytes must be preserved");
    }

    #[test]
    fn a_key_release_carries_no_character() {
        // Same key, Kd = 0. Releasing must not re-trigger anything.
        let keys = parse(b"\x1b[66;48;2;0;40;1_");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].ch, None);
    }

    #[test]
    fn win32_input_mode_letter_d_decodes() {
        // VK_D (68), char 100 = 'd', key down.
        let keys = parse(b"\x1b[68;32;100;1;32;1_");
        assert_eq!(keys[0].ch, Some(b'd'));
    }

    #[test]
    fn a_modifier_press_alone_carries_no_character() {
        // Ctrl pressed by itself: char field is 0.
        let keys = parse(b"\x1b[17;29;0;1;40;1_");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].ch, None);
    }

    #[test]
    fn cursor_keys_are_opaque_but_forwarded() {
        let keys = parse(b"\x1b[A");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].ch, None);
        assert_eq!(keys[0].raw, b"\x1b[A".to_vec());
    }

    #[test]
    fn focus_events_are_opaque() {
        // ESC[O is focus-lost; it appeared in a real capture.
        let keys = parse(b"\x1b[O");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].ch, None);
    }

    #[test]
    fn alt_combinations_are_opaque() {
        let keys = parse(b"\x1bx");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].ch, None);
        assert_eq!(keys[0].raw, b"\x1bx".to_vec());
    }

    #[test]
    fn a_sequence_split_across_reads_is_reassembled() {
        let mut parser = InputParser::new();
        assert!(parser.feed(b"\x1b[66;48;").is_empty(), "incomplete so far");
        let keys = parser.feed(b"2;1;40;1_");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].ch, Some(0x02));
        assert_eq!(keys[0].raw, b"\x1b[66;48;2;1;40;1_".to_vec());
    }

    #[test]
    fn a_lone_trailing_escape_is_held_back() {
        let mut parser = InputParser::new();
        assert!(!parser.feed(b"a\x1b").is_empty());
        // 'a' came out; the ESC is still buffered awaiting its sequence.
        let keys = parser.feed(b"[66;48;2;1;40;1_");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].ch, Some(0x02));
    }

    #[test]
    fn mixed_plain_and_win32_input_interleave_correctly() {
        let keys = parse(b"a\x1b[66;48;2;1;40;1_b");
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0].ch, Some(b'a'));
        assert_eq!(keys[1].ch, Some(0x02));
        assert_eq!(keys[2].ch, Some(b'b'));
    }

    #[test]
    fn malformed_parameters_do_not_panic() {
        for raw in [
            &b"\x1b[_"[..],
            &b"\x1b[;;;;;_"[..],
            // Parameter bytes (0x30..=0x3f) that are not digits.
            &b"\x1b[<;>;?_"[..],
            // Wider than u32.
            &b"\x1b[99999999999999999999;1;2;1;0;1_"[..],
        ] {
            let keys = parse(raw);
            assert_eq!(keys.len(), 1, "{raw:?}");
            assert_eq!(keys[0].raw, raw.to_vec());
        }
    }

    #[test]
    fn a_letter_terminates_a_csi_sequence() {
        // Per ECMA-48 the final byte is 0x40..=0x7e, so "\x1b[abc" is the
        // complete sequence "\x1b[a" followed by two ordinary characters.
        // Worth pinning: it is easy to mistake this for malformed input.
        let keys = parse(b"\x1b[abc");
        assert_eq!(keys.len(), 3);
        assert_eq!(keys[0].raw, b"\x1b[a".to_vec());
        assert_eq!(keys[0].ch, None);
        assert_eq!(keys[1].ch, Some(b'b'));
        assert_eq!(keys[2].ch, Some(b'c'));
    }

    #[test]
    fn a_character_above_the_byte_range_is_not_a_prefix_candidate() {
        // U+20AC, outside what a single-byte prefix can express.
        let keys = parse(b"\x1b[0;0;8364;1;0;1_");
        assert_eq!(keys[0].ch, None);
    }
}
