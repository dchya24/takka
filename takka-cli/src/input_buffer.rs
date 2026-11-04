//! Custom input buffer replacing reedline for interactive REPL mode.
//!
//! Provides single-line and multi-line text editing, cursor management, and in-memory history.
//! Used by the raw-mode event loop in `interactive.rs`.

/// Maximum input length (single-line mode)
const MAX_INPUT_LEN: usize = 4096;

/// Maximum number of lines in multi-line mode
const MAX_LINES: usize = 50;

/// Input buffer with cursor position and in-memory history.
/// Supports both single-line and multi-line editing.
#[derive(Debug)]
pub struct InputBuffer {
    /// Current input text (may contain newlines for multi-line mode)
    text: String,
    /// Cursor position (byte offset within `text`)
    cursor: usize,
    /// Submitted input history (most recent last)
    history: Vec<String>,
    /// Current history browse index (None = not browsing)
    history_idx: Option<usize>,
    /// Saved input when user started browsing history
    saved_input: String,
    /// Whether multi-line mode is active
    multiline: bool,
}

impl InputBuffer {
    pub fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: None,
            saved_input: String::new(),
            multiline: false,
        }
    }

    /// Current input text
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Current cursor position (byte offset)
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Is input empty?
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Insert a character at cursor position
    pub fn insert_char(&mut self, c: char) {
        if self.text.len() + c.len_utf8() > MAX_INPUT_LEN {
            return;
        }
        self.text.insert(self.cursor, c);
        self.cursor += c.len_utf8();
    }

    /// Delete character before cursor (Backspace)
    pub fn delete_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prev = self.text[..self.cursor]
            .char_indices()
            .last()
            .map(|(i, _)| i)
            .unwrap_or(0);
        self.text.drain(prev..self.cursor);
        self.cursor = prev;
    }

    /// Delete character at cursor (Delete key)
    pub fn delete_forward(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let next = self.text[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(i, _)| self.cursor + i)
            .unwrap_or(self.text.len());
        self.text.drain(self.cursor..next);
    }

    /// Delete from cursor to start of previous word (Ctrl+W / Ctrl+Backspace)
    pub fn delete_word_backward(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut pos = self.cursor;
        // Skip trailing whitespace
        while pos > 0 {
            let prev = self.text[..pos]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            if !self.text[prev..pos]
                .chars()
                .next()
                .is_none_or(|c| c.is_whitespace())
            {
                break;
            }
            pos = prev;
        }
        // Skip word characters
        while pos > 0 {
            let prev = self.text[..pos]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            if self.text[prev..pos]
                .chars()
                .next()
                .is_some_and(|c| c.is_whitespace())
            {
                break;
            }
            pos = prev;
        }
        self.text.drain(pos..self.cursor);
        self.cursor = pos;
    }

    /// Move cursor left one character
    pub fn cursor_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.text[..self.cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    /// Move cursor right one character
    pub fn cursor_right(&mut self) {
        if self.cursor < self.text.len() {
            self.cursor = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
        }
    }

    /// Move cursor to start (Home)
    pub fn cursor_home(&mut self) {
        self.cursor = 0;
    }

    /// Move cursor to end (End)
    pub fn cursor_end(&mut self) {
        self.cursor = self.text.len();
    }

    /// Move cursor to start of previous word (Ctrl+Left)
    pub fn cursor_word_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let mut pos = self.cursor;
        // Skip whitespace
        while pos > 0 {
            let prev = self.text[..pos]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            if !self.text[prev..pos]
                .chars()
                .next()
                .is_none_or(|c| c.is_whitespace())
            {
                break;
            }
            pos = prev;
        }
        // Skip word
        while pos > 0 {
            let prev = self.text[..pos]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            if self.text[prev..pos]
                .chars()
                .next()
                .is_some_and(|c| c.is_whitespace())
            {
                break;
            }
            pos = prev;
        }
        self.cursor = pos;
    }

    /// Move cursor to start of next word (Ctrl+Right)
    pub fn cursor_word_right(&mut self) {
        let len = self.text.len();
        if self.cursor >= len {
            return;
        }
        let mut pos = self.cursor;
        // Skip word
        while pos < len {
            let next = self.text[pos..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| pos + i)
                .unwrap_or(len);
            if self.text[pos..next]
                .chars()
                .next()
                .is_none_or(|c| c.is_whitespace())
            {
                break;
            }
            pos = next;
        }
        // Skip whitespace
        while pos < len {
            let next = self.text[pos..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| pos + i)
                .unwrap_or(len);
            if !self.text[pos..next]
                .chars()
                .next()
                .is_none_or(|c| c.is_whitespace())
            {
                break;
            }
            pos = next;
        }
        self.cursor = pos;
    }

    /// Clear entire input
    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.multiline = false;
    }

    // ── Multi-line support ──────────────────────────────────────

    /// Check if multi-line mode is active
    pub fn is_multiline(&self) -> bool {
        self.multiline
    }

    /// Get the number of lines in the input
    pub fn line_count(&self) -> usize {
        if self.text.is_empty() {
            1
        } else {
            // Count newlines + 1
            self.text.chars().filter(|c| *c == '\n').count() + 1
        }
    }

    /// Get the current line index (0-based)
    pub fn current_line(&self) -> usize {
        self.text[..self.cursor].lines().count().saturating_sub(1)
    }

    /// Insert a line break at cursor position (Shift+Enter)
    pub fn insert_line_break(&mut self) {
        if self.line_count() >= MAX_LINES {
            return;
        }
        self.text.insert(self.cursor, '\n');
        self.cursor += 1;
        self.multiline = true;
    }

    /// Get lines as a vector of strings
    pub fn lines(&self) -> Vec<&str> {
        if self.text.is_empty() {
            vec![""]
        } else {
            self.text.lines().collect()
        }
    }

    /// Get the text of the current line
    pub fn current_line_text(&self) -> &str {
        let line_idx = self.current_line();
        self.lines().get(line_idx).copied().unwrap_or("")
    }

    /// Move cursor up one line
    pub fn cursor_up(&mut self) {
        let lines: Vec<&str> = self.text.lines().collect();
        let current_line = self.current_line();

        if current_line == 0 {
            return;
        }

        let prev_line = current_line - 1;
        let prev_line_start = lines[..prev_line]
            .iter()
            .map(|l| l.len() + 1)
            .sum::<usize>();
        let current_line_start = lines[..current_line]
            .iter()
            .map(|l| l.len() + 1)
            .sum::<usize>();
        let col = self.cursor - current_line_start;
        let prev_line_len = lines[prev_line].len();

        // Move to same column or end of previous line
        let new_col = col.min(prev_line_len);
        self.cursor = prev_line_start + new_col;
    }

    /// Move cursor down one line
    pub fn cursor_down(&mut self) {
        let lines: Vec<&str> = self.text.lines().collect();
        let current_line = self.current_line();

        if current_line + 1 >= lines.len() {
            return;
        }

        let next_line = current_line + 1;
        let current_line_start = lines[..current_line]
            .iter()
            .map(|l| l.len() + 1)
            .sum::<usize>();
        let next_line_start = lines[..next_line]
            .iter()
            .map(|l| l.len() + 1)
            .sum::<usize>();
        let col = self.cursor - current_line_start;
        let next_line_len = lines[next_line].len();

        // Move to same column or end of next line
        let new_col = col.min(next_line_len);
        self.cursor = next_line_start + new_col;
    }

    /// Merge current line with previous line (Backspace at line start)
    pub fn merge_with_previous_line(&mut self) {
        // Find the newline before the cursor
        if self.cursor == 0 {
            return;
        }

        // Find the last newline before cursor
        let newline_pos = self.text[..self.cursor].rfind('\n');

        match newline_pos {
            Some(pos) => {
                // Remove the newline character
                self.text.remove(pos);
                self.cursor = pos;
            }
            None => {
                // No newline before cursor, nothing to merge
                return;
            }
        }

        // Update multiline flag
        if self.line_count() <= 1 {
            self.multiline = false;
        }
    }

    /// Submit input — pushes to history, returns the text, clears buffer.
    /// Returns empty string if the trimmed input is empty.
    pub fn submit(&mut self) -> String {
        let input = self.text.trim().to_string();
        if !input.is_empty() {
            // Don't push duplicate of last history entry
            if self.history.last() != Some(&input) {
                self.history.push(input.clone());
            }
        }
        self.text.clear();
        self.cursor = 0;
        self.history_idx = None;
        self.multiline = false;
        input
    }

    /// Navigate history up (older entries)
    pub fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        if self.history_idx.is_none() {
            self.saved_input = self.text.clone();
            self.history_idx = Some(self.history.len() - 1);
        } else if let Some(idx) = self.history_idx {
            if idx > 0 {
                self.history_idx = Some(idx - 1);
            }
        }
        if let Some(idx) = self.history_idx {
            self.text = self.history[idx].clone();
            self.cursor = self.text.len();
        }
    }

    /// Navigate history down (newer entries)
    pub fn history_down(&mut self) {
        if let Some(idx) = self.history_idx {
            if idx + 1 >= self.history.len() {
                // Back to saved input
                self.history_idx = None;
                self.text = self.saved_input.clone();
                self.cursor = self.text.len();
            } else {
                self.history_idx = Some(idx + 1);
                self.text = self.history[idx + 1].clone();
                self.cursor = self.text.len();
            }
        }
    }

    /// Reset history browse state (e.g. after inserting a char)
    pub fn reset_history_browse(&mut self) {
        if self.history_idx.is_some() {
            self.history_idx = None;
            self.saved_input.clear();
        }
    }

    /// Replace text from `start` to `cursor` with `replacement`.
    /// Used by dropdown accept to replace @query with @selected_path.
    pub fn replace_range(&mut self, start: usize, replacement: &str) {
        let end = self.cursor;
        if start > end || end > self.text.len() {
            return;
        }
        self.text.drain(start..end);
        let rep_len = replacement.len();
        self.text.insert_str(start, replacement);
        self.cursor = start + rep_len;
    }

    /// Set the entire text and move cursor to end.
    /// Used by dropdown accept for commands (replace entire input).
    pub fn set_text(&mut self, text: String) {
        self.text = text;
        self.cursor = self.text.len();
    }

    /// Get history length
    #[allow(dead_code)]
    pub fn history_len(&self) -> usize {
        self.history.len()
    }
}

impl Default for InputBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_insert_char() {
        let mut buf = InputBuffer::new();
        buf.insert_char('h');
        buf.insert_char('i');
        assert_eq!(buf.text(), "hi");
        assert_eq!(buf.cursor(), 2);
    }

    #[test]
    fn test_insert_char_in_middle() {
        let mut buf = InputBuffer::new();
        buf.insert_char('a');
        buf.insert_char('c');
        // cursor at 2, move left, insert 'b'
        buf.cursor_left();
        assert_eq!(buf.cursor(), 1);
        buf.insert_char('b');
        assert_eq!(buf.text(), "abc");
        assert_eq!(buf.cursor(), 2);
    }

    #[test]
    fn test_delete_backward() {
        let mut buf = InputBuffer::new();
        buf.set_text("hello".to_string());
        buf.cursor = 5;
        buf.delete_backward();
        assert_eq!(buf.text(), "hell");
        assert_eq!(buf.cursor(), 4);
    }

    #[test]
    fn test_delete_backward_at_start() {
        let mut buf = InputBuffer::new();
        buf.delete_backward(); // cursor at 0, should be no-op
        assert_eq!(buf.text(), "");
    }

    #[test]
    fn test_delete_forward() {
        let mut buf = InputBuffer::new();
        buf.set_text("hello".to_string());
        buf.cursor = 0;
        buf.delete_forward();
        assert_eq!(buf.text(), "ello");
        assert_eq!(buf.cursor(), 0);
    }

    #[test]
    fn test_delete_forward_at_end() {
        let mut buf = InputBuffer::new();
        buf.set_text("hi".to_string());
        buf.delete_forward(); // cursor at end, no-op
        assert_eq!(buf.text(), "hi");
    }

    #[test]
    fn test_delete_word_backward() {
        let mut buf = InputBuffer::new();
        buf.set_text("hello world".to_string());
        buf.cursor = 11; // end
        buf.delete_word_backward();
        assert_eq!(buf.text(), "hello ");
        assert_eq!(buf.cursor(), 6);
        buf.delete_word_backward();
        assert_eq!(buf.text(), "");
    }

    #[test]
    fn test_cursor_navigation() {
        let mut buf = InputBuffer::new();
        buf.set_text("hello".to_string());
        assert_eq!(buf.cursor(), 5);

        buf.cursor_left();
        assert_eq!(buf.cursor(), 4);

        buf.cursor_home();
        assert_eq!(buf.cursor(), 0);

        buf.cursor_right();
        assert_eq!(buf.cursor(), 1);

        buf.cursor_end();
        assert_eq!(buf.cursor(), 5);
    }

    #[test]
    fn test_cursor_word_navigation() {
        let mut buf = InputBuffer::new();
        buf.set_text("hello world test".to_string());
        buf.cursor_home();

        buf.cursor_word_right();
        assert_eq!(buf.cursor(), 6); // "hello " -> start of "world"

        buf.cursor_word_right();
        assert_eq!(buf.cursor(), 12); // "hello world " -> start of "test"

        buf.cursor_word_left();
        assert_eq!(buf.cursor(), 6); // back to "world"

        buf.cursor_word_left();
        assert_eq!(buf.cursor(), 0); // back to start
    }

    #[test]
    fn test_submit() {
        let mut buf = InputBuffer::new();
        buf.set_text("hello".to_string());
        let result = buf.submit();
        assert_eq!(result, "hello");
        assert!(buf.is_empty());
        assert_eq!(buf.history_len(), 1);
    }

    #[test]
    fn test_submit_dedup() {
        let mut buf = InputBuffer::new();
        buf.set_text("hello".to_string());
        buf.submit();
        buf.set_text("hello".to_string());
        buf.submit();
        assert_eq!(buf.history_len(), 1);
    }

    #[test]
    fn test_submit_empty() {
        let mut buf = InputBuffer::new();
        buf.set_text("   ".to_string());
        let result = buf.submit();
        assert_eq!(result, "");
        assert_eq!(buf.history_len(), 0);
    }

    #[test]
    fn test_history_navigation() {
        let mut buf = InputBuffer::new();
        buf.set_text("first".to_string());
        buf.submit();
        buf.set_text("second".to_string());
        buf.submit();
        buf.set_text("third".to_string());
        buf.submit();

        // Now buffer is empty, go up
        buf.history_up();
        assert_eq!(buf.text(), "third");

        buf.history_up();
        assert_eq!(buf.text(), "second");

        buf.history_up();
        assert_eq!(buf.text(), "first");

        // At oldest, should stay
        buf.history_up();
        assert_eq!(buf.text(), "first");

        // Go back down
        buf.history_down();
        assert_eq!(buf.text(), "second");

        buf.history_down();
        assert_eq!(buf.text(), "third");

        // Back to saved (empty)
        buf.history_down();
        assert_eq!(buf.text(), "");
    }

    #[test]
    fn test_history_saves_current_input() {
        let mut buf = InputBuffer::new();
        buf.set_text("first".to_string());
        buf.submit();

        // Type something new
        buf.insert_char('n');
        buf.insert_char('e');
        buf.insert_char('w');
        assert_eq!(buf.text(), "new");

        // Go up — should save "new"
        buf.history_up();
        assert_eq!(buf.text(), "first");

        // Go back down — should restore "new"
        buf.history_down();
        assert_eq!(buf.text(), "new");
    }

    #[test]
    fn test_replace_range() {
        let mut buf = InputBuffer::new();
        buf.set_text("read @src/main.rs".to_string());
        // "read @src/main.rs" = 17 bytes. cursor at 17 (end).
        // Replace from @ position (5) to cursor (17)
        assert_eq!(buf.cursor(), 17);
        buf.replace_range(5, "@lib/core.rs");
        assert_eq!(buf.text(), "read @lib/core.rs");
        assert_eq!(buf.cursor(), 17);
    }

    #[test]
    fn test_clear() {
        let mut buf = InputBuffer::new();
        buf.set_text("hello".to_string());
        buf.clear();
        assert!(buf.is_empty());
        assert_eq!(buf.cursor(), 0);
    }

    #[test]
    fn test_unicode_cursor() {
        let mut buf = InputBuffer::new();
        // Insert multi-byte characters
        buf.insert_char('h');
        buf.insert_char('é');
        buf.insert_char('l');
        buf.insert_char('l');
        buf.insert_char('ö');
        assert_eq!(buf.text(), "hél lö".replace(" ", "")); // "hél lö" without space = "hél lö"... let me fix
                                                           // Actually: h, é, l, l, ö → "hél lö"? No, just "hél lö" without space
                                                           // Let's just check navigation works
        buf.cursor_home();
        assert_eq!(buf.cursor(), 0);
        buf.cursor_right(); // h
        assert_eq!(buf.cursor(), 1);
        buf.cursor_right(); // é (2 bytes)
        assert_eq!(buf.cursor(), 3);
        buf.cursor_left(); // back to h
        assert_eq!(buf.cursor(), 1);
    }

    // ── Multi-line tests ──────────────────────────────────────

    #[test]
    fn test_multiline_insert_line_break() {
        let mut buf = InputBuffer::new();
        buf.set_text("hello".to_string());
        buf.cursor_end();

        assert!(!buf.is_multiline());
        assert_eq!(buf.line_count(), 1);

        buf.insert_line_break();

        assert!(buf.is_multiline());
        assert_eq!(buf.text(), "hello\n");
        assert_eq!(buf.line_count(), 2);
    }

    #[test]
    fn test_multiline_current_line() {
        let mut buf = InputBuffer::new();
        buf.set_text("line1\nline2\nline3".to_string());
        buf.multiline = true;

        // Cursor at end
        buf.cursor_end();
        assert_eq!(buf.current_line(), 2); // line3 (0-indexed)

        // Move to start
        buf.cursor_home();
        assert_eq!(buf.current_line(), 0); // line1
    }

    #[test]
    fn test_multiline_lines() {
        let mut buf = InputBuffer::new();
        buf.set_text("line1\nline2\nline3".to_string());
        buf.multiline = true;

        let lines = buf.lines();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "line1");
        assert_eq!(lines[1], "line2");
        assert_eq!(lines[2], "line3");
    }

    #[test]
    fn test_multiline_cursor_navigation() {
        let mut buf = InputBuffer::new();
        buf.set_text("line1\nline2\nline3".to_string());
        buf.multiline = true;
        buf.cursor_end(); // at end of "line3"

        // Move up to "line2"
        buf.cursor_up();
        assert_eq!(buf.current_line(), 1);

        // Move up to "line1"
        buf.cursor_up();
        assert_eq!(buf.current_line(), 0);

        // Can't move up further
        buf.cursor_up();
        assert_eq!(buf.current_line(), 0);

        // Move down
        buf.cursor_down();
        assert_eq!(buf.current_line(), 1);
    }

    #[test]
    fn test_multiline_merge_with_previous() {
        let mut buf = InputBuffer::new();
        buf.set_text("line1\nline2".to_string());
        buf.multiline = true;

        // Move to start of line2
        buf.cursor = 6; // after "line1\n"

        buf.merge_with_previous_line();

        assert_eq!(buf.text(), "line1line2");
        assert!(!buf.is_multiline());
    }

    #[test]
    fn test_multiline_clear_resets_flag() {
        let mut buf = InputBuffer::new();
        buf.set_text("line1\nline2".to_string());
        buf.multiline = true;

        buf.clear();

        assert!(!buf.is_multiline());
        assert!(buf.is_empty());
    }

    #[test]
    fn test_multiline_submit() {
        let mut buf = InputBuffer::new();
        buf.set_text("line1\nline2\nline3".to_string());
        buf.multiline = true;

        let result = buf.submit();
        assert_eq!(result, "line1\nline2\nline3");
        assert!(!buf.is_multiline());
    }
}
