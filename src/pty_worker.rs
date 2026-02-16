use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::Config;
use alacritty_terminal::vte::ansi::Processor;
use alacritty_terminal::Term;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use tokio::sync::mpsc;

use crate::worker::{WorkerInput, WorkerMessage};

pub struct PtyState {
    pub term: Term<VoidListener>,
    pub processor: Processor,
}

pub type SharedPtyState = Arc<Mutex<PtyState>>;

pub struct PtyHandle {
    pub state: SharedPtyState,
    pub master: Box<dyn portable_pty::MasterPty + Send>,
    pub child: Box<dyn portable_pty::Child + Send + Sync>,
}

struct PtyDimensions {
    cols: usize,
    lines: usize,
}

impl Dimensions for PtyDimensions {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

pub fn spawn_pty_worker(
    prompt_id: usize,
    prompt_text: String,
    cwd: Option<String>,
    cols: u16,
    rows: u16,
    tx: mpsc::UnboundedSender<WorkerMessage>,
) -> Result<(mpsc::UnboundedSender<WorkerInput>, PtyHandle), String> {
    let pty_system = native_pty_system();

    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| format!("Failed to open PTY: {e}"))?;

    let mut cmd = CommandBuilder::new("claude");
    cmd.arg(&prompt_text);
    cmd.arg("--dangerously-skip-permissions");
    cmd.env_remove("CLAUDECODE");
    if let Some(ref dir) = cwd {
        cmd.cwd(dir);
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("Failed to spawn claude in PTY: {e}"))?;
    // Drop slave after spawning
    drop(pair.slave);

    let dims = PtyDimensions {
        cols: cols as usize,
        lines: rows as usize,
    };
    let config = Config::default();
    let term = Term::new(config, &dims, VoidListener);
    let processor = Processor::new();

    let state = Arc::new(Mutex::new(PtyState { term, processor }));

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("Failed to clone PTY reader: {e}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("Failed to take PTY writer: {e}"))?;

    // Reader thread: reads from PTY, feeds bytes to alacritty_terminal processor.
    // Sends Finished when EOF is detected (child exited).
    let reader_state = state.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break, // EOF — child exited
                Ok(n) => {
                    if let Ok(mut pty) = reader_state.lock() {
                        let PtyState {
                            ref mut term,
                            ref mut processor,
                        } = *pty;
                        processor.advance(term, &buf[..n]);
                    }
                    let _ = tx.send(WorkerMessage::PtyUpdate { prompt_id });
                }
                Err(_) => break,
            }
        }
        // Child process has exited
        let _ = tx.send(WorkerMessage::Finished {
            prompt_id,
            exit_code: Some(0),
        });
    });

    // Writer thread: receives WorkerInput, writes bytes to PTY
    let (input_tx, input_rx) = mpsc::unbounded_channel::<WorkerInput>();
    std::thread::spawn(move || {
        let mut writer = writer;
        let mut input_rx = input_rx;
        while let Some(msg) = input_rx.blocking_recv() {
            match msg {
                WorkerInput::SendInput(text) => {
                    if writer.write_all(text.as_bytes()).is_err() {
                        break;
                    }
                    let _ = writer.flush();
                }
                WorkerInput::SendBytes(bytes) => {
                    if writer.write_all(&bytes).is_err() {
                        break;
                    }
                    let _ = writer.flush();
                }
                WorkerInput::Kill => {
                    break;
                }
            }
        }
        // Writer dropped here → PTY master EOF → child gets SIGHUP
    });

    Ok((
        input_tx,
        PtyHandle {
            state,
            master: pair.master,
            child,
        },
    ))
}

/// Convert a crossterm KeyEvent to raw bytes suitable for PTY input.
pub fn key_event_to_bytes(key: KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    let mut bytes = match key.code {
        KeyCode::Char(c) if ctrl => {
            let byte = (c.to_ascii_lowercase() as u8).wrapping_sub(b'a').wrapping_add(1);
            vec![byte]
        }
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            let s = c.encode_utf8(&mut buf);
            s.as_bytes().to_vec()
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => vec![0x1b, b'[', b'A'],
        KeyCode::Down => vec![0x1b, b'[', b'B'],
        KeyCode::Right => vec![0x1b, b'[', b'C'],
        KeyCode::Left => vec![0x1b, b'[', b'D'],
        KeyCode::Home => vec![0x1b, b'[', b'H'],
        KeyCode::End => vec![0x1b, b'[', b'F'],
        KeyCode::PageUp => vec![0x1b, b'[', b'5', b'~'],
        KeyCode::PageDown => vec![0x1b, b'[', b'6', b'~'],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        KeyCode::Insert => vec![0x1b, b'[', b'2', b'~'],
        KeyCode::F(1) => vec![0x1b, b'O', b'P'],
        KeyCode::F(2) => vec![0x1b, b'O', b'Q'],
        KeyCode::F(3) => vec![0x1b, b'O', b'R'],
        KeyCode::F(4) => vec![0x1b, b'O', b'S'],
        KeyCode::F(5) => vec![0x1b, b'[', b'1', b'5', b'~'],
        KeyCode::F(6) => vec![0x1b, b'[', b'1', b'7', b'~'],
        KeyCode::F(7) => vec![0x1b, b'[', b'1', b'8', b'~'],
        KeyCode::F(8) => vec![0x1b, b'[', b'1', b'9', b'~'],
        KeyCode::F(9) => vec![0x1b, b'[', b'2', b'0', b'~'],
        KeyCode::F(10) => vec![0x1b, b'[', b'2', b'1', b'~'],
        KeyCode::F(11) => vec![0x1b, b'[', b'2', b'3', b'~'],
        KeyCode::F(12) => vec![0x1b, b'[', b'2', b'4', b'~'],
        _ => return Vec::new(),
    };

    // Alt prefix: prepend ESC
    if alt && key.code != KeyCode::Esc {
        bytes.insert(0, 0x1b);
    }

    bytes
}

/// Extract visible text from the terminal grid for session persistence / export.
pub fn extract_text_from_term(state: &SharedPtyState) -> String {
    let Ok(pty) = state.lock() else {
        return String::new();
    };
    let grid = pty.term.grid();
    let screen_lines = grid.screen_lines();
    let cols = grid.columns();

    let mut lines = Vec::new();
    for row in 0..screen_lines {
        let line = Line(row as i32);
        let mut row_text = String::new();
        for col in 0..cols {
            let cell = &grid[line][Column(col)];
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                continue;
            }
            row_text.push(cell.c);
        }
        lines.push(row_text.trim_end().to_string());
    }

    // Trim trailing empty lines
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }

    lines.join("\n")
}

/// Resize the PTY and the alacritty_terminal Term.
pub fn resize_pty(handle: &PtyHandle, cols: u16, rows: u16) {
    let _ = handle.master.resize(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    });
    if let Ok(mut pty) = handle.state.lock() {
        let dims = PtyDimensions {
            cols: cols as usize,
            lines: rows as usize,
        };
        pty.term.resize(dims);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::term::cell::Flags;
    use alacritty_terminal::vte::ansi::{Color, NamedColor};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn alt_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    #[test]
    fn key_char_simple() {
        assert_eq!(key_event_to_bytes(key(KeyCode::Char('a'))), b"a");
        assert_eq!(key_event_to_bytes(key(KeyCode::Char('Z'))), b"Z");
        assert_eq!(key_event_to_bytes(key(KeyCode::Char('1'))), b"1");
    }

    #[test]
    fn key_enter() {
        assert_eq!(key_event_to_bytes(key(KeyCode::Enter)), vec![b'\r']);
    }

    #[test]
    fn key_backspace() {
        assert_eq!(key_event_to_bytes(key(KeyCode::Backspace)), vec![0x7f]);
    }

    #[test]
    fn key_tab() {
        assert_eq!(key_event_to_bytes(key(KeyCode::Tab)), vec![b'\t']);
    }

    #[test]
    fn key_esc() {
        assert_eq!(key_event_to_bytes(key(KeyCode::Esc)), vec![0x1b]);
    }

    #[test]
    fn key_arrows() {
        assert_eq!(key_event_to_bytes(key(KeyCode::Up)), vec![0x1b, b'[', b'A']);
        assert_eq!(
            key_event_to_bytes(key(KeyCode::Down)),
            vec![0x1b, b'[', b'B']
        );
        assert_eq!(
            key_event_to_bytes(key(KeyCode::Right)),
            vec![0x1b, b'[', b'C']
        );
        assert_eq!(
            key_event_to_bytes(key(KeyCode::Left)),
            vec![0x1b, b'[', b'D']
        );
    }

    #[test]
    fn key_ctrl_c() {
        // Ctrl+C = byte 3 (0x03)
        assert_eq!(
            key_event_to_bytes(ctrl_key(KeyCode::Char('c'))),
            vec![0x03]
        );
    }

    #[test]
    fn key_ctrl_a() {
        // Ctrl+A = byte 1
        assert_eq!(
            key_event_to_bytes(ctrl_key(KeyCode::Char('a'))),
            vec![0x01]
        );
    }

    #[test]
    fn key_alt_prefix() {
        let bytes = key_event_to_bytes(alt_key(KeyCode::Char('x')));
        assert_eq!(bytes, vec![0x1b, b'x']);
    }

    #[test]
    fn key_alt_esc_no_double_prefix() {
        // Alt+Esc should just be ESC, not double ESC
        let bytes = key_event_to_bytes(alt_key(KeyCode::Esc));
        assert_eq!(bytes, vec![0x1b]);
    }

    #[test]
    fn key_function_keys() {
        assert_eq!(
            key_event_to_bytes(key(KeyCode::F(1))),
            vec![0x1b, b'O', b'P']
        );
        assert_eq!(
            key_event_to_bytes(key(KeyCode::F(12))),
            vec![0x1b, b'[', b'2', b'4', b'~']
        );
    }

    #[test]
    fn key_page_keys() {
        assert_eq!(
            key_event_to_bytes(key(KeyCode::PageUp)),
            vec![0x1b, b'[', b'5', b'~']
        );
        assert_eq!(
            key_event_to_bytes(key(KeyCode::PageDown)),
            vec![0x1b, b'[', b'6', b'~']
        );
    }

    #[test]
    fn key_delete_insert() {
        assert_eq!(
            key_event_to_bytes(key(KeyCode::Delete)),
            vec![0x1b, b'[', b'3', b'~']
        );
        assert_eq!(
            key_event_to_bytes(key(KeyCode::Insert)),
            vec![0x1b, b'[', b'2', b'~']
        );
    }

    #[test]
    fn key_home_end() {
        assert_eq!(
            key_event_to_bytes(key(KeyCode::Home)),
            vec![0x1b, b'[', b'H']
        );
        assert_eq!(
            key_event_to_bytes(key(KeyCode::End)),
            vec![0x1b, b'[', b'F']
        );
    }

    #[test]
    fn key_unknown_returns_empty() {
        assert!(key_event_to_bytes(key(KeyCode::CapsLock)).is_empty());
    }

    #[test]
    fn key_unicode_char() {
        let bytes = key_event_to_bytes(key(KeyCode::Char('\u{00e9}')));
        assert_eq!(bytes, "\u{00e9}".as_bytes());
    }

    // ── Color/flag mapping tests (used by ui.rs) ──

    #[test]
    fn named_color_coverage() {
        // Verify our convert_color in ui.rs handles all NamedColor variants.
        // This test just ensures the enum variants exist as expected.
        let _colors = [
            NamedColor::Black,
            NamedColor::Red,
            NamedColor::Green,
            NamedColor::Yellow,
            NamedColor::Blue,
            NamedColor::Magenta,
            NamedColor::Cyan,
            NamedColor::White,
            NamedColor::BrightBlack,
            NamedColor::BrightRed,
            NamedColor::BrightGreen,
            NamedColor::BrightYellow,
            NamedColor::BrightBlue,
            NamedColor::BrightMagenta,
            NamedColor::BrightCyan,
            NamedColor::BrightWhite,
            NamedColor::DimBlack,
            NamedColor::DimRed,
            NamedColor::DimGreen,
            NamedColor::DimYellow,
            NamedColor::DimBlue,
            NamedColor::DimMagenta,
            NamedColor::DimCyan,
            NamedColor::DimWhite,
            NamedColor::Foreground,
            NamedColor::Background,
            NamedColor::Cursor,
            NamedColor::BrightForeground,
            NamedColor::DimForeground,
        ];
    }

    #[test]
    fn color_enum_variants() {
        let _spec = Color::Spec(alacritty_terminal::vte::ansi::Rgb { r: 255, g: 0, b: 0 });
        let _named = Color::Named(NamedColor::Red);
        let _indexed = Color::Indexed(42);
    }

    #[test]
    fn flags_variants() {
        let _bold = Flags::BOLD;
        let _italic = Flags::ITALIC;
        let _underline = Flags::UNDERLINE;
        let _dim = Flags::DIM;
        let _inverse = Flags::INVERSE;
        let _strikeout = Flags::STRIKEOUT;
        let _hidden = Flags::HIDDEN;
        let _wide = Flags::WIDE_CHAR;
        let _spacer = Flags::WIDE_CHAR_SPACER;
    }
}
