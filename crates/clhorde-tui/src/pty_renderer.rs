use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::term::Config;
use alacritty_terminal::vte::ansi::Processor;
use alacritty_terminal::Term;

use clhorde_core::pty::PtyDimensions;

/// Local headless terminal emulator for PTY rendering.
/// Lives in App's HashMap, accessed synchronously — no Arc<Mutex<>> needed.
pub struct PtyRenderer {
    term: Term<VoidListener>,
    processor: Processor,
}

impl PtyRenderer {
    /// Create a new PtyRenderer with the given terminal dimensions.
    pub fn new(cols: u16, rows: u16) -> Self {
        let dims = PtyDimensions {
            cols: cols as usize,
            lines: rows as usize,
        };
        let config = Config::default();
        let term = Term::new(config, &dims, VoidListener);
        let processor = Processor::new();
        Self { term, processor }
    }

    /// Feed raw PTY bytes into the terminal emulator.
    pub fn feed_bytes(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
    }

    /// Get a reference to the terminal for grid reading during render.
    pub fn term(&self) -> &Term<VoidListener> {
        &self.term
    }

    /// Resize the terminal emulator.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let dims = PtyDimensions {
            cols: cols as usize,
            lines: rows as usize,
        };
        self.term.resize(dims);
    }

    /// Scroll the terminal viewport (positive delta = up into history).
    pub fn scroll_display(&mut self, delta: i32) {
        self.term.scroll_display(Scroll::Delta(delta));
    }

    /// Get the current display offset (0 = at bottom / latest output).
    pub fn display_offset(&self) -> usize {
        self.term.grid().display_offset()
    }

    /// Scroll the terminal viewport back to the bottom.
    pub fn scroll_to_bottom(&mut self) {
        self.term.scroll_display(Scroll::Bottom);
    }
}
