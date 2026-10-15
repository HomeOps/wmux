//! A screen snapshot: characters *and* colours.
//!
//! `capture` returns plain text, which is enough to check what a shell
//! printed but not to see what a full-screen program looks like: highlight
//! bars, colour-coded file types, and dialog frames are all attributes. A
//! [`Snapshot`] keeps one console attribute per cell.
//!
//! Attributes use the classic console encoding regardless of backend, since
//! that is what a legacy program actually writes: the low nibble is the
//! foreground colour, the high nibble the background, each `IRGB` (bit 3 is
//! intensity, then red, green, blue). `0x1E` is bright yellow on blue. ConPTY
//! sessions are converted to the same encoding from the vt100 model, mapping
//! 256-colour and RGB values to the nearest of the 16.

/// One character cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    /// Console attribute: `bg << 4 | fg`, each IRGB.
    pub attr: u8,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            attr: DEFAULT_ATTR,
        }
    }
}

/// Light grey on black: the console's own default.
pub const DEFAULT_ATTR: u8 = 0x07;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub cols: u16,
    /// Rows in `cells`, including `history`.
    pub rows: u16,
    /// How many of the leading rows are scrollback above the visible screen.
    pub history: u16,
    /// Cursor position relative to the visible screen.
    pub cursor_x: u16,
    pub cursor_y: u16,
    /// Row-major, `cols * rows` long.
    pub cells: Vec<Cell>,
}

impl Snapshot {
    pub fn blank(cols: u16, rows: u16) -> Snapshot {
        Snapshot {
            cols,
            rows,
            history: 0,
            cursor_x: 0,
            cursor_y: 0,
            cells: vec![Cell::default(); cols as usize * rows as usize],
        }
    }

    pub fn row(&self, y: u16) -> &[Cell] {
        let start = y as usize * self.cols as usize;
        &self.cells[start..start + self.cols as usize]
    }

    /// Each row with trailing blanks removed.
    pub fn lines(&self) -> Vec<String> {
        (0..self.rows)
            .map(|y| {
                let s: String = self.row(y).iter().map(|c| c.ch).collect();
                s.trim_end().to_string()
            })
            .collect()
    }

    /// The rows as text, newline separated, with trailing blank rows dropped
    /// so the output matches what `capture` has always printed.
    pub fn to_text(&self) -> String {
        let mut lines = self.lines();
        while lines.last().is_some_and(|l| l.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// The attributes of row `y` as two hex digits per cell.
    pub fn attr_hex(&self, y: u16) -> String {
        use std::fmt::Write;
        let mut s = String::with_capacity(self.cols as usize * 2);
        for c in self.row(y) {
            let _ = write!(s, "{:02X}", c.attr);
        }
        s
    }

    /// JSON with untrimmed `lines` and parallel `attrs`, so column `x` of
    /// `lines[y]` is described by `attrs[y][2x..2x+2]`.
    ///
    /// Hand-written to keep the dependency list as small as the rest of the
    /// crate; the shape is fixed and flat.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{{\"cols\":{},\"rows\":{},\"history\":{},\"cursor\":{{\"x\":{},\"y\":{}}},\"lines\":[",
            self.cols,
            self.rows - self.history,
            self.history,
            self.cursor_x,
            self.cursor_y
        ));
        for y in 0..self.rows {
            if y > 0 {
                out.push(',');
            }
            let s: String = self.row(y).iter().map(|c| c.ch).collect();
            push_json_string(&mut out, &s);
        }
        out.push_str("],\"attrs\":[");
        for y in 0..self.rows {
            if y > 0 {
                out.push(',');
            }
            out.push('"');
            out.push_str(&self.attr_hex(y));
            out.push('"');
        }
        out.push_str("]}");
        out
    }

    /// Wire encoding: five u16 header fields, then `ch u32, attr u8` per cell.
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(10 + self.cells.len() * 5);
        for n in [
            self.cols,
            self.rows,
            self.history,
            self.cursor_x,
            self.cursor_y,
        ] {
            v.extend_from_slice(&n.to_le_bytes());
        }
        for c in &self.cells {
            v.extend_from_slice(&(c.ch as u32).to_le_bytes());
            v.push(c.attr);
        }
        v
    }

    pub fn decode(b: &[u8]) -> Option<Snapshot> {
        if b.len() < 10 {
            return None;
        }
        let u = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let (cols, rows, history, cursor_x, cursor_y) = (u(0), u(2), u(4), u(6), u(8));
        let n = cols as usize * rows as usize;
        let body = &b[10..];
        if body.len() != n * 5 || history > rows {
            return None;
        }
        let cells = body
            .chunks_exact(5)
            .map(|c| Cell {
                ch: char::from_u32(u32::from_le_bytes([c[0], c[1], c[2], c[3]])).unwrap_or('?'),
                attr: c[4],
            })
            .collect();
        Some(Snapshot {
            cols,
            rows,
            history,
            cursor_x,
            cursor_y,
            cells,
        })
    }

    /// Builds a snapshot from the vt100 model of a ConPTY session.
    ///
    /// `scrollback` rows of history above the screen are included when the
    /// model has them. vt100 only exposes history through its scrollback
    /// *offset*, so this pages the view upward and restores it afterwards.
    pub fn from_vt100(parser: &mut vt100::Parser, scrollback: usize) -> Snapshot {
        let (rows, cols) = parser.screen().size();
        let (cy, cx) = parser.screen().cursor_position();

        let mut out: Vec<Cell> = Vec::new();
        // vt100 0.15 computes `rows - scrollback_offset` unchecked, so an
        // offset larger than the screen panics. Make the model temporarily
        // taller instead: blank rows are added at the bottom and removed
        // again, and with the width unchanged nothing is rewrapped, so the
        // round trip leaves the content exactly as it was. At offset `n` the
        // first `n` visible rows are then the most recent `n` history rows.
        let want = scrollback.min((u16::MAX - rows) as usize) as u16;
        let mut available = 0u16;
        if want > 0 {
            parser.set_size(rows + want, cols);
            parser.set_scrollback(want as usize);
            available = parser.screen().scrollback() as u16;
            for y in 0..available {
                push_vt_row(&mut out, parser.screen(), y, cols);
            }
            parser.set_scrollback(0);
            parser.set_size(rows, cols);
        }
        for y in 0..rows {
            push_vt_row(&mut out, parser.screen(), y, cols);
        }

        Snapshot {
            cols,
            rows: rows + available,
            history: available,
            cursor_x: cx,
            cursor_y: cy,
            cells: out,
        }
    }
}

fn push_vt_row(out: &mut Vec<Cell>, screen: &vt100::Screen, y: u16, cols: u16) {
    for x in 0..cols {
        let cell = screen.cell(y, x);
        let ch = cell
            .map(|c| c.contents())
            .and_then(|s| s.chars().next())
            .unwrap_or(' ');
        let attr = cell.map_or(DEFAULT_ATTR, vt_attr);
        out.push(Cell { ch, attr });
    }
}

/// Maps a vt100 cell's colours onto a console attribute byte.
fn vt_attr(cell: &vt100::Cell) -> u8 {
    let mut fg = match cell.fgcolor() {
        vt100::Color::Default => 0x7,
        c => to_console16(c),
    };
    let mut bg = match cell.bgcolor() {
        vt100::Color::Default => 0x0,
        c => to_console16(c),
    };
    // Bold on a basic colour is how most terminals show "bright".
    if cell.bold() && fg < 8 {
        fg |= 8;
    }
    if cell.inverse() {
        std::mem::swap(&mut fg, &mut bg);
    }
    (bg << 4) | fg
}

/// Converts an indexed or RGB colour to the nearest console colour.
fn to_console16(c: vt100::Color) -> u8 {
    let (r, g, b) = match c {
        vt100::Color::Default => return 7,
        vt100::Color::Idx(i) if i < 16 => return ansi_to_console(i),
        vt100::Color::Idx(i) => xterm256_rgb(i),
        vt100::Color::Rgb(r, g, b) => (r, g, b),
    };
    nearest16(r, g, b)
}

/// ANSI orders colours `BGR` (1 is red); the console orders them `RGB`
/// (4 is red). Swap bits 0 and 2, keep intensity.
pub fn ansi_to_console(i: u8) -> u8 {
    let base = i & 7;
    let swapped = ((base & 1) << 2) | (base & 2) | ((base & 4) >> 2);
    swapped | (i & 8)
}

fn xterm256_rgb(i: u8) -> (u8, u8, u8) {
    if i >= 232 {
        let v = 8 + (i - 232) * 10;
        return (v, v, v);
    }
    let i = i - 16;
    let level = |n: u8| if n == 0 { 0 } else { 55 + n * 40 };
    (level(i / 36), level((i / 6) % 6), level(i % 6))
}

/// The classic console palette, indexed by console colour.
const CONSOLE_RGB: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (0, 0, 128),
    (0, 128, 0),
    (0, 128, 128),
    (128, 0, 0),
    (128, 0, 128),
    (128, 128, 0),
    (192, 192, 192),
    (128, 128, 128),
    (0, 0, 255),
    (0, 255, 0),
    (0, 255, 255),
    (255, 0, 0),
    (255, 0, 255),
    (255, 255, 0),
    (255, 255, 255),
];

fn nearest16(r: u8, g: u8, b: u8) -> u8 {
    let dist = |&(pr, pg, pb): &(u8, u8, u8)| {
        let d = |a: u8, b: u8| (a as i32 - b as i32).pow(2);
        d(r, pr) + d(g, pg) + d(b, pb)
    };
    CONSOLE_RGB
        .iter()
        .enumerate()
        .min_by_key(|(_, p)| dist(p))
        .map_or(7, |(i, _)| i as u8)
}

fn push_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Snapshot {
        let mut s = Snapshot::blank(3, 2);
        s.cells[0] = Cell {
            ch: 'a',
            attr: 0x1E,
        };
        s.cells[1] = Cell {
            ch: '"',
            attr: 0x70,
        };
        s.cursor_x = 1;
        s
    }

    #[test]
    fn wire_roundtrip() {
        let s = sample();
        assert_eq!(Snapshot::decode(&s.encode()), Some(s));
    }

    #[test]
    fn decode_rejects_a_body_of_the_wrong_length() {
        let mut bytes = sample().encode();
        bytes.pop();
        assert_eq!(Snapshot::decode(&bytes), None);
    }

    #[test]
    fn text_trims_trailing_blanks_and_blank_rows() {
        assert_eq!(sample().to_text(), "a\"");
    }

    #[test]
    fn json_escapes_and_keeps_columns_aligned() {
        let json = sample().to_json();
        assert!(json.contains(r#""lines":["a\" ","   "]"#), "{json}");
        assert!(json.contains(r#""attrs":["1E7007","070707"]"#), "{json}");
        assert!(json.contains(r#""cursor":{"x":1,"y":0}"#), "{json}");
    }

    #[test]
    fn ansi_colours_map_to_console_order() {
        assert_eq!(ansi_to_console(1), 4); // red
        assert_eq!(ansi_to_console(4), 1); // blue
        assert_eq!(ansi_to_console(3), 6); // yellow
        assert_eq!(ansi_to_console(11), 14); // bright yellow
        assert_eq!(ansi_to_console(7), 7);
    }

    #[test]
    fn vt100_colours_become_console_attributes() {
        let mut p = vt100::Parser::new(2, 10, 0);
        // Bright yellow on blue, then reverse video.
        p.process(b"\x1b[93;44mZ\x1b[0m\x1b[7mR");
        let s = Snapshot::from_vt100(&mut p, 0);
        assert_eq!(
            s.cells[0],
            Cell {
                ch: 'Z',
                attr: 0x1E
            }
        );
        assert_eq!(
            s.cells[1],
            Cell {
                ch: 'R',
                attr: 0x70
            }
        );
        assert_eq!(s.cells[2].attr, DEFAULT_ATTR);
    }

    #[test]
    fn rgb_maps_to_the_nearest_console_colour() {
        assert_eq!(to_console16(vt100::Color::Rgb(250, 5, 5)), 12);
        assert_eq!(to_console16(vt100::Color::Idx(21)), 9); // 256-colour blue
    }

    #[test]
    fn scrollback_rows_precede_the_screen() {
        let mut p = vt100::Parser::new(2, 5, 100);
        p.process(b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
        let s = Snapshot::from_vt100(&mut p, 10);
        assert_eq!(s.history, 3);
        assert_eq!(s.lines(), vec!["one", "two", "three", "four", "five"]);
        // The live view is restored.
        assert_eq!(p.screen().scrollback(), 0);
        assert_eq!(p.screen().contents(), "four\nfive");
    }

    #[test]
    fn scrollback_pages_when_history_exceeds_the_screen() {
        let mut p = vt100::Parser::new(2, 4, 100);
        for i in 0..9 {
            p.process(format!("{i}\r\n").as_bytes());
        }
        let s = Snapshot::from_vt100(&mut p, 5);
        assert_eq!(s.history, 5);
        assert_eq!(s.lines(), vec!["3", "4", "5", "6", "7", "8", ""]);
    }
}
