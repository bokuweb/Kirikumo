//! The framing of a watch, and the rules for restarting one.
//!
//! `GET …?watch=true` answers with a stream that never ends: one JSON object
//! per line, each `{"type": …, "object": …}`. That is the whole protocol.
//! What makes it work in practice is the two things around it — bookmarks,
//! which let a reconnect resume without re-listing, and `410 Gone`, which is
//! the apiserver saying the version we asked to resume from has aged out of
//! its window (roadmap §4.7).
//!
//! The stream is read on a thread of its own, blocking, because there is no
//! reactor in this process to read it on (`AGENTS.md` rule 3).

use crate::error::Error;
use crate::model::Object;
use serde_json::Value;
use std::io::BufRead;
use std::time::Duration;

/// One line of a watch.
///
/// Not `Clone` or `PartialEq`, because [`Error`] is neither: a failure
/// carries an apiserver's own words and is meant to be matched on, not
/// compared.
#[derive(Debug)]
pub enum WatchEvent {
    /// An object appeared, or was seen for the first time.
    Added(Object),
    /// An object changed.
    Modified(Object),
    /// An object went away.
    Deleted(Object),
    /// Nothing changed, but everything up to this `resourceVersion` has been
    /// seen. Sent when `allowWatchBookmarks=true`, and the reason a
    /// reconnect after an idle hour does not have to re-list.
    Bookmark(String),
    /// The apiserver sent a `Status` instead of an object. Almost always
    /// [`Error::Gone`], which means list again.
    Failed(Error),
}

impl WatchEvent {
    /// The `resourceVersion` this event advances the watch to, if it carries
    /// one. A caller keeps the newest so a reconnect resumes from it.
    pub fn resource_version(&self) -> Option<&str> {
        match self {
            Self::Added(object) | Self::Modified(object) | Self::Deleted(object) => {
                Some(object.meta.resource_version.as_str()).filter(|version| !version.is_empty())
            }
            Self::Bookmark(version) => Some(version.as_str()),
            Self::Failed(_) => None,
        }
    }
}

/// A watch, as the store reads it.
///
/// Blocking: [`Self::next_event`] parks the calling thread until the
/// apiserver says something, which is why a watch owns a thread.
pub trait WatchStream: Send {
    /// The next event, or `None` when the connection has ended — which it
    /// will, because apiservers close idle watches on a timeout of their own.
    /// An end is not an error: the caller reconnects from the last version it
    /// saw.
    fn next_event(&mut self) -> Option<WatchEvent>;
}

/// A watch over newline-delimited JSON.
pub struct JsonLines<R> {
    reader: R,
    line: String,
}

impl<R: BufRead> JsonLines<R> {
    /// Read a watch from anything that yields lines.
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            line: String::new(),
        }
    }
}

impl<R: BufRead + Send> WatchStream for JsonLines<R> {
    fn next_event(&mut self) -> Option<WatchEvent> {
        loop {
            self.line.clear();
            match self.reader.read_line(&mut self.line) {
                // End of stream: the apiserver closed the watch, which it
                // does routinely. Not an error.
                Ok(0) => return None,
                Ok(_) => {
                    // A blank line, or one we cannot read, is skipped rather
                    // than ending the watch: one unreadable frame must not
                    // cost the connection.
                    if let Some(event) = parse_frame(&self.line) {
                        return Some(event);
                    }
                }
                Err(error) => {
                    return Some(WatchEvent::Failed(Error::Transport(error.to_string())));
                }
            }
        }
    }
}

/// Parse one line of a watch.
///
/// `None` for a line that says nothing — blank, or malformed — so the caller
/// can go round again. A `Status` object becomes [`WatchEvent::Failed`],
/// because the apiserver reports a watch's own failures in the stream rather
/// than in the status line: the response was `200` an hour ago.
pub fn parse_frame(line: &str) -> Option<WatchEvent> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let frame: Value = serde_json::from_str(line).ok()?;
    let kind = frame.get("type")?.as_str()?;
    let object = frame.get("object")?;
    if kind == "ERROR" || object.get("kind").and_then(Value::as_str) == Some("Status") {
        let code = object.get("code").and_then(Value::as_u64).unwrap_or(500) as u16;
        return Some(WatchEvent::Failed(Error::from_status(
            code,
            &object.to_string(),
        )));
    }
    if kind == "BOOKMARK" {
        let version = object
            .pointer("/metadata/resourceVersion")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        return Some(WatchEvent::Bookmark(version));
    }
    let object = Object::new(object.clone()).ok()?;
    match kind {
        "ADDED" => Some(WatchEvent::Added(object)),
        "MODIFIED" => Some(WatchEvent::Modified(object)),
        "DELETED" => Some(WatchEvent::Deleted(object)),
        _ => None,
    }
}

/// How long to wait before trying a dropped watch again.
///
/// Doubling from a second to half a minute. The first retry is quick because
/// most drops are the apiserver's own idle timeout and reconnect instantly;
/// the ceiling is there so a cluster that has gone away is not hammered by a
/// window someone left open.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    current: Duration,
}

impl Backoff {
    /// The first delay.
    pub const FIRST: Duration = Duration::from_secs(1);
    /// The longest delay.
    pub const CEILING: Duration = Duration::from_secs(30);

    /// A backoff that has not waited yet.
    pub fn new() -> Self {
        Self {
            current: Self::FIRST,
        }
    }

    /// How long to wait now, and lengthen the next one.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = (self.current * 2).min(Self::CEILING);
        delay
    }

    /// A watch connected: forget how long we had got to.
    pub fn reset(&mut self) {
        self.current = Self::FIRST;
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn frame(kind: &str, name: &str, version: &str) -> String {
        format!(
            r#"{{"type":"{kind}","object":{{"kind":"Pod","metadata":{{"name":"{name}","resourceVersion":"{version}"}}}}}}"#
        )
    }

    #[test]
    fn each_kind_of_frame_becomes_its_event() {
        assert!(matches!(
            parse_frame(&frame("ADDED", "a", "1")),
            Some(WatchEvent::Added(_))
        ));
        assert!(matches!(
            parse_frame(&frame("MODIFIED", "a", "2")),
            Some(WatchEvent::Modified(_))
        ));
        assert!(matches!(
            parse_frame(&frame("DELETED", "a", "3")),
            Some(WatchEvent::Deleted(_))
        ));
    }

    #[test]
    fn a_bookmark_carries_only_a_version_and_that_is_the_point() {
        let line =
            r#"{"type":"BOOKMARK","object":{"kind":"Pod","metadata":{"resourceVersion":"9912"}}}"#;
        let event = parse_frame(line).unwrap();
        assert!(matches!(&event, WatchEvent::Bookmark(version) if version == "9912"));
        assert_eq!(event.resource_version(), Some("9912"));
    }

    #[test]
    fn a_status_in_the_stream_is_the_watch_failing_not_an_object() {
        let line = r#"{"type":"ERROR","object":{"kind":"Status","code":410,
            "message":"too old resource version","reason":"Expired"}}"#;
        match parse_frame(line) {
            Some(WatchEvent::Failed(Error::Gone)) => {}
            other => panic!("expected Gone, got {other:?}"),
        }
    }

    #[test]
    fn a_status_can_arrive_without_the_error_type() {
        let line = r#"{"type":"ADDED","object":{"kind":"Status","code":410}}"#;
        assert!(matches!(
            parse_frame(line),
            Some(WatchEvent::Failed(Error::Gone))
        ));
    }

    #[test]
    fn a_line_that_says_nothing_is_skipped_rather_than_ending_the_watch() {
        assert!(parse_frame("").is_none());
        assert!(parse_frame("   \n").is_none());
        assert!(parse_frame("{not json").is_none());
        assert!(parse_frame(r#"{"type":"ADDED"}"#).is_none());
        assert!(parse_frame(r#"{"type":"WHAT","object":{"metadata":{"name":"a"}}}"#).is_none());
    }

    #[test]
    fn a_stream_yields_its_frames_in_order_and_ends_when_the_connection_does() {
        let body = format!(
            "{}\n\n{}\n",
            frame("ADDED", "a", "1"),
            frame("DELETED", "a", "2")
        );
        let mut watch = JsonLines::new(Cursor::new(body));
        assert!(matches!(watch.next_event(), Some(WatchEvent::Added(_))));
        assert!(matches!(watch.next_event(), Some(WatchEvent::Deleted(_))));
        assert!(watch.next_event().is_none());
    }

    #[test]
    fn one_unreadable_frame_does_not_cost_the_connection() {
        let body = format!("garbage\n{}\n", frame("ADDED", "a", "1"));
        let mut watch = JsonLines::new(Cursor::new(body));
        assert!(matches!(watch.next_event(), Some(WatchEvent::Added(_))));
    }

    #[test]
    fn an_events_version_is_what_a_reconnect_resumes_from() {
        let event = parse_frame(&frame("MODIFIED", "a", "77")).unwrap();
        assert_eq!(event.resource_version(), Some("77"));
        assert_eq!(WatchEvent::Failed(Error::Gone).resource_version(), None);
        // A frame the apiserver sent without a version advances nothing.
        let versionless =
            parse_frame(r#"{"type":"ADDED","object":{"kind":"Pod","metadata":{"name":"a"}}}"#)
                .unwrap();
        assert_eq!(versionless.resource_version(), None);
    }

    #[test]
    fn backoff_doubles_to_a_ceiling_and_a_reconnect_forgets_it() {
        let mut backoff = Backoff::new();
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
        assert_eq!(backoff.next_delay(), Duration::from_secs(2));
        assert_eq!(backoff.next_delay(), Duration::from_secs(4));
        for _ in 0..10 {
            backoff.next_delay();
        }
        assert_eq!(backoff.next_delay(), Backoff::CEILING);
        backoff.reset();
        assert_eq!(backoff.next_delay(), Backoff::FIRST);
    }
}
