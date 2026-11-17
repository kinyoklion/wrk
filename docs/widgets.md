# Widgets, notifications, and the wrk API

A **widget** is any terminal program that wrk shows in a floating overlay: a
shell script with `fzf`, a full TUI, anything that runs in a terminal. wrk
just hosts it. The program draws its own UI and talks back to wrk through
**`wrk api`**: list the sessions, type into one, switch to one, post a
**notification**. The same API works from any process running inside wrk (a
pane, a hook, a background script), not just from widgets.

A worked example ships in [`examples/widgets/github-prs.sh`](../examples/widgets/github-prs.sh).
It lists the repository's open pull requests and sends the one you pick to an
agent session as a review request.

## Configuring a widget

In `~/.config/wrk/settings.toml`:

```toml
[widget.prs]
command = ["/path/to/github-prs.sh"]   # required: the program and its args
title   = "Pull requests"              # overlay title (default: the name)
key     = "Alt+g"                      # optional: summon this widget directly
scope   = "project"                    # "project" (default) or "global"
# For a global widget only:
# cwd       = "~/src"                  # working directory (default $HOME)
# autostart = true                     # start hidden when wrk starts
```

- **Summoning.** `Alt+e` summons a widget: directly when you have one, from a
  picker when you have several. A widget's own `key` summons it from anywhere.
  The same key (or `Alt+e`) hides it again, and so does a click outside the
  overlay. While a widget is up it gets every key except those two and the
  notification keys.
- **It keeps running while hidden.** Hiding a widget doesn't stop it, so it
  comes back exactly as you left it, and it can keep working (and notify) in
  the background.
- **Scope.** A `project` widget runs once per project: in that project's
  directory, with `WRK_PROJECT` set. That's what you want for "this repo's
  PRs". A `global` widget runs once for the whole app. With `autostart = true`
  it starts hidden when wrk starts, so it can post notifications before you
  ever open it.
- **Exiting.** When the program exits successfully, the overlay just closes,
  so a one-shot picker can simply exit when it's done. When it exits with a
  failure while on screen, the overlay stays up with its last output and the
  exit status until you press a key. If it fails while hidden, the status bar
  says so.

A widget's environment:

| Variable | |
|---|---|
| `WRK_SOCK` | wrk's socket, which `wrk api` uses |
| `WRK_BIN` | the running wrk binary: call `"$WRK_BIN" api …`, not a bare `wrk` |
| `WRK_WIDGET` | the widget's name |
| `WRK_PROJECT` | the project, for a `project`-scoped widget |

## Notifications

`wrk api banner` posts a banner across the top row of the screen. It covers
the pane borders, so nothing below it moves. A notification can point at a
session, a project or a widget. Clicking the banner (or `Alt+o`) opens that
target; its `✕` (or `Alt+c`) dismisses it. A `--ttl` makes it go away on its
own. If several are queued, the newest shows, with a `+N` count.

```sh
"$WRK_BIN" api banner "CI passed on #12" --body "ready to merge" --session "$WRK_TAB"
```

Two lighter-weight commands also work from a widget (see the README):
`wrk notify <message>` shows a one-line message in the status bar, with
nothing to click, and `wrk confirm <question>` asks the user yes or no and
exits 0 or 1 with the answer.

## The API

### From a script: `wrk api`

| Command | |
|---|---|
| `wrk api sessions [--json]` | The open projects' sessions, one per line: `id`, `kind`, `project`, `name`, `status` (tab-separated). `--json` prints every field. |
| `wrk api send <session> [text…] [--submit] [--focus]` | Types the text into the session as a paste (bracketed when the program asked for that, so multi-line text arrives whole). With no text arguments it reads stdin, without its final newline. `--submit` presses Enter after a short pause. `--focus` switches to the session and hides the widget. |
| `wrk api focus <session>` | Switches to a session: opens its project on that tab and focuses it. |
| `wrk api banner <title> [--body B] [--session S \| --project P \| --widget W] [--ttl SECS]` | Posts a notification banner and prints its id. |
| `wrk api dismiss <id>` | Takes a notification down. |
| `wrk api show <widget>` | Shows a widget, starting it if needed. |
| `wrk api hide` | Hides the widget on screen. |

Errors (an unknown session, a session that isn't running, …) print to stderr
and exit non-zero.

**Sessions.** An agent tab's id is its `WRK_TAB` (`tab3`), so an agent's own
hooks can name it. A project's shell is `shell:<project>`. A markdown tab is
`md:<project>:<index>`; you can focus it but not type into it. `--json` adds:

| Field | |
|---|---|
| `kind` | `agent`, `shell`, or `markdown` |
| `harness` | `claude` / `kimi` (agents) |
| `status` | agents: `busy`, `idle` (done, waiting for a prompt), `attention` (needs you, e.g. a permission prompt), `none` (not running) |
| `running` | has a live terminal (`send` needs one) |
| `active` | in the project on screen, and its shown tab (or its shell) |
| `focused` | has the keyboard (once any widget is out of the way) |
| `project_path`, `session_id`, `path` | the project's directory, the agent's own session id, a markdown tab's file |

Only open projects are listed. A project you haven't opened has no running
sessions to talk to.

### From any language: the socket

`wrk api` is a thin client over a Unix socket at `$WRK_SOCK`. Connect, write
one line of JSON, and read one line back:

```sh
printf '%s\n' '{"cmd":"sessions"}' | socat - "UNIX-CONNECT:$WRK_SOCK"
# {"ok":true,"sessions":[{"id":"tab0","kind":"agent",…}]}
```

| Request | Reply body |
|---|---|
| `{"cmd":"sessions"}` | `{"sessions":[…]}` |
| `{"cmd":"send","session":"tab0","text":"…","submit":false,"focus":false}` | `{}` |
| `{"cmd":"focus","session":"tab0"}` | `{}` |
| `{"cmd":"banner","title":"…","body":"…","session":"…","project":"…","widget":"…","ttl":30}` | `{"id":1}` |
| `{"cmd":"dismiss","id":1}` | `{}` |
| `{"cmd":"show","widget":"prs"}` / `{"cmd":"hide"}` | `{}` |

Optional fields can be left out. A reply is `{"ok":true, …body}` or
`{"ok":false,"error":"…"}`.
