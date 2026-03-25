//! Shared PTY types used by both the daemon and TUI.

use alacritty_terminal::grid::Dimensions;

/// Terminal dimensions for `alacritty_terminal::Term`.
///
/// Shared between daemon (pty_worker) and TUI (pty_renderer) to avoid duplication.
pub struct PtyDimensions {
    pub cols: usize,
    pub lines: usize,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimensions_basic() {
        let dims = PtyDimensions {
            cols: 80,
            lines: 24,
        };
        assert_eq!(dims.total_lines(), 24);
        assert_eq!(dims.screen_lines(), 24);
        assert_eq!(dims.columns(), 80);
    }

    #[test]
    fn dimensions_non_standard() {
        let dims = PtyDimensions {
            cols: 200,
            lines: 50,
        };
        assert_eq!(dims.total_lines(), 50);
        assert_eq!(dims.screen_lines(), 50);
        assert_eq!(dims.columns(), 200);
    }
}
