# wrk — Implementation Plan (Ratatui + alacritty_terminal)

## Approach

A single Rust binary that owns the entire UI. We use **Ratatui** for the chrome (panels, lists, modals, tabs) and **alacritty_terminal** for the terminal state machine of each embedded PTY. Sub-processes (`claude`, shells) run as PTYs spawned via `portable-pty`; their output is parsed by alacritty_terminal into a grid; a custom Ratatui widget renders that grid into the surrounding TUI.

We inherit native rendering, HiDPI, and Wayland/X11 support from whatever terminal hosts our binary

## Architecture at a glance

```
┌──────────────────┬─────────────────────────┬──────────────────────────┐
│  PROJECTS        │  CLAUDE                 │  SHELL  shell-1 shell-2+ │
│                  │                         │                          │
│  > wrk         * │  (claude --continue,    │  $ _                     │
│    notes-app     │   PTY → alacritty_ter   │                          │
│    scratch       │   → ratatui widget)     │                          │
│                  │                         │                          │
│  [+ add]         │                         │                          │
│                  │                         │                          │
└──────────────────┴─────────────────────────┴──────────────────────────┘
                       status: project · focus · keymap hint
```

Three layout regions managed by Ratatui's constraint solver. A status line at the bottom. Modals (add-project, confirm-delete) are popups rendered over the layout.

## Components

### 1. Project store
- `~/.config/wrk/projects.toml` — array of `{ name, path, tags? }`.
- Loaded at startup, watched for external changes (notify-rs), re-read on save.
- Hand-editable, syncable.

### 2. Pane manager
- Three logical panes: **Projects**, **Claude**, **Shell** (the Shell pane has its own tab strip for N shell PTYs).
- One pane has focus at a time. `Tab` / `Shift+Tab` cycles. A leader key (e.g. `Ctrl+Space`) returns focus to chrome from inside an embedded terminal.
- Keystrokes route to the focused pane. When focus is on Projects, our app handles them. When focus is on a terminal pane, they're forwarded to the underlying PTY (with the leader key intercepted).

### 3. Embedded terminal widget
- One `alacritty_terminal::Term` per PTY, fed by a reader thread that reads from the PTY and pushes bytes through `alacritty_terminal::vte::Parser`.
- A custom `ratatui::Widget` reads the alacritty grid and emits styled `ratatui::Cell`s. Cursor position is mapped to a Ratatui cursor hint.
- Resize: when the surrounding pane resizes, we call both `Term::resize` and `pty.resize`.

### 4. Process lifecycle
- On project select: spawn `claude --continue` (cwd = project dir) in a PTY for the Claude pane, and one shell PTY for the Shell pane.
- On project switch: keep the previous project's PTYs alive but detached (not rendered). Re-attach when the user switches back. (v1 may simplify this to "kill on switch, restart on return" if state preservation proves tricky — see Risks.)
- On exit: send `SIGHUP` / close PTYs cleanly.

### 5. Project actions (UI)
- `↑`/`↓` move selection.
- `Enter` opens the selected project (becomes active).
- `+` opens an add-project modal: text input for path, optional name (defaults to dir basename); validates that the path exists; appends to `projects.toml`.
- `d` opens a confirm-delete modal.
- `/` enters fuzzy-filter mode.
- `r` reloads `projects.toml` from disk.

### 6. Tabbed shells
- The Shell pane owns a `Vec<ShellTab>` with an active index.
- Tab strip rendered at the top of the pane.
- `Ctrl+t n` opens a new shell, `Ctrl+t [1-9]` jumps, `Ctrl+t w` closes.

## Repository layout (proposed)

```
.
├── flake.nix                # rust toolchain + dev tools
├── Cargo.toml
├── src/
│   ├── main.rs              # entry, event loop, top-level App state
│   ├── store.rs             # projects.toml load/save + watcher
│   ├── ui/
│   │   ├── mod.rs           # layout + draw orchestration
│   │   ├── projects.rs      # sidebar widget + state
│   │   ├── tabs.rs          # shell tab strip widget
│   │   └── modal.rs         # add/delete popups
│   ├── pane/
│   │   ├── mod.rs           # focus mgmt, key routing
│   │   └── terminal.rs      # alacritty Term + PTY + ratatui widget
│   └── proc.rs              # PTY spawn helpers (claude, shell)
└── REQUIREMENTS.md
```

Single crate to start; split into a workspace if it grows.

## Key dependencies

| Crate                  | Purpose                                  |
|------------------------|------------------------------------------|
| `ratatui`              | TUI chrome, layout, widgets              |
| `crossterm`            | terminal backend (raw mode, key/mouse)   |
| `alacritty_terminal`   | ANSI parsing, terminal grid state        |
| `portable-pty`         | spawn child processes in PTYs            |
| `serde` + `toml`       | project store                            |
| `notify`               | watch `projects.toml` for external edits |
| `nucleo-matcher`       | fuzzy-filter projects                    |
| `anyhow`               | error handling                           |
| `tui-markdown` (later) | markdown viewer pane                     |

## Risks and open questions

1. **Background sessions across project switches.** Keeping every project's PTYs alive in the background uses memory and complicates focus routing. v1 plan: keep alive (it's the user's stated need); fall back to "restart on switch" if it causes problems.
2. **Terminal-in-terminal keybinding conflicts.** If the user is inside the Claude pane and presses something that means "switch tabs" to our app and "X" to Claude, the routing must be unambiguous. Mitigation: a single, distinctive leader key (`Ctrl+Space`) as the only hot escape; everything else passes through to the PTY.
3. **Mouse routing.** Same problem as keys. v1: leave mouse off in the embedded panes; revisit if needed.
4. **Rendering performance.** Re-rendering a full alacritty grid every frame for 2–3 panes is fine; if more panes appear (file browser, markdown, many shell tabs) we may need dirty-region tracking.
5. **Resize propagation.** Need to test thoroughly on terminal window resize and inside tmux/zellij if anyone runs us nested.

## v1 scope (what we'll actually build first)

- Sidebar with project list (read from TOML, render, fuzzy filter, +/d/r).
- Claude pane embedded with `claude --continue`.
- Shell pane with a single shell (no tabs yet).
- Tab key to cycle focus; `Ctrl+Space` to escape from embedded panes.
- Add/delete projects via UI modals + matching CLI subcommands (`wrk add`, `wrk rm`, `wrk ls`).
- Nix dev shell.

## v2 / deferred

- Tabbed shells (multiple PTYs in the right pane).
- Persistent per-project state across switches (preserve PTYs, scrollback).
- Markdown viewer pane.
- File browser pane (built-in or shell out to yazi).
- Hook-driven Claude status badges in the sidebar (running / awaiting input / idle).
- Worktree-per-task Claude sessions.
- Theming / config file beyond projects.toml.

## Verification

- Unit tests for project store load/save.
- Manual: launch `wrk`, add a project, open it, type into Claude, type into shell, switch projects, switch back — confirm both PTYs survived and cursor/state are intact.
- Visual: run inside Ghostty under both Wayland and X11 sessions on a HiDPI display; confirm crisp rendering.
