pub mod terminal;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
