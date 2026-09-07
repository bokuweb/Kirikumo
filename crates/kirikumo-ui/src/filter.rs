//! The filter box over a table.
//!
//! Fuzzy, case-insensitive, and matched against every cell of a row plus its
//! labels — so `crash`, `10.244`, `node-3` and `app=api` all find something,
//! which is what people actually type into a cluster viewer.
//!
//! Matching happens over the row's precomputed haystack rather than over the
//! object, because a filter runs on every keystroke against every row: a
//! namespace with four thousand pods must not re-read four thousand JSON
//! documents to answer one character.

use crate::table::Row;
use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};

/// A matcher, reused across keystrokes.
///
/// `nucleo`'s matcher owns scratch buffers; making one per keystroke is what
/// turns a cheap filter into an allocation storm.
pub struct Filter {
    matcher: Matcher,
}

impl Default for Filter {
    fn default() -> Self {
        Self::new()
    }
}

impl Filter {
    /// A filter.
    pub fn new() -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT),
        }
    }

    /// The rows that match, in the order they were given.
    ///
    /// Deliberately *not* sorted by score: this is a filter over a sorted
    /// table, not a picker. A table that reorders itself as the reader types
    /// loses the column they were reading down.
    pub fn apply<'rows>(&mut self, query: &str, rows: &'rows [Row]) -> Vec<&'rows Row> {
        let query = query.trim();
        if query.is_empty() {
            return rows.iter().collect();
        }
        let pattern = Pattern::new(
            query,
            CaseMatching::Ignore,
            Normalization::Smart,
            AtomKind::Fuzzy,
        );
        let mut buffer = Vec::new();
        rows.iter()
            .filter(|row| {
                buffer.clear();
                let haystack = nucleo_matcher::Utf32Str::new(&row.haystack, &mut buffer);
                pattern.score(haystack, &mut self.matcher).is_some()
            })
            .collect()
    }

    /// The indices of the rows that match, for a caller that keeps its own
    /// storage — which the virtualized table does, so that a row's position
    /// in the filtered view can be mapped back to the object behind it.
    pub fn indices(&mut self, query: &str, rows: &[Row]) -> Vec<usize> {
        let query = query.trim();
        if query.is_empty() {
            return (0..rows.len()).collect();
        }
        let pattern = Pattern::new(
            query,
            CaseMatching::Ignore,
            Normalization::Smart,
            AtomKind::Fuzzy,
        );
        let mut buffer = Vec::new();
        rows.iter()
            .enumerate()
            .filter_map(|(index, row)| {
                buffer.clear();
                let haystack = nucleo_matcher::Utf32Str::new(&row.haystack, &mut buffer);
                pattern.score(haystack, &mut self.matcher).map(|_| index)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::ColumnSet;
    use chrono::{DateTime, Utc};
    use kirikumo_kube::Object;
    use serde_json::json;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-07T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn rows() -> Vec<Row> {
        let columns = ColumnSet::for_kind("Pod", true, true);
        [
            json!({
                "metadata": {"name": "api-7d9f8c-2xk", "namespace": "shop", "uid": "1",
                             "labels": {"app": "api"}},
                "spec": {"nodeName": "node-1", "containers": [{"name": "api"}]},
                "status": {"phase": "Running", "podIP": "10.244.1.7",
                           "containerStatuses": [{"ready": true}]}
            }),
            json!({
                "metadata": {"name": "web-6b4c5d-lm2", "namespace": "shop", "uid": "2",
                             "labels": {"app": "web"}},
                "spec": {"nodeName": "node-2", "containers": [{"name": "web"}]},
                "status": {"phase": "Running", "podIP": "10.244.2.4",
                           "containerStatuses": [{"ready": true}]}
            }),
            json!({
                "metadata": {"name": "jobrunner-xk4", "namespace": "kube-system", "uid": "3"},
                "spec": {"nodeName": "node-3", "containers": [{"name": "job"}]},
                "status": {"phase": "Running", "podIP": "10.244.3.9",
                           "containerStatuses": [
                               {"ready": false, "state": {"waiting": {"reason": "CrashLoopBackOff"}}}
                           ]}
            }),
        ]
        .into_iter()
        .map(|value| columns.row(&Object::new(value).unwrap(), now()))
        .collect()
    }

    #[test]
    fn an_empty_query_keeps_every_row() {
        let rows = rows();
        let mut filter = Filter::new();
        assert_eq!(filter.apply("", &rows).len(), 3);
        assert_eq!(filter.apply("   ", &rows).len(), 3);
        assert_eq!(filter.indices("", &rows), vec![0, 1, 2]);
    }

    #[test]
    fn a_name_finds_its_row() {
        let rows = rows();
        let mut filter = Filter::new();
        let found = filter.apply("web", &rows);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "web-6b4c5d-lm2");
    }

    #[test]
    fn a_status_finds_the_rows_in_it() {
        let rows = rows();
        let mut filter = Filter::new();
        let found = filter.apply("crashloop", &rows);
        assert_eq!(found.len(), 1);
        assert!(found[0].is_bad());
    }

    #[test]
    fn a_namespace_a_node_and_an_address_all_match_because_they_are_all_in_the_row() {
        let rows = rows();
        let mut filter = Filter::new();
        assert_eq!(filter.apply("kube-system", &rows).len(), 1);
        assert_eq!(filter.apply("node-2", &rows).len(), 1);
        assert_eq!(filter.apply("10.244.1", &rows).len(), 1);
    }

    #[test]
    fn a_label_matches_even_though_it_is_in_no_column() {
        let rows = rows();
        let mut filter = Filter::new();
        let found = filter.apply("app=api", &rows);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "api-7d9f8c-2xk");
    }

    #[test]
    fn matching_ignores_case() {
        let rows = rows();
        let mut filter = Filter::new();
        assert_eq!(filter.apply("WEB", &rows).len(), 1);
    }

    #[test]
    fn the_order_of_the_table_survives_the_filter() {
        // Not sorted by score: a table that reorders as you type loses the
        // column you were reading down.
        let rows = rows();
        let mut filter = Filter::new();
        // A query every row matches fuzzily, in a different order of quality.
        let found = filter.apply("o", &rows);
        let names: Vec<&str> = found.iter().map(|row| row.name.as_str()).collect();
        let all: Vec<&str> = rows
            .iter()
            .filter(|row| names.contains(&row.name.as_str()))
            .map(|row| row.name.as_str())
            .collect();
        assert_eq!(names, all);
    }

    #[test]
    fn a_query_nothing_matches_finds_nothing_rather_than_everything() {
        let rows = rows();
        let mut filter = Filter::new();
        assert!(filter.apply("zzzzzz", &rows).is_empty());
        assert!(filter.indices("zzzzzz", &rows).is_empty());
    }

    #[test]
    fn indices_line_up_with_the_rows_they_came_from() {
        let rows = rows();
        let mut filter = Filter::new();
        let indices = filter.indices("web", &rows);
        assert_eq!(indices, vec![1]);
        assert_eq!(rows[indices[0]].name, "web-6b4c5d-lm2");
    }
}
