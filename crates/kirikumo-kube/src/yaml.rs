//! Objects as YAML, for the detail panel's YAML tab.
//!
//! The apiserver will send YAML if asked (`Accept: application/yaml`), but the
//! object is already here as JSON — it was listed or fetched a moment ago —
//! and a second round trip to see what is on screen in another notation is a
//! round trip for nothing. YAML is JSON's superset, so the conversion is
//! total, and doing it locally means the YAML tab works over a store that has
//! no network at all (a scripted cluster, or a host's cached answer).

use serde_json::Value;

/// One object as the YAML `kubectl get -o yaml` would print.
///
/// Key order is the object's own, which is the apiserver's, which is the
/// order every Kubernetes user has read a manifest in: `apiVersion`, `kind`,
/// `metadata`, `spec`, `status`. Sorting alphabetically would be tidier and
/// would put `status` second.
pub fn to_yaml(value: &Value) -> String {
    serde_norway::to_string(value).unwrap_or_else(|error| {
        // Every JSON value has a YAML representation, so this is unreachable
        // in practice; showing the failure beats showing an empty tab.
        format!("# could not render this object as YAML: {error}\n")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_object_keeps_the_order_the_apiserver_sent_it_in() {
        let yaml = to_yaml(&json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {"name": "api", "labels": {"app": "api"}},
            "spec": {"containers": [{"name": "api", "image": "nginx:1.27"}]},
            "status": {"phase": "Running"}
        }));
        let lines: Vec<&str> = yaml.lines().collect();
        assert_eq!(lines[0], "apiVersion: v1");
        assert_eq!(lines[1], "kind: Pod");
        let spec = yaml.find("spec:").unwrap();
        let status = yaml.find("status:").unwrap();
        assert!(spec < status, "status must not sort ahead of spec");
    }

    #[test]
    fn a_nested_list_renders_as_a_list_and_not_as_json() {
        let yaml = to_yaml(&json!({"spec": {"containers": [{"name": "a"}, {"name": "b"}]}}));
        assert!(yaml.contains("- name: a"), "{yaml}");
        assert!(yaml.contains("- name: b"), "{yaml}");
    }

    #[test]
    fn every_json_value_has_a_rendering() {
        for value in [
            json!(null),
            json!(1),
            json!("text"),
            json!([1, 2]),
            json!({}),
        ] {
            assert!(!to_yaml(&value).is_empty());
        }
    }
}
