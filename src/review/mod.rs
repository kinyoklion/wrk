//! In-TUI code review: compute a diff (see [`diff`]) and present it as a
//! full-screen overlay. This module holds the overlay's session state and
//! navigation; rendering lives in `crate::ui::review`. Per-line comments and
//! reporting back to the originating Claude session arrive in a later phase.

pub mod diff;

use diff::{DiffTarget, ReviewFile, Segment};

/// Which pane of the review overlay has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewFocus {
    Files,
    Diff,
}

/// A single display line of a file's diff: either a real aligned row (index into
/// [`ReviewFile::rows`]) or a collapsed-gap separator (index into `segments`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisualLine {
    Row(usize),
    Gap { seg: usize, count: usize },
}

/// A file plus its per-file view state, so switching files preserves position.
#[derive(Debug, Clone)]
pub struct FileState {
    pub file: ReviewFile,
    /// Cursor position as an index into [`FileState::visual_lines`].
    pub cursor: usize,
    /// Top visible display line (updated by the renderer to follow the cursor).
    pub scroll: usize,
}

impl FileState {
    fn new(file: ReviewFile) -> Self {
        Self {
            file,
            cursor: 0,
            scroll: 0,
        }
    }

    /// Flatten the collapse structure into the sequence of display lines. A
    /// collapsed, unrevealed gap contributes a single separator line; revealed
    /// or visible spans contribute one line per row.
    pub fn visual_lines(&self) -> Vec<VisualLine> {
        let mut out = Vec::new();
        for (si, seg) in self.file.segments.iter().enumerate() {
            match seg {
                Segment::Visible { start, end } => {
                    out.extend((*start..*end).map(VisualLine::Row));
                }
                Segment::Collapsed {
                    start,
                    end,
                    revealed,
                } => {
                    if *revealed {
                        out.extend((*start..*end).map(VisualLine::Row));
                    } else {
                        out.push(VisualLine::Gap {
                            seg: si,
                            count: end - start,
                        });
                    }
                }
            }
        }
        out
    }

    fn line_count(&self) -> usize {
        self.visual_lines().len()
    }
}

/// One active code review (one per wrk instance), bound to the Claude tab that
/// started it for later reporting.
pub struct ReviewSession {
    /// `WRK_TAB` of the originating Claude session (used when reporting comments).
    pub tab_id: Option<String>,
    /// `WRK_PROJECT` name, for the overlay title.
    pub project: String,
    pub target: DiffTarget,
    pub files: Vec<FileState>,
    pub selected: usize,
    pub focus: ReviewFocus,
}

impl ReviewSession {
    pub fn new(
        project: String,
        tab_id: Option<String>,
        target: DiffTarget,
        files: Vec<ReviewFile>,
    ) -> Self {
        // Start on the diff when there's something to read, else on the (empty)
        // file list.
        let focus = if files.is_empty() {
            ReviewFocus::Files
        } else {
            ReviewFocus::Diff
        };
        Self {
            tab_id,
            project,
            target,
            files: files.into_iter().map(FileState::new).collect(),
            selected: 0,
            focus,
        }
    }

    pub fn current(&self) -> Option<&FileState> {
        self.files.get(self.selected)
    }

    pub fn current_mut(&mut self) -> Option<&mut FileState> {
        self.files.get_mut(self.selected)
    }

    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            ReviewFocus::Files => ReviewFocus::Diff,
            ReviewFocus::Diff => ReviewFocus::Files,
        };
    }

    /// Move the file-list selection by `delta`, clamped.
    pub fn select_file(&mut self, delta: isize) {
        if self.files.is_empty() {
            return;
        }
        let last = self.files.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last) as usize;
    }

    /// Move the diff cursor by `delta` display lines, clamped.
    pub fn move_cursor(&mut self, delta: isize) {
        let Some(fs) = self.files.get_mut(self.selected) else {
            return;
        };
        let n = fs.line_count();
        if n == 0 {
            return;
        }
        fs.cursor = (fs.cursor as isize + delta).clamp(0, n as isize - 1) as usize;
    }

    pub fn cursor_to_edge(&mut self, top: bool) {
        if let Some(fs) = self.files.get_mut(self.selected) {
            fs.cursor = if top {
                0
            } else {
                fs.line_count().saturating_sub(1)
            };
        }
    }

    /// Reveal the collapsed gap under the cursor, if the cursor is on one.
    pub fn reveal_at_cursor(&mut self) {
        let Some(fs) = self.files.get_mut(self.selected) else {
            return;
        };
        let lines = fs.visual_lines();
        if let Some(VisualLine::Gap { seg, .. }) = lines.get(fs.cursor)
            && let Some(Segment::Collapsed { revealed, .. }) = fs.file.segments.get_mut(*seg)
        {
            *revealed = true;
        }
    }

    /// Reveal or collapse every gap in the current file (`e` / `o`).
    pub fn set_all_revealed(&mut self, revealed: bool) {
        let Some(fs) = self.files.get_mut(self.selected) else {
            return;
        };
        for seg in &mut fs.file.segments {
            if let Segment::Collapsed { revealed: r, .. } = seg {
                *r = revealed;
            }
        }
        let n = fs.line_count();
        if fs.cursor >= n {
            fs.cursor = n.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::diff::{FileStatus, build_review_file};
    use super::*;

    fn session_with(before: &str, after: &str) -> ReviewSession {
        let file = build_review_file("f.rs".into(), FileStatus::Modified, before, after);
        ReviewSession::new("p".into(), None, DiffTarget::WorkingVsHead, vec![file])
    }

    #[test]
    fn collapsed_gap_is_one_visual_line_until_revealed() {
        // 12 equal lines then a change → a leading collapsed gap.
        let mut before = String::new();
        for i in 0..12 {
            before.push_str(&format!("l{i}\n"));
        }
        before.push_str("x\n");
        let after = before.replace("x\n", "y\n");
        let mut s = session_with(&before, &after);

        let before_reveal = s.current().unwrap().line_count();
        // The leading gap collapses several equal lines into one separator.
        assert!(before_reveal < 13);

        // Put the cursor on the gap (line 0) and reveal it.
        s.current_mut().unwrap().cursor = 0;
        assert!(matches!(
            s.current().unwrap().visual_lines()[0],
            VisualLine::Gap { .. }
        ));
        s.reveal_at_cursor();
        assert!(s.current().unwrap().line_count() > before_reveal);
        assert!(
            s.current()
                .unwrap()
                .visual_lines()
                .iter()
                .all(|l| matches!(l, VisualLine::Row(_)))
        );
    }

    #[test]
    fn expand_all_then_collapse_all() {
        let mut before = String::new();
        for i in 0..20 {
            before.push_str(&format!("l{i}\n"));
        }
        before.push_str("mid\n");
        for i in 0..20 {
            before.push_str(&format!("t{i}\n"));
        }
        let after = before.replace("mid", "MID");
        let mut s = session_with(&before, &after);

        let collapsed = s.current().unwrap().line_count();
        s.set_all_revealed(true);
        let expanded = s.current().unwrap().line_count();
        assert!(expanded > collapsed);
        s.set_all_revealed(false);
        assert_eq!(s.current().unwrap().line_count(), collapsed);
    }

    #[test]
    fn cursor_navigation_is_clamped() {
        let mut s = session_with("a\nb\n", "a\nB\n");
        s.move_cursor(-5);
        assert_eq!(s.current().unwrap().cursor, 0);
        s.move_cursor(1000);
        let n = s.current().unwrap().line_count();
        assert_eq!(s.current().unwrap().cursor, n - 1);
        s.cursor_to_edge(true);
        assert_eq!(s.current().unwrap().cursor, 0);
    }
}
