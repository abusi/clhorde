pub use clhorde_core::keymap::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyCode;

    #[test]
    fn re_exported_keymap_loads_defaults() {
        let km = Keymap::default();
        assert_eq!(
            km.normal.get(&KeyCode::Char('q')),
            Some(&NormalAction::Quit)
        );
    }

    #[test]
    fn re_exported_parse_key_works() {
        assert_eq!(parse_key("Enter"), Some(KeyCode::Enter));
    }
}
