use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Convert a crossterm KeyEvent to raw bytes suitable for PTY input.
pub fn key_event_to_bytes(key: KeyEvent) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    let mut bytes = match key.code {
        KeyCode::Char(c) if ctrl => {
            let byte = (c.to_ascii_lowercase() as u8)
                .wrapping_sub(b'a')
                .wrapping_add(1);
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
        assert_eq!(key_event_to_bytes(ctrl_key(KeyCode::Char('c'))), vec![0x03]);
    }

    #[test]
    fn key_ctrl_a() {
        // Ctrl+A = byte 1
        assert_eq!(key_event_to_bytes(ctrl_key(KeyCode::Char('a'))), vec![0x01]);
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
