//! Logical-line editing state for Dunking Sheep's text editor (`text_editor.py`).
//!
//! Text is kept as logical lines, never as a flat character grid: screen width only
//! affects scrolling and never changes the stored text. Columns count characters (code
//! points), as the Python's string indices do.

fn char_len(line: &str) -> usize {
    line.chars().count()
}

fn byte_at(line: &str, col: usize) -> usize {
    line.char_indices().nth(col).map_or(line.len(), |(i, _)| i)
}

/// `TextEditorBuffer`: a small, UI-independent multiline buffer with a movable cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEditorBuffer {
    pub lines: Vec<String>,
    pub row: usize,
    pub col: usize,
    pub preferred_col: usize,
}

fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

impl TextEditorBuffer {
    pub fn new(text: &str) -> Self {
        let lines: Vec<String> = normalize(text).split('\n').map(str::to_string).collect();
        let row = lines.len() - 1;
        let col = char_len(&lines[row]);
        TextEditorBuffer {
            lines,
            row,
            col,
            preferred_col: col,
        }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn set_cursor(&mut self, row: i64, col: i64) {
        self.row = row.clamp(0, self.lines.len() as i64 - 1) as usize;
        self.col = col.clamp(0, char_len(&self.lines[self.row]) as i64) as usize;
        self.preferred_col = self.col;
    }

    /// Insert text at the cursor, preserving logical newline boundaries.
    pub fn insert(&mut self, value: &str) {
        for ch in normalize(value).chars() {
            if ch == '\n' {
                self.newline();
            } else {
                let at = byte_at(&self.lines[self.row], self.col);
                self.lines[self.row].insert(at, ch);
                self.col += 1;
                self.preferred_col = self.col;
            }
        }
    }

    pub fn newline(&mut self) {
        let at = byte_at(&self.lines[self.row], self.col);
        let rest = self.lines[self.row].split_off(at);
        self.lines.insert(self.row + 1, rest);
        self.row += 1;
        self.col = 0;
        self.preferred_col = 0;
    }

    pub fn backspace(&mut self) {
        if self.col > 0 {
            let at = byte_at(&self.lines[self.row], self.col - 1);
            self.lines[self.row].remove(at);
            self.col -= 1;
        } else if self.row > 0 {
            let previous_length = char_len(&self.lines[self.row - 1]);
            let current = self.lines.remove(self.row);
            self.lines[self.row - 1].push_str(&current);
            self.row -= 1;
            self.col = previous_length;
        }
        self.preferred_col = self.col;
    }

    pub fn delete(&mut self) {
        if self.col < char_len(&self.lines[self.row]) {
            let at = byte_at(&self.lines[self.row], self.col);
            self.lines[self.row].remove(at);
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
        self.preferred_col = self.col;
    }

    pub fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = char_len(&self.lines[self.row]);
        }
        self.preferred_col = self.col;
    }

    pub fn move_right(&mut self) {
        if self.col < char_len(&self.lines[self.row]) {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
        self.preferred_col = self.col;
    }

    /// `move_up(amount)`: the Python's `max(1, int(amount))` floor is kept.
    pub fn move_up(&mut self, amount: i64) {
        self.row = self.row.saturating_sub(amount.max(1) as usize);
        self.col = self.preferred_col.min(char_len(&self.lines[self.row]));
    }

    pub fn move_down(&mut self, amount: i64) {
        self.row = (self.row + amount.max(1) as usize).min(self.lines.len() - 1);
        self.col = self.preferred_col.min(char_len(&self.lines[self.row]));
    }

    pub fn move_home(&mut self) {
        self.col = 0;
        self.preferred_col = 0;
    }

    pub fn move_end(&mut self) {
        self.col = char_len(&self.lines[self.row]);
        self.preferred_col = self.col;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_only_changes_current_logical_line() {
        let mut editor = TextEditorBuffer::new("alpha\nbeta\ngamma");
        editor.set_cursor(0, 2);
        editor.insert("   ");
        assert_eq!(vec!["al   pha", "beta", "gamma"], editor.lines);
        assert_eq!("al   pha\nbeta\ngamma", editor.text());
    }

    #[test]
    fn long_lines_are_not_truncated_or_reflowed() {
        let first = "x".repeat(5000);
        let mut editor = TextEditorBuffer::new(&format!("{first}\nsecond"));
        editor.set_cursor(0, 2500);
        editor.insert(" ");
        assert_eq!(5001, editor.lines[0].chars().count());
        assert_eq!("second", editor.lines[1]);
        assert_eq!(
            format!("{} {}\nsecond", &first[..2500], &first[2500..]),
            editor.text()
        );
    }

    #[test]
    fn initial_crlf_is_normalized_to_logical_lines() {
        let editor = TextEditorBuffer::new("first\r\nsecond\rthird");
        assert_eq!(vec!["first", "second", "third"], editor.lines);
        assert_eq!("first\nsecond\nthird", editor.text());
    }

    #[test]
    fn newline_splits_and_backspace_rejoins_lines() {
        let mut editor = TextEditorBuffer::new("firstsecond");
        editor.set_cursor(0, 5);
        editor.newline();
        assert_eq!(vec!["first", "second"], editor.lines);
        editor.backspace();
        assert_eq!(vec!["firstsecond"], editor.lines);
        assert_eq!((0, 5), (editor.row, editor.col));
    }

    #[test]
    fn delete_at_end_joins_the_next_line() {
        let mut editor = TextEditorBuffer::new("first\nsecond");
        editor.set_cursor(0, 5);
        editor.delete();
        assert_eq!("firstsecond", editor.text());
        assert_eq!((0, 5), (editor.row, editor.col));
    }

    #[test]
    fn vertical_motion_remembers_preferred_column() {
        let mut editor = TextEditorBuffer::new("long line\nx\nlong line");
        editor.set_cursor(0, 7);
        editor.move_down(1);
        assert_eq!((1, 1), (editor.row, editor.col));
        editor.move_down(1);
        assert_eq!((2, 7), (editor.row, editor.col));
    }

    // Beyond the Python tests: columns are characters, not bytes.
    #[test]
    fn columns_count_characters_not_bytes() {
        let mut editor = TextEditorBuffer::new("héé");
        editor.set_cursor(0, 2);
        editor.insert("x");
        assert_eq!("héxé", editor.text());
        editor.backspace();
        editor.backspace();
        assert_eq!("hé", editor.text());
    }
}
