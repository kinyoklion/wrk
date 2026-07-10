# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

`wrk` is a Linux TUI for juggling concurrent Claude Code sessions, one per project. It pairs Ratatui chrome with `alacritty_terminal` for ANSI parsing and `portable-pty` for spawning child processes. Rust edition 2024, MSRV 1.86.

### Workspace layout

A Cargo workspace. The `wrk` package keeps its manifest and `src/` at the repo
root (so the synthase release tooling, configured against path `.`, and the
root `CHANGELOG.md` keep working); the other members live under `crates/`:

- root (`./`) — `wrk` binary, the TUI.
- `crates/markdown` — `wrk-markdown` library: CommonMark/GFM → a `RenderedDoc`
  (a `Vec<MdBlock>` of `Text` runs + `ImageRef`s) plus a scrollable
  `MarkdownView` widget. `highlight` feature (default, syntect via pure-Rust
  fancy-regex); `images` feature (default, MSRV 1.86) renders image links —
  incl. SVG via resvg + a bundled Liberation Sans — as terminal graphics through
  ratatui-image; and a pluggable `DiagramBackend`. The `mermaid` feature
  (implies `images`) renders ```mermaid``` fences through `CarcimaidBackend`
  (pure-Rust carcimaid → SVG → the image pipeline); without it the default
  `NullBackend` shows the fence source with a hint.
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

The primary pane hosts a `Vec<Tab>` where `Tab` is `Claude(ClaudeTab)` or `Markdown(MarkdownTab)` (`active_tab` indexes it). Only Claude tabs own a PTY and are persisted to `projects.toml`; markdown tabs (opened with `Alt+m`, rendered via the `wrk-markdown` crate) are ephemeral. Use `ProjectSession::claude_tabs()/claude_tabs_mut()` to iterate just the Claude tabs (spawn/resize/reap/persist), and `current()`/`active_claude_pane()` for the active tab. When the active tab is markdown, key/scroll input routes to its `MarkdownViewState` instead of a PTY (`active_tab_is_markdown`, `handle_markdown_key`, `scroll_at`).

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

Color config: `[theme]` (chrome) resolves via `ThemeConfig::resolve()` → `Theme`, and `[markdown]` (markdown viewer palette) resolves via `MarkdownConfig::resolve()` → `wrk_markdown::MdTheme`, both reusing `parse_color`. `App` caches the resolved `theme` and `md_theme` at startup; markdown tabs copy `md_theme` at open and pass it to the renderer via `RenderOptions::with_theme` in `MarkdownTab::ensure_rendered`.

Markdown images: `render_blocks` produces a `RenderedDoc` (text + `ImageRef` blocks). `App.picker` is a `wrk_markdown::Picker` (re-exported ratatui-image), detected once via `wrk_markdown::query_picker()` (which also queries the terminal background for diagram theming) right after entering the alternate screen (it must run there). `MarkdownTab::ensure_rendered` re-renders on width change and returns whether it did; on a re-render the draw site (`ui::mod`) calls `state.prepare_images(&doc, picker)` to (re)rasterize image blocks into per-block protocols — once per render, not per frame. `RenderOptions::with_base_dir` (the doc's parent dir) resolves relative `![](x.png)` links. The `MarkdownView` widget keeps the flat single-`Paragraph` fast path when a doc has no images; otherwise it lays blocks out vertically and draws images with the terminal graphics protocol. SVG rasterization (`crates/markdown/src/image.rs`) uses resvg with a bundled Liberation Sans so `<text>` never depends on host fonts. Diagram fences route through the `DiagramBackend` seam (`crates/markdown/src/diagram.rs`): `render(lang, source, theme, ctx: DiagramCtx)` returns a `DiagramOutput` of either `Lines` (spliced inline) or an `Image(ImageSource)`. With the `mermaid` feature, `CarcimaidBackend` calls `carcimaid::render_to_svg_with` and returns `Image(ImageSource::Svg(..))`, which becomes an `MdBlock::Image` sharing the SVG rasterizer; a carcimaid parse error falls back to a `Lines` source dump so a bad diagram never breaks the doc. carcimaid is a git dependency (pinned to a release tag, fetched over HTTPS).

Diagram theming (`DiagramCtx`, carried on `RenderOptions::diagram_ctx`): diagrams render on a transparent background and auto-theme to the terminal. `prefers_dark` → carcimaid's dark palette (light lines/text) so a transparent diagram stays legible on a dark terminal; the terminal background is detected once at startup via `wrk_markdown::query_picker()` (ratatui-image OSC 11 query) + `terminal_prefers_dark`, stored on `App.prefers_dark` and fixed onto each `MarkdownTab.diagram_ctx` at open. `opaque_background` (the `b` key → `MarkdownTab::toggle_diagram_background`, which zeroes `render_width` to force a re-render) is the readability escape hatch: carcimaid's classic white card, forced light, for when auto-detection reads the terminal wrong or a diagram's colors still don't work. A diagram's own frontmatter `theme:` still wins over the ctx theme (carcimaid treats ours as a default). carcimaid dark-themes both diagram types it supports (flowchart and sequence) as of v0.1.4.

### Status hooks (`src/status.rs`)

The sidebar shows green ● (waiting), yellow · (busy), red ● (notification). Two signal sources:

1. **Precise**: Claude Code hooks installed in `~/.claude/settings.json`. When wrk spawns a Claude PTY it sets `WRK_STATUS_FILE=<runtime_dir>/wrk/status/<sanitized name>.status`. The hook commands (`UserPromptSubmit`, `Stop`, `Notification`) are guarded with `[ -n "$WRK_STATUS_FILE" ] && ... ; true` — the `; true` is critical because the failed `[ -n "" ]` test would otherwise propagate as exit 1 and Claude Code would log a hook error for sessions not launched by wrk.
2. **Fallback heuristic**: time since the reader thread last saw PTY output (>500 ms = waiting).

`install_hooks` merges entries into the user's existing `settings.json` and re-running it refreshes the command (so command bug fixes propagate). `uninstall_hooks` only removes entries containing the `WRK_STATUS_FILE` marker. Hook entries are detected by that marker, not by exact-match — preserve it when editing.

`install_hooks`/`uninstall_hooks` (the `install-hooks` subcommands) also write/remove the `wrk-view` **skill** at `~/.claude/skills/wrk-view/SKILL.md` (`install_skill`/`uninstall_skill`); the SKILL.md carries a marker comment so uninstall never deletes a user's own same-named skill.

### `wrk view` and IPC (`src/ipc.rs`)

Each running TUI binds a per-instance Unix socket at `<runtime>/wrk/sock/wrk-<pid>.sock` (`ipc::serve`), stored on `App.socket_path` and exported to every spawned PTY (Claude **and** shell) as `WRK_SOCK`, alongside `WRK_PROJECT` (see `App::base_pty_env`). `wrk view <file>` (`cmd_view`) canonicalizes the path; if `WRK_SOCK` is set and connectable it sends a one-line-JSON `ipc::OpenRequest` which the event loop drains and turns into a markdown tab in the named project (`App::handle_open_request` → `add_markdown_tab`); otherwise it execs the `wrk-md` pager (sibling of the current exe, else `PATH`). The socket file is removed on shutdown.

## Conventions

- Errors bubble via `anyhow::Result`; UI-recoverable errors set `App.error` (rendered in the status bar) using `push_error` to concatenate.
- `serde(rename = "layout")` and `serde(rename = "project")` are load-bearing for the on-disk TOML schema — don't rename without updating any user configs.
- Coordinate math: `body_rect` reserves the bottom row for the status bar; `inset` strips the 1-cell border before passing rects to PTY resize. Always pass *inner* dimensions to `PtyPane::spawn` / `resize`, not the bordered rects.
- Tests live alongside code in `#[cfg(test)] mod tests` blocks (`store.rs`, `status.rs`); use `tempfile::tempdir` for filesystem fixtures.
