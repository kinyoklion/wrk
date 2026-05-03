# wrk — Requirements

## Purpose

A single application for managing the user's day-to-day work across multiple projects, where each project hosts one or more concurrent Claude Code sessions plus interactive shells. The user works on several projects in parallel and wants one tool to keep them organized, switch between them quickly, and keep each project's context isolated.

## Functional requirements

### Projects

- A **project** is a named entity associated with a directory on disk.
- The user has many projects and switches between them frequently.
- The user can **add a project from inside the UI** — analogous to "new file" in an IDE — by pointing at a directory and giving it a name.
- The user can rename and remove projects from the UI.
- The list of projects is the **source of truth in a single plain-text file** the user can read, hand-edit, sync to a dotfiles repo, push to GitHub, etc. No hidden database.
- The project list is visible at all times (a persistent sidebar) so the user can switch projects from anywhere.

### Per-project work surfaces

For the active project, the user expects three primary surfaces, all visible together:

1. **Project list** (left sidebar) — see above.
2. **Claude pane** (center) — runs a Claude Code session in the project's directory; resumes prior session when reopened.
3. **Shell pane** (right) — interactive shell(s) in the project's directory.

The **shell pane is tabbed** — the user can open multiple concurrent shells per project and switch between them.

### Optional surfaces

These are nice-to-have, not required for v1:

- **File browser** showing the project's tree.
- **Markdown viewer** for rendering the project's research/plan files.

### Research and plan files

- Markdown files Claude produces during research and planning (`/plan` mode output, scratch notes, etc.) must land **in the current project's directory**, not in a global location. They become part of the project and can be checked in or shared.

### Session continuity

- Switching away from a project and back to it should not lose state — Claude's session resumes, shells stay open (or are restored), the user picks up where they left off.

## Non-functional requirements

### Platform

- **Linux only.** The user runs this on multiple Linux machines.
- Must work cleanly on **both X11 and Wayland**.
- **Native rendering** — no webview, no browser engine.
- **Crisp HiDPI** — looks sharp on high-DPI displays without manual scaling tweaks.

### Performance and language

- Written in **Rust** (Go or Zig acceptable alternatives).
- Self-contained: prefer a single binary with no host-application sandbox to fight.

### Environment

- The user's machines run **NixOS**. A reproducible Nix dev environment (flake or shell) is expected for development.
- The `claude` binary is assumed installed on the user's `PATH`.

### Simplicity

- The architecture should not depend on coordinating multiple unrelated applications, embedding into another application's plugin sandbox, or other indirection that introduces fragility. One application, one binary, one mental model.

## Out of scope

- Windows, macOS support.
- A GUI toolkit (Qt/GTK/etc.) — see "native rendering" above; the application can be a TUI as long as it meets the platform requirements.
- Cloud sync, telemetry, account systems.
- Any feature requiring code changes to the `claude` CLI itself.
