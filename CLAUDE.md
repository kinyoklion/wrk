# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`wrk` is a Linux TUI for juggling concurrent Claude Code sessions, one per project. It pairs Ratatui chrome with `alacritty_terminal` for ANSI parsing and `portable-pty` for spawning child processes. Rust edition 2024, MSRV 1.85.

### Workspace layout

A Cargo workspace. The `wrk` package keeps its manifest and `src/` at the repo
root (so the synthase release tooling, configured against path `.`, and the
root `CHANGELOG.md` keep working); the other members live under `crates/`:

- root (`./`) — `wrk` binary, the TUI.
- `crates/markdown` — `wrk-markdown` library: CommonMark/GFM → ratatui `Text`
  plus a scrollable `MarkdownView` widget. `highlight` feature (default, syntect
  via pure-Rust fancy-regex) and a pluggable `DiagramBackend` (default
  `NullBackend` renders mermaid as a code block + hint).
- `crates/viewer` — `wrk-md` binary: a standalone markdown pager over
  `wrk-markdown`, usable in any shell (`--print` for a plain stdout dump).

Shared deps that cross crate boundaries (notably `ratatui`) live in
`[workspace.dependencies]` so the types stay identical across crates.

## Commands

```sh
nix develop                       # rust toolchain + dev tools (rust-analyzer, clippy, rustfmt)
cargo build                       # debug build (whole workspace)
cargo build --release             # release build → target/release/{wrk,wrk-md}
cargo test                        # all tests (workspace-wide)
cargo test -p wrk-markdown        # tests for a single crate
cargo test <name>                 # single test, e.g. `cargo test round_trip_with_projects`
cargo test -- --nocapture         # show println! during tests
cargo clippy --all-targets        # lint
cargo fmt                         # format
```

CLI subcommands (also documented in `README.md`): `wrk`, `wrk ls`, `wrk add <path> [--name N]`, `wrk rm <name>`, `wrk install-hooks`, `wrk uninstall-hooks`.

## Architecture

### Top-level App state (`src/main.rs`)

`App` owns everything: the loaded `ProjectStore`, `Settings`, the projects sidebar, the focus state, and a `HashMap<String, ProjectSession>` keyed by project name. **Sessions are kept alive across project switches** — switching projects just changes `active_project_name` and `focus`; the inactive PTYs continue running with their grids intact. `open_selected` only spawns a PTY if the entry is missing or its child has died.

The event loop in `event_loop()` does five things per tick: draw, resize the active session's PTYs to current geometry, drain `notify` file-watch events (reload `projects.toml`), poll crossterm for input (33 ms), and reap dead children on the active session.

### Layout & focus

`compute_layout` produces a `LayoutRects` with `sidebar` (optional), `claude`, `shell`, and an optional `tab_strip`. `LayoutMode` is per-project (`Split` or `Tabbed`, persisted to `projects.toml` via `set_layout_mode` → `store::save`). In `Tabbed` mode `claude` and `shell` rects point at the same content area and only the focused pane renders.

`Focus` is `Projects | Claude | Shell`. `handle_key` is layered: modal keys first, then global `Alt+…` / `Ctrl+Space`, then per-focus dispatch. **Anything not consumed by global keys is forwarded to the focused PTY** as bytes via `pane::key_to_bytes` (which handles the CSI vs SS3 cursor-key distinction based on the terminal's DECCKM application-cursor mode — needed for ncurses programs like htop).

### Embedded terminal (`src/pane/terminal.rs` + `src/proc.rs`)

Each `PtyPane` wraps:
- a `portable_pty` master/child pair (spawned via `proc::spawn`, which sets `TERM=xterm-256color` and any extra env),
- a shared `Arc<Mutex<Term<PtyEventListener>>>` (alacritty grid),
- a reader thread that pumps PTY bytes through `vte::ansi::Processor` into the `Term`,
- a `last_output: Arc<Mutex<Instant>>` updated by the reader thread for the idle-heuristic fallback.

A custom `PtyPaneWidget` reads the alacritty grid each frame and emits styled `ratatui::Cell`s. On resize we call both `Term::resize` and `pty.resize` together. Mouse wheel scrolls the alacritty scrollback (10k lines).

### Project store (`src/store.rs`)

`~/.config/wrk/projects.toml` is the source of truth — plain TOML, hand-editable. `ProjectStore` is `serde`-serialized; `notify-rs` watches the parent dir and signals reload via a channel. `Project.layout_mode` is the per-project `Split`/`Tabbed` preference (TOML key: `layout`).

When the file changes externally and we reload, sessions whose project was removed are dropped from `App.sessions`.

### Settings (`src/settings.rs`)

Optional `~/.config/wrk/settings.toml`. Key fields: `claude_command` (defaults to `["claude", "--continue"]`) and `shell_command` (falls back to `$SHELL`, then `/bin/bash`). Used to support quirky setups like `["steam-run", "claude", "--continue"]` on NixOS.

### Status hooks (`src/status.rs`)

The sidebar shows green ● (waiting), yellow · (busy), red ● (notification). Two signal sources:

1. **Precise**: Claude Code hooks installed in `~/.claude/settings.json`. When wrk spawns a Claude PTY it sets `WRK_STATUS_FILE=<runtime_dir>/wrk/status/<sanitized name>.status`. The hook commands (`UserPromptSubmit`, `Stop`, `Notification`) are guarded with `[ -n "$WRK_STATUS_FILE" ] && ... ; true` — the `; true` is critical because the failed `[ -n "" ]` test would otherwise propagate as exit 1 and Claude Code would log a hook error for sessions not launched by wrk.
2. **Fallback heuristic**: time since the reader thread last saw PTY output (>500 ms = waiting).

`install_hooks` merges entries into the user's existing `settings.json` and re-running it refreshes the command (so command bug fixes propagate). `uninstall_hooks` only removes entries containing the `WRK_STATUS_FILE` marker. Hook entries are detected by that marker, not by exact-match — preserve it when editing.

## Conventions

- Errors bubble via `anyhow::Result`; UI-recoverable errors set `App.error` (rendered in the status bar) using `push_error` to concatenate.
- `serde(rename = "layout")` and `serde(rename = "project")` are load-bearing for the on-disk TOML schema — don't rename without updating any user configs.
- Coordinate math: `body_rect` reserves the bottom row for the status bar; `inset` strips the 1-cell border before passing rects to PTY resize. Always pass *inner* dimensions to `PtyPane::spawn` / `resize`, not the bordered rects.
- Tests live alongside code in `#[cfg(test)] mod tests` blocks (`store.rs`, `status.rs`); use `tempfile::tempdir` for filesystem fixtures.
