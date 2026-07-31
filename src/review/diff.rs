//! Diff engine for the in-TUI code review.
//!
//! Git supplies the *what* (which files changed, and each side's bytes); the
//! pure-Rust [`similar`] crate supplies the *alignment* (which lines pair up
//! into a side-by-side view). Because we keep both full file texts, collapsed
//! unchanged regions can be revealed without re-running git.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, anyhow};

/// Number of unchanged context lines kept visible on each side of a change.
pub const CONTEXT: usize = 3;

/// What a review compares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffTarget {
    /// Uncommitted changes (staged + unstaged, plus untracked) vs `HEAD`.
    WorkingVsHead,
    /// The working tree vs an arbitrary revision.
    WorkingVs(String),
    /// A commit range `a..b` (both sides are committed trees).
    Range(String, String),
}

impl DiffTarget {
    /// Parse the optional argument of `wrk review start [target]`.
    ///
    /// `""` → [`DiffTarget::WorkingVsHead`]; `"a..b"` (or `"a...b"`) →
    /// [`DiffTarget::Range`]; anything else → [`DiffTarget::WorkingVs`].
    pub fn parse(s: &str) -> DiffTarget {
        let s = s.trim();
        if s.is_empty() {
            return DiffTarget::WorkingVsHead;
        }
        if let Some((a, b)) = s.split_once("..") {
            let a = a.trim();
            // Tolerate the three-dot `a...b` form by stripping the extra dot.
            let b = b.trim().trim_start_matches('.').trim();
            let a = if a.is_empty() { "HEAD" } else { a };
            let b = if b.is_empty() { "HEAD" } else { b };
            return DiffTarget::Range(a.to_string(), b.to_string());
        }
        DiffTarget::WorkingVs(s.to_string())
    }

    /// Short human label for the status/confirmation line.
    pub fn label(&self) -> String {
        match self {
            DiffTarget::WorkingVsHead => "uncommitted vs HEAD".to_string(),
            DiffTarget::WorkingVs(rev) => format!("working tree vs {rev}"),
            DiffTarget::Range(a, b) => format!("{a}..{b}"),
        }
    }

    /// Revision whose blob is the "before" side (`None` → the working tree,
    /// which never happens for the before side but keeps the shape uniform).
    fn base_rev(&self) -> &str {
        match self {
            DiffTarget::WorkingVsHead => "HEAD",
            DiffTarget::WorkingVs(rev) => rev,
            DiffTarget::Range(a, _) => a,
        }
    }
}

/// Change kind for a file in the review, driving its sidebar glyph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed { from: String },
}

impl FileStatus {
    /// Single-character sidebar glyph (GitHub/VS Code style).
    pub fn glyph(&self) -> char {
        match self {
            FileStatus::Added => 'A',
            FileStatus::Modified => 'M',
            FileStatus::Deleted => 'D',
            FileStatus::Renamed { .. } => 'R',
        }
    }
}

/// One aligned row of the side-by-side view. A row shows a left cell, a right
/// cell, or both; the kind colors it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRow {
    pub kind: RowKind,
    pub left: Option<DiffCell>,
    pub right: Option<DiffCell>,
}

/// One side of a [`DiffRow`]: a 1-based file line number and its text (newline
/// stripped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffCell {
    pub line: u32,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Equal,
    Delete,
    Insert,
    Replace,
}

/// A span of rows in the collapse structure over a file. Long unchanged runs
/// become a [`Segment::Collapsed`] that can be revealed in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Visible {
        start: usize,
        end: usize,
    },
    Collapsed {
        start: usize,
        end: usize,
        revealed: bool,
    },
}

/// A single file's computed diff.
#[derive(Debug, Clone)]
pub struct ReviewFile {
    /// Repo-relative path (the post-image path for a rename).
    pub path: String,
    pub status: FileStatus,
    pub added: usize,
    pub removed: usize,
    /// True when either side isn't valid UTF-8; `rows`/`segments` are then empty
    /// and the UI shows a "binary file" placeholder.
    pub binary: bool,
    pub rows: Vec<DiffRow>,
    pub segments: Vec<Segment>,
}

/// Build the full review for `project` under `target`. Runs git per file.
pub fn build_review(project: &Path, target: &DiffTarget) -> Result<Vec<ReviewFile>> {
    let files = changed_files(project, target)?;
    let mut out = Vec::with_capacity(files.len());
    for (path, status) in files {
        // The before side of a rename lives at the old path.
        let old_path = match &status {
            FileStatus::Renamed { from } => from.clone(),
            _ => path.clone(),
        };
        let before = if matches!(status, FileStatus::Added) {
            Some(String::new())
        } else {
            decode(blob(project, target.base_rev(), &old_path))
        };
        let after = if matches!(status, FileStatus::Deleted) {
            Some(String::new())
        } else {
            decode(new_side(project, target, &path)?)
        };
        out.push(match (before, after) {
            (Some(b), Some(a)) => build_review_file(path, status, &b, &a),
            // Either side is binary → no textual diff.
            _ => ReviewFile {
                path,
                status,
                added: 0,
                removed: 0,
                binary: true,
                rows: vec![],
                segments: vec![],
            },
        });
    }
    Ok(out)
}

/// Assemble a [`ReviewFile`] from the two decoded texts.
pub fn build_review_file(
    path: String,
    status: FileStatus,
    before: &str,
    after: &str,
) -> ReviewFile {
    let (rows, added, removed) = align(before, after);
    let segments = segment(&rows);
    ReviewFile {
        path,
        status,
        added,
        removed,
        binary: false,
        rows,
        segments,
    }
}

/// Pair up the lines of `before`/`after` into side-by-side rows. Returns the
/// rows plus the added/removed line counts.
pub fn align(before: &str, after: &str) -> (Vec<DiffRow>, usize, usize) {
    use similar::{DiffTag, TextDiff};

    let diff = TextDiff::from_lines(before, after);
    let olds = diff.old_slices();
    let news = diff.new_slices();
    let mut rows = Vec::new();
    let (mut added, mut removed) = (0usize, 0usize);

    for op in diff.ops() {
        match op.tag() {
            DiffTag::Equal => {
                for (oi, ni) in op.old_range().zip(op.new_range()) {
                    rows.push(DiffRow {
                        kind: RowKind::Equal,
                        left: Some(cell(oi, olds[oi])),
                        right: Some(cell(ni, news[ni])),
                    });
                }
            }
            DiffTag::Delete => {
                for oi in op.old_range() {
                    removed += 1;
                    rows.push(DiffRow {
                        kind: RowKind::Delete,
                        left: Some(cell(oi, olds[oi])),
                        right: None,
                    });
                }
            }
            DiffTag::Insert => {
                for ni in op.new_range() {
                    added += 1;
                    rows.push(DiffRow {
                        kind: RowKind::Insert,
                        left: None,
                        right: Some(cell(ni, news[ni])),
                    });
                }
            }
            DiffTag::Replace => {
                let olds_r: Vec<usize> = op.old_range().collect();
                let news_r: Vec<usize> = op.new_range().collect();
                removed += olds_r.len();
                added += news_r.len();
                for i in 0..olds_r.len().max(news_r.len()) {
                    rows.push(DiffRow {
                        kind: RowKind::Replace,
                        left: olds_r.get(i).map(|&oi| cell(oi, olds[oi])),
                        right: news_r.get(i).map(|&ni| cell(ni, news[ni])),
                    });
                }
            }
        }
    }
    (rows, added, removed)
}

/// Build the collapse structure: consecutive changed rows are always visible;
/// a run of `Equal` rows longer than the context it needs on each side becomes
/// `Visible(lead) + Collapsed(middle) + Visible(trail)`. A run at the very start
/// or end of the file has no change on that side, so it keeps no context there.
pub fn segment(rows: &[DiffRow]) -> Vec<Segment> {
    let n = rows.len();
    let mut segs: Vec<Segment> = Vec::new();
    let mut i = 0;
    while i < n {
        if rows[i].kind == RowKind::Equal {
            let start = i;
            while i < n && rows[i].kind == RowKind::Equal {
                i += 1;
            }
            let end = i;
            let lead = if start > 0 { CONTEXT } else { 0 };
            let trail = if end < n { CONTEXT } else { 0 };
            if end - start <= lead + trail {
                push_visible(&mut segs, start, end);
            } else {
                push_visible(&mut segs, start, start + lead);
                segs.push(Segment::Collapsed {
                    start: start + lead,
                    end: end - trail,
                    revealed: false,
                });
                push_visible(&mut segs, end - trail, end);
            }
        } else {
            let start = i;
            while i < n && rows[i].kind != RowKind::Equal {
                i += 1;
            }
            push_visible(&mut segs, start, i);
        }
    }
    segs
}

fn push_visible(segs: &mut Vec<Segment>, start: usize, end: usize) {
    if end <= start {
        return;
    }
    if let Some(Segment::Visible { end: prev_end, .. }) = segs.last_mut()
        && *prev_end == start
    {
        *prev_end = end;
    } else {
        segs.push(Segment::Visible { start, end });
    }
}

fn cell(index: usize, raw: &str) -> DiffCell {
    DiffCell {
        line: index as u32 + 1,
        text: strip_eol(raw).to_string(),
    }
}

fn strip_eol(s: &str) -> &str {
    s.strip_suffix('\n')
        .map(|s| s.strip_suffix('\r').unwrap_or(s))
        .unwrap_or(s)
}

fn decode(bytes: Vec<u8>) -> Option<String> {
    String::from_utf8(bytes).ok()
}

// --- git plumbing ---------------------------------------------------------

/// The changed files under `target`, sorted by path. For `WorkingVsHead` this
/// includes untracked files (reported as `Added`).
pub fn changed_files(project: &Path, target: &DiffTarget) -> Result<Vec<(String, FileStatus)>> {
    let name_status = match target {
        DiffTarget::WorkingVsHead => git_text(project, &["diff", "--name-status", "HEAD"])?,
        DiffTarget::WorkingVs(rev) => git_text(project, &["diff", "--name-status", rev])?,
        DiffTarget::Range(a, b) => {
            git_text(project, &["diff", "--name-status", &format!("{a}..{b}")])?
        }
    };
    let mut files = parse_name_status(&name_status);
    if matches!(target, DiffTarget::WorkingVsHead) {
        let untracked = git_text(project, &["ls-files", "--others", "--exclude-standard"])?;
        for path in untracked.lines().filter(|l| !l.is_empty()) {
            files.push((path.to_string(), FileStatus::Added));
        }
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// The "after" side bytes for `path` (working tree, or the range's `b` blob).
fn new_side(project: &Path, target: &DiffTarget, path: &str) -> Result<Vec<u8>> {
    match target {
        DiffTarget::WorkingVsHead | DiffTarget::WorkingVs(_) => {
            Ok(std::fs::read(project.join(path)).unwrap_or_default())
        }
        DiffTarget::Range(_, b) => Ok(blob(project, b, path)),
    }
}

/// Parse `git diff --name-status` output into (post-image path, status) pairs.
fn parse_name_status(s: &str) -> Vec<(String, FileStatus)> {
    let mut out = Vec::new();
    for line in s.lines() {
        let mut parts = line.split('\t');
        let Some(code) = parts.next() else { continue };
        match code.chars().next().unwrap_or(' ') {
            'A' => push(&mut out, parts.next(), FileStatus::Added),
            'M' | 'T' => push(&mut out, parts.next(), FileStatus::Modified),
            'D' => push(&mut out, parts.next(), FileStatus::Deleted),
            'R' => {
                if let (Some(from), Some(to)) = (parts.next(), parts.next()) {
                    out.push((
                        to.to_string(),
                        FileStatus::Renamed {
                            from: from.to_string(),
                        },
                    ));
                }
            }
            'C' => {
                // Copy: from, to — treat the new file as added.
                let (_from, to) = (parts.next(), parts.next());
                push(&mut out, to, FileStatus::Added);
            }
            _ => {}
        }
    }
    out
}

fn push(out: &mut Vec<(String, FileStatus)>, path: Option<&str>, status: FileStatus) {
    if let Some(p) = path {
        out.push((p.to_string(), status));
    }
}

/// `git show <rev>:<path>` bytes; empty on error (e.g. the path is absent at
/// that revision — an added file has no base blob).
fn blob(project: &Path, rev: &str, path: &str) -> Vec<u8> {
    git_bytes(project, &["show", &format!("{rev}:{path}")]).unwrap_or_default()
}

fn git_text(project: &Path, args: &[&str]) -> Result<String> {
    let bytes = git_bytes(project, args)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn git_bytes(project: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("git")
        .current_dir(project)
        .args(args)
        .output()
        .with_context(|| format!("running `git {}`", args.join(" ")))?;
    if !out.status.success() {
        return Err(anyhow!(
            "`git {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(rows: &[DiffRow]) -> Vec<RowKind> {
        rows.iter().map(|r| r.kind).collect()
    }

    #[test]
    fn parse_target_forms() {
        assert_eq!(DiffTarget::parse(""), DiffTarget::WorkingVsHead);
        assert_eq!(DiffTarget::parse("  "), DiffTarget::WorkingVsHead);
        assert_eq!(
            DiffTarget::parse("main"),
            DiffTarget::WorkingVs("main".into())
        );
        assert_eq!(
            DiffTarget::parse("main..HEAD"),
            DiffTarget::Range("main".into(), "HEAD".into())
        );
        // Three-dot form tolerated.
        assert_eq!(
            DiffTarget::parse("origin/main...feature"),
            DiffTarget::Range("origin/main".into(), "feature".into())
        );
        // Open-ended ranges default the empty side to HEAD.
        assert_eq!(
            DiffTarget::parse("v1.0.."),
            DiffTarget::Range("v1.0".into(), "HEAD".into())
        );
    }

    #[test]
    fn align_pure_insert() {
        let (rows, added, removed) = align("a\nb\n", "a\nx\nb\n");
        assert_eq!(added, 1);
        assert_eq!(removed, 0);
        assert_eq!(
            kinds(&rows),
            vec![RowKind::Equal, RowKind::Insert, RowKind::Equal]
        );
        let ins = &rows[1];
        assert!(ins.left.is_none());
        assert_eq!(ins.right.as_ref().unwrap().text, "x");
        assert_eq!(ins.right.as_ref().unwrap().line, 2); // 1-based new line
    }

    #[test]
    fn align_pure_delete() {
        let (rows, added, removed) = align("a\nb\nc\n", "a\nc\n");
        assert_eq!((added, removed), (0, 1));
        assert_eq!(
            kinds(&rows),
            vec![RowKind::Equal, RowKind::Delete, RowKind::Equal]
        );
        assert_eq!(rows[1].left.as_ref().unwrap().text, "b");
        assert!(rows[1].right.is_none());
    }

    #[test]
    fn align_replace_pads_shorter_side() {
        // One old line replaced by two new lines.
        let (rows, added, removed) = align("a\nOLD\nb\n", "a\nNEW1\nNEW2\nb\n");
        assert_eq!((added, removed), (2, 1));
        // Equal, Replace, Replace(left padded), Equal
        assert_eq!(
            kinds(&rows),
            vec![
                RowKind::Equal,
                RowKind::Replace,
                RowKind::Replace,
                RowKind::Equal
            ]
        );
        assert_eq!(rows[1].left.as_ref().unwrap().text, "OLD");
        assert_eq!(rows[1].right.as_ref().unwrap().text, "NEW1");
        assert!(rows[2].left.is_none()); // padded
        assert_eq!(rows[2].right.as_ref().unwrap().text, "NEW2");
    }

    #[test]
    fn no_trailing_newline_is_handled() {
        let (rows, _, _) = align("a\nb", "a\nB");
        assert_eq!(rows.last().unwrap().left.as_ref().unwrap().text, "b");
        assert_eq!(rows.last().unwrap().right.as_ref().unwrap().text, "B");
    }

    #[test]
    fn segment_collapses_only_long_equal_runs() {
        // 10 equal lines, then a change, then 10 equal.
        let mut before = String::new();
        for i in 0..10 {
            before.push_str(&format!("line{i}\n"));
        }
        before.push_str("CHANGE\n");
        for i in 0..10 {
            before.push_str(&format!("tail{i}\n"));
        }
        let after = before.replace("CHANGE", "CHANGED");
        let (rows, _, _) = align(&before, &after);
        let segs = segment(&rows);
        // Expect: Collapsed(leading gap), Visible(context+change+context), Collapsed(trailing gap).
        let collapsed: Vec<_> = segs
            .iter()
            .filter(|s| matches!(s, Segment::Collapsed { .. }))
            .collect();
        assert_eq!(collapsed.len(), 2, "segs = {segs:?}");
        // Leading run: 10 equal lines, no change before → lead=0, trail=CONTEXT.
        // So the first 7 collapse and 3 stay visible.
        if let Segment::Collapsed {
            start,
            end,
            revealed,
        } = &segs[0]
        {
            assert_eq!(*start, 0);
            assert_eq!(*end, 10 - CONTEXT);
            assert!(!revealed);
        } else {
            panic!("first segment should be Collapsed, got {:?}", segs[0]);
        }
    }

    #[test]
    fn segment_short_gap_stays_visible() {
        // A 4-line gap between two changes: 4 <= lead(3)+trail(3), stays visible.
        let before = "A\ne1\ne2\ne3\ne4\nB\n";
        let after = "A2\ne1\ne2\ne3\ne4\nB2\n";
        let (rows, _, _) = align(before, after);
        let segs = segment(&rows);
        assert!(
            segs.iter().all(|s| matches!(s, Segment::Visible { .. })),
            "no collapse expected: {segs:?}"
        );
    }

    #[test]
    fn segment_all_equal_no_changes() {
        let (rows, _, _) = align("x\ny\nz\n", "x\ny\nz\n");
        let segs = segment(&rows);
        // Whole-file equal run, no change either side → nothing to collapse to,
        // lead=trail=0, so 3 lines <= 0 is false → it collapses entirely.
        assert_eq!(
            segs,
            vec![Segment::Collapsed {
                start: 0,
                end: 3,
                revealed: false
            }]
        );
    }

    // --- git integration ---------------------------------------------------

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("run git")
            .status
            .success();
        assert!(ok, "git {args:?} failed");
    }

    /// End-to-end against a throwaway repo: modified + deleted + added(untracked)
    /// files are all surfaced with the right status, and `build_review` computes
    /// a real diff for the modified one.
    #[test]
    fn build_review_over_a_real_repo() {
        // Skip cleanly if git isn't on PATH (it is in CI).
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q"]);
        git(p, &["config", "user.email", "t@example.com"]);
        git(p, &["config", "user.name", "Test"]);
        git(p, &["config", "commit.gpgsign", "false"]);

        std::fs::write(p.join("keep.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(p.join("gone.txt"), "bye\n").unwrap();
        git(p, &["add", "."]);
        git(p, &["commit", "-qm", "init"]);

        // Working-tree changes: modify, delete, and add an untracked file.
        std::fs::write(p.join("keep.txt"), "one\nTWO\nthree\n").unwrap();
        std::fs::remove_file(p.join("gone.txt")).unwrap();
        std::fs::write(p.join("new.txt"), "fresh\n").unwrap();

        let files = build_review(p, &DiffTarget::WorkingVsHead).unwrap();
        let by_path: std::collections::HashMap<_, _> =
            files.iter().map(|f| (f.path.as_str(), f)).collect();

        assert_eq!(by_path["keep.txt"].status, FileStatus::Modified);
        assert_eq!(by_path["gone.txt"].status, FileStatus::Deleted);
        assert_eq!(by_path["new.txt"].status, FileStatus::Added); // untracked → Added

        let keep = by_path["keep.txt"];
        assert_eq!((keep.added, keep.removed), (1, 1));
        // The middle line is a Replace row: two → TWO.
        let replaced = keep
            .rows
            .iter()
            .find(|r| r.kind == RowKind::Replace)
            .unwrap();
        assert_eq!(replaced.left.as_ref().unwrap().text, "two");
        assert_eq!(replaced.right.as_ref().unwrap().text, "TWO");
    }
}
