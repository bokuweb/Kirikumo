//! Following a container's log.
//!
//! `GET …/log?follow=true` answers with a stream that ends when the container
//! does. There is no framing to speak of — it is the container's stdout and
//! stderr, line by line — so the whole of this module is about the two things
//! that are not obvious: that a line can arrive without its newline when the
//! container writes one slowly, and that a log that has stopped is *not* an
//! error, because a pod that exited is the commonest thing to be reading the
//! log of.
//!
//! Read on a thread of its own, blocking, like a watch (`AGENTS.md` rule 3).

use crate::error::Error;
use std::io::BufRead;

/// A log being followed.
///
/// Blocking: [`Self::next_line`] parks the calling thread until the container
/// says something.
pub trait LogStream: Send {
    /// The next line, without its newline; `None` when the log has ended.
    ///
    /// An error ends the stream too, after being reported once — a caller
    /// that kept reading would spin.
    fn next_line(&mut self) -> Option<Result<String, Error>>;
}

/// A log read from anything that yields lines.
pub struct Lines<R> {
    reader: R,
    line: String,
    /// Set once the stream has ended or failed, so a caller that keeps asking
    /// gets `None` rather than a second error.
    done: bool,
}

impl<R: BufRead> Lines<R> {
    /// Follow a log from a reader.
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            line: String::new(),
            done: false,
        }
    }
}

impl<R: BufRead + Send> LogStream for Lines<R> {
    fn next_line(&mut self) -> Option<Result<String, Error>> {
        if self.done {
            return None;
        }
        self.line.clear();
        match self.reader.read_line(&mut self.line) {
            // The container's output ended. Not an error: a pod that has
            // exited is the commonest thing to be reading the log of.
            Ok(0) => {
                self.done = true;
                None
            }
            Ok(_) => Some(Ok(self
                .line
                .trim_end_matches('\n')
                .trim_end_matches('\r')
                .to_string())),
            Err(error) => {
                self.done = true;
                Some(Err(Error::Transport(error.to_string())))
            }
        }
    }
}

/// How many lines of one container's log are kept.
///
/// A bounded scrollback, like a terminal's: a pod that has been logging for a
/// week would otherwise be held whole in a window nobody closed. The oldest
/// go first, because the reason anyone follows a log is what happens next.
pub const SCROLLBACK: usize = 50_000;

/// Add a line to a scrollback, dropping the oldest when it is full.
pub fn append(lines: &mut Vec<String>, line: String) {
    if lines.len() >= SCROLLBACK {
        // Drained in one go rather than one `remove(0)` per line, which would
        // be a memmove of the whole buffer for every line a chatty container
        // writes.
        let excess = lines.len() + 1 - SCROLLBACK;
        lines.drain(..excess);
    }
    lines.push(line);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn collect<R: BufRead + Send>(mut stream: Lines<R>) -> Vec<String> {
        let mut lines = Vec::new();
        while let Some(Ok(line)) = stream.next_line() {
            lines.push(line);
        }
        lines
    }

    #[test]
    fn lines_arrive_without_their_newlines() {
        let stream = Lines::new(Cursor::new("first\nsecond\n"));
        assert_eq!(collect(stream), vec!["first", "second"]);
    }

    #[test]
    fn a_windows_line_ending_is_not_part_of_the_line() {
        let stream = Lines::new(Cursor::new("first\r\n"));
        assert_eq!(collect(stream), vec!["first"]);
    }

    #[test]
    fn a_last_line_without_a_newline_is_still_a_line() {
        let stream = Lines::new(Cursor::new("first\nhalf"));
        assert_eq!(collect(stream), vec!["first", "half"]);
    }

    #[test]
    fn a_blank_line_is_a_line() {
        let stream = Lines::new(Cursor::new("first\n\nthird\n"));
        assert_eq!(collect(stream), vec!["first", "", "third"]);
    }

    #[test]
    fn a_log_that_ends_is_not_an_error_and_stays_ended() {
        let mut stream = Lines::new(Cursor::new("only\n"));
        assert!(matches!(stream.next_line(), Some(Ok(_))));
        assert!(stream.next_line().is_none());
        // Asked again: still nothing, and still not an error.
        assert!(stream.next_line().is_none());
    }

    #[test]
    fn the_scrollback_is_bounded_and_drops_the_oldest() {
        let mut lines: Vec<String> = (0..SCROLLBACK).map(|n| n.to_string()).collect();
        append(&mut lines, "newest".into());
        assert_eq!(lines.len(), SCROLLBACK);
        assert_eq!(lines.last().map(String::as_str), Some("newest"));
        // The oldest went, not the newest.
        assert_eq!(lines.first().map(String::as_str), Some("1"));
    }

    #[test]
    fn appending_under_the_cap_keeps_everything() {
        let mut lines = vec!["a".to_string()];
        append(&mut lines, "b".into());
        assert_eq!(lines, vec!["a", "b"]);
    }
}
