//! The JSONPath a custom resource's columns are written in.
//!
//! A CRD declares its `kubectl get` columns as `additionalPrinterColumns`,
//! each with a `jsonPath` the apiserver evaluates when it prints a table.
//! Evaluating them here — against the objects the window already holds —
//! is what gives a custom resource its own columns with no code written for
//! it (`AGENTS.md` rule 8), and without asking the apiserver for a second
//! copy of every list in table form.
//!
//! This is the subset real CRDs use, not the whole language: dotted keys,
//! bracketed keys for names with dots in them, indexes, `[*]`, and the one
//! filter form that is everywhere — `[?(@.type=="Ready")]` — with `==` and
//! `!=`. Anything else evaluates to nothing, which draws as an empty cell,
//! which is also what `kubectl` shows for a path it cannot follow.

use serde_json::Value;

/// One step of a path.
#[derive(Debug, Clone, PartialEq)]
enum Step {
    /// `.name` or `['name']`.
    Key(String),
    /// `[3]`, or `[-1]` for the last.
    Index(i64),
    /// `[*]`, every element.
    All,
    /// `[?(@.key=="value")]`; `equal` is false for `!=`.
    Filter {
        /// The key inside each element to compare.
        key: Vec<String>,
        /// The literal to compare against.
        value: String,
        /// `==` or `!=`.
        equal: bool,
    },
}

/// Parse a path. `None` when it is not one this app can follow.
fn parse(path: &str) -> Option<Vec<Step>> {
    let mut rest = path.trim();
    if let Some(after) = rest.strip_prefix('$') {
        rest = after;
    }
    let mut steps = Vec::new();
    while !rest.is_empty() {
        if let Some(after) = rest.strip_prefix('.') {
            // `.name` up to the next `.` or `[`.
            let end = after.find(['.', '[']).unwrap_or(after.len());
            let key = &after[..end];
            if key.is_empty() {
                return None;
            }
            steps.push(Step::Key(key.to_string()));
            rest = &after[end..];
        } else {
            let after = rest.strip_prefix('[')?;
            let end = matching_bracket(after)?;
            let inside = after[..end].trim();
            rest = &after[end + 1..];
            steps.push(bracket(inside)?);
        }
    }
    Some(steps)
}

/// Where the `]` that closes a `[` is, allowing for brackets inside a filter.
fn matching_bracket(text: &str) -> Option<usize> {
    let mut depth = 0;
    for (index, character) in text.char_indices() {
        match character {
            '[' => depth += 1,
            ']' if depth == 0 => return Some(index),
            ']' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// What is inside one `[…]`.
fn bracket(inside: &str) -> Option<Step> {
    if inside == "*" {
        return Some(Step::All);
    }
    if let Ok(index) = inside.parse::<i64>() {
        return Some(Step::Index(index));
    }
    if let Some(quoted) = inside
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .or_else(|| inside.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
    {
        return Some(Step::Key(quoted.to_string()));
    }
    // `?(@.a.b=="value")` or `?(@.a!="value")`.
    let expression = inside
        .strip_prefix("?(")
        .and_then(|s| s.strip_suffix(')'))?
        .trim();
    let (equal, (lhs, rhs)) = match expression.split_once("==") {
        Some(parts) => (true, parts),
        None => (false, expression.split_once("!=")?),
    };
    let key: Vec<String> = lhs
        .trim()
        .strip_prefix("@.")?
        .split('.')
        .map(str::to_string)
        .collect();
    let value = rhs
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .to_string();
    Some(Step::Filter { key, value, equal })
}

/// Everything a path reaches in a value.
///
/// More than one thing when the path has a `[*]` or a filter in it; nothing
/// when any step cannot be taken.
pub fn eval<'a>(value: &'a Value, path: &str) -> Vec<&'a Value> {
    let Some(steps) = parse(path) else {
        return Vec::new();
    };
    let mut current: Vec<&Value> = vec![value];
    for step in &steps {
        let mut next = Vec::new();
        for item in current {
            match step {
                Step::Key(key) => {
                    if let Some(found) = item.get(key) {
                        next.push(found);
                    }
                }
                Step::Index(index) => {
                    if let Some(items) = item.as_array() {
                        let position = match *index < 0 {
                            true => items.len().checked_sub(index.unsigned_abs() as usize),
                            false => Some(*index as usize),
                        };
                        if let Some(found) = position.and_then(|p| items.get(p)) {
                            next.push(found);
                        }
                    }
                }
                Step::All => {
                    if let Some(items) = item.as_array() {
                        next.extend(items.iter());
                    }
                }
                Step::Filter { key, value, equal } => {
                    if let Some(items) = item.as_array() {
                        next.extend(items.iter().filter(|element| {
                            let mut probe = *element;
                            for part in key {
                                match probe.get(part) {
                                    Some(inner) => probe = inner,
                                    None => return !equal,
                                }
                            }
                            // A number or a boolean in the object against a
                            // quoted literal in the path: `@.ready==true` and
                            // `@.count==3` are written that way in CRDs, so
                            // the literal is compared against the value's
                            // JSON spelling.
                            let matches = match probe {
                                Value::String(text) => text == value,
                                other => other.to_string().as_str() == value.as_str(),
                            };
                            matches == *equal
                        }));
                    }
                }
            }
        }
        current = next;
        if current.is_empty() {
            break;
        }
    }
    current
}

/// A path's result as a cell: the values, joined with commas the way
/// `kubectl` joins a path that reaches several.
pub fn cell(value: &Value, path: &str) -> String {
    eval(value, path)
        .into_iter()
        .map(|found| match found {
            Value::String(text) => text.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn widget() -> Value {
        json!({
            "metadata": {"name": "red-one", "labels": {"app.kubernetes.io/name": "widget"}},
            "spec": {"colour": "red", "replicas": 3, "tags": ["a", "b", "c"]},
            "status": {"phase": "Spinning", "conditions": [
                {"type": "Ready", "status": "True"},
                {"type": "Synced", "status": "False", "reason": "Drift"}
            ]}
        })
    }

    #[test]
    fn a_dotted_path_reaches_a_field() {
        assert_eq!(cell(&widget(), ".spec.colour"), "red");
        assert_eq!(cell(&widget(), ".spec.replicas"), "3");
        assert_eq!(cell(&widget(), "$.status.phase"), "Spinning");
    }

    #[test]
    fn a_key_with_dots_in_it_is_bracketed() {
        assert_eq!(
            cell(&widget(), ".metadata.labels['app.kubernetes.io/name']"),
            "widget"
        );
        assert_eq!(
            cell(&widget(), ".metadata.labels[\"app.kubernetes.io/name\"]"),
            "widget"
        );
    }

    #[test]
    fn an_index_picks_one_and_a_negative_one_counts_from_the_end() {
        assert_eq!(cell(&widget(), ".spec.tags[0]"), "a");
        assert_eq!(cell(&widget(), ".spec.tags[-1]"), "c");
        assert_eq!(cell(&widget(), ".spec.tags[9]"), "");
    }

    #[test]
    fn a_star_reaches_every_element_and_the_cell_joins_them() {
        assert_eq!(cell(&widget(), ".spec.tags[*]"), "a,b,c");
        assert_eq!(
            cell(&widget(), ".status.conditions[*].type"),
            "Ready,Synced"
        );
    }

    #[test]
    fn the_filter_every_crd_uses_finds_the_condition_by_type() {
        // cert-manager, Argo, Crossplane: all of them write this one.
        assert_eq!(
            cell(&widget(), ".status.conditions[?(@.type==\"Ready\")].status"),
            "True"
        );
        assert_eq!(
            cell(&widget(), ".status.conditions[?(@.type=='Synced')].reason"),
            "Drift"
        );
        assert_eq!(
            cell(&widget(), ".status.conditions[?(@.type!=\"Ready\")].type"),
            "Synced"
        );
    }

    #[test]
    fn a_path_that_cannot_be_followed_is_an_empty_cell_not_a_panic() {
        assert_eq!(cell(&widget(), ".status.nothing.here"), "");
        assert_eq!(cell(&widget(), ".spec.colour.deeper"), "");
        assert_eq!(cell(&widget(), ".spec.tags[?(@.x==1)]"), "");
        // And a path this subset does not speak.
        assert_eq!(cell(&widget(), ".spec[?(@.a>1)]"), "");
        assert_eq!(cell(&widget(), "spec.colour"), "");
    }

    #[test]
    fn a_missing_field_in_a_filter_counts_as_not_equal() {
        // `!=` matches an element that has no such key at all, which is what
        // `kubectl` does.
        assert_eq!(
            cell(&widget(), ".status.conditions[?(@.reason!=\"Drift\")].type"),
            "Ready"
        );
    }
}
