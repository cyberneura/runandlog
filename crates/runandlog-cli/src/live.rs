//! The output of the command running right now, as the front ends show it.
//!
//! A live view is a window onto a run, not a record of it: the whole output is
//! written back to the Markdown when the command ends, so anything kept here only
//! has to last until then. That is why everything in this module is bounded --
//! a command that prints for an hour must not grow the front end's memory with it.

/// A bounded, rolling buffer of what the running command has printed.
///
/// Only the tail is kept. Once the buffer is full the oldest text is dropped,
/// since what a live view shows is the newest lines.
#[derive(Debug)]
pub struct LiveOutput {
    text: String,
    limit: usize,
}

impl LiveOutput {
    /// A buffer holding roughly `limit` bytes of the most recent output.
    pub fn new(limit: usize) -> LiveOutput {
        LiveOutput {
            text: String::new(),
            limit,
        }
    }

    /// Appends what the command has printed since the last chunk.
    pub fn push(&mut self, chunk: &str) {
        self.text.push_str(chunk);
        if self.text.len() > self.limit {
            // Rebuilt rather than drained in place: `String::drain` would move the
            // remaining bytes down on every chunk, which for a command printing
            // steadily is the whole buffer each time.
            self.text = tail(&self.text, self.limit).to_string();
        }
    }

    /// Forgets everything, for the next run.
    pub fn clear(&mut self) {
        self.text.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// The last `count` lines, oldest first.
    ///
    /// A trailing newline does not count as a line of its own: a command that has
    /// printed three complete lines should show those three, not two and a blank.
    /// A line still being written, on the other hand, is shown as it grows -- that
    /// is the one the command is working on.
    pub fn tail_lines(&self, count: usize) -> Vec<&str> {
        let text = self.text.strip_suffix('\n').unwrap_or(&self.text);
        if text.is_empty() {
            return Vec::new();
        }
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(count)..].to_vec()
    }
}

/// The last `max_bytes` or so of `text`, cut at a character boundary.
///
/// Cutting inside a character would leave text that is not valid UTF-8, so the cut
/// moves forward to the next boundary and takes slightly less than asked for.
pub fn tail(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut start = text.len() - max_bytes;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_the_last_lines() {
        let mut live = LiveOutput::new(1024);
        live.push("one\ntwo\n");
        live.push("three\nfour\n");

        assert_eq!(live.tail_lines(3), vec!["two", "three", "four"]);
    }

    #[test]
    fn asking_for_more_lines_than_there_are_gives_what_there_is() {
        let mut live = LiveOutput::new(1024);
        live.push("only\n");

        assert_eq!(live.tail_lines(3), vec!["only"]);
    }

    #[test]
    fn a_trailing_newline_is_not_a_line() {
        // Counting it would cost one of the three lines on offer, and show a blank
        // where the command's last line should be.
        let mut live = LiveOutput::new(1024);
        live.push("one\ntwo\nthree\n");

        assert_eq!(live.tail_lines(3), vec!["one", "two", "three"]);
    }

    #[test]
    fn a_line_still_being_written_is_shown() {
        // A command printing a progress line without a newline is exactly the case a
        // live view is for; holding it back until the line ends would show nothing.
        let mut live = LiveOutput::new(1024);
        live.push("done\nworking");

        assert_eq!(live.tail_lines(2), vec!["done", "working"]);
    }

    #[test]
    fn nothing_printed_yet_is_no_lines() {
        let live = LiveOutput::new(1024);
        assert!(live.is_empty());
        assert!(live.tail_lines(3).is_empty());
    }

    #[test]
    fn clearing_starts_the_next_run_empty() {
        let mut live = LiveOutput::new(1024);
        live.push("from the last run\n");
        live.clear();

        // Left behind, the previous cell's output would appear under the next one.
        assert!(live.is_empty());
    }

    #[test]
    fn a_command_that_never_stops_printing_does_not_grow_the_buffer() {
        let mut live = LiveOutput::new(64);
        for index in 0..1000 {
            live.push(&format!("line {index}\n"));
        }

        assert!(live.text.len() <= 64, "buffer grew to {}", live.text.len());
        // The newest lines are the ones kept.
        assert_eq!(live.tail_lines(1), vec!["line 999"]);
    }

    #[test]
    fn dropping_the_oldest_text_never_splits_a_character() {
        let mut live = LiveOutput::new(16);
        for _ in 0..20 {
            // Three bytes each, so a cut at a fixed byte count lands mid-character
            // unless it is moved to a boundary.
            live.push("行\n");
        }

        assert!(live.text.ends_with("行\n"));
    }

    #[test]
    fn text_shorter_than_the_limit_is_left_alone() {
        assert_eq!(tail("short", 64), "short");
    }
}
