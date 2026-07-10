# wrk

A terminal UI for juggling several concurrent Claude Code sessions, one per
project. Linux-only, native rendering (via your terminal), built on
[Ratatui](https://ratatui.rs) + [alacritty_terminal](https://crates.io/crates/alacritty_terminal)
+ [portable-pty](https://crates.io/crates/portable-pty).

```
┌── projects ──┐┌── claude ──────────────┐┌── shell ────────────┐
│  ● wrk *     ││ (claude --resume …,    ││ $ cargo build       │
│  · notes-app ││  embedded PTY → grid   ││                     │
│    scratch   ││  → ratatui widget)     ││                     │
│              ││                        ││                     │
└──────────────┘└────────────────────────┘└─────────────────────┘
 wrk · [claude] (3 live · split)  Alt+1/2/3 panes  Alt+t tabs ...
```

## What it gives you

- **One sidebar of projects**, hand-editable plain-text store at
  `~/.config/wrk/projects.toml`. No DB, syncable to a dotfiles repo.
- **Per-project sessions** — claude and a shell, both embedded as PTYs.
  Switching projects keeps every prior session alive in the background;
  switching back resumes the existing grid + scrollback, no respawn.
- **Multiple Claude sessions per project**: each project can hold any number of
  named Claude tabs. Switch with `Alt+<` / `Alt+>`, open a new one with
  `Alt+n` (session picker shows sessions discovered on disk), close the active
  tab with `Alt+w`. Resumption is **deterministic**: every tab is tied to a
  specific Claude session ID, persisted to `projects.toml`, and restored via
  `claude --resume <id>`. A tab with no recorded ID yet (brand-new project, or
  a hand-written entry) spawns a fresh new session; wrk captures its ID a few
  seconds later and writes it back to `projects.toml`. wrk does **not** use
  `claude --continue`, which would non-deterministically attach to whatever
  session was newest in the directory — wrong when multiple projects share a
  path.
- **Multiple projects per directory**: project names are unique; paths need
  not be, so you can have `wrk-feature` and `wrk-bugfix` both pointing at the
  same directory with separate Claude sessions each. Each project resumes its
  own sessions independently.
- **Two layouts per project, persisted**: split (claude | shell side-by-side,
  resizable) or tabbed (one content area, claude/shell as tabs). Stored in
  `projects.toml` as `layout = "split" | "tabbed"`.
- **Status indicators** in the sidebar — green ● when claude is waiting for
  input, yellow · while busy, red ● on a Notification (permission prompt).
  Driven by Claude Code hooks when installed (precise), with a
  time-since-output heuristic as fallback. A trailing `*` marks a project with
  a live session — bright on the active project, dim on ones loaded in the
  background. Press `u` on the projects pane to unload one.
- **Mouse**: click panes / tabs / projects to focus, double-click a project to
  open it, scroll wheel paginates the alacritty scrollback (10k lines).
  **Ctrl+click** (or **Shift+click**, if your outer terminal swallows Ctrl) on
  a URL in a claude or shell pane opens it in the browser (`xdg-open`). Works
  for OSC 8 hyperlinks and plain `http(s)://` / `ftp://` URLs. For keyboard
  access, **`Alt+u`** opens a picker over every URL in the focused pane's
  scrollback (newest first, filter by typing).
- **Configurable claude command** for quirky setups (e.g. `steam-run claude`
  on NixOS).
- **Per-pane select + copy**: press **`Alt+s`** to enter select mode (status
  bar shows a `[select]` chip), drag with the mouse to highlight cells in the
  focused pane, release to copy via OSC 52 — works through SSH and bypasses
  the host terminal's whole-row selection. Esc cancels without copying.
- **Configurable claude command** for quirky setups (e.g. `steam-run claude
  --continue` on NixOS).

## Build

```sh
nix develop                # rust toolchain + dev tools
cargo build --release
./target/release/wrk
```

The Nix flake provides the toolchain. Without Nix, any Rust ≥ 1.86 (edition
2024) works.

`wrk` is a Cargo workspace. `cargo build --release` produces two binaries:
`target/release/wrk` (the TUI) and `target/release/wrk-md` (the standalone
markdown viewer, below). The markdown renderer itself lives in the reusable
`wrk-markdown` library crate under `crates/markdown`. Release tarballs bundle
both `wrk` and `wrk-md`.

## Standalone markdown viewer (`wrk-md`)

`wrk-md` renders a markdown file in the terminal — headings, emphasis, lists,
block quotes, and syntax-highlighted code blocks — using the same engine that
backs wrk's in-TUI markdown. Tables are laid out to the display width, wrapping
long cells within their columns (the widest column shrinks first, so short
columns keep their natural width). ```mermaid``` fences render as real diagrams:
[carcimaid] (pure-Rust) turns the mermaid source into SVG, which is drawn like
any other image (see below). This is the `mermaid` feature (on by default,
implies `images`); a diagram carcimaid can't parse falls back to its source with
a note, and building `--no-default-features` shows all diagram fences that way.

Diagrams are drawn on a transparent background and auto-themed to your terminal:
on a dark terminal they use a dark palette (light lines and text) so they stay
legible, on a light terminal the classic light palette. The terminal's
background is detected once at startup (OSC 11); a diagram's own frontmatter
`theme:` still wins. If a diagram is still hard to read — auto-detection guessed
wrong, or its colors don't suit your terminal — press `b` in the viewer to
toggle an opaque white "card" behind it.

Image links (`![](photo.png)`, `![](diagram.svg)`) render as real images in
terminals with a graphics protocol (kitty, sixel, iterm2), falling back to
unicode half-blocks elsewhere; where no image can be drawn they show as a `🖼`
placeholder. SVG is rasterized with [resvg] against a bundled Liberation Sans,
so `<text>` in diagrams renders the same regardless of the fonts installed on
the host. This is the `images` feature (on by default; drop it with
`--no-default-features` to shed the graphics/SVG stack, which also lowers the
build's MSRV back below 1.86). Remote (`http(s)://`) and `data:` links stay
placeholders.

On a graphics terminal, `H1`–`H3` headings render at a true, larger font size
(the same SVG pipeline, in the bundled sans font and the heading color, with the
`#` stripped); a heading wider than the pane scales down to fit. `H4`–`H6` stay
as `#`-prefixed styled text. Turn it off with `heading_images = false` in the
`[markdown]` settings; without a graphics protocol every heading falls back to
text automatically.

[resvg]: https://github.com/linebender/resvg
[carcimaid]: https://github.com/kinyoklion/carcimaid

```sh
wrk-md README.md            # scrollable pager (j/k, PgUp/PgDn, g/G, r reload, b diagram bg, q quit)
wrk-md --no-highlight FILE  # disable code syntax highlighting
wrk-md --print FILE         # plain-text render to stdout (for piping)
wrk-md --print --width 80 F # force a wrap width for --print (else terminal width)
```

## CLI

| | |
|---|---|
| `wrk` | launch the TUI |
| `wrk ls` | list configured projects |
| `wrk add <path> [--name N]` | append to `projects.toml` |
| `wrk rm <name>` | remove from `projects.toml` |
| `wrk view <file>` | open a markdown file — as a tab in the running wrk (from inside a pane), else in the `wrk-md` pager |
| `wrk install-hooks` | merge wrk hooks into `~/.claude/settings.json` **and** install the `wrk-view` skill |
| `wrk uninstall-hooks` | remove both |

### Letting Claude open files

`wrk install-hooks` also writes a `wrk-view` skill to `~/.claude/skills/` that
teaches Claude to run `wrk view <file>`. When Claude runs it from within a wrk
pane, wrk opens the file as a markdown tab in that project (over a per-instance
Unix socket exported to the pane as `WRK_SOCK`, with `WRK_PROJECT` naming the
project). Ask Claude to "preview the README" and it renders in a tab beside the
conversation. Run the same command in a plain shell and it opens the `wrk-md`
pager instead.

## Keybindings (in the TUI)

**Global (work from any pane, including inside claude/shell):**

| Key | Action |
|---|---|
| `Alt+1` / `Alt+2` / `Alt+3` | focus projects / claude / shell |
| `Alt+0` | toggle sidebar |
| `Alt+t` | toggle split / tabbed layout (persisted per project) |
| `Alt+h` / `Alt+l` | shrink / grow claude pane (split mode) |
| `Alt+q` | quit |
| `Alt+u` | open URL picker (scans the focused pane's scrollback) |
| `Alt+s` | enter select mode — drag to highlight, release to copy via OSC 52 |
| `Ctrl+Space` | jump back to projects from any pane |
| `F12` | toggle shell-pane passthrough (persisted per project) |

**Shell-pane passthrough.** When toggled on, wrk forwards every key (including
`Alt+…` and `Ctrl+Space`) directly to the shell pane's PTY — useful for nested
apps like tmux, zellij, or vim that have their own conflicting shortcuts. Only
the focused shell pane is affected; the claude and projects panes still see
wrk's normal shortcuts. `F12` itself is always intercepted so you can toggle
it back off. The current state is shown as a `[passthru]` chip in the status
bar and persisted per project as `passthrough = true` in `projects.toml`.

**Select mode.** `Alt+s` enters a transient text-selection mode (status bar
shows a `[select]` chip): plain mouse-drag highlights cells in whichever pane
(a claude/shell PTY or a markdown tab) the drag started on — bypassing both PTY
mouse capture and the host terminal's whole-row selection — and releasing the
button copies the selection to your clipboard via OSC 52. In a markdown tab the
selection covers the visible area (scroll first, then drag). The mode auto-exits after the copy;
press `Esc` to cancel without copying. OSC 52 must be allowed by your outer
terminal for the clipboard write to land (xterm/Ghostty/kitty/foot allow it
by default; tmux needs `set -g allow-passthrough on`).

**On the projects pane:**

| Key | Action |
|---|---|
| `↑` / `↓` (or `j` / `k`) | move selection |
| `Enter` (or double-click) | open the selected project |
| `+` | add a project (modal) |
| `d` | delete the selected project (modal) |
| `u` | unload the selected project — kill its claude + shell, free the session (modal) |
| `/` | fuzzy filter |
| `r` | reload `projects.toml` from disk |
| `q` | quit |

**Claude session tabs:**

| Key | Action |
|---|---|
| `Alt+n` | new Claude tab (session picker) — only fires while the claude pane is focused |
| `Alt+m` | open a markdown file as a tab in the primary pane |
| `Alt+w` | close active tab (Claude or markdown; keeps ≥1 Claude tab) |
| `Alt+<` | previous tab |
| `Alt+>` | next tab |

A markdown tab renders the file with the bundled `wrk-markdown` engine
(headings, lists, tables, syntax-highlighted code, inline images incl. SVG, and
mermaid diagrams auto-themed to the terminal). When focused, scroll with `j`/`k`,
`PgUp`/`PgDn`, `Space`, `g`/`G`, reload from disk with `r`, and toggle a diagram's
opaque background with `b`. Markdown tabs are ephemeral — they aren't recorded in
`projects.toml` and are dropped on unload.

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
  { name = "refactor" },   # no session_id → spawns a fresh new session; wrk
                           # fills in the ID on first run and persists it
]

[[project]]
name = "notes-app"
path = "/home/rlamb/projects/notes-app"
tags = ["personal"]
```

Multiple projects can share the same `path` — they are differentiated by name
and each carries its own `claude_sessions` list. Resumption is keyed on the
recorded `session_id`, so two projects pointing at the same directory each
reopen their own Claude session, never each other's.

Closing every Claude tab leaves `claude_sessions = []`; the next time you open
the project, a fresh new session is spawned (not the one you just closed). Use
`Alt+n` to bring back a previously-used session from the on-disk session list.

The TUI watches this file (notify-rs) and reloads on external edits.

### `~/.config/wrk/settings.toml`

Optional. Lets you override the commands wrk launches per pane.

```toml
# Defaults: claude_command = ["claude"]
# wrk appends --resume <id> per tab; do not add --continue here (it's
# stripped for backwards compat and otherwise unused — resumption is
# always driven by a specific recorded session ID).
claude_command = ["steam-run", "claude"]

# Optional. Defaults to $SHELL, then /bin/bash.
# shell_command = ["zsh"]

# Optional theme overrides. Each value accepts a hex color (#rrggbb or
# #rgb) or one of the standard ratatui color names (case-insensitive:
# black, red, green, yellow, blue, magenta, cyan, white, gray, darkgray,
# lightred, lightgreen, lightyellow, lightblue, lightmagenta, lightcyan,
# reset). Anything you don't set keeps wrk's built-in default.
[theme]
border_focused    = "#5fafff"   # focused pane border
border_unfocused  = "darkgray"  # unfocused pane border
accent            = "#5fafff"   # active markers, tab + selection bg, status chip bg
accent_fg         = "black"     # text drawn on top of `accent`
hint              = "darkgray"  # hint/placeholder text
info              = "gray"      # subtle info text inside modals
error             = "#ff5f5f"   # error messages, danger borders
status_waiting    = "#5faf87"   # sidebar/tab dot: claude has finished (Stop)
status_busy       = "#d7af00"   # sidebar/tab dot: claude is processing
status_attention  = "#ff5f5f"   # sidebar/tab dot: claude needs attention
focus_indicator   = "yellow"    # `[focus]` label in the status bar

# Optional colors for the markdown viewer (Alt+m tabs and `wrk-md`).
# Same color syntax as [theme]; anything unset keeps the built-in default.
[markdown]
heading  = "cyan"       # heading text (all levels)
code     = "#d7d7af"    # inline code + code-block fallback text
code_bg  = "#262626"    # background behind code (unset = transparent)
link     = "blue"       # link text
quote    = "green"      # block-quote text + gutter
rule     = "darkgray"   # thematic-break rules + table borders
marker   = "yellow"     # list bullets/ordinals + task markers
faint    = "darkgray"   # image placeholders, diagram hints
table_row_bg     = "reset"     # even table body rows (unset = inherit surface)
table_row_alt_bg = "#262626"   # odd table body rows — the alternating stripe
heading_images = true          # render H1–H3 at true font size (false = # text)

# Optional shortcut overrides. Anything you don't set keeps the
# default. Modifiers: Ctrl, Alt, Shift, Super (case-insensitive).
# Keys: a single char, F1–F24, or a named key (Space, Enter, Tab,
# Esc, Backspace, Delete, Insert, Up/Down/Left/Right, Home, End,
# PageUp, PageDown). Use `Shift+a` for `A`; for symbols, use the
# shifted character directly (e.g. `Alt+<`, not `Alt+Shift+,`).
# Status-bar hints update automatically to reflect overrides.
[keys.global]
quit                     = "Alt+q"
focus_projects           = "Alt+1"
focus_claude             = "Alt+2"
focus_shell              = "Alt+3"
toggle_sidebar           = "Alt+0"
shrink_claude            = "Alt+h"
grow_claude              = "Alt+l"
toggle_layout            = "Alt+t"
new_claude_tab           = "Alt+n"   # only fires while the claude pane is focused
close_claude_tab         = "Alt+w"
prev_claude_tab          = "Alt+<"
next_claude_tab          = "Alt+>"
open_markdown            = "Alt+m"
leader_focus_projects    = "Ctrl+Space"
toggle_shell_passthrough = "F12"
open_link_picker         = "Alt+u"
enter_select_mode        = "Alt+s"
dump_grid                = "Alt+x"   # diagnostic: dump the focused PTY grid
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

`wrk-markdown` bundles the Liberation Sans fonts (used to rasterize SVG text
with the `images` feature), which are licensed under the SIL Open Font License
1.1 — see [crates/markdown/assets/fonts/LICENSE](crates/markdown/assets/fonts/LICENSE).
The OFL covers only those font files; it does not affect wrk's MIT license.
