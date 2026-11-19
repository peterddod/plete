//! The explorer screen's frame: one or two explorers side by side.
//!
//! Everything that decides which pane a key or a click belongs to lives here,
//! apart from the terminal, so it can be tested without one. The explorers
//! themselves know nothing of each other.

use anyhow::Result;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::Frame;

use super::state::Explorer;
use super::ui;

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

/// One or two explorers and which of them has the keyboard.
pub struct Workspace {
    panes: Vec<Explorer>,
    focus: usize,
    /// Where each pane was drawn last frame, so a click can be mapped to it.
    /// A pane that was not drawn has an empty rect, which nothing is inside.
    areas: Vec<Rect>,
}

impl Workspace {
    pub fn new(explorer: Explorer) -> Self {
        Self {
            panes: vec![explorer],
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

    pub fn focused(&self) -> &Explorer {
        &self.panes[self.focus]
    }

    pub fn focused_mut(&mut self) -> &mut Explorer {
        &mut self.panes[self.focus]
    }

    pub fn panes(&self) -> &[Explorer] {
        &self.panes
    }

    pub fn panes_mut(&mut self) -> &mut [Explorer] {
        &mut self.panes
    }

    pub fn into_focused(mut self) -> Explorer {
        self.panes.swap_remove(self.focus)
    }

    /// Act on a pane-level key. `make` builds the second pane from the focused
    /// one; it is only called when there is room for one.
    pub fn apply(
        &mut self,
        key: PaneKey,
        make: impl FnOnce(&Explorer) -> Result<Explorer>,
    ) -> Result<()> {
        match key {
            PaneKey::Split if self.is_split() => {
                // Like closing a split in an editor from the window you are
                // staying in: focus does not move.
                self.close(1 - self.focus);
                self.focused_mut().set_status("closed the other pane");
            }
            PaneKey::Split => {
                let mut other = make(self.focused())?;
                other.set_status("split · ^l other pane · ^v unsplit");
                self.panes.push(other);
                self.areas.push(Rect::default());
                self.focus = 1;
            }
            PaneKey::Other => self.focus = 1 - self.focus,
            PaneKey::Close => {
                self.close(self.focus);
                self.focused_mut().set_status("closed a pane");
            }
        }
        self.sync();
        Ok(())
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

    pub fn set_focus(&mut self, pane: usize) {
        if pane < self.panes.len() {
            self.focus = pane;
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
        for pane in &mut self.panes {
            pane.split = split;
        }
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let whole = frame.area();
        let wanted = split_areas(whole, self.panes.len());
        self.areas.fill(Rect::default());
        if wanted.len() == self.panes.len() {
            for (i, area) in wanted.into_iter().enumerate() {
                self.areas[i] = area;
                ui::draw_in(frame, area, &mut self.panes[i], i == self.focus, None);
            }
            return;
        }
        // Too narrow for both: the focused one, and a marker saying there is
        // another behind it.
        let marker = format!("pane {}/{}", self.focus + 1, self.panes.len());
        self.areas[self.focus] = whole;
        ui::draw_in(
            frame,
            whole,
            &mut self.panes[self.focus],
            true,
            Some(&marker),
        );
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
        assert!(ws.panes().iter().all(|p| p.split));
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
        assert!(!ws.panes()[0].should_quit);
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
        assert_eq!(ws.panes()[1].list_area.x, 60);

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
}
