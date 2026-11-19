//! The explorer screen's frame: one or two panes side by side — explorers, or
//! an explorer and the file it is showing.
//!
//! Everything that decides which pane a key or a click belongs to lives here,
//! apart from the terminal, so it can be tested without one. The panes
//! themselves know nothing of each other.

use std::path::PathBuf;

use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::Frame;

use super::state::Explorer;
use super::ui;
use super::viewer::FileView;

/// Narrower than this and two panes would each be too thin to browse in, so
/// only the focused one is drawn.
pub const SPLIT_MIN_WIDTH: u16 = 100;

/// What a key means for the panes rather than for the explorer under them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneKey {
    /// ctrl-v: open a second pane, or close the one that is not focused.
    Split,
    /// ctrl-l: move the focus to the other pane.
    Other,
    /// esc on an empty filter: close the focused pane.
    Close,
}

/// Which pane-level key `key` is, if any. `browsing` is false while a panel is
/// up: the panel owns the keyboard, and esc there closes the panel.
pub fn route(key: KeyEvent, split: bool, browsing: bool, filter_empty: bool) -> Option<PaneKey> {
    if !browsing {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Char('v') if ctrl => Some(PaneKey::Split),
        // With one pane there is no other, and the key does nothing.
        KeyCode::Char('l') if ctrl && split => Some(PaneKey::Other),
        // A filter goes first, then the pane, then plete.
        KeyCode::Esc if split && filter_empty => Some(PaneKey::Close),
        _ => None,
    }
}

/// Where each pane goes: side by side from [`SPLIT_MIN_WIDTH`] columns, else
/// the whole area for the one pane that shows.
pub fn split_areas(area: Rect, panes: usize) -> Vec<Rect> {
    if panes < 2 || area.width < SPLIT_MIN_WIDTH {
        return vec![area];
    }
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area)
        .to_vec()
}

/// A second explorer on the same root as `from`, with the same cwd and the
/// folders above it open — `Explorer::new` opens the way down to its start.
pub fn sibling(from: &Explorer) -> Result<Explorer> {
    let mut other = Explorer::new(
        from.repo.clone(),
        from.root.clone(),
        from.root_label.clone(),
        from.root_kind.clone(),
        from.root_status.clone(),
        from.cwd.clone(),
    )?;
    // Quitting from either pane leaves the shell and the screen behind it as
    // if there had only ever been one.
    other.back = from.back.clone();
    other.origin = from.origin.clone();
    other.away_from = from.away_from.clone();
    other.left_task = from.left_task.clone();
    // The same folder should look the same in both: dotfiles too.
    if from.show_hidden {
        other.toggle_hidden(None)?;
    }
    Ok(other)
}

/// What a pane shows: a tree to browse, or one file, read-only.
// There are never more than two, so the size gap costs nothing worth a Box
// at every place an explorer goes in or comes out.
#[allow(clippy::large_enum_variant)]
pub enum Pane {
    Explorer(Explorer),
    File(FileView),
}

impl Pane {
    pub fn explorer(&self) -> Option<&Explorer> {
        match self {
            Pane::Explorer(explorer) => Some(explorer),
            Pane::File(_) => None,
        }
    }

    pub fn explorer_mut(&mut self) -> Option<&mut Explorer> {
        match self {
            Pane::Explorer(explorer) => Some(explorer),
            Pane::File(_) => None,
        }
    }

    pub fn file(&self) -> Option<&FileView> {
        match self {
            Pane::File(file) => Some(file),
            Pane::Explorer(_) => None,
        }
    }

    pub fn file_mut(&mut self) -> Option<&mut FileView> {
        match self {
            Pane::File(file) => Some(file),
            Pane::Explorer(_) => None,
        }
    }
}

/// One or two panes and which of them has the keyboard. There is always an
/// explorer among them: a file is only ever shown beside one, and closing
/// either pane leaves the explorer — see [`Workspace::apply`].
pub struct Workspace {
    panes: Vec<Pane>,
    focus: usize,
    /// Where each pane was drawn last frame, so a click can be mapped to it.
    /// A pane that was not drawn has an empty rect, which nothing is inside.
    areas: Vec<Rect>,
}

impl Workspace {
    pub fn new(explorer: Explorer) -> Self {
        Self {
            panes: vec![Pane::Explorer(explorer)],
            focus: 0,
            areas: vec![Rect::default()],
        }
    }

    pub fn is_split(&self) -> bool {
        self.panes.len() > 1
    }

    pub fn focus(&self) -> usize {
        self.focus
    }

    /// The explorer that quitting, status and the explorer's keys belong to:
    /// the focused pane, or — with a file focused — the explorer beside it.
    fn explorer_index(&self) -> usize {
        match self.panes[self.focus] {
            Pane::Explorer(_) => self.focus,
            Pane::File(_) => 1 - self.focus,
        }
    }

    pub fn focused(&self) -> &Explorer {
        match &self.panes[self.explorer_index()] {
            Pane::Explorer(explorer) => explorer,
            Pane::File(_) => unreachable!("a workspace always has an explorer"),
        }
    }

    pub fn focused_mut(&mut self) -> &mut Explorer {
        let at = self.explorer_index();
        match &mut self.panes[at] {
            Pane::Explorer(explorer) => explorer,
            Pane::File(_) => unreachable!("a workspace always has an explorer"),
        }
    }

    /// The file view that has the keyboard, if the focus is on one.
    pub fn focused_file_mut(&mut self) -> Option<&mut FileView> {
        self.panes[self.focus].file_mut()
    }

    #[cfg(test)]
    pub fn panes(&self) -> &[Pane] {
        &self.panes
    }

    pub fn panes_mut(&mut self) -> &mut [Pane] {
        &mut self.panes
    }

    pub fn explorers(&self) -> impl Iterator<Item = &Explorer> {
        self.panes.iter().filter_map(Pane::explorer)
    }

    pub fn explorers_mut(&mut self) -> impl Iterator<Item = &mut Explorer> {
        self.panes.iter_mut().filter_map(Pane::explorer_mut)
    }

    pub fn into_focused(mut self) -> Explorer {
        let at = self.explorer_index();
        match self.panes.swap_remove(at) {
            Pane::Explorer(explorer) => explorer,
            Pane::File(_) => unreachable!("a workspace always has an explorer"),
        }
    }

    /// Act on a pane-level key. `make` builds the second pane from the focused
    /// explorer; it is only called when there is room for one.
    ///
    /// Closing never leaves a file alone: ctrl-v and esc in a file pane close
    /// that file, and from the explorer beside a file they close the file
    /// too — the explorer is the one that stays whichever pane is asked to go.
    pub fn apply(
        &mut self,
        key: PaneKey,
        make: impl FnOnce(&Explorer) -> Result<Explorer>,
    ) -> Result<()> {
        match key {
            PaneKey::Split if self.is_split() => {
                // Like closing a split in an editor from the window you are
                // staying in: focus does not move.
                let on_file = self.panes[self.focus].file().is_some();
                self.close(if on_file { self.focus } else { 1 - self.focus });
                self.focused_mut().set_status("closed the other pane");
            }
            PaneKey::Split => {
                let mut other = make(self.focused())?;
                other.set_status("split · ^l other pane · ^v unsplit");
                self.panes.push(Pane::Explorer(other));
                self.areas.push(Rect::default());
                self.focus = 1;
            }
            PaneKey::Other => self.set_focus(1 - self.focus),
            PaneKey::Close => {
                let gone = match self.panes.get(1 - self.focus) {
                    Some(Pane::File(_)) if self.panes[self.focus].file().is_none() => {
                        1 - self.focus
                    }
                    _ => self.focus,
                };
                self.close(gone);
                self.focused_mut().set_status("closed a pane");
            }
        }
        self.sync();
        Ok(())
    }

    /// Show `path` in the pane beside the focused explorer: a new pane if
    /// there is only one, else whatever the other pane held — a file or the
    /// second explorer — is replaced. The explorer keeps the keyboard so
    /// browsing goes on, except where the screen is too narrow to draw both
    /// and the file would be out of sight.
    pub fn show_file(&mut self, path: PathBuf) {
        let narrow = self.narrow();
        let view = FileView::open(path, self.focused().root.clone());
        let status = format!("showing {} · ^e edit · ^l to scroll", view.title());
        let shown = match self.is_split() {
            true => {
                let at = 1 - self.focus;
                self.panes[at] = Pane::File(view);
                at
            }
            false => {
                self.panes.push(Pane::File(view));
                self.areas.push(Rect::default());
                1
            }
        };
        self.sync();
        match narrow {
            true => {
                self.focus = shown;
                self.focused_mut()
                    .set_status(status.replace("^l to scroll", "esc to close"));
            }
            false => self.focused_mut().set_status(status),
        }
    }

    /// Whether the last frame had room for only one pane.
    fn narrow(&self) -> bool {
        let drawn: Vec<u16> = self
            .areas
            .iter()
            .map(|a| a.width)
            .filter(|&w| w > 0)
            .collect();
        match (self.panes.len(), drawn.as_slice()) {
            (_, []) => false,
            (1, [width]) => *width < SPLIT_MIN_WIDTH,
            (n, drawn) => drawn.len() < n,
        }
    }

    /// Show the file the focused explorer asked for, if it asked.
    pub fn open_requested(&mut self) {
        if let Some(path) = self.focused_mut().show.take() {
            self.show_file(path);
        }
    }

    /// Read every file view and folder again, after an editor may have
    /// changed them.
    pub fn refresh(&mut self) {
        for pane in &mut self.panes {
            match pane {
                Pane::File(file) => file.reload(),
                Pane::Explorer(explorer) => {
                    let keep = explorer.selected_entry().map(|e| e.path.clone());
                    if let Err(e) = explorer.reload(keep.as_deref()) {
                        explorer.set_status(format!("error: {e:#}"));
                    }
                }
            }
        }
    }

    /// Whether a file view is still waiting for its colours.
    pub fn awaiting_highlights(&self) -> bool {
        self.panes
            .iter()
            .any(|p| p.file().is_some_and(FileView::awaiting))
    }

    /// Read again any file shown that has changed since, as after an edit in
    /// a tmux split, which plete does not wait on.
    pub fn refresh_files(&mut self) {
        for file in self.panes.iter_mut().filter_map(Pane::file_mut) {
            file.reload_if_changed();
        }
    }

    pub fn poll_highlights(&mut self) {
        for file in self.panes.iter_mut().filter_map(Pane::file_mut) {
            file.poll();
        }
    }

    /// The pane under a screen cell, if one was drawn there.
    pub fn pane_at(&self, column: u16, row: u16) -> Option<usize> {
        self.areas.iter().position(|area| {
            column >= area.x
                && column < area.x + area.width
                && row >= area.y
                && row < area.y + area.height
        })
    }

    /// Move the keyboard to `pane`, reading its folders again: a command in
    /// the other pane may have moved or removed what it lists.
    pub fn set_focus(&mut self, pane: usize) {
        if pane >= self.panes.len() || pane == self.focus {
            return;
        }
        self.focus = pane;
        if let Pane::Explorer(explorer) = &mut self.panes[pane] {
            let keep = explorer.selected_entry().map(|e| e.path.clone());
            if let Err(e) = explorer.reload(keep.as_deref()) {
                explorer.set_status(format!("error: {e:#}"));
            }
        }
    }

    /// Drop pane `gone`; the one left has the focus.
    fn close(&mut self, gone: usize) {
        self.panes.remove(gone);
        self.areas.remove(gone);
        self.focus = 0;
    }

    fn sync(&mut self) {
        let split = self.is_split();
        for explorer in self.explorers_mut() {
            explorer.split = split;
        }
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let whole = frame.area();
        let wanted = split_areas(whole, self.panes.len());
        self.areas.fill(Rect::default());
        if wanted.len() == self.panes.len() {
            for (i, area) in wanted.into_iter().enumerate() {
                self.areas[i] = area;
                self.draw_pane(frame, i, area, None);
            }
            ui::draw_on_top(frame, self.focused());
            return;
        }
        // Too narrow for both: the focused one, and a marker saying there is
        // another behind it.
        let marker = format!("pane {}/{}", self.focus + 1, self.panes.len());
        self.areas[self.focus] = whole;
        self.draw_pane(frame, self.focus, whole, Some(&marker));
        ui::draw_on_top(frame, self.focused());
    }

    fn draw_pane(&mut self, frame: &mut Frame, at: usize, area: Rect, marker: Option<&str>) {
        let focused = at == self.focus;
        match &mut self.panes[at] {
            Pane::Explorer(explorer) => ui::draw_in(frame, area, explorer, focused, marker),
            Pane::File(file) => file.draw(frame, area, focused, marker),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::state::Exit;
    use crate::worktrees::WorktreeStatus;
    use std::path::Path;
    use tempfile::TempDir;

    fn fixture() -> TempDir {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("src/inner")).unwrap();
        std::fs::write(dir.path().join("README.md"), "hi").unwrap();
        dir
    }

    fn explorer(root: &Path) -> Explorer {
        Explorer::new(
            None,
            root.to_path_buf(),
            "main".into(),
            "trunk".into(),
            WorktreeStatus::default(),
            root.to_path_buf(),
        )
        .unwrap()
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn ctrl(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn esc() -> KeyEvent {
        key(KeyCode::Esc, KeyModifiers::NONE)
    }

    fn split(ws: &mut Workspace) {
        ws.apply(PaneKey::Split, sibling).unwrap();
    }

    #[test]
    fn splitting_copies_the_place_and_focuses_the_new_pane() {
        let dir = fixture();
        let mut first = explorer(dir.path());
        first.back = Some("tasks".into());
        first.origin = dir.path().join("elsewhere");
        first.descend_typed().unwrap();
        let cwd = first.cwd.clone();
        assert_eq!(cwd, dir.path().join("src"));
        let mut ws = Workspace::new(first);
        assert!(!ws.focused().split);

        split(&mut ws);
        assert!(ws.is_split() && ws.focus() == 1);
        let new = ws.focused();
        assert_eq!(new.root, dir.path());
        assert_eq!(new.cwd, cwd);
        assert!(new.is_open(&cwd));
        assert_eq!(new.back.as_deref(), Some("tasks"));
        assert_eq!(new.origin, dir.path().join("elsewhere"));
        assert!(ws.explorers().all(|p| p.split));
    }

    #[test]
    fn ctrl_l_toggles_between_two_panes() {
        let dir = fixture();
        let mut ws = Workspace::new(explorer(dir.path()));
        split(&mut ws);
        ws.apply(PaneKey::Other, sibling).unwrap();
        assert_eq!(ws.focus(), 0);
        ws.apply(PaneKey::Other, sibling).unwrap();
        assert_eq!(ws.focus(), 1);
    }

    #[test]
    fn esc_closes_the_focused_pane_and_focus_lands_on_the_other() {
        let dir = fixture();
        let mut ws = Workspace::new(explorer(dir.path()));
        ws.focused_mut().push_filter('x');
        ws.focused_mut().clear_filter();
        split(&mut ws);
        ws.focused_mut().descend_typed().unwrap();
        let marked = ws.focused().cwd.clone();

        // Close the first pane while focused on it: the second remains.
        ws.set_focus(0);
        ws.apply(PaneKey::Close, sibling).unwrap();
        assert!(!ws.is_split() && ws.focus() == 0);
        assert_eq!(ws.focused().cwd, marked);
        assert!(!ws.focused().split);
    }

    #[test]
    fn ctrl_v_when_split_closes_the_other_pane_and_keeps_focus() {
        let dir = fixture();
        let mut ws = Workspace::new(explorer(dir.path()));
        split(&mut ws);
        ws.focused_mut().descend_typed().unwrap();
        let here = ws.focused().cwd.clone();
        ws.apply(PaneKey::Split, sibling).unwrap();
        assert!(!ws.is_split());
        assert_eq!(ws.focused().cwd, here);
    }

    #[test]
    fn quitting_reports_the_focused_panes_exit() {
        let dir = fixture();
        let mut ws = Workspace::new(explorer(dir.path()));
        split(&mut ws);
        ws.focused_mut().descend_typed().unwrap();
        ws.focused_mut().quit_here();
        assert!(ws.focused().should_quit);
        assert!(!ws.explorers().next().unwrap().should_quit);
        assert!(matches!(ws.into_focused().exit, Exit::ChangeDir(_)));
    }

    #[test]
    fn keys_route_to_panes_only_where_they_mean_it() {
        let v = ctrl('v');
        let l = ctrl('l');
        assert_eq!(route(v, false, true, true), Some(PaneKey::Split));
        assert_eq!(route(v, true, true, false), Some(PaneKey::Split));
        assert_eq!(route(l, true, true, true), Some(PaneKey::Other));
        assert_eq!(route(l, false, true, true), None);
        assert_eq!(route(esc(), true, true, true), Some(PaneKey::Close));
        // A filter, a lone pane or a panel keeps esc for itself.
        assert_eq!(route(esc(), true, true, false), None);
        assert_eq!(route(esc(), false, true, true), None);
        assert_eq!(route(esc(), true, false, true), None);
        assert_eq!(route(v, true, false, true), None);
        // The bare letters belong to the filter.
        assert_eq!(
            route(
                key(KeyCode::Char('v'), KeyModifiers::NONE),
                true,
                true,
                true
            ),
            None
        );
    }

    #[test]
    fn two_panes_need_a_hundred_columns() {
        let wide = split_areas(Rect::new(0, 0, 100, 30), 2);
        assert_eq!(wide.len(), 2);
        assert_eq!(wide[0].width + wide[1].width, 100);
        assert_eq!(wide[1].x, wide[0].width);
        assert_eq!(split_areas(Rect::new(0, 0, 99, 30), 2).len(), 1);
        assert_eq!(split_areas(Rect::new(0, 0, 200, 30), 1).len(), 1);
        let narrow = Rect::new(0, 0, 80, 30);
        assert_eq!(split_areas(narrow, 2), vec![narrow]);
    }

    #[test]
    fn a_click_finds_the_pane_it_landed_in() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let dir = fixture();
        let mut ws = Workspace::new(explorer(dir.path()));
        split(&mut ws);

        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal.draw(|frame| ws.draw(frame)).unwrap();
        assert_eq!(ws.pane_at(5, 5), Some(0));
        assert_eq!(ws.pane_at(65, 5), Some(1));
        assert_eq!(ws.explorers().nth(1).unwrap().list_area.x, 60);

        // Narrow: only the focused pane exists on screen, whichever it is.
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|frame| ws.draw(frame)).unwrap();
        assert_eq!(ws.pane_at(5, 5), Some(1));
        ws.apply(PaneKey::Other, sibling).unwrap();
        terminal.draw(|frame| ws.draw(frame)).unwrap();
        assert_eq!(ws.pane_at(70, 5), Some(0));
        let top: String = (0..80)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol().to_string())
            .collect();
        assert!(top.contains("pane 1/2"), "{top}");
    }

    /// A panel opened in the left pane is drawn over both, not cut in half by
    /// the right pane being drawn after it.
    #[test]
    fn a_panel_in_the_left_pane_is_drawn_over_the_right() {
        use super::super::state::Overlay;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let dir = fixture();
        let mut ws = Workspace::new(explorer(dir.path()));
        split(&mut ws);
        ws.apply(PaneKey::Other, sibling).unwrap();
        assert_eq!(ws.focus(), 0);
        ws.focused_mut().overlay = Some(Overlay::Help { scroll: 0 });

        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|frame| ws.draw(frame)).unwrap();
        let here = super::super::keys::explorer_hints(ws.explorers().next().unwrap());
        let (area, _) = ui::help_geometry(Rect::new(0, 0, 120, 40), &here);
        assert!(area.x + area.width > 60, "the panel reaches the right pane");
        let buffer = terminal.backend().buffer();
        let right = area.x + area.width - 1;
        assert_eq!(buffer[(right, area.y)].symbol(), "┐");
        assert_eq!(buffer[(right, area.y + area.height - 1)].symbol(), "┘");
    }

    fn file_tree() -> TempDir {
        let dir = fixture();
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "three\n").unwrap();
        dir
    }

    /// ⏎ on `name`, the way the key handler leaves it for the workspace.
    fn show(ws: &mut Workspace, dir: &Path, name: &str) {
        ws.focused_mut().show = Some(dir.join(name));
        ws.open_requested();
    }

    fn shown(ws: &Workspace) -> Option<String> {
        ws.panes().iter().find_map(|p| p.file()).map(|f| f.title())
    }

    #[test]
    fn showing_a_file_splits_and_keeps_the_focus_on_the_explorer() {
        let dir = file_tree();
        let mut ws = Workspace::new(explorer(dir.path()));
        show(&mut ws, dir.path(), "a.txt");
        assert!(ws.is_split() && ws.focus() == 0);
        assert_eq!(shown(&ws).as_deref(), Some("a.txt"));
        assert!(ws.focused().split);
        assert_eq!(
            ws.focused().status_line,
            "showing a.txt · ^e edit · ^l to scroll"
        );

        // Another file replaces the view instead of adding a pane.
        show(&mut ws, dir.path(), "b.txt");
        assert_eq!(ws.panes().len(), 2);
        assert_eq!(shown(&ws).as_deref(), Some("b.txt"));
        assert_eq!(ws.focus(), 0);
    }

    #[test]
    fn a_file_replaces_the_other_explorer() {
        let dir = file_tree();
        let mut ws = Workspace::new(explorer(dir.path()));
        split(&mut ws);
        ws.apply(PaneKey::Other, sibling).unwrap();
        show(&mut ws, dir.path(), "a.txt");
        assert_eq!(ws.panes().len(), 2);
        assert!(ws.panes()[1].file().is_some());
        assert_eq!(ws.explorers().count(), 1);
    }

    #[test]
    fn esc_in_the_file_pane_closes_it_and_focus_returns() {
        let dir = file_tree();
        let mut ws = Workspace::new(explorer(dir.path()));
        show(&mut ws, dir.path(), "a.txt");
        ws.apply(PaneKey::Other, sibling).unwrap();
        assert_eq!(ws.focus(), 1);
        assert!(ws.focused_file_mut().is_some());
        // The keys a file pane routes: esc closes.
        assert_eq!(route(esc(), true, true, true), Some(PaneKey::Close));
        ws.apply(PaneKey::Close, sibling).unwrap();
        assert!(!ws.is_split() && ws.focus() == 0);
        assert!(shown(&ws).is_none());
        assert!(!ws.focused().split);
    }

    #[test]
    fn a_workspace_is_never_left_with_only_a_file() {
        let dir = file_tree();
        // esc in the explorer beside a file closes the file, not the explorer.
        let mut ws = Workspace::new(explorer(dir.path()));
        show(&mut ws, dir.path(), "a.txt");
        ws.apply(PaneKey::Close, sibling).unwrap();
        assert_eq!(ws.panes().len(), 1);
        assert!(ws.panes()[0].explorer().is_some());

        // ctrl-v from either side leaves the explorer.
        show(&mut ws, dir.path(), "a.txt");
        ws.apply(PaneKey::Split, sibling).unwrap();
        assert!(ws.panes()[0].explorer().is_some() && ws.panes().len() == 1);
        show(&mut ws, dir.path(), "a.txt");
        ws.apply(PaneKey::Other, sibling).unwrap();
        ws.apply(PaneKey::Split, sibling).unwrap();
        assert!(ws.panes()[0].explorer().is_some() && ws.panes().len() == 1);
        // Quitting still finds the explorer with the file focused.
        show(&mut ws, dir.path(), "a.txt");
        ws.apply(PaneKey::Other, sibling).unwrap();
        ws.focused_mut().quit_here();
        assert!(matches!(ws.into_focused().exit, Exit::Stay));
    }

    #[test]
    fn a_file_is_drawn_beside_the_explorer_and_focused_when_narrow() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let dir = file_tree();
        let mut ws = Workspace::new(explorer(dir.path()));
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal.draw(|frame| ws.draw(frame)).unwrap();
        show(&mut ws, dir.path(), "a.txt");
        terminal.draw(|frame| ws.draw(frame)).unwrap();
        assert_eq!(ws.focus(), 0);
        assert_eq!(ws.pane_at(65, 5), Some(1));

        // On a narrow screen the file would be out of sight, so it takes focus.
        let mut ws = Workspace::new(explorer(dir.path()));
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal.draw(|frame| ws.draw(frame)).unwrap();
        show(&mut ws, dir.path(), "a.txt");
        assert_eq!(ws.focus(), 1);
        terminal.draw(|frame| ws.draw(frame)).unwrap();
        assert!(ws.focused_file_mut().is_some());
    }
}
