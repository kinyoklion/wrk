#!/usr/bin/env bash
# A wrk widget: pick one of the repository's open pull requests and hand it to
# an agent session.
#
# Needs `gh` (signed in). Uses `fzf` when it's installed, else a numbered menu.
# Runs in the project's directory (widgets are project-scoped by default), so
# `gh` lists that repository's PRs. Add to ~/.config/wrk/settings.toml:
#
#   [widget.prs]
#   command = ["/path/to/github-prs.sh"]
#   title = "Pull requests"
#   key = "Alt+g"        # optional: summon it directly
#
# Esc (in fzf) or q (in the menu) closes the widget.
set -uo pipefail

# Choose one of the lines on stdin; prints it. Fails when nothing was chosen.
pick() {
  local prompt=$1
  if command -v fzf >/dev/null; then
    fzf --prompt "$prompt> " --reverse --delimiter '\t' --with-nth 1,2
    return
  fi
  local items line i
  mapfile -t items
  # The menu reads the terminal: stdin is the list we were piped.
  while true; do
    for i in "${!items[@]}"; do
      printf '%3d) %s\n' $((i + 1)) "$(cut -f1,2 <<<"${items[$i]}" | tr '\t' ' ')" >&2
    done
    read -r -p "$prompt (number, q to quit): " line </dev/tty || return 1
    [[ $line == q ]] && return 1
    if [[ $line =~ ^[0-9]+$ ]] && ((line >= 1 && line <= ${#items[@]})); then
      printf '%s\n' "${items[$((line - 1))]}"
      return
    fi
  done
}

while true; do
  clear
  echo "Pull requests in $(basename "$PWD")"
  if ! prs=$(gh pr list --limit 50 --json number,title,headRefName,url \
    --jq '.[] | "#\(.number)\t\(.title)\t\(.headRefName)\t\(.url)"'); then
    read -r -p "gh pr list failed. Enter to retry. " _
    continue
  fi
  if [[ -z $prs ]]; then
    read -r -p "No open pull requests. Enter to refresh. " _
    continue
  fi
  pr=$(pick "pull request" <<<"$prs") || exit 0

  # Agent sessions only: id, kind, project, name, status.
  sessions=$("$WRK_BIN" api sessions | awk -F'\t' '$2 == "agent" { print $1 "\t" $3 "/" $4 " (" $5 ")" }')
  if [[ -z $sessions ]]; then
    read -r -p "No agent sessions are open. Enter to go back. " _
    continue
  fi
  session=$(pick "send to" <<<"$sessions") || continue

  IFS=$'\t' read -r number title branch url <<<"$pr"
  # Types the request into the session, presses Enter, and switches to it
  # (which also hides this widget, ready with a fresh list next time).
  "$WRK_BIN" api send "${session%%$'\t'*}" --submit --focus \
    "Please review pull request $number ($branch): $title — $url"
done
