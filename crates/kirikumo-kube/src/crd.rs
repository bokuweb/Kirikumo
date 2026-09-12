//! What a custom resource says its columns are.
//!
//! A `CustomResourceDefinition` carries, per served version, the
//! `additionalPrinterColumns` that `kubectl get` prints for that kind: a
//! name, a type, a JSONPath, and a priority. Read once per connection, they
//! are what turn a table of custom resources from *Name · Namespace · Age*
//! into the table the kind's own authors designed — with no code written
//! here for the kind (`AGENTS.md` rule 8).

use crate::model::{Object, ResourceKey};
use serde_json::Value;
use std::collections::HashMap;

/// One declared column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrinterColumn {
    /// The heading, as the CRD spells it.
    pub name: String,
    /// `string`, `integer`, `number`, `boolean` or `date`.
    pub kind: String,
    /// Where in the object the value is.
    pub json_path: String,
    /// `0` is shown always; anything higher only with `-o wide`, which this
    /// table always is.
    pub priority: u32,
}

impl PrinterColumn {
    /// Whether the column holds a number, and so is drawn right-aligned.
    pub fn is_numeric(&self) -> bool {
        matches!(self.kind.as_str(), "integer" | "number")
    }

    /// Whether the column holds a timestamp to be shown as an age.
    pub fn is_date(&self) -> bool {
        self.kind == "date"
    }
}

/// The columns each custom kind declares, by kind.
pub type PrinterColumns = HashMap<ResourceKey, Vec<PrinterColumn>>;

/// Read the printer columns out of one CRD.
///
/// The columns come from the version marked `storage`, which is the one the
/// apiserver serves objects in and the one discovery prefers; a CRD with no
/// storage version marked — malformed, but seen — falls back to the first
/// served one. A version with no columns declared gets an empty list, which
/// draws the default table.
pub fn printer_columns(crd: &Object) -> Option<(ResourceKey, Vec<PrinterColumn>)> {
    let group = crd.str_at("spec.group");
    let kind = crd.str_at("spec.names.kind");
    if group.is_empty() || kind.is_empty() {
        return None;
    }
    let versions = crd.array_at("spec.versions");
    let chosen = versions
        .iter()
        .find(|version| version.get("storage").and_then(Value::as_bool) == Some(true))
        .or_else(|| {
            versions
                .iter()
                .find(|version| version.get("served").and_then(Value::as_bool) == Some(true))
        })?;
    let columns = chosen
        .get("additionalPrinterColumns")
        .and_then(Value::as_array)
        .map(|columns| {
            columns
                .iter()
                .filter_map(|column| {
                    Some(PrinterColumn {
                        name: column.get("name")?.as_str()?.to_string(),
                        kind: column
                            .get("type")
                            .and_then(Value::as_str)
                            .unwrap_or("string")
                            .to_string(),
                        json_path: column.get("jsonPath")?.as_str()?.to_string(),
                        priority: column.get("priority").and_then(Value::as_u64).unwrap_or(0)
                            as u32,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some((ResourceKey::new(group, kind), columns))
}

/// The printer columns of every CRD in a list.
pub fn from_list(crds: &[Object]) -> PrinterColumns {
    crds.iter().filter_map(printer_columns).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn crd(versions: Value) -> Object {
        Object::new(json!({
            "metadata": {"name": "widgets.example.kirikumo.dev"},
            "spec": {"group": "example.kirikumo.dev",
                     "names": {"kind": "Widget", "plural": "widgets"},
                     "versions": versions}
        }))
        .unwrap()
    }

    #[test]
    fn the_storage_versions_columns_are_the_ones() {
        let crd = crd(json!([
            {"name": "v1alpha1", "served": true, "storage": false,
             "additionalPrinterColumns": [{"name": "Old", "type": "string", "jsonPath": ".spec.old"}]},
            {"name": "v1", "served": true, "storage": true,
             "additionalPrinterColumns": [
                {"name": "Colour", "type": "string", "jsonPath": ".spec.colour"},
                {"name": "Replicas", "type": "integer", "jsonPath": ".spec.replicas"},
                {"name": "Started", "type": "date", "jsonPath": ".metadata.creationTimestamp", "priority": 1}
             ]}
        ]));
        let (key, columns) = printer_columns(&crd).unwrap();
        assert_eq!(key, ResourceKey::new("example.kirikumo.dev", "Widget"));
        let names: Vec<&str> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["Colour", "Replicas", "Started"]);
        assert!(columns[1].is_numeric());
        assert!(columns[2].is_date());
        assert_eq!(columns[2].priority, 1);
        assert!(!columns[0].is_numeric());
    }

    #[test]
    fn a_crd_with_no_columns_declared_gets_an_empty_list_not_nothing() {
        let crd = crd(json!([{"name": "v1", "served": true, "storage": true}]));
        let (_, columns) = printer_columns(&crd).unwrap();
        assert!(columns.is_empty());
    }

    #[test]
    fn a_crd_with_no_storage_version_falls_back_to_a_served_one() {
        let crd = crd(json!([
            {"name": "v1", "served": true,
             "additionalPrinterColumns": [{"name": "X", "type": "string", "jsonPath": ".x"}]}
        ]));
        assert_eq!(printer_columns(&crd).unwrap().1.len(), 1);
    }

    #[test]
    fn a_column_without_a_path_is_dropped_and_the_rest_kept() {
        let crd = crd(json!([
            {"name": "v1", "served": true, "storage": true,
             "additionalPrinterColumns": [
                {"name": "NoPath", "type": "string"},
                {"name": "Fine", "type": "string", "jsonPath": ".spec.fine"}
             ]}
        ]));
        let (_, columns) = printer_columns(&crd).unwrap();
        assert_eq!(columns.len(), 1);
        assert_eq!(columns[0].name, "Fine");
    }

    #[test]
    fn something_that_is_not_a_crd_is_skipped() {
        let not = Object::new(json!({"metadata": {"name": "x"}, "spec": {}})).unwrap();
        assert!(printer_columns(&not).is_none());
        assert!(from_list(&[not]).is_empty());
    }
}
