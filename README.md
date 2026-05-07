# wrk

A terminal UI for juggling several concurrent Claude Code sessions, one per
project. Linux-only, native rendering (via your terminal), built on
[Ratatui](https://ratatui.rs) + [alacritty_terminal](https://crates.io/crates/alacritty_terminal)
+ [portable-pty](https://crates.io/crates/portable-pty).

```
┌── projects ──┐┌── claude ──────────────┐┌── shell ────────────┐
│  ● wrk *     ││ (claude --continue,    ││ $ cargo build       │
│  · notes-app ││  embedded PTY → grid   ││                     │
│    scratch   ││  → ratatui widget)     ││                     │
│              ││                        ││                     │
└──────────────┘└────────────────────────┘└─────────────────────┘
 wrk · [claude] (3 live · split)  Alt+1/2/3 panes  Alt+t tabs ...
```

## What it gives you

- **One sidebar of projects**, hand-editable plain-text store at
  `~/.config/wrk/projects.toml`. No DB, syncable to a dotfiles repo.
- **Per-project sessions** — claude (`claude --continue`) and a shell, both
  embedded as PTYs. Switching projects keeps every prior session alive in the
  background; switching back resumes the existing grid + scrollback, no
  respawn.
- **Multiple Claude sessions per project**: each project can hold any number of
  named Claude tabs. Switch with `Alt+<` / `Alt+>`, open a new one with
  `Alt+n` (session picker shows sessions discovered on disk), close the active
  tab with `Alt+w`. Sessions are persisted in `projects.toml` so wrk resumes
  the same conversations (`claude --resume <session-id>`) on restart.
- **Multiple projects per directory**: project names are unique; paths need
  not be, so you can have `wrk-feature` and `wrk-bugfix` both pointing at the
  same directory with separate Claude sessions each.
- **Two layouts per project, persisted**: split (claude | shell side-by-side,
  resizable) or tabbed (one content area, claude/shell as tabs). Stored in
  `projects.toml` as `layout = "split" | "tabbed"`.
- **Status indicators** in the sidebar — green ● when claude is waiting for
  input, yellow · while busy, red ● on a Notification (permission prompt).
  Driven by Claude Code hooks when installed (precise), with a
  time-since-output heuristic as fallback.
- **Mouse**: click panes / tabs / projects to focus, double-click a project to
  open it, scroll wheel paginates the alacritty scrollback (10k lines).
  **Ctrl+click** on a URL in a claude or shell pane opens it in the browser
  (`xdg-open`). Works for OSC 8 hyperlinks and plain `http(s)://` / `ftp://`
  URLs.
- **Configurable claude command** for quirky setups (e.g. `steam-run claude
  --continue` on NixOS).

## Build

```sh
nix develop                # rust toolchain + dev tools
cargo build --release
./target/release/wrk
```

The Nix flake provides the toolchain. Without Nix, any Rust ≥ 1.85 (edition
2024) works.

## CLI

| | |
|---|---|
| `wrk` | launch the TUI |
| `wrk ls` | list configured projects |
| `wrk add <path> [--name N]` | append to `projects.toml` |
| `wrk rm <name>` | remove from `projects.toml` |
| `wrk install-hooks` | merge wrk hook entries into `~/.claude/settings.json` |
| `wrk uninstall-hooks` | remove them |

## Keybindings (in the TUI)

**Global (work from any pane, including inside claude/shell):**

| Key | Action |
|---|---|
| `Alt+1` / `Alt+2` / `Alt+3` | focus projects / claude / shell |
| `Alt+0` | toggle sidebar |
| `Alt+t` | toggle split / tabbed layout (persisted per project) |
| `Alt+h` / `Alt+l` | shrink / grow claude pane (split mode) |
| `Alt+q` | quit |
| `Ctrl+Space` | jump back to projects from any pane |
| `F12` | toggle shell-pane passthrough (persisted per project) |

**Shell-pane passthrough.** When toggled on, wrk forwards every key (including
`Alt+…` and `Ctrl+Space`) directly to the shell pane's PTY — useful for nested
apps like tmux, zellij, or vim that have their own conflicting shortcuts. Only
the focused shell pane is affected; the claude and projects panes still see
wrk's normal shortcuts. `F12` itself is always intercepted so you can toggle
it back off. The current state is shown as a `[passthru]` chip in the status
bar and persisted per project as `passthrough = true` in `projects.toml`.

**On the projects pane:**

| Key | Action |
|---|---|
| `↑` / `↓` (or `j` / `k`) | move selection |
| `Enter` (or double-click) | open the selected project |
| `+` | add a project (modal) |
| `d` | delete the selected project (modal) |
| `/` | fuzzy filter |
| `r` | reload `projects.toml` from disk |
| `q` | quit |

**Claude session tabs:**

| Key | Action |
|---|---|
| `Alt+n` | new Claude tab (session picker) — only fires while the claude pane is focused |
| `Alt+w` | close active Claude tab |
| `Alt+<` | previous Claude tab |
| `Alt+>` | next Claude tab |

Anywhere else the pane has focus, all keys (including `Tab`) pass through to
the embedded PTY child.

## Configuration

### `~/.config/wrk/projects.toml`

```toml
[[project]]
name = "wrk"
path = "/home/rlamb/projects/wrk"
layout = "tabbed"   # optional, defaults to split
claude_sessions = [
  { name = "main",      session_id = "5d1f9f10-56bc-43f2-9dd5-ca711af4f3f9" },
  { name = "refactor" },   # no session_id → uses --continue
]

[[project]]
name = "notes-app"
path = "/home/rlamb/projects/notes-app"
tags = ["personal"]
```

Multiple projects can share the same `path` — they are differentiated by name
and each carries its own `claude_sessions` list.

The TUI watches this file (notify-rs) and reloads on external edits.

### `~/.config/wrk/settings.toml`

Optional. Lets you override the commands wrk launches per pane.

```toml
# Defaults: claude_command = ["claude", "--continue"]
claude_command = ["steam-run", "claude", "--continue"]

# Optional. Defaults to $SHELL, then /bin/bash.
# shell_command = ["zsh"]
```

### Status hooks (`wrk install-hooks`)

Adds three entries to `~/.claude/settings.json` (`UserPromptSubmit`, `Stop`,
`Notification`) that write event names to a per-session status file. The hook
commands are gated on `[ -n "$WRK_STATUS_FILE" ]` and only fire for Claude
sessions launched by wrk — other sessions are unaffected. Re-run
`install-hooks` to pick up updates; `uninstall-hooks` removes only the
wrk-installed entries.

## License

MIT — see [LICENSE](LICENSE).
