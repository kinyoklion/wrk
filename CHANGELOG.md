# Changelog

## [0.1.14](https://github.com/kinyoklion/wrk/compare/wrk-v0.1.13...wrk-v0.1.14) (2026-07-17)

### Features

* **markdown:** fullscreen zoom/pan image viewer (#74) ([#74](https://github.com/kinyoklion/wrk/issues/74))

## [0.1.13](https://github.com/kinyoklion/wrk/compare/wrk-v0.1.12...wrk-v0.1.13) (2026-07-14)

### Features

* confirm before quitting the app (#73) ([#73](https://github.com/kinyoklion/wrk/issues/73)) ([#61](https://github.com/kinyoklion/wrk/issues/61))

### Bug Fixes

* expand `~` in project and markdown paths (#72) ([#72](https://github.com/kinyoklion/wrk/issues/72))
* **markdown:** fixed-size images with slice-on-scroll (#70) ([#70](https://github.com/kinyoklion/wrk/issues/70)) ([#69](https://github.com/kinyoklion/wrk/issues/69))

## [0.1.12](https://github.com/kinyoklion/wrk/compare/wrk-v0.1.11...wrk-v0.1.12) (2026-07-10)

### Features

* **markdown:** render H1–H3 headings at true font size (#66) ([#66](https://github.com/kinyoklion/wrk/issues/66))

## [0.1.11](https://github.com/kinyoklion/wrk/compare/wrk-v0.1.10...wrk-v0.1.11) (2026-07-10)

### Features

* **markdown:** transparent, auto-themed mermaid diagrams + background toggle (#65) ([#65](https://github.com/kinyoklion/wrk/issues/65))
* **markdown:** render mermaid diagrams via carcimaid (#64) ([#64](https://github.com/kinyoklion/wrk/issues/64))
* **markdown:** inline images incl. SVG in the viewer (#41) (#63) ([#41](https://github.com/kinyoklion/wrk/issues/41)) ([#63](https://github.com/kinyoklion/wrk/issues/63)) ([#41](https://github.com/kinyoklion/wrk/issues/41))
* **markdown:** alternating table row stripes + delineation (#56) (#60) ([#56](https://github.com/kinyoklion/wrk/issues/56)) ([#60](https://github.com/kinyoklion/wrk/issues/60))
* **markdown:** expose the markdown palette in settings (#57) (#59) ([#57](https://github.com/kinyoklion/wrk/issues/57)) ([#59](https://github.com/kinyoklion/wrk/issues/59)) ([#57](https://github.com/kinyoklion/wrk/issues/57))
* **markdown:** text selection + OSC 52 copy in the viewer (#49) (#58) ([#49](https://github.com/kinyoklion/wrk/issues/49)) ([#58](https://github.com/kinyoklion/wrk/issues/58)) ([#49](https://github.com/kinyoklion/wrk/issues/49))
* **markdown:** wrap table cells to the display width (#50) (#53) ([#50](https://github.com/kinyoklion/wrk/issues/50)) ([#53](https://github.com/kinyoklion/wrk/issues/53)) ([#50](https://github.com/kinyoklion/wrk/issues/50)) ([#50](https://github.com/kinyoklion/wrk/issues/50))

## [0.1.10](https://github.com/kinyoklion/wrk/compare/wrk-v0.1.9...wrk-v0.1.10) (2026-07-07)

### Bug Fixes

* **release:** keep workspace member versions in lockstep with release bumps (#51) ([#51](https://github.com/kinyoklion/wrk/issues/51))

## [0.1.9](https://github.com/kinyoklion/wrk/compare/wrk-v0.1.8...wrk-v0.1.9) (2026-07-06)

### Features

* **ipc:** `wrk view` opens files in the running TUI + Claude skill (#41) (#48) ([#41](https://github.com/kinyoklion/wrk/issues/41)) ([#48](https://github.com/kinyoklion/wrk/issues/48)) ([#41](https://github.com/kinyoklion/wrk/issues/41))
* **tui:** markdown documents as tabs in the primary pane (#41) (#47) ([#41](https://github.com/kinyoklion/wrk/issues/41)) ([#47](https://github.com/kinyoklion/wrk/issues/47))
* **markdown:** workspace split + render library + standalone viewer (#41) (#45) ([#41](https://github.com/kinyoklion/wrk/issues/41)) ([#45](https://github.com/kinyoklion/wrk/issues/45)) ([#41](https://github.com/kinyoklion/wrk/issues/41)) ([#41](https://github.com/kinyoklion/wrk/issues/41))
* unload a project's session from the sidebar (#40) (#43) ([#40](https://github.com/kinyoklion/wrk/issues/40)) ([#43](https://github.com/kinyoklion/wrk/issues/43))

## 0.1.8 (2026-06-10)

### Features

* per-pane text selection + OSC 52 copy (#14) (#35) ([#14](https://github.com/kinyoklion/wrk/issues/14)) ([#35](https://github.com/kinyoklion/wrk/issues/35)) ([#14](https://github.com/kinyoklion/wrk/issues/14))
* bracketed paste support (#29) (#34) ([#29](https://github.com/kinyoklion/wrk/issues/29)) ([#34](https://github.com/kinyoklion/wrk/issues/34))
* open URLs by Shift+click or Alt+u picker (#32) (#33) ([#32](https://github.com/kinyoklion/wrk/issues/32)) ([#33](https://github.com/kinyoklion/wrk/issues/33))
* Customizable global shortcuts via [keys.global] in settings.toml (#28) ([#28](https://github.com/kinyoklion/wrk/issues/28)) ([#12](https://github.com/kinyoklion/wrk/issues/12))
* Per-project shell-pane passthrough toggled with F12 (#24) ([#24](https://github.com/kinyoklion/wrk/issues/24)) ([#11](https://github.com/kinyoklion/wrk/issues/11))
* Add chrome theming via [theme] in settings.toml (#22) ([#22](https://github.com/kinyoklion/wrk/issues/22)) ([#15](https://github.com/kinyoklion/wrk/issues/15))
* Forward Shift+Enter to claude as ESC+CR for multi-line input (#21) ([#21](https://github.com/kinyoklion/wrk/issues/21)) ([#16](https://github.com/kinyoklion/wrk/issues/16))
* Forward mouse events to PTY apps that enable mouse reporting (#9) ([#9](https://github.com/kinyoklion/wrk/issues/9)) ([#4](https://github.com/kinyoklion/wrk/issues/4))
* Multiple Claude sessions per project and per-directory projects (#3) ([#3](https://github.com/kinyoklion/wrk/issues/3)) ([#1](https://github.com/kinyoklion/wrk/issues/1))

### Bug Fixes

* deterministic claude session resumption (#36) ([#36](https://github.com/kinyoklion/wrk/issues/36))
* Forward Down click to mouse-aware panes regardless of focus (#25) ([#25](https://github.com/kinyoklion/wrk/issues/25)) ([#13](https://github.com/kinyoklion/wrk/issues/13)) ([#22](https://github.com/kinyoklion/wrk/issues/22)) ([#24](https://github.com/kinyoklion/wrk/issues/24)) ([#13](https://github.com/kinyoklion/wrk/issues/13)) ([#13](https://github.com/kinyoklion/wrk/issues/13))
* Build break on main from theme + passthrough merge collision (#26) ([#26](https://github.com/kinyoklion/wrk/issues/26)) ([#22](https://github.com/kinyoklion/wrk/issues/22)) ([#24](https://github.com/kinyoklion/wrk/issues/24))
* Restore Alt+</> claude-tab shortcuts under kitty keyboard protocol (#23) ([#23](https://github.com/kinyoklion/wrk/issues/23)) ([#21](https://github.com/kinyoklion/wrk/issues/21))
* Route clicks on the Claude tab strip to switch tabs (#20) ([#20](https://github.com/kinyoklion/wrk/issues/20)) ([#17](https://github.com/kinyoklion/wrk/issues/17)) ([#9](https://github.com/kinyoklion/wrk/issues/9))
* Mark wide-char spacer cells as skip and capture combining marks (#18) ([#18](https://github.com/kinyoklion/wrk/issues/18))

## 0.1.7 (2026-06-10)

### Features

* per-pane text selection + OSC 52 copy (#14) (#35) ([#14](https://github.com/kinyoklion/wrk/issues/14)) ([#35](https://github.com/kinyoklion/wrk/issues/35)) ([#14](https://github.com/kinyoklion/wrk/issues/14))
* bracketed paste support (#29) (#34) ([#29](https://github.com/kinyoklion/wrk/issues/29)) ([#34](https://github.com/kinyoklion/wrk/issues/34))
* open URLs by Shift+click or Alt+u picker (#32) (#33) ([#32](https://github.com/kinyoklion/wrk/issues/32)) ([#33](https://github.com/kinyoklion/wrk/issues/33))
* Customizable global shortcuts via [keys.global] in settings.toml (#28) ([#28](https://github.com/kinyoklion/wrk/issues/28)) ([#12](https://github.com/kinyoklion/wrk/issues/12))
* Per-project shell-pane passthrough toggled with F12 (#24) ([#24](https://github.com/kinyoklion/wrk/issues/24)) ([#11](https://github.com/kinyoklion/wrk/issues/11))
* Add chrome theming via [theme] in settings.toml (#22) ([#22](https://github.com/kinyoklion/wrk/issues/22)) ([#15](https://github.com/kinyoklion/wrk/issues/15))
* Forward Shift+Enter to claude as ESC+CR for multi-line input (#21) ([#21](https://github.com/kinyoklion/wrk/issues/21)) ([#16](https://github.com/kinyoklion/wrk/issues/16))
* Forward mouse events to PTY apps that enable mouse reporting (#9) ([#9](https://github.com/kinyoklion/wrk/issues/9)) ([#4](https://github.com/kinyoklion/wrk/issues/4))
* Multiple Claude sessions per project and per-directory projects (#3) ([#3](https://github.com/kinyoklion/wrk/issues/3)) ([#1](https://github.com/kinyoklion/wrk/issues/1))

### Bug Fixes

* deterministic claude session resumption (#36) ([#36](https://github.com/kinyoklion/wrk/issues/36))
* Forward Down click to mouse-aware panes regardless of focus (#25) ([#25](https://github.com/kinyoklion/wrk/issues/25)) ([#13](https://github.com/kinyoklion/wrk/issues/13)) ([#22](https://github.com/kinyoklion/wrk/issues/22)) ([#24](https://github.com/kinyoklion/wrk/issues/24)) ([#13](https://github.com/kinyoklion/wrk/issues/13)) ([#13](https://github.com/kinyoklion/wrk/issues/13))
* Build break on main from theme + passthrough merge collision (#26) ([#26](https://github.com/kinyoklion/wrk/issues/26)) ([#22](https://github.com/kinyoklion/wrk/issues/22)) ([#24](https://github.com/kinyoklion/wrk/issues/24))
* Restore Alt+</> claude-tab shortcuts under kitty keyboard protocol (#23) ([#23](https://github.com/kinyoklion/wrk/issues/23)) ([#21](https://github.com/kinyoklion/wrk/issues/21))
* Route clicks on the Claude tab strip to switch tabs (#20) ([#20](https://github.com/kinyoklion/wrk/issues/20)) ([#17](https://github.com/kinyoklion/wrk/issues/17)) ([#9](https://github.com/kinyoklion/wrk/issues/9))
* Mark wide-char spacer cells as skip and capture combining marks (#18) ([#18](https://github.com/kinyoklion/wrk/issues/18))

## 0.1.6 (2026-05-13)

### Features

* per-pane text selection + OSC 52 copy (#14) (#35) ([#14](https://github.com/kinyoklion/wrk/issues/14)) ([#35](https://github.com/kinyoklion/wrk/issues/35)) ([#14](https://github.com/kinyoklion/wrk/issues/14))
* bracketed paste support (#29) (#34) ([#29](https://github.com/kinyoklion/wrk/issues/29)) ([#34](https://github.com/kinyoklion/wrk/issues/34))
* open URLs by Shift+click or Alt+u picker (#32) (#33) ([#32](https://github.com/kinyoklion/wrk/issues/32)) ([#33](https://github.com/kinyoklion/wrk/issues/33))
* Customizable global shortcuts via [keys.global] in settings.toml (#28) ([#28](https://github.com/kinyoklion/wrk/issues/28)) ([#12](https://github.com/kinyoklion/wrk/issues/12))
* Per-project shell-pane passthrough toggled with F12 (#24) ([#24](https://github.com/kinyoklion/wrk/issues/24)) ([#11](https://github.com/kinyoklion/wrk/issues/11))
* Add chrome theming via [theme] in settings.toml (#22) ([#22](https://github.com/kinyoklion/wrk/issues/22)) ([#15](https://github.com/kinyoklion/wrk/issues/15))
* Forward Shift+Enter to claude as ESC+CR for multi-line input (#21) ([#21](https://github.com/kinyoklion/wrk/issues/21)) ([#16](https://github.com/kinyoklion/wrk/issues/16))
* Forward mouse events to PTY apps that enable mouse reporting (#9) ([#9](https://github.com/kinyoklion/wrk/issues/9)) ([#4](https://github.com/kinyoklion/wrk/issues/4))
* Multiple Claude sessions per project and per-directory projects (#3) ([#3](https://github.com/kinyoklion/wrk/issues/3)) ([#1](https://github.com/kinyoklion/wrk/issues/1))

### Bug Fixes

* Forward Down click to mouse-aware panes regardless of focus (#25) ([#25](https://github.com/kinyoklion/wrk/issues/25)) ([#13](https://github.com/kinyoklion/wrk/issues/13)) ([#22](https://github.com/kinyoklion/wrk/issues/22)) ([#24](https://github.com/kinyoklion/wrk/issues/24)) ([#13](https://github.com/kinyoklion/wrk/issues/13)) ([#13](https://github.com/kinyoklion/wrk/issues/13))
* Build break on main from theme + passthrough merge collision (#26) ([#26](https://github.com/kinyoklion/wrk/issues/26)) ([#22](https://github.com/kinyoklion/wrk/issues/22)) ([#24](https://github.com/kinyoklion/wrk/issues/24))
* Restore Alt+</> claude-tab shortcuts under kitty keyboard protocol (#23) ([#23](https://github.com/kinyoklion/wrk/issues/23)) ([#21](https://github.com/kinyoklion/wrk/issues/21))
* Route clicks on the Claude tab strip to switch tabs (#20) ([#20](https://github.com/kinyoklion/wrk/issues/20)) ([#17](https://github.com/kinyoklion/wrk/issues/17)) ([#9](https://github.com/kinyoklion/wrk/issues/9))
* Mark wide-char spacer cells as skip and capture combining marks (#18) ([#18](https://github.com/kinyoklion/wrk/issues/18))

## 0.1.5 (2026-05-12)

### Features

* bracketed paste support (#29) (#34) ([#29](https://github.com/kinyoklion/wrk/issues/29)) ([#34](https://github.com/kinyoklion/wrk/issues/34))
* open URLs by Shift+click or Alt+u picker (#32) (#33) ([#32](https://github.com/kinyoklion/wrk/issues/32)) ([#33](https://github.com/kinyoklion/wrk/issues/33))
* Customizable global shortcuts via [keys.global] in settings.toml (#28) ([#28](https://github.com/kinyoklion/wrk/issues/28)) ([#12](https://github.com/kinyoklion/wrk/issues/12))
* Per-project shell-pane passthrough toggled with F12 (#24) ([#24](https://github.com/kinyoklion/wrk/issues/24)) ([#11](https://github.com/kinyoklion/wrk/issues/11))
* Add chrome theming via [theme] in settings.toml (#22) ([#22](https://github.com/kinyoklion/wrk/issues/22)) ([#15](https://github.com/kinyoklion/wrk/issues/15))
* Forward Shift+Enter to claude as ESC+CR for multi-line input (#21) ([#21](https://github.com/kinyoklion/wrk/issues/21)) ([#16](https://github.com/kinyoklion/wrk/issues/16))
* Forward mouse events to PTY apps that enable mouse reporting (#9) ([#9](https://github.com/kinyoklion/wrk/issues/9)) ([#4](https://github.com/kinyoklion/wrk/issues/4))
* Multiple Claude sessions per project and per-directory projects (#3) ([#3](https://github.com/kinyoklion/wrk/issues/3)) ([#1](https://github.com/kinyoklion/wrk/issues/1))

### Bug Fixes

* Forward Down click to mouse-aware panes regardless of focus (#25) ([#25](https://github.com/kinyoklion/wrk/issues/25)) ([#13](https://github.com/kinyoklion/wrk/issues/13)) ([#22](https://github.com/kinyoklion/wrk/issues/22)) ([#24](https://github.com/kinyoklion/wrk/issues/24)) ([#13](https://github.com/kinyoklion/wrk/issues/13)) ([#13](https://github.com/kinyoklion/wrk/issues/13))
* Build break on main from theme + passthrough merge collision (#26) ([#26](https://github.com/kinyoklion/wrk/issues/26)) ([#22](https://github.com/kinyoklion/wrk/issues/22)) ([#24](https://github.com/kinyoklion/wrk/issues/24))
* Restore Alt+</> claude-tab shortcuts under kitty keyboard protocol (#23) ([#23](https://github.com/kinyoklion/wrk/issues/23)) ([#21](https://github.com/kinyoklion/wrk/issues/21))
* Route clicks on the Claude tab strip to switch tabs (#20) ([#20](https://github.com/kinyoklion/wrk/issues/20)) ([#17](https://github.com/kinyoklion/wrk/issues/17)) ([#9](https://github.com/kinyoklion/wrk/issues/9))
* Mark wide-char spacer cells as skip and capture combining marks (#18) ([#18](https://github.com/kinyoklion/wrk/issues/18))

## 0.1.4 (2026-05-07)

### Features

* Per-project shell-pane passthrough toggled with F12 (#24) ([#24](https://github.com/kinyoklion/wrk/issues/24)) ([#11](https://github.com/kinyoklion/wrk/issues/11))
* Add chrome theming via [theme] in settings.toml (#22) ([#22](https://github.com/kinyoklion/wrk/issues/22)) ([#15](https://github.com/kinyoklion/wrk/issues/15))
* Forward Shift+Enter to claude as ESC+CR for multi-line input (#21) ([#21](https://github.com/kinyoklion/wrk/issues/21)) ([#16](https://github.com/kinyoklion/wrk/issues/16))
* Forward mouse events to PTY apps that enable mouse reporting (#9) ([#9](https://github.com/kinyoklion/wrk/issues/9)) ([#4](https://github.com/kinyoklion/wrk/issues/4))
* Multiple Claude sessions per project and per-directory projects (#3) ([#3](https://github.com/kinyoklion/wrk/issues/3)) ([#1](https://github.com/kinyoklion/wrk/issues/1))

### Bug Fixes

* Forward Down click to mouse-aware panes regardless of focus (#25) ([#25](https://github.com/kinyoklion/wrk/issues/25)) ([#13](https://github.com/kinyoklion/wrk/issues/13)) ([#22](https://github.com/kinyoklion/wrk/issues/22)) ([#24](https://github.com/kinyoklion/wrk/issues/24)) ([#13](https://github.com/kinyoklion/wrk/issues/13)) ([#13](https://github.com/kinyoklion/wrk/issues/13))
* Build break on main from theme + passthrough merge collision (#26) ([#26](https://github.com/kinyoklion/wrk/issues/26)) ([#22](https://github.com/kinyoklion/wrk/issues/22)) ([#24](https://github.com/kinyoklion/wrk/issues/24))
* Restore Alt+</> claude-tab shortcuts under kitty keyboard protocol (#23) ([#23](https://github.com/kinyoklion/wrk/issues/23)) ([#21](https://github.com/kinyoklion/wrk/issues/21))
* Route clicks on the Claude tab strip to switch tabs (#20) ([#20](https://github.com/kinyoklion/wrk/issues/20)) ([#17](https://github.com/kinyoklion/wrk/issues/17)) ([#9](https://github.com/kinyoklion/wrk/issues/9))
* Mark wide-char spacer cells as skip and capture combining marks (#18) ([#18](https://github.com/kinyoklion/wrk/issues/18))

## 0.1.3 (2026-05-06)

### Features

* Forward mouse events to PTY apps that enable mouse reporting (#9) ([#9](https://github.com/kinyoklion/wrk/issues/9)) ([#4](https://github.com/kinyoklion/wrk/issues/4))
* Multiple Claude sessions per project and per-directory projects (#3) ([#3](https://github.com/kinyoklion/wrk/issues/3)) ([#1](https://github.com/kinyoklion/wrk/issues/1))

## 0.1.2 (2026-05-06)

### Features

* Multiple Claude sessions per project and per-directory projects (#3) ([#3](https://github.com/kinyoklion/wrk/issues/3)) ([#1](https://github.com/kinyoklion/wrk/issues/1))

## 0.1.1 (2026-05-06)

### Features

* Multiple Claude sessions per project and per-directory projects (#3) ([#3](https://github.com/kinyoklion/wrk/issues/3)) ([#1](https://github.com/kinyoklion/wrk/issues/1))
