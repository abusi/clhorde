use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::Config;
use alacritty_terminal::vte::ansi::Processor;
use alacritty_terminal::Term;
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use tokio::sync::mpsc;

use clhorde_core::pty::PtyDimensions;

use crate::worker::{WorkerInput, WorkerMessage};

/// Fixed-size circular buffer for PTY output bytes (for late-join replay).
pub struct PtyRingBuffer {
    buf: Vec<u8>,
    capacity: usize,
    write_pos: usize,
    full: bool,
}

impl PtyRingBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: vec![0u8; capacity],
            capacity,
            write_pos: 0,
            full: false,
        }
    }

    /// Append bytes to the ring buffer.
    pub fn extend(&mut self, data: &[u8]) {
        for &byte in data {
            self.buf[self.write_pos] = byte;
            self.write_pos = (self.write_pos + 1) % self.capacity;
            if self.write_pos == 0 && !self.full {
                self.full = true;
            }
        }
        // Handle the case where we wrapped during this extend
        if data.len() >= self.capacity {
            self.full = true;
        }
    }

    /// Snapshot the current buffer contents in order.
    pub fn snapshot(&self) -> Vec<u8> {
        if self.full {
            let mut out = Vec::with_capacity(self.capacity);
            out.extend_from_slice(&self.buf[self.write_pos..]);
            out.extend_from_slice(&self.buf[..self.write_pos]);
            out
        } else {
            self.buf[..self.write_pos].to_vec()
        }
    }
}

pub struct PtyState {
    pub term: Term<VoidListener>,
    pub processor: Processor,
}

pub type SharedPtyState = Arc<Mutex<PtyState>>;

pub struct PtyHandle {
    pub state: SharedPtyState,
    pub master: Box<dyn portable_pty::MasterPty + Send>,
    pub child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    pub ring_buffer: Arc<Mutex<PtyRingBuffer>>,
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_pty_worker(
    prompt_id: usize,
    prompt_text: String,
    cwd: Option<String>,
    cols: u16,
    rows: u16,
    tx: mpsc::Sender<WorkerMessage>,
    resume_session_id: Option<String>,
    pty_byte_tx: tokio::sync::broadcast::Sender<(usize, Vec<u8>)>,
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
    if let Some(ref session_id) = resume_session_id {
        if session_id.is_empty() {
            cmd.arg("--resume");
        } else {
            cmd.arg("--resume");
            cmd.arg(session_id);
        }
    } else {
        cmd.arg(&prompt_text);
    }
    cmd.arg("--dangerously-skip-permissions");
    cmd.env_remove("CLAUDECODE");
    match cwd {
        Some(ref dir) => cmd.cwd(dir),
        None => cmd.cwd(std::env::current_dir().unwrap_or_default()),
    };

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
    let ring_buffer = Arc::new(Mutex::new(PtyRingBuffer::new(64 * 1024)));

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| format!("Failed to clone PTY reader: {e}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| format!("Failed to take PTY writer: {e}"))?;

    // Reader thread: reads from PTY, feeds bytes to alacritty_terminal processor,
    // appends to ring buffer, and broadcasts to subscribers.
    let reader_state = state.clone();
    let reader_ring = ring_buffer.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break, // EOF — child exited
                Ok(n) => {
                    let bytes = &buf[..n];

                    // Feed to alacritty terminal emulator
                    if let Ok(mut pty) = reader_state.lock() {
                        let PtyState {
                            ref mut term,
                            ref mut processor,
                        } = *pty;
                        processor.advance(term, bytes);
                    }

                    // Append to ring buffer for late-join replay
                    if let Ok(mut ring) = reader_ring.lock() {
                        ring.extend(bytes);
                    }

                    // Broadcast raw bytes to connected clients
                    let _ = pty_byte_tx.send((prompt_id, bytes.to_vec()));

                    let _ = tx.blocking_send(WorkerMessage::PtyUpdate { prompt_id });
                }
                Err(_) => break,
            }
        }
        // PTY EOF — child process output is done, but we need to wait() for real exit code
        let _ = tx.blocking_send(WorkerMessage::PtyEof { prompt_id });
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
        // Writer dropped here -> PTY master EOF -> child gets SIGHUP
    });

    Ok((
        input_tx,
        PtyHandle {
            state,
            master: pair.master,
            child: Some(child),
            ring_buffer,
        },
    ))
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

    #[test]
    fn ring_buffer_basic() {
        let mut rb = PtyRingBuffer::new(8);
        rb.extend(b"hello");
        assert_eq!(rb.snapshot(), b"hello");
    }

    #[test]
    fn ring_buffer_wrap() {
        let mut rb = PtyRingBuffer::new(8);
        rb.extend(b"12345678"); // fill exactly
        rb.extend(b"AB"); // wrap around
        let snap = rb.snapshot();
        assert_eq!(snap, b"345678AB");
    }

    #[test]
    fn ring_buffer_overflow() {
        let mut rb = PtyRingBuffer::new(4);
        rb.extend(b"abcdefgh"); // 2x capacity
        let snap = rb.snapshot();
        assert_eq!(snap.len(), 4);
        assert_eq!(snap, b"efgh");
    }

    #[test]
    fn ring_buffer_empty() {
        let rb = PtyRingBuffer::new(16);
        assert!(rb.snapshot().is_empty());
    }
}
