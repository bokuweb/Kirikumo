//! What an attached shell's bytes look like on a screen.
//!
//! The cluster owns the shell (`Cluster::attach`) and hands back the bytes it
//! prints, escapes and all; this is the half that decides what those escapes
//! mean. It is here rather than in the views because a terminal emulator has
//! behaviour worth testing — wrapping, clearing, colour, where the cursor is
//! — and none of it needs a window.
//!
//! `alacritty_terminal` does the emulating, at the version Ginka picked for
//! its own terminals so that the two are one when they meet (roadmap §4.3,
//! K6). What this adds is the shape a view can draw without knowing anything
//! about alacritty's grid, and the bytes a keystroke turns into.

// `VoidListener` is alacritty's own do-nothing sink: the events it would
// carry — bell, title changes, clipboard requests — are for a terminal
// application to act on, and this screen only draws.
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Processor};
use gpui::Keystroke;

/// A colour a program asked for.
///
/// The sixteen named ones are left named rather than resolved here: the
/// theme decides what "red" is on its own background, and a terminal that
/// hardcoded it would clash with every theme but the one it was written on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Colour {
    /// One of the 256 indexed colours; the first sixteen are the ANSI ones.
    Named(u8),
    /// An exact colour, from a truecolour escape.
    Rgb(u8, u8, u8),
}

/// How a run of characters is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    /// `None` is the theme's ordinary text colour.
    pub foreground: Option<Colour>,
    /// `None` is the terminal's own background.
    pub background: Option<Colour>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    /// Whether the cursor sits here.
    pub cursor: bool,
}

/// One character on the screen, with how it is drawn.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub text: char,
    pub style: Style,
}

/// One row of the screen.
pub type Row = Vec<Cell>;

/// A run of neighbouring characters that share a style.
///
/// What a view draws: one row is a handful of these, not a hundred cells, so
/// a screen of eighty by thirty is a few hundred elements rather than a few
/// thousand.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

/// Fold a row into its spans.
///
/// Trailing blanks are kept: a view that trimmed them would have to guess
/// where a background colour ends, and a program that painted a bar across
/// the width would lose its right-hand end.
pub fn spans(row: &[Cell]) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    for cell in row {
        match spans.last_mut() {
            Some(last) if last.style == cell.style => last.text.push(cell.text),
            _ => spans.push(Span {
                text: cell.text.to_string(),
                style: cell.style,
            }),
        }
    }
    spans
}

/// The size a terminal was told it has.
#[derive(Debug, Clone, Copy)]
struct Size {
    rows: usize,
    cols: usize,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// A terminal's screen, fed by the bytes its shell prints.
pub struct Screen {
    term: Term<VoidListener>,
    parser: Processor,
    rows: u16,
    cols: u16,
}

impl Screen {
    /// An empty screen of `cols` by `rows`.
    pub fn new(cols: u16, rows: u16) -> Self {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let size = Size {
            rows: rows as usize,
            cols: cols as usize,
        };
        Self {
            term: Term::new(Config::default(), &size, VoidListener),
            parser: Processor::new(),
            rows,
            cols,
        }
    }

    /// How many rows it has.
    pub fn rows(&self) -> u16 {
        self.rows
    }

    /// How many columns it has.
    pub fn cols(&self) -> u16 {
        self.cols
    }

    /// Feed it what the shell printed.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Tell it the panel is a different size now.
    ///
    /// The shell has to be told separately — `Tunnel::resize` — but the
    /// screen has to agree, or the text wraps at a column the shell is not
    /// using. Returns whether anything changed, so the caller knows whether
    /// the shell needs telling.
    pub fn resize(&mut self, cols: u16, rows: u16) -> bool {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if (cols, rows) == (self.cols, self.rows) {
            return false;
        }
        self.term.resize(Size {
            rows: rows as usize,
            cols: cols as usize,
        });
        self.cols = cols;
        self.rows = rows;
        true
    }

    /// The screen as rows of cells, top to bottom.
    pub fn rows_of_cells(&self) -> Vec<Row> {
        let grid = self.term.grid();
        let cursor = grid.cursor.point;
        let display_offset = grid.display_offset();
        let mut screen = Vec::with_capacity(self.rows as usize);
        for line in 0..self.rows as usize {
            let mut row = Vec::with_capacity(self.cols as usize);
            for column in 0..self.cols as usize {
                let point = Point::new(Line(line as i32), Column(column));
                let cell = &grid[point];
                row.push(Cell {
                    text: cell.c,
                    style: Style {
                        foreground: colour(cell.fg),
                        background: colour(cell.bg),
                        bold: cell.flags.contains(Flags::BOLD),
                        italic: cell.flags.contains(Flags::ITALIC),
                        underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
                        // Only while looking at the live screen: a cursor
                        // drawn over scrollback is a cursor in the wrong
                        // place.
                        cursor: display_offset == 0
                            && cursor.line.0 == line as i32
                            && cursor.column.0 == column,
                    },
                });
            }
            screen.push(row);
        }
        screen
    }

    /// The screen as plain text, trailing blanks dropped: for tests and for
    /// copying.
    pub fn text(&self) -> String {
        self.rows_of_cells()
            .iter()
            .map(|row| {
                row.iter()
                    .map(|cell| cell.text)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_string()
    }
}

/// What a cell's colour is, or `None` for the theme's default.
fn colour(from: Color) -> Option<Colour> {
    match from {
        Color::Named(NamedColor::Foreground | NamedColor::Background) => None,
        Color::Named(named) => Some(Colour::Named(named as u8)),
        Color::Spec(rgb) => Some(Colour::Rgb(rgb.r, rgb.g, rgb.b)),
        Color::Indexed(index) => Some(Colour::Named(index)),
    }
}

/// What a keystroke sends to a shell.
///
/// A shell reads bytes, so this is where a key becomes the bytes a terminal
/// would have sent: the control characters for `ctrl-`, the escape
/// sequences for the arrows and the editing keys, and the typed character
/// otherwise. Anything this does not know is not sent, because a wrong byte
/// is worse than none — and `⌘` chords are never sent at all, because they
/// are the window's, not the shell's.
pub fn keystroke_bytes(keystroke: &Keystroke) -> Option<Vec<u8>> {
    let key = keystroke.key.as_str();
    let modifiers = &keystroke.modifiers;
    if modifiers.platform {
        return None;
    }

    if modifiers.control && key.len() == 1 {
        // ctrl-a is 0x01, and so on up the alphabet; ctrl-c is what stops a
        // runaway command, which is the whole reason this branch exists.
        let letter = key.chars().next()?.to_ascii_lowercase();
        if letter.is_ascii_lowercase() {
            return Some(vec![letter as u8 - b'a' + 1]);
        }
        return match letter {
            '[' => Some(vec![0x1b]),
            '\\' => Some(vec![0x1c]),
            ']' => Some(vec![0x1d]),
            _ => None,
        };
    }

    let sequence: &[u8] = match key {
        "enter" => b"\r",
        "tab" => b"\t",
        "backspace" => b"\x7f",
        "escape" => b"\x1b",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "pageup" => b"\x1b[5~",
        "pagedown" => b"\x1b[6~",
        "delete" => b"\x1b[3~",
        "space" => b" ",
        _ => b"",
    };
    if !sequence.is_empty() {
        return Some(sequence.to_vec());
    }

    // What the keyboard actually produced, which is what carries the layout
    // and the shift state.
    keystroke
        .key_char
        .as_ref()
        .filter(|typed| !typed.is_empty())
        .map(|typed| typed.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_the_shell_prints_is_what_the_screen_shows() {
        let mut screen = Screen::new(20, 4);
        screen.feed(b"$ ls\r\nbin  etc\r\n$ ");
        assert_eq!(screen.text(), "$ ls\nbin  etc\n$");
        let rows = screen.rows_of_cells();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].len(), 20);
        // The cursor sits after the prompt on the third row.
        assert!(rows[2][2].style.cursor);
        assert!(!rows[0][0].style.cursor);
    }

    #[test]
    fn a_long_line_wraps_at_the_width_and_a_resize_reflows_it() {
        let mut screen = Screen::new(5, 3);
        screen.feed(b"abcdefgh");
        assert_eq!(screen.text(), "abcde\nfgh");
        assert!(screen.resize(10, 3));
        assert!(!screen.resize(10, 3));
        assert_eq!((screen.cols(), screen.rows()), (10, 3));
    }

    #[test]
    fn colours_and_attributes_come_through_as_style() {
        let mut screen = Screen::new(10, 1);
        screen.feed(b"\x1b[1;31mred\x1b[0m ok");
        let row = &screen.rows_of_cells()[0];
        assert_eq!(row[0].style.foreground, Some(Colour::Named(1)));
        assert!(row[0].style.bold);
        assert_eq!(row[4].style.foreground, None);
        assert!(!row[4].style.bold);
        screen.feed(b"\x1b[38;2;10;20;30mX");
        let row = &screen.rows_of_cells()[0];
        assert_eq!(row[6].style.foreground, Some(Colour::Rgb(10, 20, 30)));
    }

    #[test]
    fn a_row_folds_into_one_span_per_style() {
        let mut screen = Screen::new(8, 1);
        screen.feed(b"\x1b[32mab\x1b[0mcd");
        let row = &screen.rows_of_cells()[0];
        let spans = spans(row);
        // green "ab", then "cd" — but the cursor sits on the fifth cell, so
        // that one is its own span, and the blanks after it another.
        let texts: Vec<&str> = spans.iter().map(|span| span.text.as_str()).collect();
        assert_eq!(texts, vec!["ab", "cd", " ", "   "]);
        assert_eq!(spans[0].style.foreground, Some(Colour::Named(2)));
        assert!(spans[2].style.cursor);
    }

    #[test]
    fn a_clear_screen_clears_it() {
        let mut screen = Screen::new(10, 2);
        screen.feed(b"hello\r\nthere");
        screen.feed(b"\x1b[2J\x1b[H");
        assert_eq!(screen.text(), "");
    }

    #[test]
    fn keys_become_the_bytes_a_terminal_sends() {
        let bytes = |text: &str| keystroke_bytes(&Keystroke::parse(text).unwrap());
        assert_eq!(bytes("enter"), Some(b"\r".to_vec()));
        assert_eq!(bytes("ctrl-c"), Some(vec![3]));
        assert_eq!(bytes("ctrl-d"), Some(vec![4]));
        assert_eq!(bytes("up"), Some(b"\x1b[A".to_vec()));
        assert_eq!(bytes("backspace"), Some(b"\x7f".to_vec()));
        assert_eq!(bytes("space"), Some(b" ".to_vec()));
    }

    #[test]
    fn a_typed_character_is_what_the_keyboard_produced() {
        let mut keystroke = Keystroke::parse("a").unwrap();
        keystroke.key_char = Some("a".to_string());
        assert_eq!(keystroke_bytes(&keystroke), Some(b"a".to_vec()));
        keystroke.key_char = Some("é".to_string());
        assert_eq!(keystroke_bytes(&keystroke), Some("é".as_bytes().to_vec()));
    }

    #[test]
    fn a_window_chord_and_an_unknown_key_send_nothing() {
        assert_eq!(keystroke_bytes(&Keystroke::parse("cmd-k").unwrap()), None);
        assert_eq!(keystroke_bytes(&Keystroke::parse("f5").unwrap()), None);
        assert_eq!(keystroke_bytes(&Keystroke::parse("shift").unwrap()), None);
    }
}
