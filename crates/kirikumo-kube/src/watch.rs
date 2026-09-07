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
use crate::model::{Object, ObjectList};
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

/// What applying one watch event did to a list.
///
/// The index matters: a caller that keeps rows alongside the objects updates
/// one of them rather than rebuilding all four thousand.
///
/// Not `PartialEq`, because [`Error`] is not; match on it.
#[derive(Debug)]
pub enum Applied {
    /// An object appeared, at this index.
    Added(usize),
    /// An object was replaced, at this index.
    Changed(usize),
    /// An object went away, from this index.
    Removed(usize),
    /// Only the version a reconnect resumes from moved.
    Version,
    /// The watch cannot continue: list again, then start a new one from the
    /// version the list came back with.
    Restart,
    /// The watch failed. [`Error::is_retryable`] says whether reconnecting is
    /// worth trying.
    Failed(Error),
}

/// Apply one watch event to the list it belongs to.
///
/// The whole of a watch's semantics, written where it can be tested without a
/// window or an apiserver (`AGENTS.md` rule 6). Three rules are load-bearing:
///
/// - Objects are matched by [`crate::ObjectMeta::identity`], not by name, so
///   a pod deleted and recreated under the same name is a new row rather than
///   an edit of the old one.
/// - A modification keeps the object's *position*. The table is sorted by
///   whatever the reader clicked, and moving a row to the end on every status
///   change would make a busy namespace unreadable.
/// - The list's `resourceVersion` advances on every event, bookmarks
///   included. That is the entire point of asking for bookmarks: an idle
///   watch still tells us where to resume, so a reconnect after an hour of
///   nothing costs no re-list.
pub fn apply(list: &mut ObjectList, event: WatchEvent) -> Applied {
    if let Some(version) = event.resource_version() {
        list.resource_version = version.to_string();
    }
    match event {
        WatchEvent::Added(object) | WatchEvent::Modified(object) => match position(list, &object) {
            Some(index) => {
                list.items[index] = object;
                Applied::Changed(index)
            }
            None => {
                list.items.push(object);
                Applied::Added(list.items.len() - 1)
            }
        },
        WatchEvent::Deleted(object) => match position(list, &object) {
            Some(index) => {
                list.items.remove(index);
                Applied::Removed(index)
            }
            // A delete for something we never had. Normal after a re-list,
            // and nothing to do about it.
            None => Applied::Version,
        },
        WatchEvent::Bookmark(_) => Applied::Version,
        WatchEvent::Failed(Error::Gone) => Applied::Restart,
        WatchEvent::Failed(error) => Applied::Failed(error),
    }
}

/// Where an object already sits in a list, if it does.
fn position(list: &ObjectList, object: &Object) -> Option<usize> {
    let identity = object.meta.identity();
    list.items
        .iter()
        .position(|held| held.meta.identity() == identity)
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

    fn list(names: &[(&str, &str)]) -> ObjectList {
        ObjectList {
            items: names
                .iter()
                .map(|(name, uid)| {
                    Object::new(serde_json::json!({
                        "metadata": {"name": name, "uid": uid, "resourceVersion": "1"}
                    }))
                    .unwrap()
                })
                .collect(),
            resource_version: "1".into(),
            next: None,
        }
    }

    fn object(name: &str, uid: &str, version: &str) -> Object {
        Object::new(serde_json::json!({
            "metadata": {"name": name, "uid": uid, "resourceVersion": version}
        }))
        .unwrap()
    }

    fn names(list: &ObjectList) -> Vec<&str> {
        list.items.iter().map(|o| o.meta.name.as_str()).collect()
    }

    #[test]
    fn an_addition_lands_at_the_end_and_advances_the_version() {
        let mut list = list(&[("a", "1"), ("b", "2")]);
        let applied = apply(&mut list, WatchEvent::Added(object("c", "3", "12")));
        assert!(matches!(applied, Applied::Added(2)));
        assert_eq!(names(&list), vec!["a", "b", "c"]);
        assert_eq!(list.resource_version, "12");
    }

    #[test]
    fn a_modification_keeps_the_objects_position() {
        // The table is sorted by whatever the reader clicked; a row that
        // jumped to the end on every status change would be unreadable.
        let mut list = list(&[("a", "1"), ("b", "2"), ("c", "3")]);
        let applied = apply(&mut list, WatchEvent::Modified(object("b", "2", "20")));
        assert!(matches!(applied, Applied::Changed(1)));
        assert_eq!(names(&list), vec!["a", "b", "c"]);
        assert_eq!(list.items[1].meta.resource_version, "20");
    }

    #[test]
    fn a_deletion_removes_it() {
        let mut list = list(&[("a", "1"), ("b", "2")]);
        let applied = apply(&mut list, WatchEvent::Deleted(object("a", "1", "30")));
        assert!(matches!(applied, Applied::Removed(0)));
        assert_eq!(names(&list), vec!["b"]);
        assert_eq!(list.resource_version, "30");
    }

    #[test]
    fn a_deletion_for_something_we_never_had_is_harmless() {
        let mut list = list(&[("a", "1")]);
        let applied = apply(&mut list, WatchEvent::Deleted(object("gone", "9", "31")));
        assert!(matches!(applied, Applied::Version));
        assert_eq!(names(&list), vec!["a"]);
        assert_eq!(list.resource_version, "31");
    }

    #[test]
    fn a_pod_recreated_under_the_same_name_is_a_new_row_and_not_an_edit() {
        // Matched by uid, not by name: a Deployment rolling a pod out reuses
        // names all day, and treating the new one as an edit of the old would
        // hide the restart.
        let mut list = list(&[("api", "old")]);
        let applied = apply(&mut list, WatchEvent::Added(object("api", "new", "40")));
        assert!(matches!(applied, Applied::Added(1)));
        assert_eq!(list.items.len(), 2);
    }

    #[test]
    fn an_object_with_no_uid_falls_back_to_where_it_lives() {
        let mut list = ObjectList {
            items: vec![
                Object::new(serde_json::json!({
                    "metadata": {"name": "a", "namespace": "one"}
                }))
                .unwrap(),
            ],
            resource_version: "1".into(),
            next: None,
        };
        let same = Object::new(serde_json::json!({
            "metadata": {"name": "a", "namespace": "one", "resourceVersion": "2"}
        }))
        .unwrap();
        assert!(matches!(
            apply(&mut list, WatchEvent::Modified(same)),
            Applied::Changed(0)
        ));
        let elsewhere = Object::new(serde_json::json!({
            "metadata": {"name": "a", "namespace": "two"}
        }))
        .unwrap();
        assert!(matches!(
            apply(&mut list, WatchEvent::Added(elsewhere)),
            Applied::Added(1)
        ));
    }

    #[test]
    fn a_bookmark_moves_only_the_version_and_that_is_the_whole_point() {
        // An idle watch still says where to resume, so a reconnect after an
        // hour of nothing costs no re-list.
        let mut list = list(&[("a", "1")]);
        let applied = apply(&mut list, WatchEvent::Bookmark("9912".into()));
        assert!(matches!(applied, Applied::Version));
        assert_eq!(list.resource_version, "9912");
        assert_eq!(list.items.len(), 1);
    }

    #[test]
    fn a_gone_asks_for_a_relist_and_anything_else_is_reported_as_it_is() {
        let mut list = list(&[("a", "1")]);
        assert!(matches!(
            apply(&mut list, WatchEvent::Failed(Error::Gone)),
            Applied::Restart
        ));
        assert!(matches!(
            apply(&mut list, WatchEvent::Failed(Error::Forbidden("no".into()))),
            Applied::Failed(Error::Forbidden(_))
        ));
        // Neither touched the objects.
        assert_eq!(list.items.len(), 1);
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
