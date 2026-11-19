//! The emulated screen of one terminal: what `terminals.read` returns,
//! what `terminals.wait` matches, how keys are encoded for the modes the
//! program set, and the redraw an attaching frontend gets.
//! It parses sandbox output with libghostty-vt.
use goblins_protocol::contract;
use goblins_runtime::terminal::{self, LINE_CAP, SCROLLBACK, Source, TEXT_CAP};
use libghostty_vt::{
    Error, Terminal,
    fmt::{Extras, Format, Formatter},
    mode::Mode,
    point::{Coordinate, Point},
    render::{CursorVisualStyle, RenderState},
    screen::{CellWide, Screen as ActiveScreen},
};
use serde_json::{Value, json};
use std::{cell::RefCell, rc::Rc};

pub struct Screen {
    terminal: Terminal<'static, 'static>,
    replies: Rc<RefCell<Vec<u8>>>,
}

/// What `wait` looks for in a line.
pub enum Matcher {
    Text(String),
    Regex(regex::Regex),
}
impl Matcher {
    fn matches(&self, line: &str) -> bool {
        match self {
            Matcher::Text(text) => line.contains(text.as_str()),
            Matcher::Regex(regex) => regex.is_match(line),
        }
    }
}

/// Keep the newest lines of `lines` within `limit` bytes of text; says
/// whether anything was cut.
fn cap(lines: &[String], limit: usize) -> (String, bool) {
    let mut size = 0;
    let mut first = lines.len();
    while first > 0 {
        let add = lines[first - 1].len() + usize::from(first < lines.len());
        if size + add > limit {
            break;
        }
        size += add;
        first -= 1;
    }
    let mut text = lines[first..].join("\n");
    let mut cut = first > 0;
    if first == lines.len() && !lines.is_empty() {
        // Not even the newest line fits: keep its end.
        let last = &lines[first - 1];
        let mut start = last.len() - limit.min(last.len());
        while !last.is_char_boundary(start) {
            start += 1;
        }
        text = last[start..].to_string();
        cut = true;
    }
    (text, cut)
}

/// A line cut to `limit` bytes, at a character boundary.
fn clip(line: &str, limit: usize) -> (String, bool) {
    if line.len() <= limit {
        return (line.to_string(), false);
    }
    let mut end = limit;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    (line[..end].to_string(), true)
}

fn drop_blank(lines: &mut Vec<String>) {
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
}

impl Screen {
    pub fn new(rows: u16, cols: u16) -> Self {
        let replies = Rc::new(RefCell::new(Vec::new()));
        let mut terminal = Terminal::new(cols, rows).expect("create Ghostty terminal");
        terminal
            .set_scrollback_max_lines(Some(SCROLLBACK))
            .expect("set Ghostty scrollback limit");
        // Use the line limit for retention instead of Ghostty's default byte limit.
        terminal
            .set_scrollback_max_bytes(Some(usize::MAX))
            .expect("use Ghostty scrollback line limit");
        terminal
            .set_continuation_max_bytes(65536)
            .expect("set Ghostty continuation limit");
        let reply_sink = Rc::clone(&replies);
        terminal
            .on_pty_write(move |_, bytes| reply_sink.borrow_mut().extend_from_slice(bytes))
            .expect("register Ghostty PTY reply callback");
        Self { terminal, replies }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.terminal.vt_write(bytes);
    }

    /// The answers to the queries fed since this was last called.
    pub fn take_replies(&mut self) -> Vec<u8> {
        self.replies.take()
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if (rows, cols) != self.size() {
            self.terminal
                .resize(cols, rows, 0, 0)
                .expect("resize Ghostty terminal");
        }
    }

    pub fn alternate(&self) -> bool {
        self.terminal.active_screen().expect("read Ghostty screen") == ActiveScreen::Alternate
    }

    pub fn size(&self) -> (u16, u16) {
        (
            self.terminal.rows().expect("read Ghostty rows"),
            self.terminal.cols().expect("read Ghostty columns"),
        )
    }

    /// One physical row in either the active area or the entire screen.
    fn row(&self, point: fn(Coordinate) -> Point, y: u32, cols: u16) -> (String, bool) {
        let first = self
            .terminal
            .grid_ref(point(Coordinate { x: 0, y }))
            .expect("read Ghostty row");
        let row = first.row().expect("read Ghostty row attributes");
        let wrapped = row.is_wrapped().expect("read Ghostty wrap");
        let mut text = String::new();
        let mut graphemes = ['\0'; 16];
        for x in 0..cols {
            let grid = self
                .terminal
                .grid_ref(point(Coordinate { x, y }))
                .expect("read Ghostty cell");
            let cell = grid.cell().expect("read Ghostty cell attributes");
            if matches!(
                cell.wide().expect("read Ghostty cell width"),
                CellWide::SpacerHead | CellWide::SpacerTail
            ) {
                continue;
            }
            match grid.graphemes(&mut graphemes) {
                Ok(0) => text.push(' '),
                Ok(len) => text.extend(&graphemes[..len]),
                Err(Error::OutOfSpace { required }) => {
                    let mut cluster = vec!['\0'; required];
                    let len = grid
                        .graphemes(&mut cluster)
                        .expect("read Ghostty grapheme cluster");
                    text.extend(&cluster[..len]);
                }
                Err(error) => panic!("read Ghostty graphemes: {error}"),
            }
        }
        text.truncate(text.trim_end_matches(' ').len());
        (text, wrapped)
    }

    fn rows(&self) -> impl DoubleEndedIterator<Item = (String, bool)> + '_ {
        let (rows, cols) = self.size();
        (0..u32::from(rows)).map(move |y| self.row(Point::Active, y, cols))
    }

    /// The screen's lines without trailing blank ones.
    pub fn visible(&self) -> Vec<String> {
        let mut lines: Vec<_> = self.rows().map(|(text, _)| text).collect();
        drop_blank(&mut lines);
        lines
    }

    /// The last `want` lines of scrollback and screen, with soft wraps joined.
    pub fn recent(&mut self, want: usize) -> Vec<String> {
        let (_, cols) = self.size();
        let total = self
            .terminal
            .total_rows()
            .expect("count Ghostty screen rows") as u32;
        let mut upward = Vec::new();
        let mut ends = 0;
        let mut seen_text = false;
        for y in (0..total).rev() {
            let (text, wrapped) = self.row(Point::Screen, y, cols);
            seen_text |= !text.trim().is_empty();
            if !wrapped && seen_text {
                if ends == want {
                    break;
                }
                ends += 1;
            }
            upward.push((text, wrapped));
        }

        let mut lines = Vec::new();
        let mut line = String::new();
        for (text, wrapped) in upward.into_iter().rev() {
            line.push_str(&text);
            if !wrapped {
                lines.push(std::mem::take(&mut line));
            }
        }
        if !line.is_empty() {
            lines.push(line);
        }
        drop_blank(&mut lines);
        let skip = lines.len().saturating_sub(want);
        lines.drain(..skip);
        lines
    }
    /// What `terminals.read` answers.
    pub fn read(&mut self, source: Source, lines: Option<usize>) -> Value {
        let lines = lines.unwrap_or(terminal::DEFAULT_LINES);
        let all = match source {
            Source::Visible => self.visible(),
            Source::Recent => self.recent(lines),
        };
        let (rows, cols) = self.size();
        let (text, cut) = cap(&all, TEXT_CAP);
        json!(contract::TerminalReadResult {
            text,
            alternate_screen: self.alternate(),
            rows,
            cols,
            truncated: cut.then_some(true)
        })
    }
    /// The newest line `matcher` matches among the screen's rows and the
    /// last `lines` recent lines, cut to the line limit.
    pub fn find(&mut self, matcher: &Matcher, lines: usize) -> Option<(String, bool)> {
        let on_screen = self
            .rows()
            .rev()
            .map(|(text, _)| text)
            .find(|line| matcher.matches(line));
        on_screen
            .or_else(|| {
                self.recent(lines)
                    .into_iter()
                    .rev()
                    .find(|line| matcher.matches(line))
            })
            .map(|line| clip(&line, LINE_CAP))
    }
    /// The text a terminal that exited leaves: its last `lines` lines.
    pub fn last_lines(&mut self, lines: usize) -> (String, bool) {
        cap(&self.recent(lines), TEXT_CAP)
    }
    /// The bytes to write to the program for `text`, then `keys`, in the
    /// input modes it set.
    pub fn input(&self, text: Option<&str>, keys: &[String]) -> Vec<u8> {
        let mut bytes = vec![];
        if let Some(text) = text {
            let paste = self
                .terminal
                .mode(Mode::BRACKETED_PASTE)
                .expect("read Ghostty paste mode");
            if paste {
                bytes.extend_from_slice(b"\x1b[200~");
            }
            bytes.extend_from_slice(text.as_bytes());
            if paste {
                bytes.extend_from_slice(b"\x1b[201~");
            }
        }
        let application_cursor = self
            .terminal
            .mode(Mode::DECCKM)
            .expect("read Ghostty cursor mode");
        for key in keys {
            if let Some(key) = terminal::key_bytes(key, application_cursor) {
                bytes.extend_from_slice(&key);
            }
        }
        bytes
    }
    /// Replay the active screen, plus the history on the primary screen.
    pub fn redraw(&mut self) -> Vec<u8> {
        let mut bytes = Vec::new();
        if self.alternate() {
            bytes.extend_from_slice(b"\x1b[?1049h");
        }
        // History needs the trailing blank rows to scroll into place before
        // the cursor moves. Trimming keeps replays without history compact.
        let trim = self
            .terminal
            .scrollback_rows()
            .expect("count Ghostty scrollback rows")
            == 0;
        bytes.extend_from_slice(
            &Formatter::new(&self.terminal)
                .and_then(|mut formatter| {
                    formatter
                        .set_format(Format::Vt)?
                        .set_trim(trim)?
                        .set_unwrap(true)?
                        .set_extras(
                            Extras::ALL - Extras::PALETTE - Extras::PWD - Extras::TABSTOPS,
                        )?;
                    formatter.format_alloc(None)
                })
                .expect("format Ghostty redraw"),
        );

        // The VT formatter omits the window title and cursor shape.
        let title = self.terminal.title().expect("read Ghostty window title");
        if !title.is_empty() {
            let title: String = String::from_utf8_lossy(title)
                .chars()
                .filter(|ch| !ch.is_control())
                .collect();
            bytes.extend_from_slice(format!("\x1b]2;{title}\x1b\\").as_bytes());
        }
        let mut render = RenderState::new().expect("create Ghostty render state");
        let state = render
            .update(&self.terminal)
            .expect("read Ghostty render state");
        let shape = match state
            .cursor_visual_style()
            .expect("read Ghostty cursor shape")
        {
            CursorVisualStyle::Underline => 3,
            CursorVisualStyle::Bar => 5,
            _ => 1,
        };
        let steady = u8::from(!state.cursor_blinking().expect("read Ghostty cursor blink"));
        bytes.extend_from_slice(format!("\x1b[{} q", shape + steady).as_bytes());

        // Release a frame that was open when the frontend attached.
        if self
            .terminal
            .mode(Mode::SYNC_OUTPUT)
            .expect("read Ghostty synchronized output")
        {
            bytes.extend_from_slice(b"\x1b[?2026l");
        }
        bytes
    }

    /// The whole terminal state as a libghostty-vt binary snapshot, for a
    /// frontend that restores it itself instead of parsing a redraw. It
    /// fails while an escape sequence longer than the continuation limit is
    /// unfinished, until the program ends it.
    pub fn snapshot(&mut self) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        match self.terminal.encode_snapshot(&mut bytes) {
            Ok(()) => Ok(bytes),
            Err(Error::InvalidValue) => Err(
                "the terminal is in the middle of a long escape sequence; attach again".into(),
            ),
            Err(error) => Err(format!("cannot take the terminal's snapshot: {error}")),
        }
    }
}

#[cfg(test)]
impl Screen {
    /// The screen a frontend restores from `snapshot`.
    pub fn restore(snapshot: &[u8]) -> Self {
        let terminal = libghostty_vt::snapshot::Decoder::new_buf(snapshot)
            .and_then(|decoder| decoder.decode())
            .expect("decode Ghostty snapshot");
        Self {
            terminal,
            replies: Rc::default(),
        }
    }
    /// What a restored frontend must agree on: the text with its history,
    /// the size, the cursor, the active screen and the input modes.
    pub fn state(&mut self) -> (Vec<String>, (u16, u16), (u16, u16), bool, [bool; 2]) {
        (
            self.recent(SCROLLBACK),
            self.size(),
            (
                self.terminal.cursor_y().unwrap(),
                self.terminal.cursor_x().unwrap(),
            ),
            self.alternate(),
            [Mode::BRACKETED_PASTE, Mode::DECCKM].map(|mode| self.terminal.mode(mode).unwrap()),
        )
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn generated_read_results_preserve_public_dimension_boundaries() {
        let mut screen = super::Screen::new(1, 500);
        for (rows, cols) in [(1, 500), (500, 1), (1, 1)] {
            screen.resize(rows, cols);
            let result = screen.read(goblins_runtime::terminal::Source::Visible, None);
            assert_eq!(result["rows"].as_u64(), Some(u64::from(rows)));
            assert_eq!(result["cols"].as_u64(), Some(u64::from(cols)));
        }
    }

    use super::*;

    fn screen(rows: u16, cols: u16, output: &str) -> Screen {
        let mut screen = Screen::new(rows, cols);
        screen.feed(output.as_bytes());
        screen
    }

    fn cursor(screen: &Screen) -> (u16, u16) {
        (
            screen.terminal.cursor_y().unwrap(),
            screen.terminal.cursor_x().unwrap(),
        )
    }

    #[test]
    fn visible_is_the_screen_without_trailing_blank_lines() {
        let mut s = screen(4, 20, "one\r\ntwo\r\n");
        assert_eq!(s.visible(), ["one", "two"]);
        let read = s.read(Source::Visible, None);
        assert_eq!(read["text"], "one\ntwo");
        assert_eq!(read["rows"], 4);
        assert_eq!(read["cols"], 20);
        assert_eq!(read["alternate_screen"], false);
        assert!(read.get("truncated").is_none());
    }

    #[test]
    fn recent_includes_scrollback_and_joins_wrapped_rows() {
        let mut s = screen(3, 5, "0123456789\r\nb\r\nc\r\nd\r\ne\r\n");
        // The long line wraps over two rows and has scrolled away.
        assert_eq!(s.recent(100), ["0123456789", "b", "c", "d", "e"]);
        assert_eq!(s.recent(2), ["d", "e"]);
        assert_eq!(s.recent(5)[0], "0123456789");
        assert_eq!(s.visible(), ["d", "e"]);
        let read = s.read(Source::Recent, Some(3));
        assert_eq!(read["text"], "c\nd\ne");

        // Erasing the last row of a wrapped line still leaves one recent line.
        let mut erased = screen(3, 5, "older\r\nABCDEf\x1b[2K");
        assert_eq!(erased.recent(1), ["ABCDE"]);
    }

    #[test]
    fn recent_pages_through_more_scrollback_than_the_screen_holds() {
        let output: String = (0..200).map(|i| format!("line {i}\r\n")).collect();
        let mut s = screen(5, 20, &output);
        let recent = s.recent(120);
        assert_eq!(recent.len(), 120);
        assert_eq!(recent[0], "line 80");
        assert_eq!(recent[119], "line 199");
        assert_eq!(s.recent(5000).len(), 200);
        assert_eq!(
            s.row(Point::Viewport, 0, 20),
            s.row(Point::Active, 0, 20),
            "the viewport stays at the active area"
        );
    }

    #[test]
    fn recent_retains_dense_scrollback_up_to_the_line_limit() {
        let mut s = Screen::new(10, 120);
        let fill = "x".repeat(114);
        let last = SCROLLBACK + 99;
        for i in 0..=last {
            s.feed(format!("{i:04} {fill}\r\n").as_bytes());
        }
        let recent = s.recent(SCROLLBACK);
        // Ghostty evicts complete pages and can fall short of the line limit.
        // The default byte cap would retain only about 500 of these lines.
        assert!((SCROLLBACK * 9 / 10..=SCROLLBACK).contains(&recent.len()));
        assert_eq!(recent[0], format!("{:04} {fill}", last + 1 - recent.len()));
        assert_eq!(recent.last().unwrap(), &format!("{last:04} {fill}"));
    }

    #[test]
    fn the_alternate_screen_is_read_like_the_visible_one() {
        let mut s = screen(3, 20, "history\r\n\x1b[?1049h\x1b[Hfull\x1b[2;1Hscreen");
        assert!(s.alternate());
        assert_eq!(s.recent(100), ["full", "screen"]);
        let read = s.read(Source::Recent, None);
        assert_eq!(read["alternate_screen"], true);
        assert_eq!(read["text"], "full\nscreen");
        s.feed(b"\x1b[?1049l");
        assert!(!s.alternate());
        assert_eq!(s.recent(100), ["history"]);
    }

    #[test]
    fn long_text_keeps_the_newest_lines_and_says_so() {
        let lines: Vec<String> = (0..10).map(|i| format!("{i}{}", "x".repeat(9))).collect();
        let (text, cut) = cap(&lines, 35);
        assert!(cut);
        assert_eq!(text.lines().count(), 3);
        assert!(text.ends_with(&lines[9]));
        assert_eq!(cap(&lines, 1000), (lines.join("\n"), false));
        // A single line longer than the limit keeps its end.
        let (text, cut) = cap(&["é".repeat(10)], 5);
        assert!(cut);
        assert_eq!(text, "éé");
        assert_eq!(clip("ééé", 3), ("é".to_string(), true));
        assert_eq!(clip("abc", 3), ("abc".to_string(), false));
    }

    #[test]
    fn find_sees_the_newest_match_on_screen_and_in_scrollback() {
        let mut s = screen(3, 20, "done 1\r\nx\r\ny\r\nz\r\nw\r\n");
        let text = |t: &str| Matcher::Text(t.into());
        assert_eq!(s.find(&text("done"), 100), Some(("done 1".into(), false)));
        assert_eq!(s.find(&text("done"), 3), None, "outside the lines asked");
        assert_eq!(s.find(&text("nothing"), 100), None);
        let anchored = Matcher::Regex(terminal::compile("^y$").unwrap());
        assert_eq!(s.find(&anchored, 100).unwrap().0, "y");
        let echoed = screen(3, 40, "$ echo done\r\ndone\r\n");
        let mut echoed = echoed;
        let anchored = Matcher::Regex(terminal::compile("^done$").unwrap());
        assert_eq!(echoed.find(&anchored, 100).unwrap().0, "done");
        let long = "y".repeat(LINE_CAP + 10);
        let mut s = screen(2, 5000, &format!("{long}\r\n"));
        let (line, cut) = s.find(&text("y"), 10).unwrap();
        assert!(cut);
        assert_eq!(line.len(), LINE_CAP);
    }

    #[test]
    fn input_follows_the_modes_the_program_set() {
        let mut s = screen(3, 20, "");
        let keys = ["up".to_string(), "enter".to_string()];
        assert_eq!(s.input(Some("ls"), &keys), b"ls\x1b[A\r");
        s.feed(b"\x1b[?1h\x1b[?2004h");
        assert_eq!(
            s.input(Some("a\nb"), &keys),
            b"\x1b[200~a\nb\x1b[201~\x1bOA\r"
        );
        s.feed(b"\x1b[?1l\x1b[?2004l");
        assert_eq!(s.input(None, &keys), b"\x1b[A\r");
    }

    #[test]
    fn the_redraw_reproduces_the_screen_and_the_modes() {
        let output: String = (0..20)
            .map(|i| format!("\x1b[3{}mline {i}\x1b[m\r\n", i % 8))
            .collect();
        let mut source = screen(5, 20, &format!("{output}\x1b[?2004h\x1b[?1h"));
        let redraw = source.redraw();
        let mut frontend = Screen::new(5, 20);
        frontend.feed(&redraw);
        assert_eq!(frontend.visible(), source.visible());
        assert_eq!(cursor(&frontend), cursor(&source));
        assert!(frontend.terminal.mode(Mode::BRACKETED_PASTE).unwrap());
        assert!(frontend.terminal.mode(Mode::DECCKM).unwrap());
        // The scrollback is in the frontend's, with colour, above the screen.
        let history = frontend.recent(1000);
        assert_eq!(history[0], "line 0");
        assert_eq!(history[19], "line 19");
        let cell_style = |screen: &Screen| {
            screen
                .terminal
                .grid_ref(Point::Screen(Coordinate { x: 0, y: 0 }))
                .unwrap()
                .style()
                .unwrap()
        };
        assert_ne!(
            cell_style(&source).fg_color,
            libghostty_vt::style::StyleColor::None
        );
        assert_eq!(cell_style(&frontend).fg_color, cell_style(&source).fg_color);
    }

    #[test]
    fn shrinking_keeps_the_newest_output_and_its_cursor() {
        // 39 lines and READY on the bottom row of 40, then a 24-row frontend.
        let output: String = (0..39).map(|i| format!("line {i}\r\n")).collect();
        let mut s = screen(40, 120, &format!("{output}READY"));
        s.resize(24, 100);
        assert_eq!(s.size(), (24, 100));
        assert_eq!(s.visible().last().unwrap(), "READY");
        assert_eq!(s.recent(100).first().unwrap(), "line 0");
        assert_eq!(s.recent(100).last().unwrap(), "READY");
        assert_eq!(cursor(&s), (23, 5));
        // Output that follows lands after it.
        s.feed(b"!\r\nnext");
        assert_eq!(&s.recent(2), &["READY!", "next"]);
        // Text with blank rows below it does not move into the scrollback.
        let mut s = screen(40, 120, "one\r\ntwo");
        s.resize(24, 120);
        assert_eq!(s.visible(), ["one", "two"]);
        assert_eq!(s.recent(100), ["one", "two"]);
        // Growing and an unchanged size change nothing.
        s.resize(24, 120);
        s.resize(50, 130);
        assert_eq!(s.visible(), ["one", "two"]);
    }

    #[test]
    fn the_redraw_keeps_a_pending_wrap() {
        let mut source = screen(3, 5, "ABCDE");
        let mut frontend = Screen::new(3, 5);
        frontend.feed(&source.redraw());
        source.feed(b"F");
        frontend.feed(b"F");
        assert_eq!(source.visible(), ["ABCDE", "F"]);
        assert_eq!(frontend.visible(), source.visible());
        // After history, with colour, and below the cursor row.
        let mut source = screen(3, 5, "x\r\ny\r\n\x1b[31mABCDE\x1b[m\r\n\x1b[1;5H");
        let mut frontend = Screen::new(3, 5);
        frontend.feed(&source.redraw());
        source.feed(b"Z");
        frontend.feed(b"Z");
        // Scrollback replays above the active area and the cursor stays on
        // the same line.
        assert_eq!(frontend.recent(100), source.recent(100));
        assert_eq!(frontend.recent(100), ["x", "y   Z", "ABCDE"]);
    }

    #[test]
    fn the_redraw_keeps_a_pending_wrap_after_a_wide_character() {
        let mut source = screen(3, 5, "abc界");
        let mut frontend = Screen::new(3, 5);
        frontend.feed(&source.redraw());
        source.feed(b"F");
        frontend.feed(b"F");
        assert_eq!(source.visible(), ["abc界", "F"]);
        assert_eq!(frontend.visible(), source.visible());
    }

    #[test]
    fn a_resize_in_the_middle_of_a_sequence_does_not_cut_into_it() {
        for (cut, rest) in [
            ("ab\x1b[", "31mred"),
            ("ab\x1b[3", "1mred"),
            ("ab\x1b]0;ti", "tle\x07red"),
            ("ab\x1b]0;title\x1b", "\\red"),
            ("ab\x1bPq", "\x07PAYLOAD\x1b\\red"),
        ] {
            let mut s = screen(40, 120, "");
            let output: String = (0..39).map(|i| format!("l{i}\r\n")).collect();
            s.feed(output.as_bytes());
            s.feed(cut.as_bytes());
            s.resize(24, 100);
            assert_eq!(s.size(), (24, 100), "{cut:?}");
            s.feed(rest.as_bytes());
            assert_eq!(s.visible().last().unwrap(), "abred", "{cut:?}");
            assert_eq!(s.recent(100)[0], "l0", "{cut:?}");
        }
        // A character split across reads is whole after the resize.
        let bytes = "界".as_bytes();
        let mut s = screen(5, 20, "");
        s.feed(&bytes[..2]);
        s.resize(3, 20);
        s.feed(&bytes[2..]);
        assert_eq!(s.visible(), ["界"]);
        // The output after the sequence that ends it is read at the new size.
        let mut s = Screen::new(3, 10);
        s.feed(b"\x1b[");
        s.resize(2, 5);
        s.feed(b"31mABCDEFGHI");
        assert_eq!(s.visible(), ["ABCDE", "FGHI"]);
        // An open DCS keeps ignoring its payload, a BEL included.
        let mut s = screen(3, 20, "one\r\ntwo\r\nold\x1bPq");
        s.resize(2, 20);
        s.feed(b"\x07PAYLOAD\x1b\\Z");
        assert_eq!(s.visible(), ["two", "oldZ"]);
    }

    #[test]
    fn the_alternate_screen_redraw_enters_it_first() {
        let mut source = screen(3, 20, "shell\r\n\x1b[?1049h\x1b[2;3Hvim\x1b[?25l");
        let redraw = source.redraw();
        assert!(redraw.starts_with(b"\x1b[?1049h"));
        let mut frontend = Screen::new(3, 20);
        frontend.feed(&redraw);
        assert!(frontend.alternate());
        assert_eq!(frontend.visible(), source.visible());
        assert!(!frontend.terminal.mode(Mode::CURSOR_VISIBLE).unwrap());
        frontend.feed(b"\x1b[?1049l");
        assert_eq!(
            frontend.visible(),
            Vec::<String>::new(),
            "no normal-screen history"
        );
    }

    #[test]
    fn alternate_screen_redraw_keeps_content_in_a_shorter_frontend() {
        let mut source = screen(40, 120, "\x1b[?1049h\x1b[2J\x1b[3;5HFULLSCREEN");
        let mut frontend = Screen::new(16, 80);
        frontend.feed(&source.redraw());
        assert!(frontend.alternate());
        assert_eq!(frontend.visible(), ["", "", "    FULLSCREEN"]);
        assert_eq!(cursor(&frontend), cursor(&source));
    }
    #[test]
    fn it_answers_the_device_attributes_status_and_cursor_position_queries() {
        let mut screen = Screen::new(24, 80);
        screen.feed(b"\x1b[c");
        assert_eq!(screen.take_replies(), b"\x1b[?62;22c");
        screen.feed(b"\x1b[0c\x1b[5n");
        assert_eq!(screen.take_replies(), b"\x1b[?62;22c\x1b[0n");
        // The position is where the cursor is at that point of the output.
        screen.feed(b"\x1b[3;5H\x1b[6n");
        assert_eq!(screen.take_replies(), b"\x1b[3;5R");
        screen.feed(b"ab\x1b[6n\r\n\x1b[6n");
        assert_eq!(screen.take_replies(), b"\x1b[3;7R\x1b[4;1R");
        // At the right margin the cursor is in the last column, not past it.
        let mut narrow = Screen::new(3, 10);
        narrow.feed(b"0123456789\x1b[6n");
        assert_eq!(narrow.take_replies(), b"\x1b[1;10R");
        narrow.feed(b"x\x1b[6n");
        assert_eq!(narrow.take_replies(), b"\x1b[2;2R");
        // A query split across reads is answered once it is whole.
        screen.feed(b"\x1b[");
        assert!(screen.take_replies().is_empty());
        screen.feed(b"6");
        assert!(screen.take_replies().is_empty());
        screen.feed(b"n");
        assert_eq!(screen.take_replies(), b"\x1b[4;1R");
        // Answers are taken once.
        assert!(screen.take_replies().is_empty());
    }

    #[test]
    fn it_reports_the_current_bracketed_paste_mode() {
        let mut screen = Screen::new(24, 80);
        screen.feed(b"\x1b[?2004$p");
        assert_eq!(screen.take_replies(), b"\x1b[?2004;2$y");
        screen.feed(b"\x1b[?2004h\x1b[?2004$p");
        assert_eq!(screen.take_replies(), b"\x1b[?2004;1$y");
    }

    #[test]
    fn redraw_replays_scrollback_with_soft_wrapped_lines() {
        let mut source = screen(
            4,
            8,
            "long line in history\r\nsecond\r\nthird\r\nfourth\r\nfifth",
        );
        let mut frontend = Screen::new(4, 8);
        frontend.feed(&source.redraw());
        assert_eq!(frontend.recent(100), source.recent(100));
        assert_eq!(
            frontend.recent(100),
            ["long line in history", "second", "third", "fourth", "fifth"]
        );
    }

    #[test]
    fn redraw_restores_the_title_and_cursor_shape_and_releases_synchronized_output() {
        let mut source = screen(3, 20, "\x1b]2;terminal title\x07\x1b[6 q\x1b[?2026h");
        let redraw = source.redraw();
        let mut frontend = Screen::new(3, 20);
        frontend.feed(&redraw);
        assert_eq!(frontend.terminal.title().unwrap(), b"terminal title");
        let mut render = RenderState::new().unwrap();
        let state = render.update(&frontend.terminal).unwrap();
        assert_eq!(state.cursor_visual_style().unwrap(), CursorVisualStyle::Bar);
        assert!(!state.cursor_blinking().unwrap());
        assert!(!frontend.terminal.mode(Mode::SYNC_OUTPUT).unwrap());
        assert!(source.terminal.mode(Mode::SYNC_OUTPUT).unwrap());
    }

    #[test]
    fn multibyte_text_reads_and_redraws_unchanged() {
        let text = "* EXPERIMENTAL \u{b7} AI-GENERATED\r\n\u{65e5}\u{672c}\u{8a9e} \u{e9} \u{273b} \u{1f41f} \x1b[31mred \u{b7}\x1b[m end";
        let mut screen = Screen::new(6, 40);
        screen.feed(text.as_bytes());
        assert_eq!(screen.visible()[0], "* EXPERIMENTAL \u{b7} AI-GENERATED");
        assert_eq!(
            screen.visible()[1],
            "\u{65e5}\u{672c}\u{8a9e} \u{e9} \u{273b} \u{1f41f} red \u{b7} end"
        );
        // What a frontend that attaches sees is the same text, bytes and all.
        let mut frontend = Screen::new(6, 40);
        frontend.feed(&screen.redraw());
        assert_eq!(frontend.visible(), screen.visible());
        // Also split in the middle of a character across two reads.
        let mut split = Screen::new(6, 40);
        let bytes = text.as_bytes();
        let at = text.find('\u{b7}').unwrap() + 1;
        split.feed(&bytes[..at]);
        split.feed(&bytes[at..]);
        assert_eq!(split.visible(), screen.visible());
    }

    #[test]
    fn single_characters_are_typed_after_the_text_and_never_pasted() {
        let mut screen = Screen::new(24, 80);
        screen.feed(b"\x1b[?2004h");
        let keys = |keys: &[&str]| keys.iter().map(|k| k.to_string()).collect::<Vec<_>>();
        // Vim: `gg`, `A`, then text as a paste, then escape and `:wq`.
        assert_eq!(screen.input(None, &keys(&["g", "g", "A"])), b"ggA");
        assert_eq!(
            screen.input(Some(" START"), &keys(&["esc", ":", "w", "q", "enter"])),
            b"\x1b[200~ START\x1b[201~\x1b:wq\r"
        );
        // Keys follow the text in the order given, and a letter keeps its case.
        assert_eq!(
            screen.input(Some("x"), &keys(&["Y", "z"])),
            b"\x1b[200~x\x1b[201~Yz"
        );
        // A multi-character entry that is no key is not typed.
        assert_eq!(screen.input(None, &keys(&["gg", "g"])), b"g");
    }
}
