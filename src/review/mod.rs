//! In-TUI code review: compute a diff (see [`diff`]) and present it as a
//! full-screen overlay. This module holds the overlay's session state and
//! navigation; rendering lives in `crate::ui::review`. Per-line comments and
//! reporting back to the originating Claude session arrive in a later phase.

pub mod diff;

use std::path::PathBuf;

use diff::{DiffCell, DiffTarget, ReviewFile, Segment};
use serde::{Deserialize, Serialize};

/// Which pane of the review overlay has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewFocus {
    Files,
    Diff,
}

/// Which side of the diff a line comment attaches to (before = left/old,
/// after = right/new).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Before,
    After,
}

impl Side {
    fn as_str(self) -> &'static str {
        match self {
            Side::Before => "before",
            Side::After => "after",
        }
    }
}

/// A saved line comment, keyed by (file, side, line).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    pub file: usize,
    pub side: Side,
    pub line: u32,
    /// The commented line's text, captured so `wrk review end` can quote it.
    pub line_text: String,
    pub body: String,
}

/// A comment being typed in the overlay's input box.
#[derive(Debug, Clone)]
pub struct CommentDraft {
    pub file: usize,
    pub side: Side,
    pub line: u32,
    pub line_text: String,
    pub buffer: String,
}

/// On-disk mirror of a comment (written under `runtime_dir()/review/<key>.json`),
/// read back by `wrk review end` to report to the Claude session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewComment {
    pub path: String,
    pub side: String,
    pub line: u32,
    pub line_text: String,
    pub body: String,
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
    /// Horizontal scroll offset (columns) for long lines that don't fit.
    pub hscroll: usize,
    /// Width in cells of the widest line on either side, for clamping `hscroll`.
    pub content_width: usize,
}

impl FileState {
    fn new(file: ReviewFile) -> Self {
        let content_width = file
            .rows
            .iter()
            .map(|r| {
                let w =
                    |c: &Option<diff::DiffCell>| c.as_ref().map_or(0, |c| c.text.chars().count());
                w(&r.left).max(w(&r.right))
            })
            .max()
            .unwrap_or(0);
        Self {
            file,
            cursor: 0,
            scroll: 0,
            hscroll: 0,
            content_width,
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
    /// Which side new comments attach to when the cursor row has both.
    pub side: Side,
    pub comments: Vec<Comment>,
    /// The comment currently being typed, if any.
    pub editing: Option<CommentDraft>,
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
            side: Side::After,
            comments: Vec::new(),
            editing: None,
        }
    }

    /// Mirror of the comments for the on-disk file / IPC report.
    pub fn comment_records(&self) -> Vec<ReviewComment> {
        self.comments
            .iter()
            .map(|c| ReviewComment {
                path: self.files[c.file].file.path.clone(),
                side: c.side.as_str().to_string(),
                line: c.line,
                line_text: c.line_text.clone(),
                body: c.body.clone(),
            })
            .collect()
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

    /// Scroll the diff horizontally by `delta` columns (for lines wider than the
    /// pane), clamped to `[0, content_width]`.
    pub fn scroll_h(&mut self, delta: isize) {
        if let Some(fs) = self.files.get_mut(self.selected) {
            let max = fs.content_width as isize;
            fs.hscroll = (fs.hscroll as isize + delta).clamp(0, max) as usize;
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

    pub fn set_side(&mut self, side: Side) {
        self.side = side;
    }

    fn other_side(&self) -> Side {
        match self.side {
            Side::Before => Side::After,
            Side::After => Side::Before,
        }
    }

    /// Resolve the comment anchor at the cursor: the (file, side, line, text) of
    /// the current diff row, preferring the active [`Side`] but falling back to
    /// whichever side the row actually has (e.g. an inserted line is after-only).
    /// Returns `None` when the cursor isn't on a real row (e.g. a collapsed gap).
    fn cursor_target(&self) -> Option<(usize, Side, u32, String)> {
        let fs = self.current()?;
        let VisualLine::Row(r) = *fs.visual_lines().get(fs.cursor)? else {
            return None;
        };
        let row = &fs.file.rows[r];
        let cell = |side: Side| -> Option<(Side, &DiffCell)> {
            match side {
                Side::Before => row.left.as_ref(),
                Side::After => row.right.as_ref(),
            }
            .map(|c| (side, c))
        };
        let (side, c) = cell(self.side).or_else(|| cell(self.other_side()))?;
        Some((self.selected, side, c.line, c.text.clone()))
    }

    fn comment_index(&self, file: usize, side: Side, line: u32) -> Option<usize> {
        self.comments
            .iter()
            .position(|c| c.file == file && c.side == side && c.line == line)
    }

    /// The comment body attached to a specific side/line of a file, if any.
    pub fn comment_for(&self, file: usize, side: Side, line: u32) -> Option<&str> {
        self.comment_index(file, side, line)
            .map(|i| self.comments[i].body.as_str())
    }

    /// Open the comment editor for the cursor's row, pre-filling any existing
    /// comment so `c` edits in place. No-op on a non-row cursor line.
    pub fn begin_comment(&mut self) {
        let Some((file, side, line, line_text)) = self.cursor_target() else {
            return;
        };
        let buffer = self
            .comment_index(file, side, line)
            .map(|i| self.comments[i].body.clone())
            .unwrap_or_default();
        self.editing = Some(CommentDraft {
            file,
            side,
            line,
            line_text,
            buffer,
        });
    }

    pub fn editor_push_char(&mut self, c: char) {
        if let Some(d) = &mut self.editing {
            d.buffer.push(c);
        }
    }

    pub fn editor_backspace(&mut self) {
        if let Some(d) = &mut self.editing {
            d.buffer.pop();
        }
    }

    pub fn cancel_comment(&mut self) {
        self.editing = None;
    }

    /// Commit the in-progress comment. An empty body removes an existing comment.
    /// Returns whether the comment set changed (so the caller can re-mirror it).
    pub fn save_comment(&mut self) -> bool {
        let Some(d) = self.editing.take() else {
            return false;
        };
        let body = d.buffer.trim().to_string();
        let idx = self.comment_index(d.file, d.side, d.line);
        if body.is_empty() {
            if let Some(i) = idx {
                self.comments.remove(i);
                return true;
            }
            return false;
        }
        match idx {
            Some(i) => self.comments[i].body = body,
            None => self.comments.push(Comment {
                file: d.file,
                side: d.side,
                line: d.line,
                line_text: d.line_text,
                body,
            }),
        }
        true
    }

    /// Delete the comment on the cursor's row, if any. Returns whether it changed.
    pub fn delete_comment_at_cursor(&mut self) -> bool {
        let Some((file, side, line, _)) = self.cursor_target() else {
            return false;
        };
        if let Some(i) = self.comment_index(file, side, line) {
            self.comments.remove(i);
            true
        } else {
            false
        }
    }
}

/// Path of the on-disk comment mirror for `key` (the originating `WRK_TAB`, or
/// the project name as a fallback). Lives beside the socket under the runtime
/// dir so `wrk review end` — a separate process — can read it back.
pub fn mirror_path(key: &str) -> PathBuf {
    crate::status::runtime_dir()
        .join("review")
        .join(format!("{}.json", sanitize(key)))
}

/// Write (or overwrite) the comment mirror for `key`.
pub fn write_mirror(key: &str, comments: &[ReviewComment]) -> std::io::Result<()> {
    let path = mirror_path(key);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(comments).unwrap_or_else(|_| "[]".to_string());
    std::fs::write(path, json)
}

/// Read the comment mirror for `key`; empty when absent or unreadable.
pub fn read_mirror(key: &str) -> Vec<ReviewComment> {
    std::fs::read_to_string(mirror_path(key))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Remove the comment mirror for `key` (best-effort).
pub fn remove_mirror(key: &str) {
    let _ = std::fs::remove_file(mirror_path(key));
}

fn sanitize(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
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

    #[test]
    fn comment_add_edit_delete_and_records() {
        // Line 2 is replaced (b → B); the diff is Equal, Replace, Equal.
        let mut s = session_with("a\nb\nc\n", "a\nB\nc\n");
        s.current_mut().unwrap().cursor = 1;
        s.side = Side::After;

        s.begin_comment();
        assert!(s.editing.is_some());
        for ch in "looks off".chars() {
            s.editor_push_char(ch);
        }
        assert!(s.save_comment());
        assert_eq!(s.comments.len(), 1);
        let c = &s.comments[0];
        assert_eq!(c.side, Side::After);
        assert_eq!(c.line, 2);
        assert_eq!(c.line_text, "B");
        assert_eq!(c.body, "looks off");

        // Records mirror the on-disk shape.
        let recs = s.comment_records();
        assert_eq!(recs[0].path, "f.rs");
        assert_eq!(recs[0].side, "after");
        assert_eq!(recs[0].line, 2);

        // Re-`c` pre-fills the existing body and edits in place.
        s.begin_comment();
        assert_eq!(s.editing.as_ref().unwrap().buffer, "looks off");
        s.editor_push_char('!');
        s.save_comment();
        assert_eq!(s.comments.len(), 1);
        assert_eq!(s.comments[0].body, "looks off!");

        // Delete.
        assert!(s.delete_comment_at_cursor());
        assert!(s.comments.is_empty());
    }

    #[test]
    fn comment_side_falls_back_to_the_available_side() {
        // Inserted line (right-only). Even with Before selected, the comment must
        // anchor to the after side.
        let mut s = session_with("a\nc\n", "a\nb\nc\n");
        s.current_mut().unwrap().cursor = 1; // the inserted row
        s.side = Side::Before;
        s.begin_comment();
        let d = s.editing.as_ref().unwrap();
        assert_eq!(d.side, Side::After);
        assert_eq!(d.line, 2);
        assert_eq!(d.line_text, "b");
    }

    #[test]
    fn empty_comment_is_not_saved() {
        let mut s = session_with("a\nb\nc\n", "a\nB\nc\n");
        s.current_mut().unwrap().cursor = 1;
        s.begin_comment();
        // No text typed.
        assert!(!s.save_comment());
        assert!(s.comments.is_empty());
    }
}
