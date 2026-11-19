//! A file shown read-only in an explorer pane: line numbers, syntax colours,
//! a cursor line to scroll with. Editing is not done here — `e` hands the
//! file to `$EDITOR`.
//!
//! The file is read once when it is opened (and again after an editor
//! returns). Highlighting takes syntect a while, so it runs on a thread and
//! the text is drawn plain until the colours arrive.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthChar;

use super::keys;
use super::review::highlight::{self, Run};
use super::review::render::Palette;

/// Past this a file is not read: a log or a build artefact is not something
/// to page through in a pane.
const MAX_BYTES: u64 = 2 * 1024 * 1024;
/// How much of the start of a file is looked at for a NUL, as git does.
const BINARY_SNIFF: usize = 8 * 1024;
/// A tab is shown as this. A fixed run of spaces, not a tab stop, so the
/// text is expanded once, before highlighting, and the runs stay valid.
const TAB: &str = "    ";
/// Columns ←/→ scroll by.
const HSTEP: usize = 8;
/// Lines the wheel scrolls by.
const WHEEL: usize = 3;

/// How soon a changed file may be read again.
const REREAD: Duration = Duration::from_secs(1);

pub struct FileView {
    pub path: PathBuf,
    root: PathBuf,
    /// The text, tabs already expanded. Empty when the file was refused.
    lines: Vec<String>,
    /// Why there is nothing to show, instead of the text.
    refused: Option<String>,
    scroll: usize,
    cursor: usize,
    /// Columns the text is scrolled to the left.
    hscroll: usize,
    /// Runs per line, once syntect has them.
    highlight: Option<Vec<Vec<Run>>>,
    pending: Option<Receiver<Vec<Vec<Run>>>>,
    /// When the file was last changed, as of the last read, so an edit made
    /// elsewhere — in a tmux split, say — can be noticed.
    modified: Option<std::time::SystemTime>,
    /// When it was last read, so a file rewritten all the time — a log, an
    /// agent at work — is read at most once a second, not once per key.
    read_at: Option<Instant>,
    /// Rows of text that fit, as of the last draw.
    height: usize,
    /// Where the text was drawn, for the mouse.
    pub area: Rect,
    palette: Palette,
}

impl FileView {
    pub fn open(path: PathBuf, root: PathBuf) -> Self {
        let mut view = Self {
            path,
            root,
            lines: Vec::new(),
            refused: None,
            scroll: 0,
            cursor: 0,
            hscroll: 0,
            highlight: None,
            pending: None,
            modified: None,
            read_at: None,
            height: 20,
            area: Rect::default(),
            palette: Palette::detect(),
        };
        view.load();
        view
    }

    /// Read the file again, keeping the cursor where it was as far as the
    /// new text allows.
    pub fn reload(&mut self) {
        self.load();
        self.settle();
    }

    /// Read the file again if it changed since it was read. A stat per
    /// event is cheap; re-highlighting an unchanged file on every one is not.
    pub fn reload_if_changed(&mut self) -> bool {
        if self.read_at.is_some_and(|at| at.elapsed() < REREAD) {
            return false;
        }
        if modified(&self.path) == self.modified {
            return false;
        }
        self.reload();
        true
    }

    fn load(&mut self) {
        self.modified = modified(&self.path);
        self.read_at = Some(Instant::now());
        // The old colours stay until the new ones come: a few lines tinted for
        // a moment by their neighbours' old runs beats the file going plain on
        // every change. A run past the end of its line is simply not drawn.
        self.pending = None;
        self.lines.clear();
        self.refused = None;
        match read_text(&self.path) {
            Ok(text) => {
                self.lines = text.lines().map(clean).collect();
                self.start_highlighting();
            }
            Err(why) => self.refused = Some(why),
        }
    }

    fn start_highlighting(&mut self) {
        let name = self.path.to_string_lossy().into_owned();
        let text = self.lines.join("\n");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            // The viewer may have moved on; nobody minds then.
            let _ = tx.send(highlight::highlight_text(&name, &text));
        });
        self.pending = Some(rx);
    }

    /// Whether colours are still being worked out, so the event loop keeps
    /// polling for them.
    pub fn awaiting(&self) -> bool {
        self.pending.is_some()
    }

    /// Take the colours if they have arrived.
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.pending else {
            return false;
        };
        match rx.try_recv() {
            Ok(runs) => {
                self.highlight = Some(runs);
                self.pending = None;
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
            // The thread died: stay plain.
            Err(mpsc::TryRecvError::Disconnected) => {
                self.pending = None;
                false
            }
        }
    }

    /// The path as the title shows it: relative to the worktree root.
    pub fn title(&self) -> String {
        self.path
            .strip_prefix(&self.root)
            .unwrap_or(&self.path)
            .display()
            .to_string()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    #[cfg(test)]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn move_cursor(&mut self, delta: isize) {
        self.cursor = self.cursor.saturating_add_signed(delta);
        self.settle();
    }

    pub fn page(&mut self, down: bool) {
        let step = self.height.saturating_sub(1).max(1) as isize;
        self.move_cursor(if down { step } else { -step });
    }

    pub fn first(&mut self) {
        self.cursor = 0;
        self.settle();
    }

    pub fn last(&mut self) {
        self.cursor = usize::MAX;
        self.settle();
    }

    /// The wheel moves the view, and the cursor only as far as it must to
    /// stay in it.
    pub fn wheel(&mut self, down: bool) {
        let max = self.lines.len().saturating_sub(self.height);
        self.scroll = match down {
            true => (self.scroll + WHEEL).min(max),
            false => self.scroll.saturating_sub(WHEEL),
        };
        // A pane squeezed to no rows still has one for the cursor to be on.
        let last = self.scroll + self.height.max(1) - 1;
        self.cursor = self.cursor.clamp(self.scroll, last);
        self.settle();
    }

    pub fn scroll_x(&mut self, right: bool) {
        self.hscroll = match right {
            true => self.hscroll + HSTEP,
            false => self.hscroll.saturating_sub(HSTEP),
        };
        // Not past the end of the longest line.
        let longest = self.lines.iter().map(|l| width(l)).max().unwrap_or(0);
        self.hscroll = self.hscroll.min(longest.saturating_sub(1));
    }

    /// A click puts the cursor on the line under it.
    pub fn click(&mut self, row: u16) {
        if row < self.area.y || row >= self.area.y + self.area.height {
            return;
        }
        self.cursor = self.scroll + (row - self.area.y) as usize;
        self.settle();
    }

    /// Keep the cursor on a line and in view, and the view on the text.
    fn settle(&mut self) {
        self.cursor = self.cursor.min(self.lines.len().saturating_sub(1));
        let height = self.height.max(1);
        self.scroll = self.scroll.min(self.lines.len().saturating_sub(height));
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        }
        if self.cursor >= self.scroll + height {
            self.scroll = self.cursor + 1 - height;
        }
    }

    /// Draw into `area`: the text in a bordered block, then the status and
    /// the keys, in the rows an explorer keeps for them.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, focused: bool, marker: Option<&str>) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(2),
                Constraint::Length(1),
            ])
            .split(area);
        let (border, title) = match focused {
            true => (Style::default(), Style::default().fg(Color::Green)),
            false => (
                Style::default().fg(Color::DarkGray),
                Style::default().fg(Color::DarkGray),
            ),
        };
        let mut heading = format!(" {} · read-only ", self.title());
        if let Some(marker) = marker {
            heading = format!("{heading}· {marker} ");
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border)
            .title(Span::styled(heading, title.add_modifier(Modifier::BOLD)));
        let inner = block.inner(chunks[0]);
        frame.render_widget(block, chunks[0]);
        self.area = inner;
        self.height = inner.height as usize;
        self.settle();

        let body = match &self.refused {
            Some(why) => vec![Line::from(Span::styled(
                why.clone(),
                Style::default().fg(Color::Yellow),
            ))],
            None => self.body(inner.width as usize),
        };
        frame.render_widget(Paragraph::new(body), inner);

        let status = match (&self.refused, self.lines.len()) {
            (Some(_), _) => String::new(),
            (None, 0) => "empty file".to_string(),
            (None, total) => format!("line {}/{total}", self.cursor + 1),
        };
        frame.render_widget(
            Paragraph::new(Span::styled(status, Style::default().fg(Color::Yellow))),
            chunks[1],
        );
        frame.render_widget(
            Paragraph::new(Span::styled(
                keys::file_hint_line(&keys::file_hints(), chunks[2].width as usize),
                Style::default().fg(Color::DarkGray),
            )),
            chunks[2],
        );
    }

    /// The visible lines: a dim number, then the text from `hscroll`, cut at
    /// the pane's edge.
    fn body(&self, width: usize) -> Vec<Line<'static>> {
        let digits = self.lines.len().max(1).to_string().len();
        let gutter = digits + 1;
        let room = width.saturating_sub(gutter);
        let end = (self.scroll + self.height).min(self.lines.len());
        (self.scroll..end)
            .map(|n| {
                let selected = n == self.cursor;
                let back = |style: Style| match selected {
                    true => style.bg(Color::DarkGray),
                    false => style,
                };
                let mut spans = vec![Span::styled(
                    format!("{:>digits$} ", n + 1),
                    back(Style::default().fg(Color::Gray)),
                )];
                let text = &self.lines[n];
                let shown = visible(text, self.hscroll, room);
                let runs = self
                    .highlight
                    .as_ref()
                    .and_then(|h| h.get(n))
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                for (range, tint, _) in highlight::pieces(text, runs, None) {
                    let start = range.start.max(shown.start);
                    let stop = range.end.min(shown.end);
                    if start >= stop {
                        continue;
                    }
                    let mut style = Style::default();
                    if let Some(rgb) = tint.fg {
                        style = style.fg(self.palette.code(rgb));
                    }
                    if tint.bold {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if tint.italic {
                        style = style.add_modifier(Modifier::ITALIC);
                    }
                    spans.push(Span::styled(text[start..stop].to_string(), back(style)));
                }
                // Carry the cursor line's background to the edge.
                if selected {
                    let used = gutter + width_of(text, &shown);
                    spans.push(Span::styled(
                        " ".repeat(width.saturating_sub(used)),
                        back(Style::default()),
                    ));
                }
                Line::from(spans)
            })
            .collect()
    }
}

/// The file's text, or the one line saying why it is not shown.
fn read_text(path: &Path) -> Result<String, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("could not read the file: {e}"))?;
    if !meta.is_file() {
        return Err("not a regular file".to_string());
    }
    if meta.len() > MAX_BYTES {
        return Err(format!(
            "file too large to show ({} MiB, limit {} MiB) · e opens it in your editor",
            meta.len() / (1024 * 1024),
            MAX_BYTES / (1024 * 1024)
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("could not read the file: {e}"))?;
    if bytes.iter().take(BINARY_SNIFF).any(|&b| b == 0) {
        return Err("binary file · e opens it in your editor".to_string());
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// A line as drawn: tabs as spaces, and other control characters made
/// visible rather than let loose on the terminal.
fn clean(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for c in line.chars() {
        match c {
            '\t' => out.push_str(TAB),
            c if c.is_control() => out.push('\u{fffd}'),
            c => out.push(c),
        }
    }
    out
}

fn width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

fn width_of(text: &str, range: &Range<usize>) -> usize {
    width(&text[range.clone()])
}

/// The bytes of `text` that show when it is scrolled `skip` columns left in a
/// space `room` columns wide. A wide character that would straddle an edge is
/// left out.
fn visible(text: &str, skip: usize, room: usize) -> Range<usize> {
    let mut col = 0;
    let mut used = 0;
    let mut range = text.len()..text.len();
    for (at, c) in text.char_indices() {
        let w = c.width().unwrap_or(0);
        if col < skip {
            col += w;
            continue;
        }
        if used + w > room {
            break;
        }
        if range.start == text.len() {
            range.start = at;
        }
        used += w;
        range.end = at + c.len_utf8();
    }
    // Nothing showed: an empty range, not an inverted one.
    if range.start > range.end {
        range.start = range.end;
    }
    range
}

fn modified(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use tempfile::TempDir;

    fn view(name: &str, content: &[u8]) -> (TempDir, FileView) {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        let view = FileView::open(path, dir.path().to_path_buf());
        (dir, view)
    }

    #[test]
    fn loads_text_and_titles_it_by_its_relative_path() {
        let (_dir, view) = view("src/main.rs", b"fn main() {}\nlet x = 1;\n");
        assert_eq!(view.len(), 2);
        assert_eq!(view.refused, None);
        assert_eq!(view.title(), "src/main.rs");
        assert!(view.awaiting());
    }

    #[test]
    fn refuses_binary_and_huge_files_with_a_message() {
        let (_d, binary) = view("a.bin", b"abc\0def");
        assert_eq!(binary.len(), 0);
        assert!(binary.refused.as_deref().unwrap().contains("binary"));
        assert!(!binary.awaiting());

        let big = vec![b'a'; MAX_BYTES as usize + 1];
        let (_d, huge) = view("big.txt", &big);
        assert_eq!(huge.len(), 0);
        assert!(huge.refused.as_deref().unwrap().contains("too large"));

        // A NUL past the sniffed start does not make a file binary.
        let mut late = vec![b'a'; BINARY_SNIFF];
        late.push(0);
        let (_d, ok) = view("late.txt", &late);
        assert!(ok.refused.is_none());
    }

    #[test]
    fn invalid_utf8_is_decoded_lossily() {
        let (_d, v) = view("l.txt", b"ok \xff\xfe end\n");
        assert_eq!(v.lines[0], "ok \u{fffd}\u{fffd} end");
    }

    #[test]
    fn tabs_are_four_spaces_and_controls_are_defused() {
        assert_eq!(clean("\tx\ty"), "    x    y");
        assert_eq!(clean("a\u{1b}[31mb"), "a\u{fffd}[31mb");
    }

    #[test]
    fn the_cursor_and_view_stay_on_the_text() {
        let text: String = (0..50).map(|n| format!("line {n}\n")).collect();
        let (_d, mut v) = view("t.txt", text.as_bytes());
        v.height = 10;
        v.move_cursor(-5);
        assert_eq!((v.cursor(), v.scroll), (0, 0));
        v.move_cursor(12);
        assert_eq!(v.cursor(), 12);
        assert_eq!(v.scroll, 3, "the cursor line is the last in view");
        v.last();
        assert_eq!((v.cursor(), v.scroll), (49, 40));
        v.move_cursor(5);
        assert_eq!(v.cursor(), 49);
        v.page(false);
        assert_eq!(v.cursor(), 40);
        v.first();
        assert_eq!((v.cursor(), v.scroll), (0, 0));
        // The wheel moves the view and drags the cursor along only if needed.
        v.wheel(true);
        assert_eq!((v.scroll, v.cursor()), (3, 3));
        v.wheel(false);
        v.wheel(false);
        assert_eq!(v.scroll, 0);
    }

    #[test]
    fn an_empty_file_has_nothing_to_clamp_to() {
        let (_d, mut v) = view("e.txt", b"");
        v.move_cursor(5);
        v.last();
        v.wheel(true);
        assert_eq!((v.cursor(), v.scroll), (0, 0));
    }

    #[test]
    fn long_lines_are_clipped_and_scroll_sideways() {
        assert_eq!(visible("abcdefghij", 0, 4), 0..4);
        assert_eq!(visible("abcdefghij", 8, 4), 8..10);
        assert_eq!(visible("abc", 5, 4), 3..3);
        // A wide character that does not fit is left out, not split.
        assert_eq!(visible("a\u{4e2d}b", 0, 2), 0..1);
    }

    #[test]
    fn draws_numbers_text_and_colours_once_they_arrive() {
        let (_d, mut v) = view("src/a.rs", b"fn a() {}\n\tlet b = 1;\n");
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let text = |terminal: &Terminal<TestBackend>| -> Vec<String> {
            let buffer = terminal.backend().buffer();
            (0..12)
                .map(|y| (0..60).map(|x| buffer[(x, y)].symbol()).collect::<String>())
                .collect()
        };
        terminal.draw(|f| v.draw(f, f.area(), true, None)).unwrap();
        let rows = text(&terminal);
        assert!(rows[0].contains("src/a.rs") && rows[0].contains("read-only"));
        assert!(rows[1].starts_with("│1 fn a() {}"), "{}", rows[1]);
        assert!(rows[2].starts_with("│2     let b = 1;"), "{}", rows[2]);
        assert!(rows[11].contains("^l other pane"), "{}", rows[11]);

        // Wait for syntect, then the keyword is no longer in the default colour.
        for _ in 0..500 {
            if v.poll() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!v.awaiting());
        terminal.draw(|f| v.draw(f, f.area(), true, None)).unwrap();
        let cell = &terminal.backend().buffer()[(3, 1)];
        assert_eq!(cell.symbol(), "f");
        assert_ne!(cell.fg, Color::Reset);
    }

    #[test]
    fn a_refused_file_says_why_in_the_pane() {
        let (_d, mut v) = view("a.bin", b"\0\0");
        let mut terminal = Terminal::new(TestBackend::new(60, 8)).unwrap();
        terminal.draw(|f| v.draw(f, f.area(), false, None)).unwrap();
        let row: String = (0..60)
            .map(|x| terminal.backend().buffer()[(x, 1)].symbol().to_string())
            .collect();
        assert!(row.contains("binary file"), "{row}");
    }

    /// An edit made outside plete shows the next time the view is asked.
    #[test]
    fn a_changed_file_is_read_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one\n").unwrap();
        let mut view = FileView::open(path.clone(), dir.path().to_path_buf());
        assert!(!view.reload_if_changed());
        std::fs::write(&path, "one\ntwo\n").unwrap();
        // Some filesystems keep a coarse clock: make the change visible.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(later)
            .unwrap();
        // Read under a second ago: left alone, however it changed.
        assert!(!view.reload_if_changed());
        view.read_at = None;
        assert!(view.reload_if_changed());
        assert_eq!(view.len(), 2);
    }

    /// The wheel in a pane squeezed to no rows does not panic.
    #[test]
    fn the_wheel_survives_a_pane_with_no_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("long.txt");
        std::fs::write(&path, "x\n".repeat(100)).unwrap();
        let mut view = FileView::open(path, dir.path().to_path_buf());
        view.wheel(true);
        view.height = 0;
        view.wheel(true);
        view.wheel(false);
    }
}
