//! The emulated screen of one terminal: what `terminals.read` returns,
//! what `terminals.wait` matches, how keys are encoded for the modes the
//! program set, and the redraw an attaching frontend gets.
//! It parses untrusted sandbox output with the `vt100` crate.
use goblins_runtime::terminal::{self, LINE_CAP, SCROLLBACK, Source, TEXT_CAP};
use serde_json::{Value, json};

pub struct Screen {
    parser: vt100::Parser<Queries>,
}

/// The queries the emulator answers for a program nobody is attached to:
/// device attributes, the status report and the cursor position. They are
/// the ones a shell asks at startup and waits on; the answers are queued
/// here for the caller to send.
#[derive(Default)]
struct Queries {
    replies: Vec<u8>,
}
/// What the emulator says it is when asked for its device attributes: a
/// plain VT220-class terminal with colour (`62`) and nothing else.
const DEVICE_ATTRIBUTES: &[u8] = b"\x1b[?62;22c";
impl vt100::Callbacks for Queries {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        // Only the plain forms: `CSI > c`, `CSI ? 6 n`, `CSI 0:1 c` and the
        // rest are not ours.
        if i1.is_some() || i2.is_some() || params.len() > 1 || params.iter().any(|p| p.len() > 1) {
            return;
        }
        let parameter = params
            .first()
            .map_or(0, |p| p.first().copied().unwrap_or(0));
        match (c, parameter) {
            ('c', 0) => self.replies.extend_from_slice(DEVICE_ATTRIBUTES),
            ('n', 5) => self.replies.extend_from_slice(b"\x1b[0n"),
            ('n', 6) => {
                // After a character in the last column the emulator waits to
                // wrap with the cursor one past it; a terminal reports the
                // last column, where the cursor is drawn.
                let (row, col) = screen.cursor_position();
                let (_, cols) = screen.size();
                let col = col.min(cols.saturating_sub(1));
                self.replies
                    .extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            _ => (),
        }
    }
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
        Self {
            parser: vt100::Parser::new_with_callbacks(rows, cols, SCROLLBACK, Queries::default()),
        }
    }
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }
    /// The answers to the queries fed since this was last called.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.parser.callbacks_mut().replies)
    }
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (old_rows, old_cols) = self.size();
        if (rows, cols) == (old_rows, old_cols) {
            return;
        }
        if rows < old_rows && !self.alternate() {
            // The emulator drops the bottom rows. Keep the newest output:
            // scroll what would not fit into the scrollback first, and the
            // cursor with the text it was on.
            let screen = self.parser.screen();
            let (cursor_row, _) = screen.cursor_position();
            let used = screen
                .rows(0, old_cols)
                .enumerate()
                .filter(|(_, text)| !text.trim().is_empty())
                .map(|(i, _)| i)
                .last()
                .unwrap_or(0)
                .max(usize::from(cursor_row));
            let hidden = (used + 1).saturating_sub(usize::from(rows));
            if hidden > 0 {
                // Scroll the screen with a parser of its own, so a sequence
                // the program's output left open in this one stays open.
                let mut scratch = vt100::Parser::new(old_rows, old_cols, SCROLLBACK);
                std::mem::swap(self.parser.screen_mut(), scratch.screen_mut());
                scratch.process(format!("\x1b[{hidden}S\x1b[{hidden}A").as_bytes());
                std::mem::swap(self.parser.screen_mut(), scratch.screen_mut());
            }
        }
        self.parser.screen_mut().set_size(rows, cols);
    }
    pub fn alternate(&self) -> bool {
        self.parser.screen().alternate_screen()
    }
    pub fn size(&self) -> (u16, u16) {
        self.parser.screen().size()
    }
    /// The rows of the screen in use, top to bottom.
    fn rows(&self) -> Vec<(String, bool)> {
        let screen = self.parser.screen();
        let (_, cols) = screen.size();
        screen
            .rows(0, cols)
            .enumerate()
            .map(|(i, row)| (row, screen.row_wrapped(i as u16)))
            .collect()
    }
    /// The screen's lines without trailing blank ones.
    pub fn visible(&self) -> Vec<String> {
        let mut lines: Vec<_> = self.rows().into_iter().map(|(text, _)| text).collect();
        drop_blank(&mut lines);
        lines
    }
    /// The last `want` lines of scrollback and screen, soft-wrapped rows
    /// joined, without trailing blank lines. On the alternate screen, which
    /// has no scrollback, the screen.
    pub fn recent(&mut self, want: usize) -> Vec<String> {
        let screen = self.parser.screen_mut();
        let (height, _) = screen.size();
        let height = usize::from(height);
        screen.set_scrollback(usize::MAX);
        let scrollback = screen.scrollback();
        // Rows from the bottom up, a page at a time, until `want` lines end
        // in them and the first of those is known to be whole.
        let mut upward: Vec<(String, bool)> = vec![];
        let mut ends = 0;
        let mut text = false;
        let mut offset = 0;
        let mut held = 0;
        loop {
            let screen = self.parser.screen_mut();
            screen.set_scrollback(offset);
            let (_, cols) = screen.size();
            let page: Vec<_> = screen
                .rows(0, cols)
                .enumerate()
                .map(|(i, row)| (row, screen.row_wrapped(i as u16)))
                .collect();
            // The page shows combined rows [scrollback - offset, ..); the
            // ones not seen on the earlier page are its first `fresh`.
            let fresh = if offset == 0 {
                page.len()
            } else {
                page.len().min(offset - held)
            };
            held = offset;
            for row in page[..fresh].iter().rev() {
                // Blank rows below the last text are dropped, so they end
                // no line.
                text = text || !row.0.trim().is_empty();
                ends += usize::from(!row.1 && text);
                upward.push(row.clone());
            }
            if ends > want || offset >= scrollback {
                break;
            }
            offset = (offset + height).min(scrollback);
        }
        self.parser.screen_mut().set_scrollback(0);
        let mut lines = vec![];
        let mut line = String::new();
        let mut open = false;
        for (text, wrapped) in upward.into_iter().rev() {
            line.push_str(&text);
            open = wrapped;
            if !wrapped {
                lines.push(std::mem::take(&mut line));
            }
        }
        if open {
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
        let mut result = json!({"text":text,"alternate_screen":self.alternate(),"rows":rows,"cols":cols});
        if cut {
            result["truncated"] = json!(true);
        }
        result
    }
    /// The newest line `matcher` matches among the screen's rows and the
    /// last `lines` recent lines, cut to the line limit.
    pub fn find(&mut self, matcher: &Matcher, lines: usize) -> Option<(String, bool)> {
        let screen = self.rows();
        let recent = self.recent(lines);
        screen
            .iter()
            .map(|(text, _)| text)
            .rev()
            .chain(recent.iter().rev())
            .find(|line| matcher.matches(line))
            .map(|line| clip(line, LINE_CAP))
    }
    /// The text a terminal that exited leaves: its last `lines` lines.
    pub fn last_lines(&mut self, lines: usize) -> (String, bool) {
        cap(&self.recent(lines), TEXT_CAP)
    }
    /// The bytes to write to the program for `text`, then `keys`, in the
    /// input modes it set.
    pub fn input(&self, text: Option<&str>, keys: &[String]) -> Vec<u8> {
        let screen = self.parser.screen();
        let mut bytes = vec![];
        if let Some(text) = text {
            let paste = screen.bracketed_paste();
            if paste {
                bytes.extend_from_slice(b"\x1b[200~");
            }
            bytes.extend_from_slice(text.as_bytes());
            if paste {
                bytes.extend_from_slice(b"\x1b[201~");
            }
        }
        for key in keys {
            if let Some(key) = terminal::key_bytes(key, screen.application_cursor()) {
                bytes.extend_from_slice(&key);
            }
        }
        bytes
    }
    /// What brings a frontend to this screen. On the normal screen it is
    /// the scrollback as lines and then the screen's lines down to the
    /// cursor, so the frontend keeps the history above the screen; on the
    /// alternate screen it is that screen alone. Both end with the input
    /// modes the program set. Best effort: the program is made to repaint
    /// after it, and a frontend narrower than the screen wraps its lines.
    pub fn redraw(&mut self) -> Vec<u8> {
        if self.alternate() {
            let mut bytes = b"\x1b[?1049h".to_vec();
            bytes.extend_from_slice(&self.parser.screen().state_formatted());
            return bytes;
        }
        let mut bytes = vec![];
        for row in self.scrollback_formatted() {
            bytes.extend_from_slice(&row);
            bytes.extend_from_slice(b"\x1b[m\r\n");
        }
        let screen = self.parser.screen();
        let (_, cols) = screen.size();
        let (cursor_row, cursor_column) = screen.cursor_position();
        let rows: Vec<_> = screen.rows_formatted(0, cols).collect();
        let last = screen
            .rows(0, cols)
            .enumerate()
            .filter(|(_, text)| !text.trim().is_empty())
            .map(|(i, _)| i)
            .last()
            .unwrap_or(0)
            .max(usize::from(cursor_row));
        for (i, row) in rows.iter().enumerate().take(last + 1) {
            bytes.extend_from_slice(row);
            if i < last {
                bytes.extend_from_slice(b"\x1b[m\r\n");
            }
        }
        bytes.extend_from_slice(b"\x1b[m");
        if last > usize::from(cursor_row) {
            bytes.extend_from_slice(format!("\x1b[{}A", last - usize::from(cursor_row)).as_bytes());
        }
        bytes.push(b'\r');
        if cursor_column >= cols {
            // The last column was written and the line has not wrapped yet:
            // draw its last cell again, which leaves the frontend there too.
            // A wide character ends in a continuation cell: draw both.
            let wide = cols > 1
                && screen
                    .cell(cursor_row, cols - 1)
                    .is_some_and(vt100::Cell::is_wide_continuation);
            let (start, width) = if wide { (cols - 2, 2) } else { (cols - 1, 1) };
            if start > 0 {
                bytes.extend_from_slice(format!("\x1b[{start}C").as_bytes());
            }
            if let Some(cell) = screen.rows_formatted(start, width).nth(usize::from(cursor_row)) {
                bytes.extend_from_slice(&cell);
                bytes.extend_from_slice(b"\x1b[m");
            }
        } else if cursor_column > 0 {
            bytes.extend_from_slice(format!("\x1b[{cursor_column}C").as_bytes());
        }
        bytes.extend_from_slice(&screen.input_mode_formatted());
        if screen.hide_cursor() {
            bytes.extend_from_slice(b"\x1b[?25l");
        }
        bytes.extend_from_slice(&screen.attributes_formatted());
        bytes
    }
    /// The scrollback rows, oldest first, with their colours.
    fn scrollback_formatted(&mut self) -> Vec<Vec<u8>> {
        let screen = self.parser.screen_mut();
        let (height, cols) = screen.size();
        let height = usize::from(height);
        screen.set_scrollback(usize::MAX);
        let mut left = screen.scrollback();
        let mut pages = vec![];
        while left > 0 {
            screen.set_scrollback(left);
            let take = left.min(height);
            pages.push(screen.rows_formatted(0, cols).take(take).collect::<Vec<_>>());
            left -= take;
        }
        screen.set_scrollback(0);
        pages.into_iter().flatten().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(rows: u16, cols: u16, output: &str) -> Screen {
        let mut screen = Screen::new(rows, cols);
        screen.feed(output.as_bytes());
        screen
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
        assert_eq!(s.parser.screen().scrollback(), 0, "the view is restored");
    }

    #[test]
    fn the_alternate_screen_is_read_like_the_visible_one() {
        let mut s = screen(3, 20, "history\r\n\x1b[?1049hfull\x1b[2;1Hscreen");
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
        let output: String = (0..20).map(|i| format!("\x1b[3{}mline {i}\x1b[m\r\n", i % 8)).collect();
        let mut source = screen(5, 20, &format!("{output}\x1b[?2004h\x1b[?1h"));
        let redraw = source.redraw();
        let mut frontend = Screen::new(5, 20);
        frontend.feed(&redraw);
        assert_eq!(frontend.visible(), source.visible());
        assert_eq!(
            frontend.parser.screen().cursor_position(),
            source.parser.screen().cursor_position()
        );
        assert!(frontend.parser.screen().bracketed_paste());
        assert!(frontend.parser.screen().application_cursor());
        // The scrollback is in the frontend's, with colour, above the screen.
        let history = frontend.recent(1000);
        assert_eq!(history[0], "line 0");
        assert_eq!(history[19], "line 19");
        let colour = frontend.parser.screen().cell(0, 0).map(|c| c.fgcolor());
        assert!(colour.is_some());
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
        assert_eq!(s.parser.screen().cursor_position(), (23, 5));
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
        // The frontend shows history above, so rows sit lower; the text is the same.
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
        assert!(frontend.parser.screen().hide_cursor());
        frontend.feed(b"\x1b[?1049l");
        assert_eq!(frontend.visible(), Vec::<String>::new(), "no normal-screen history");
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
    fn it_answers_nothing_else() {
        let mut screen = Screen::new(24, 80);
        for query in [
            &b"\x1b[>c"[..],
            b"\x1b[=c",
            b"\x1b[1c",
            b"\x1b[0;1c",
            b"\x1b[>0q",
            b"\x1b[?6n",
            b"\x1b[?5n",
            b"\x1b[7n",
            b"\x1b[5;6n",
            b"\x1b[0:1c",
            b"\x1b[6:1n",
            b"\x1b[5:0n",
            b"\x1b[?u",
            b"\x1b[?1$p",
            b"\x1b[18t",
            b"\x1b]10;?\x07",
            b"\x1b]11;?\x1b\\",
            b"\x1bP+q544e\x1b\\",
        ] {
            screen.feed(query);
            assert!(screen.take_replies().is_empty(), "{query:?}");
        }
    }

    #[test]
    fn multibyte_text_reads_and_redraws_unchanged() {
        let text = "* EXPERIMENTAL \u{b7} AI-GENERATED\r\n\u{65e5}\u{672c}\u{8a9e} \u{e9} \u{273b} \u{1f41f} \x1b[31mred \u{b7}\x1b[m end";
        let mut screen = Screen::new(6, 40);
        screen.feed(text.as_bytes());
        assert_eq!(screen.visible()[0], "* EXPERIMENTAL \u{b7} AI-GENERATED");
        assert_eq!(screen.visible()[1], "\u{65e5}\u{672c}\u{8a9e} \u{e9} \u{273b} \u{1f41f} red \u{b7} end");
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
        assert_eq!(screen.input(Some("x"), &keys(&["Y", "z"])), b"\x1b[200~x\x1b[201~Yz");
        // A multi-character entry that is no key is not typed.
        assert_eq!(screen.input(None, &keys(&["gg", "g"])), b"g");
    }

}
