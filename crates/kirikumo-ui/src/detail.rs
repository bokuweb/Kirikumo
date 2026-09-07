//! What the right panel says about one object.
//!
//! The Overview tab, as data: a list of sections, each a list of labelled
//! facts. Written per kind against the JSON, like everything else here, and
//! with a fallback that works for a kind nobody wrote a section for — the
//! metadata every object has, plus its conditions, which is `kubectl
//! describe` reduced to what fits in a 420 px column.

use chrono::{DateTime, Utc};
use kirikumo_kube::{Level, Object};
use serde_json::Value;

/// One labelled fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    /// The label, already localised or already a Kubernetes field name.
    pub label: String,
    /// The value, formatted for reading.
    pub value: String,
    /// Whether the value is an identifier, and so drawn in the mono family.
    pub mono: bool,
}

impl Fact {
    /// A fact whose value is prose.
    pub fn text(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            mono: false,
        }
    }

    /// A fact whose value is an identifier: a name, an image, an address.
    pub fn id(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            mono: true,
        }
    }
}

/// A group of facts under a heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// The heading, or `None` for the first block, which needs none.
    pub title: Option<String>,
    /// The facts.
    pub facts: Vec<Fact>,
}

/// One `status.conditions` entry, as the panel draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    /// `Ready`, `Available`, `PodScheduled`.
    pub kind: String,
    /// `True`, `False`, `Unknown`.
    pub status: String,
    /// Why, when the controller says.
    pub reason: String,
    /// The mark to draw beside it.
    pub level: Level,
}

/// Everything the Overview tab draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Overview {
    /// The sections, in order.
    pub sections: Vec<Section>,
    /// The conditions, drawn as their own table under the sections.
    pub conditions: Vec<Condition>,
}

/// Build the Overview for an object.
pub fn overview(kind: &str, object: &Object, now: DateTime<Utc>) -> Overview {
    let mut sections = vec![metadata(object, now)];
    match kind {
        "Pod" => sections.extend(pod(object)),
        "Node" => sections.extend(node(object)),
        "Deployment" | "StatefulSet" | "ReplicaSet" | "DaemonSet" => {
            sections.extend(controller(object))
        }
        "Service" => sections.extend(service(object)),
        "PersistentVolumeClaim" => sections.extend(claim(object)),
        "ConfigMap" | "Secret" => sections.extend(keys(object)),
        _ => {}
    }
    Overview {
        sections,
        conditions: conditions(object),
    }
}

/// The block every object has.
fn metadata(object: &Object, now: DateTime<Utc>) -> Section {
    let mut facts = vec![Fact::id("Name", object.meta.name.clone())];
    if let Some(namespace) = &object.meta.namespace {
        facts.push(Fact::id("Namespace", namespace.clone()));
    }
    facts.push(Fact::text(
        "Created",
        crate::time::age(object.meta.created, now),
    ));
    if let Some(owner) = object.meta.controller() {
        facts.push(Fact::id(
            "Controlled by",
            format!("{}/{}", owner.kind, owner.name),
        ));
    }
    if !object.meta.labels.is_empty() {
        facts.push(Fact::id("Labels", pairs(&object.meta.labels)));
    }
    if !object.meta.uid.is_empty() {
        facts.push(Fact::id("UID", object.meta.uid.clone()));
    }
    Section { title: None, facts }
}

fn pod(object: &Object) -> Vec<Section> {
    let mut sections = Vec::new();
    let mut facts = Vec::new();
    push_id(&mut facts, "Node", object.str_at("spec.nodeName"));
    push_id(&mut facts, "Pod IP", object.str_at("status.podIP"));
    push_id(&mut facts, "Host IP", object.str_at("status.hostIP"));
    push_text(&mut facts, "QoS class", object.str_at("status.qosClass"));
    push_text(
        &mut facts,
        "Service account",
        object.str_at("spec.serviceAccountName"),
    );
    if !facts.is_empty() {
        sections.push(Section {
            title: Some("Placement".into()),
            facts,
        });
    }

    // One fact per container: what it runs, and what it is doing. The state
    // is read from `status.containerStatuses` by name rather than by index,
    // because the two lists are not guaranteed to be in the same order.
    let statuses = object.array_at("status.containerStatuses");
    let mut containers = Vec::new();
    for container in object.array_at("spec.containers") {
        let name = container
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let image = container
            .get("image")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let status = statuses
            .iter()
            .find(|status| status.get("name").and_then(Value::as_str) == Some(name));
        let state = status.map(container_state).unwrap_or_default();
        let restarts = status
            .and_then(|status| status.get("restartCount"))
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let mut value = image.to_string();
        if !state.is_empty() {
            value.push_str(&format!("  ·  {state}"));
        }
        if restarts > 0 {
            value.push_str(&format!("  ·  {restarts} restarts"));
        }
        containers.push(Fact::id(name, value));
    }
    if !containers.is_empty() {
        sections.push(Section {
            title: Some("Containers".into()),
            facts: containers,
        });
    }
    sections
}

/// What one container is doing, in a word or two.
fn container_state(status: &Value) -> String {
    // `state` is a one-of: exactly one of `running`, `waiting`, `terminated`
    // is present, so the first entry is the answer.
    let Some((name, body)) = status
        .get("state")
        .and_then(Value::as_object)
        .and_then(|state| state.iter().next())
    else {
        return String::new();
    };
    body.get("reason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty())
        .map(str::to_string)
        // `running` carries no reason, and "running" is the answer.
        .unwrap_or_else(|| name.clone())
}

fn node(object: &Object) -> Vec<Section> {
    let mut sections = Vec::new();
    let addresses: Vec<String> = object
        .array_at("status.addresses")
        .iter()
        .filter_map(|address| {
            let kind = address.get("type").and_then(Value::as_str)?;
            let value = address.get("address").and_then(Value::as_str)?;
            Some(format!("{kind} {value}"))
        })
        .collect();
    let mut facts = Vec::new();
    push_text(
        &mut facts,
        "Kubelet",
        object.str_at("status.nodeInfo.kubeletVersion"),
    );
    push_text(&mut facts, "OS", object.str_at("status.nodeInfo.osImage"));
    push_text(
        &mut facts,
        "Runtime",
        object.str_at("status.nodeInfo.containerRuntimeVersion"),
    );
    if !addresses.is_empty() {
        facts.push(Fact::id("Addresses", addresses.join("\n")));
    }
    if object.bool_at("spec.unschedulable") {
        facts.push(Fact::text("Scheduling", "Disabled (cordoned)"));
    }
    sections.push(Section {
        title: Some("Node".into()),
        facts,
    });

    let mut capacity = Vec::new();
    for (label, path) in [("CPU", "cpu"), ("Memory", "memory"), ("Pods", "pods")] {
        let allocatable = object.str_at(&format!("status.allocatable.{path}"));
        let total = object.str_at(&format!("status.capacity.{path}"));
        if allocatable.is_empty() && total.is_empty() {
            continue;
        }
        capacity.push(Fact::id(label, format!("{allocatable} of {total}")));
    }
    if !capacity.is_empty() {
        sections.push(Section {
            title: Some("Allocatable".into()),
            facts: capacity,
        });
    }
    sections
}

fn controller(object: &Object) -> Vec<Section> {
    let mut facts = Vec::new();
    push_text(&mut facts, "Strategy", object.str_at("spec.strategy.type"));
    push_text(
        &mut facts,
        "Update strategy",
        object.str_at("spec.updateStrategy.type"),
    );
    let selector = object
        .at("spec.selector.matchLabels")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| Some(format!("{key}={}", value.as_str()?)))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    push_id(&mut facts, "Selector", &selector);
    let images = object
        .array_at("spec.template.spec.containers")
        .iter()
        .filter_map(|container| container.get("image").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    push_id(&mut facts, "Images", &images);
    match facts.is_empty() {
        true => Vec::new(),
        false => vec![Section {
            title: Some("Template".into()),
            facts,
        }],
    }
}

fn service(object: &Object) -> Vec<Section> {
    let mut facts = Vec::new();
    push_text(&mut facts, "Type", object.str_at("spec.type"));
    push_id(&mut facts, "Cluster IP", object.str_at("spec.clusterIP"));
    let ports: Vec<String> = object
        .array_at("spec.ports")
        .iter()
        .map(|port| {
            let name = port.get("name").and_then(Value::as_str).unwrap_or("");
            let number = port.get("port").and_then(Value::as_i64).unwrap_or_default();
            let target = port
                .get("targetPort")
                .map(|target| match target {
                    Value::String(text) => text.clone(),
                    other => other.to_string(),
                })
                .unwrap_or_default();
            match name.is_empty() {
                true => format!("{number} → {target}"),
                false => format!("{name}  {number} → {target}"),
            }
        })
        .collect();
    if !ports.is_empty() {
        facts.push(Fact::id("Ports", ports.join("\n")));
    }
    let selector = object
        .at("spec.selector")
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| Some(format!("{key}={}", value.as_str()?)))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    push_id(&mut facts, "Selector", &selector);
    vec![Section {
        title: Some("Service".into()),
        facts,
    }]
}

fn claim(object: &Object) -> Vec<Section> {
    let mut facts = Vec::new();
    push_text(&mut facts, "Phase", object.str_at("status.phase"));
    push_id(&mut facts, "Volume", object.str_at("spec.volumeName"));
    push_text(
        &mut facts,
        "Storage class",
        object.str_at("spec.storageClassName"),
    );
    push_id(
        &mut facts,
        "Requested",
        object.str_at("spec.resources.requests.storage"),
    );
    push_id(
        &mut facts,
        "Capacity",
        object.str_at("status.capacity.storage"),
    );
    let modes: Vec<&str> = object
        .array_at("spec.accessModes")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    if !modes.is_empty() {
        facts.push(Fact::text("Access modes", modes.join(", ")));
    }
    vec![Section {
        title: Some("Claim".into()),
        facts,
    }]
}

/// A ConfigMap's or a Secret's keys.
///
/// Keys only, never values: a Secret's values are base64 of something the
/// reader did not ask this window to put on a screen behind them. Revealing
/// one is an action, and actions are M4.
fn keys(object: &Object) -> Vec<Section> {
    let mut keys: Vec<String> = object
        .at("data")
        .and_then(Value::as_object)
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default();
    keys.sort();
    match keys.is_empty() {
        true => Vec::new(),
        false => vec![Section {
            title: Some("Keys".into()),
            facts: keys
                .into_iter()
                .map(|key| Fact::id(key, String::new()))
                .collect(),
        }],
    }
}

/// The conditions, with the mark each one gets.
///
/// `Ready=False` is an error and `Ready=Unknown` is too, but the negative
/// conditions — `MemoryPressure`, `NetworkUnavailable` — mean the opposite:
/// `True` is the bad one. Reading a condition's polarity from its name is
/// what stops a healthy node from showing five red rows.
pub fn conditions(object: &Object) -> Vec<Condition> {
    object
        .array_at("status.conditions")
        .iter()
        .filter_map(|condition| {
            let kind = condition.get("type").and_then(Value::as_str)?.to_string();
            let status = condition
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("Unknown")
                .to_string();
            let reason = condition
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let good = match status.as_str() {
                "True" => !is_negative(&kind),
                "False" => is_negative(&kind),
                _ => {
                    return Some(Condition {
                        kind,
                        status,
                        reason,
                        level: Level::Unknown,
                    });
                }
            };
            let level = match good {
                true => Level::Ok,
                false => Level::Attention,
            };
            Some(Condition {
                kind,
                status,
                reason,
                level,
            })
        })
        .collect()
}

/// Whether a condition is one where `True` is the bad answer.
fn is_negative(kind: &str) -> bool {
    kind.ends_with("Pressure")
        || kind.ends_with("Unavailable")
        || kind.ends_with("Failed")
        || kind == "Failure"
}

fn push_text(facts: &mut Vec<Fact>, label: &str, value: &str) {
    if !value.is_empty() {
        facts.push(Fact::text(label, value));
    }
}

fn push_id(facts: &mut Vec<Fact>, label: &str, value: &str) {
    if !value.is_empty() {
        facts.push(Fact::id(label, value));
    }
}

/// A label or annotation map as `key=value` on one line each.
fn pairs(map: &std::collections::BTreeMap<String, String>) -> String {
    map.iter()
        .map(|(key, value)| match value.is_empty() {
            true => key.clone(),
            false => format!("{key}={value}"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn object(value: serde_json::Value) -> Object {
        Object::new(value).unwrap()
    }

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-07T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn facts(overview: &Overview) -> Vec<(&str, &str)> {
        overview
            .sections
            .iter()
            .flat_map(|section| section.facts.iter())
            .map(|fact| (fact.label.as_str(), fact.value.as_str()))
            .collect()
    }

    #[test]
    fn every_object_gets_the_metadata_block_whatever_kind_it_is() {
        let overview = overview(
            "Rollout",
            &object(
                json!({"metadata": {"name": "web", "namespace": "shop", "uid": "u",
                                        "creationTimestamp": "2026-09-07T09:00:00Z"}}),
            ),
            now(),
        );
        assert_eq!(overview.sections.len(), 1);
        assert!(overview.sections[0].title.is_none());
        let facts = facts(&overview);
        assert!(facts.contains(&("Name", "web")));
        assert!(facts.contains(&("Namespace", "shop")));
        assert!(facts.contains(&("Created", "3h")));
    }

    #[test]
    fn a_pod_says_where_it_is_and_what_each_container_is_doing() {
        let overview = overview(
            "Pod",
            &object(json!({
                "metadata": {"name": "api", "namespace": "shop"},
                "spec": {"nodeName": "node-1", "qosClass": "Burstable",
                         "containers": [
                             {"name": "api", "image": "ghcr.io/x/api:1.4"},
                             {"name": "proxy", "image": "envoy:1.31"}
                         ]},
                "status": {"podIP": "10.244.1.7", "qosClass": "Burstable",
                           "containerStatuses": [
                               {"name": "proxy", "restartCount": 0, "state": {"running": {}}},
                               {"name": "api", "restartCount": 4,
                                "state": {"waiting": {"reason": "CrashLoopBackOff"}}}
                           ]}
            })),
            now(),
        );
        let facts = facts(&overview);
        assert!(facts.contains(&("Node", "node-1")));
        assert!(facts.contains(&("Pod IP", "10.244.1.7")));
        // Read by name, not by index: the two lists are in different orders.
        let api = facts.iter().find(|(label, _)| *label == "api").unwrap();
        assert!(api.1.contains("CrashLoopBackOff"), "{}", api.1);
        assert!(api.1.contains("4 restarts"), "{}", api.1);
        let proxy = facts.iter().find(|(label, _)| *label == "proxy").unwrap();
        assert!(proxy.1.contains("running"), "{}", proxy.1);
    }

    #[test]
    fn a_cordoned_node_says_so_in_words() {
        let overview = overview(
            "Node",
            &object(json!({
                "metadata": {"name": "node-2"},
                "spec": {"unschedulable": true},
                "status": {"nodeInfo": {"kubeletVersion": "v1.31.2"},
                           "capacity": {"cpu": "4", "memory": "8Gi"},
                           "allocatable": {"cpu": "3800m", "memory": "7.5Gi"}}
            })),
            now(),
        );
        let facts = facts(&overview);
        assert!(facts.iter().any(|(_, value)| value.contains("cordoned")));
        assert!(facts.contains(&("CPU", "3800m of 4")));
    }

    #[test]
    fn a_secret_shows_its_keys_and_never_its_values() {
        let overview = overview(
            "Secret",
            &object(json!({
                "metadata": {"name": "db"},
                "type": "Opaque",
                "data": {"password": "aHVudGVyMg==", "username": "cm9vdA=="}
            })),
            now(),
        );
        let facts = facts(&overview);
        assert!(facts.contains(&("password", "")));
        assert!(facts.contains(&("username", "")));
        assert!(
            !facts.iter().any(|(_, value)| value.contains("aHVudGVyMg")),
            "a secret's value must not reach the panel"
        );
    }

    #[test]
    fn a_negative_condition_is_good_when_it_is_false() {
        let node = object(json!({"metadata": {"name": "n"}, "status": {"conditions": [
            {"type": "Ready", "status": "True"},
            {"type": "MemoryPressure", "status": "False"},
            {"type": "DiskPressure", "status": "True"},
            {"type": "NetworkUnavailable", "status": "Unknown"}
        ]}}));
        let conditions = conditions(&node);
        assert_eq!(conditions[0].level, Level::Ok);
        // False pressure is the healthy answer.
        assert_eq!(conditions[1].level, Level::Ok);
        assert_eq!(conditions[2].level, Level::Attention);
        assert_eq!(conditions[3].level, Level::Unknown);
    }

    #[test]
    fn a_condition_carries_the_controllers_reason_when_there_is_one() {
        let deployment = object(json!({"metadata": {"name": "d"}, "status": {"conditions": [
            {"type": "Available", "status": "False", "reason": "MinimumReplicasUnavailable"}
        ]}}));
        let conditions = conditions(&deployment);
        assert_eq!(conditions[0].reason, "MinimumReplicasUnavailable");
        assert_eq!(conditions[0].level, Level::Attention);
    }

    #[test]
    fn an_object_with_nothing_to_add_gets_no_empty_sections() {
        let overview = overview(
            "ConfigMap",
            &object(json!({"metadata": {"name": "c"}})),
            now(),
        );
        assert_eq!(overview.sections.len(), 1);
        assert!(overview.conditions.is_empty());
    }
}
