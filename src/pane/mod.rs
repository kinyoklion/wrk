pub mod terminal;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Projects,
    Claude,
    Shell,
}

impl Focus {
    pub fn next(self) -> Self {
        match self {
            Focus::Projects => Focus::Claude,
            Focus::Claude => Focus::Shell,
            Focus::Shell => Focus::Projects,
        }
    }

    pub fn prev(self) -> Self {
        match self {
            Focus::Projects => Focus::Shell,
            Focus::Claude => Focus::Projects,
            Focus::Shell => Focus::Claude,
        }
    }
}

/// Encode a crossterm KeyEvent into the byte sequence a PTY child process expects.
/// Covers the common subset: printable chars, Enter/Tab/Backspace/Esc, arrows,
/// Home/End/PgUp/PgDn, F1-F12, and basic Ctrl-letter combos.
///
/// `app_cursor` selects between CSI form (`ESC [ X`) and SS3 form (`ESC O X`)
/// for the cursor keys (arrows, Home, End). ncurses-based programs like htop
/// flip the terminal into application-cursor mode (DECCKM) and only recognize
/// the SS3 form while it's set.
pub fn key_to_bytes(key: KeyEvent, app_cursor: bool) -> Vec<u8> {
    let mods = key.modifiers;
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let alt = mods.contains(KeyModifiers::ALT);
    let shift = mods.contains(KeyModifiers::SHIFT);

    // Shift+Enter is the conventional "insert a newline within input" key for
    // Claude Code (and many other TUIs). On terminals that report
    // disambiguated keys (Kitty keyboard protocol — wrk enables it on
    // startup), the SHIFT modifier reaches us. Translate it to ESC+CR, which
    // Claude Code accepts as a literal newline (same byte sequence as
    // Alt+Enter). Done as an early return so other modifier combinations
    // (e.g. Shift+Alt+Enter) don't double-prefix the ESC byte below.
    if matches!(key.code, KeyCode::Enter) && shift {
        return b"\x1b\r".to_vec();
    }

    let cursor = |c: char| -> Vec<u8> {
        if app_cursor {
            format!("\x1bO{c}").into_bytes()
        } else {
            format!("\x1b[{c}").into_bytes()
        }
    };

    let base: Vec<u8> = match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let c = c.to_ascii_lowercase();
                if c.is_ascii_alphabetic() {
                    vec![(c as u8) - b'a' + 1]
                } else {
                    match c {
                        ' ' => vec![0],
                        '@' => vec![0],
                        '[' => vec![27],
                        '\\' => vec![28],
                        ']' => vec![29],
                        '^' => vec![30],
                        '_' => vec![31],
                        '?' => vec![127],
                        _ => {
                            let mut buf = [0u8; 4];
                            c.encode_utf8(&mut buf).as_bytes().to_vec()
                        }
                    }
                }
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).as_bytes().to_vec()
            }
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Left => cursor('D'),
        KeyCode::Right => cursor('C'),
        KeyCode::Up => cursor('A'),
        KeyCode::Down => cursor('B'),
        KeyCode::Home => cursor('H'),
        KeyCode::End => cursor('F'),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::F(n) => match n {
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5 => b"\x1b[15~".to_vec(),
            6 => b"\x1b[17~".to_vec(),
            7 => b"\x1b[18~".to_vec(),
            8 => b"\x1b[19~".to_vec(),
            9 => b"\x1b[20~".to_vec(),
            10 => b"\x1b[21~".to_vec(),
            11 => b"\x1b[23~".to_vec(),
            12 => b"\x1b[24~".to_vec(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };

    if alt && !base.is_empty() {
        let mut out = Vec::with_capacity(base.len() + 1);
        out.push(0x1b);
        out.extend_from_slice(&base);
        out
    } else {
        base
    }
}

/// Snapshot of the embedded terminal's mouse-reporting state. Together these
/// flags determine which incoming mouse events should be encoded and forwarded
/// to the PTY child (and in which wire format).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MouseMode {
    /// `CSI ? 1000 h` — report button presses and releases.
    pub report_click: bool,
    /// `CSI ? 1002 h` — also report motion while a button is held.
    pub drag: bool,
    /// `CSI ? 1003 h` — report all motion (with or without buttons).
    pub motion: bool,
    /// `CSI ? 1006 h` — encode events with SGR escape sequences instead of
    /// the legacy single-byte form.
    pub sgr: bool,
}

impl MouseMode {
    /// True when any reporting mode is active.
    pub fn any(&self) -> bool {
        self.report_click || self.drag || self.motion
    }
}

/// Encode a crossterm `MouseEvent` as the byte sequence the PTY's child
/// program expects. `cx`/`cy` are 1-based pane-local coordinates (i.e. column
/// 1 is the leftmost cell of the inner content area).
///
/// Returns an empty `Vec` for event kinds that don't translate (e.g. plain
/// motion when no mouse mode is active — the caller is expected to gate on
/// `MouseMode` first).
pub fn mouse_to_bytes(event: MouseEvent, cx: u16, cy: u16, mode: MouseMode) -> Vec<u8> {
    let mods = event.modifiers;
    let mut mod_bits: u32 = 0;
    if mods.contains(KeyModifiers::SHIFT) {
        mod_bits |= 4;
    }
    if mods.contains(KeyModifiers::ALT) {
        mod_bits |= 8;
    }
    if mods.contains(KeyModifiers::CONTROL) {
        mod_bits |= 16;
    }

    let button_code = |b: MouseButton| -> u32 {
        match b {
            MouseButton::Left => 0,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
        }
    };

    let (cb, released) = match event.kind {
        MouseEventKind::Down(b) => (button_code(b), false),
        MouseEventKind::Up(b) => (button_code(b), true),
        MouseEventKind::Drag(b) => (button_code(b) | 32, false),
        // X10 button code 3 means "no button"; combined with the motion bit
        // this is the conventional "any-event" motion report.
        MouseEventKind::Moved => (3 | 32, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
    };
    let cb = cb | mod_bits;

    if mode.sgr {
        let final_byte = if released { 'm' } else { 'M' };
        format!("\x1b[<{cb};{cx};{cy}{final_byte}").into_bytes()
    } else {
        // Legacy X10: ESC [ M  Cb  Cx  Cy  with each byte offset by 32. On
        // release X10 reports button 3. Coordinates beyond 223 wrap; we
        // clamp to keep the bytes valid.
        let cb_byte = (if released { 3 + mod_bits } else { cb }).min(223) as u8 + 32;
        let cx_byte = (cx.min(223) as u8).saturating_add(32);
        let cy_byte = (cy.min(223) as u8).saturating_add(32);
        vec![0x1b, b'[', b'M', cb_byte, cx_byte, cy_byte]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers, MouseEvent};

    fn ev(kind: MouseEventKind, mods: KeyModifiers) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: mods,
        }
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn enter_no_modifiers_sends_cr() {
        assert_eq!(
            key_to_bytes(key(KeyCode::Enter, KeyModifiers::NONE), false),
            b"\r"
        );
    }

    #[test]
    fn shift_enter_sends_esc_cr() {
        assert_eq!(
            key_to_bytes(key(KeyCode::Enter, KeyModifiers::SHIFT), false),
            b"\x1b\r"
        );
    }

    #[test]
    fn shift_alt_enter_does_not_double_prefix() {
        // Sanity check: combining SHIFT with ALT must not produce ESC+ESC+CR.
        assert_eq!(
            key_to_bytes(
                key(KeyCode::Enter, KeyModifiers::SHIFT | KeyModifiers::ALT),
                false,
            ),
            b"\x1b\r"
        );
    }

    #[test]
    fn alt_enter_still_sends_esc_cr() {
        // Pre-existing behavior: Alt+Enter goes through the generic Alt-prefix
        // branch and produces the same ESC+CR sequence.
        assert_eq!(
            key_to_bytes(key(KeyCode::Enter, KeyModifiers::ALT), false),
            b"\x1b\r"
        );
    }

    #[test]
    fn sgr_left_press() {
        let m = MouseMode {
            report_click: true,
            sgr: true,
            ..MouseMode::default()
        };
        let bytes = mouse_to_bytes(
            ev(MouseEventKind::Down(MouseButton::Left), KeyModifiers::NONE),
            10,
            5,
            m,
        );
        assert_eq!(bytes, b"\x1b[<0;10;5M");
    }

    #[test]
    fn sgr_left_release_uses_lowercase_m() {
        let m = MouseMode {
            report_click: true,
            sgr: true,
            ..MouseMode::default()
        };
        let bytes = mouse_to_bytes(
            ev(MouseEventKind::Up(MouseButton::Left), KeyModifiers::NONE),
            10,
            5,
            m,
        );
        assert_eq!(bytes, b"\x1b[<0;10;5m");
    }

    #[test]
    fn sgr_drag_sets_motion_bit() {
        let m = MouseMode {
            drag: true,
            sgr: true,
            ..MouseMode::default()
        };
        let bytes = mouse_to_bytes(
            ev(MouseEventKind::Drag(MouseButton::Left), KeyModifiers::NONE),
            7,
            3,
            m,
        );
        // 0 (left) | 32 (motion) = 32.
        assert_eq!(bytes, b"\x1b[<32;7;3M");
    }

    #[test]
    fn sgr_motion_no_button() {
        let m = MouseMode {
            motion: true,
            sgr: true,
            ..MouseMode::default()
        };
        let bytes = mouse_to_bytes(ev(MouseEventKind::Moved, KeyModifiers::NONE), 4, 2, m);
        // 3 (no button) | 32 (motion) = 35.
        assert_eq!(bytes, b"\x1b[<35;4;2M");
    }

    #[test]
    fn sgr_wheel_up() {
        let m = MouseMode {
            report_click: true,
            sgr: true,
            ..MouseMode::default()
        };
        let bytes = mouse_to_bytes(ev(MouseEventKind::ScrollUp, KeyModifiers::NONE), 1, 1, m);
        assert_eq!(bytes, b"\x1b[<64;1;1M");
    }

    #[test]
    fn sgr_modifiers_or_into_button_code() {
        let m = MouseMode {
            report_click: true,
            sgr: true,
            ..MouseMode::default()
        };
        let bytes = mouse_to_bytes(
            ev(
                MouseEventKind::Down(MouseButton::Left),
                KeyModifiers::SHIFT | KeyModifiers::CONTROL,
            ),
            1,
            1,
            m,
        );
        // 0 (left) | 4 (shift) | 16 (ctrl) = 20.
        assert_eq!(bytes, b"\x1b[<20;1;1M");
    }

    #[test]
    fn x10_left_press_at_origin() {
        let m = MouseMode {
            report_click: true,
            ..MouseMode::default()
        };
        let bytes = mouse_to_bytes(
            ev(MouseEventKind::Down(MouseButton::Left), KeyModifiers::NONE),
            1,
            1,
            m,
        );
        // ESC [ M, button 0+32=32 (' '), col 1+32=33 ('!'), row 1+32=33 ('!').
        assert_eq!(bytes, b"\x1b[M !!");
    }

    #[test]
    fn x10_release_uses_button_three() {
        let m = MouseMode {
            report_click: true,
            ..MouseMode::default()
        };
        let bytes = mouse_to_bytes(
            ev(MouseEventKind::Up(MouseButton::Left), KeyModifiers::NONE),
            1,
            1,
            m,
        );
        // Button 3+32=35 ('#').
        assert_eq!(bytes, b"\x1b[M#!!");
    }
}
