//! In-TUI code review: compute a diff (see [`diff`]) and, in later phases,
//! present it as a full-screen overlay with per-line comments reported back to
//! the Claude session that started the review.

// The diff engine lands first (this PR); the overlay UI and `wrk review` IPC
// that consume it follow in the next PRs. Until then its public API is unused.
#[allow(dead_code)]
pub mod diff;
