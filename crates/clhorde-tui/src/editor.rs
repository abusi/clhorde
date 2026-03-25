/// A multi-line text buffer with cursor tracking for the input bar.
#[derive(Debug)]
pub struct TextBuffer {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

#[allow(dead_code)]
impl TextBuffer {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }

    pub fn from_string(s: &str) -> Self {
        let lines: Vec<String> = if s.is_empty() {
            vec![String::new()]
        } else {
            s.split('\n').map(String::from).collect()
        };
        let row = lines.len() - 1;
        let col = lines[row].len();
        Self { lines, row, col }
    }

    pub fn set(&mut self, s: &str) {
        self.lines = if s.is_empty() {
            vec![String::new()]
        } else {
            s.split('\n').map(String::from).collect()
        };
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].len();
    }

    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    pub fn is_multiline(&self) -> bool {
        self.lines.len() > 1
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    pub fn first_line(&self) -> &str {
        &self.lines[0]
    }

    /// Return the full text with leading/trailing whitespace trimmed.
    pub fn trimmed(&self) -> String {
        self.to_string().trim().to_string()
    }

    // ── Editing ──

    pub fn insert_char(&mut self, c: char) {
        self.lines[self.row].insert(self.col, c);
        self.col += c.len_utf8();
    }

    pub fn insert_newline(&mut self) {
        let rest = self.lines[self.row].split_off(self.col);
        self.row += 1;
        self.lines.insert(self.row, rest);
        self.col = 0;
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            // Find the previous char boundary
            let prev = self.lines[self.row][..self.col]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.lines[self.row].remove(prev);
            self.col = prev;
        } else if self.row > 0 {
            // Join with previous line
            let current = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.lines[self.row].len();
            self.lines[self.row].push_str(&current);
        }
    }

    pub fn delete(&mut self) {
        if self.col < self.lines[self.row].len() {
            self.lines[self.row].remove(self.col);
        } else if self.row + 1 < self.lines.len() {
            // Join next line into current
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    // ── Movement ──

    pub fn move_left(&mut self) {
        if self.col > 0 {
            let prev = self.lines[self.row][..self.col]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.col = prev;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.lines[self.row].len();
        }
    }

    pub fn move_right(&mut self) {
        if self.col < self.lines[self.row].len() {
            let ch = self.lines[self.row][self.col..].chars().next().unwrap();
            self.col += ch.len_utf8();
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    /// Move cursor up one line. Returns `false` if already on line 0 (caller can
    /// fall through to history navigation).
    pub fn move_up(&mut self) -> bool {
        if self.row == 0 {
            return false;
        }
        self.row -= 1;
        self.col = self.col.min(self.lines[self.row].len());
        true
    }

    /// Move cursor down one line. Returns `false` if already on the last line.
    pub fn move_down(&mut self) -> bool {
        if self.row + 1 >= self.lines.len() {
            return false;
        }
        self.row += 1;
        self.col = self.col.min(self.lines[self.row].len());
        true
    }

    pub fn move_home(&mut self) {
        self.col = 0;
    }

    pub fn move_end(&mut self) {
        self.col = self.lines[self.row].len();
    }

    /// Move cursor to absolute end (last line, end of line).
    pub fn move_to_end(&mut self) {
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].len();
    }
}

impl std::fmt::Display for TextBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut first = true;
        for line in &self.lines {
            if !first {
                f.write_str("\n")?;
            }
            f.write_str(line)?;
            first = false;
        }
        Ok(())
    }
}

impl PartialEq<&str> for TextBuffer {
    fn eq(&self, other: &&str) -> bool {
        let mut remaining = *other;
        for (i, line) in self.lines.iter().enumerate() {
            if i > 0 {
                if remaining.starts_with('\n') {
                    remaining = &remaining[1..];
                } else {
                    return false;
                }
            }
            if remaining.starts_with(line.as_str()) {
                remaining = &remaining[line.len()..];
            } else {
                return false;
            }
        }
        remaining.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_empty() {
        let buf = TextBuffer::new();
        assert!(buf.is_empty());
        assert!(!buf.is_multiline());
        assert_eq!(buf.line_count(), 1);
        assert_eq!(buf.cursor(), (0, 0));
        assert_eq!(buf.to_string(), "");
    }

    #[test]
    fn from_string_single_line() {
        let buf = TextBuffer::from_string("hello");
        assert_eq!(buf.to_string(), "hello");
        assert_eq!(buf.cursor(), (0, 5));
        assert!(!buf.is_multiline());
    }

    #[test]
    fn from_string_multi_line() {
        let buf = TextBuffer::from_string("hello\nworld");
        assert_eq!(buf.line_count(), 2);
        assert!(buf.is_multiline());
        assert_eq!(buf.cursor(), (1, 5));
        assert_eq!(buf.lines(), &["hello", "world"]);
    }

    #[test]
    fn from_string_empty() {
        let buf = TextBuffer::from_string("");
        assert!(buf.is_empty());
        assert_eq!(buf.cursor(), (0, 0));
    }

    #[test]
    fn set_replaces_content() {
        let mut buf = TextBuffer::new();
        buf.set("foo\nbar");
        assert_eq!(buf.to_string(), "foo\nbar");
        assert_eq!(buf.cursor(), (1, 3));
        buf.set("");
        assert!(buf.is_empty());
    }

    #[test]
    fn insert_char_at_end() {
        let mut buf = TextBuffer::new();
        buf.insert_char('a');
        buf.insert_char('b');
        assert_eq!(buf.to_string(), "ab");
        assert_eq!(buf.cursor(), (0, 2));
    }

    #[test]
    fn insert_char_in_middle() {
        let mut buf = TextBuffer::from_string("ac");
        buf.move_left(); // cursor before 'c'
        buf.insert_char('b');
        assert_eq!(buf.to_string(), "abc");
        assert_eq!(buf.cursor(), (0, 2));
    }

    #[test]
    fn insert_newline_splits_line() {
        let mut buf = TextBuffer::from_string("hello world");
        // Move cursor to after "hello"
        buf.col = 5;
        buf.insert_newline();
        assert_eq!(buf.lines(), &["hello", " world"]);
        assert_eq!(buf.cursor(), (1, 0));
    }

    #[test]
    fn backspace_within_line() {
        let mut buf = TextBuffer::from_string("abc");
        buf.backspace();
        assert_eq!(buf.to_string(), "ab");
        assert_eq!(buf.cursor(), (0, 2));
    }

    #[test]
    fn backspace_joins_lines() {
        let mut buf = TextBuffer::from_string("hello\nworld");
        buf.row = 1;
        buf.col = 0;
        buf.backspace();
        assert_eq!(buf.to_string(), "helloworld");
        assert_eq!(buf.cursor(), (0, 5));
    }

    #[test]
    fn backspace_at_start_does_nothing() {
        let mut buf = TextBuffer::from_string("hello");
        buf.col = 0;
        buf.backspace();
        assert_eq!(buf.to_string(), "hello");
        assert_eq!(buf.cursor(), (0, 0));
    }

    #[test]
    fn delete_within_line() {
        let mut buf = TextBuffer::from_string("abc");
        buf.col = 0;
        buf.delete();
        assert_eq!(buf.to_string(), "bc");
        assert_eq!(buf.cursor(), (0, 0));
    }

    #[test]
    fn delete_joins_next_line() {
        let mut buf = TextBuffer::from_string("hello\nworld");
        buf.row = 0;
        buf.col = 5;
        buf.delete();
        assert_eq!(buf.to_string(), "helloworld");
    }

    #[test]
    fn delete_at_end_does_nothing() {
        let mut buf = TextBuffer::from_string("hello");
        buf.delete(); // cursor at end
        assert_eq!(buf.to_string(), "hello");
    }

    #[test]
    fn move_left_wraps_to_prev_line() {
        let mut buf = TextBuffer::from_string("ab\ncd");
        buf.row = 1;
        buf.col = 0;
        buf.move_left();
        assert_eq!(buf.cursor(), (0, 2));
    }

    #[test]
    fn move_left_at_start_does_nothing() {
        let mut buf = TextBuffer::from_string("hello");
        buf.col = 0;
        buf.move_left();
        assert_eq!(buf.cursor(), (0, 0));
    }

    #[test]
    fn move_right_wraps_to_next_line() {
        let mut buf = TextBuffer::from_string("ab\ncd");
        buf.row = 0;
        buf.col = 2;
        buf.move_right();
        assert_eq!(buf.cursor(), (1, 0));
    }

    #[test]
    fn move_right_at_end_does_nothing() {
        let mut buf = TextBuffer::from_string("hello");
        buf.move_right();
        assert_eq!(buf.cursor(), (0, 5));
    }

    #[test]
    fn move_up_returns_false_on_first_line() {
        let mut buf = TextBuffer::from_string("hello");
        assert!(!buf.move_up());
    }

    #[test]
    fn move_up_clamps_col() {
        let mut buf = TextBuffer::from_string("hi\nhello");
        // cursor at (1, 5)
        buf.move_up();
        assert_eq!(buf.cursor(), (0, 2)); // clamped to len of "hi"
    }

    #[test]
    fn move_down_returns_false_on_last_line() {
        let mut buf = TextBuffer::from_string("hello");
        assert!(!buf.move_down());
    }

    #[test]
    fn move_down_clamps_col() {
        let mut buf = TextBuffer::from_string("hello\nhi");
        buf.row = 0;
        buf.col = 5;
        buf.move_down();
        assert_eq!(buf.cursor(), (1, 2)); // clamped to len of "hi"
    }

    #[test]
    fn move_home_end() {
        let mut buf = TextBuffer::from_string("hello");
        buf.col = 3;
        buf.move_home();
        assert_eq!(buf.col, 0);
        buf.move_end();
        assert_eq!(buf.col, 5);
    }

    #[test]
    fn move_to_end_multiline() {
        let mut buf = TextBuffer::from_string("hello\nworld\n!");
        buf.row = 0;
        buf.col = 0;
        buf.move_to_end();
        assert_eq!(buf.cursor(), (2, 1));
    }

    #[test]
    fn clear_resets() {
        let mut buf = TextBuffer::from_string("hello\nworld");
        buf.clear();
        assert!(buf.is_empty());
        assert_eq!(buf.cursor(), (0, 0));
    }

    #[test]
    fn trimmed_strips_whitespace() {
        let mut buf = TextBuffer::new();
        buf.set("  hello  \n  world  ");
        assert_eq!(buf.trimmed(), "hello  \n  world");
    }

    #[test]
    fn first_line_returns_first() {
        let buf = TextBuffer::from_string("first\nsecond");
        assert_eq!(buf.first_line(), "first");
    }

    #[test]
    fn unicode_insert_and_backspace() {
        let mut buf = TextBuffer::new();
        buf.insert_char('é');
        buf.insert_char('日');
        assert_eq!(buf.to_string(), "é日");
        buf.backspace();
        assert_eq!(buf.to_string(), "é");
        buf.backspace();
        assert_eq!(buf.to_string(), "");
    }

    #[test]
    fn unicode_movement() {
        let mut buf = TextBuffer::from_string("aé日b");
        buf.col = 0;
        buf.move_right(); // past 'a'
        assert_eq!(buf.col, 1);
        buf.move_right(); // past 'é' (2 bytes)
        assert_eq!(buf.col, 3);
        buf.move_right(); // past '日' (3 bytes)
        assert_eq!(buf.col, 6);
        buf.move_left(); // back to '日'
        assert_eq!(buf.col, 3);
    }
}
