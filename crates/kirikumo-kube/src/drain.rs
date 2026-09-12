//! Draining a node: cordon it, then move everything off it that can move.
//!
//! The one write that is a loop with policy in it, which is why it is not a
//! sixth button in `actions` but a module of its own. `kubectl drain`'s
//! rules, kept where they can be tested against the scripted cluster:
//!
//! - **Cordon first.** Nothing new lands while the old is leaving.
//! - **Mirror pods stay.** The kubelet made them from files on the node and
//!   would make them again; evicting one is a no-op with a delay.
//! - **DaemonSet pods stay.** The DaemonSet would put them straight back,
//!   and a node with no CNI pod is a node nothing can leave.
//! - **Pods nothing controls are skipped and named**, not evicted. Evicting a
//!   bare pod is deleting it — nothing will make another — and `kubectl`
//!   refuses without `--force`. A button has no `--force`; it reports.
//! - **Everything else is evicted**, through the Eviction API rather than
//!   deleted, so a PodDisruptionBudget can say no. When one does the
//!   apiserver answers `429`, and the pod is tried again, a few times, with a
//!   pause — a budget that stays full is reported, not forced.
//!
//! The report says what happened to every pod. A drain that quietly left
//! things behind is worse than one that says it did.

use crate::Cluster;
use crate::actions;
use crate::error::{Error, Result};
use crate::model::{ApiResource, Object, ResourceKey};
use std::time::Duration;

/// How many times a pod a budget refuses is tried before it is given up on.
pub const ATTEMPTS: usize = 5;

/// How long to wait between those tries.
///
/// A budget clears when another replica comes ready somewhere else, which is
/// seconds, not milliseconds; five tries at this pace is half a minute.
pub const PAUSE: Duration = Duration::from_secs(6);

/// What became of one pod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Evicted.
    Evicted,
    /// Left where it is, on purpose.
    Skipped(Skip),
    /// Could not be moved.
    Failed(String),
}

/// Why a pod was left alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// The kubelet made it from a file; it will make it again.
    Mirror,
    /// A DaemonSet would put it straight back.
    DaemonSet,
    /// Nothing would make another, so evicting it is deleting it.
    Unmanaged,
    /// A disruption budget kept saying no.
    Budget,
}

/// What a drain did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Every pod that was on the node, as `namespace/name`, and its outcome.
    pub pods: Vec<(String, Outcome)>,
}

impl Report {
    /// How many were moved.
    pub fn evicted(&self) -> usize {
        self.count(|outcome| matches!(outcome, Outcome::Evicted))
    }

    /// How many were left alone on purpose.
    pub fn skipped(&self) -> usize {
        self.count(|outcome| matches!(outcome, Outcome::Skipped(_)))
    }

    /// How many could not be moved.
    pub fn failed(&self) -> usize {
        self.count(|outcome| matches!(outcome, Outcome::Failed(_)))
    }

    /// Whether everything that could move did.
    pub fn is_clean(&self) -> bool {
        self.failed() == 0
            && !self
                .pods
                .iter()
                .any(|(_, outcome)| matches!(outcome, Outcome::Skipped(Skip::Budget)))
    }

    fn count(&self, matching: impl Fn(&Outcome) -> bool) -> usize {
        self.pods
            .iter()
            .filter(|(_, outcome)| matching(outcome))
            .count()
    }

    /// One line, for the footer: `3 evicted · 2 skipped · 1 failed`.
    pub fn summary(&self) -> String {
        let mut parts = vec![format!("{} evicted", self.evicted())];
        if self.skipped() > 0 {
            parts.push(format!("{} skipped", self.skipped()));
        }
        if self.failed() > 0 {
            parts.push(format!("{} failed", self.failed()));
        }
        parts.join(" · ")
    }
}

/// Why a pod is left where it is, if it is.
///
/// The policy, on its own, so the test can put a pod in front of it and ask.
pub fn skip_reason(pod: &Object) -> Option<Skip> {
    if pod
        .meta
        .annotations
        .contains_key("kubernetes.io/config.mirror")
    {
        return Some(Skip::Mirror);
    }
    match pod.meta.controller() {
        Some(owner) if owner.kind == "DaemonSet" => Some(Skip::DaemonSet),
        Some(_) => None,
        None => Some(Skip::Unmanaged),
    }
}

/// Drain a node.
///
/// Blocking, and slow when a budget resists — up to [`ATTEMPTS`] times
/// [`PAUSE`] per resisting pod — so it belongs on the background executor
/// like every other call into the trait. `pods` is the pod resource from the
/// catalogue, because listing needs to know where pods live; `nodes` is the
/// node resource, for the cordon.
pub fn drain(
    cluster: &dyn Cluster,
    nodes: &ApiResource,
    pods: &ApiResource,
    node: &str,
    mut pause: impl FnMut(Duration),
) -> Result<Report> {
    // Cordon first, so nothing new lands while the old is leaving. A node
    // that will not take the patch is a drain that has not started.
    cluster.patch(nodes, None, node, actions::schedulable(true))?;

    let on_node: Vec<Object> = cluster
        .list(pods, None)?
        .items
        .into_iter()
        .filter(|pod| pod.str_at("spec.nodeName") == node)
        .collect();

    let mut report = Report::default();
    for pod in on_node {
        let namespace = pod.meta.namespace.clone().unwrap_or_default();
        let label = format!("{namespace}/{}", pod.meta.name);
        if let Some(skip) = skip_reason(&pod) {
            report.pods.push((label, Outcome::Skipped(skip)));
            continue;
        }
        let outcome = evict_with_patience(cluster, &namespace, &pod.meta.name, &mut pause);
        report.pods.push((label, outcome));
    }
    Ok(report)
}

/// Evict one pod, waiting out a budget a few times before giving up.
fn evict_with_patience(
    cluster: &dyn Cluster,
    namespace: &str,
    name: &str,
    pause: &mut impl FnMut(Duration),
) -> Outcome {
    for attempt in 1..=ATTEMPTS {
        match cluster.evict(namespace, name) {
            Ok(()) => return Outcome::Evicted,
            // A budget said no: the apiserver's `429`. Wait for a replica to
            // come ready elsewhere, then ask again.
            Err(Error::Api { status: 429, .. }) if attempt < ATTEMPTS => pause(PAUSE),
            Err(Error::Api { status: 429, .. }) => return Outcome::Skipped(Skip::Budget),
            // Already gone: that is the outcome we wanted.
            Err(Error::NotFound(_)) => return Outcome::Evicted,
            Err(error) => return Outcome::Failed(error.to_string()),
        }
    }
    Outcome::Skipped(Skip::Budget)
}

/// The resource key the drain needs in the catalogue besides nodes.
pub fn pods_key() -> ResourceKey {
    ResourceKey::new("", "Pod")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scripted::Scripted;
    use serde_json::json;

    fn pod(value: serde_json::Value) -> Object {
        Object::new(value).unwrap()
    }

    #[test]
    fn a_mirror_pod_is_the_kubelets_and_stays() {
        let mirror = pod(
            json!({"metadata": {"name": "etcd-node-1", "namespace": "kube-system",
            "annotations": {"kubernetes.io/config.mirror": "abc"},
            "ownerReferences": [{"apiVersion": "v1", "kind": "Node", "name": "node-1",
                                 "controller": true}]}}),
        );
        assert_eq!(skip_reason(&mirror), Some(Skip::Mirror));
    }

    #[test]
    fn a_daemonset_pod_would_come_straight_back_and_stays() {
        let ds = pod(
            json!({"metadata": {"name": "cni-x", "namespace": "kube-system",
            "ownerReferences": [{"apiVersion": "apps/v1", "kind": "DaemonSet", "name": "cni",
                                 "controller": true}]}}),
        );
        assert_eq!(skip_reason(&ds), Some(Skip::DaemonSet));
    }

    #[test]
    fn a_pod_nothing_controls_is_named_not_evicted() {
        let bare = pod(json!({"metadata": {"name": "debug", "namespace": "default"}}));
        assert_eq!(skip_reason(&bare), Some(Skip::Unmanaged));
    }

    #[test]
    fn a_replicasets_pod_moves() {
        let managed = pod(json!({"metadata": {"name": "api-x", "namespace": "shop",
            "ownerReferences": [{"apiVersion": "apps/v1", "kind": "ReplicaSet", "name": "api",
                                 "controller": true}]}}));
        assert_eq!(skip_reason(&managed), None);
    }

    fn resources(cluster: &Scripted) -> (ApiResource, ApiResource) {
        let catalogue = cluster.catalogue().unwrap();
        (
            catalogue
                .get(&ResourceKey::new("", "Node"))
                .unwrap()
                .clone(),
            catalogue.get(&pods_key()).unwrap().clone(),
        )
    }

    #[test]
    fn a_drain_cordons_first_and_moves_what_can_move() {
        let cluster = Scripted::sample();
        let (nodes, pods) = resources(&cluster);
        let before = cluster.list(&pods, None).unwrap();
        let on_node_1 = before
            .items
            .iter()
            .filter(|pod| pod.str_at("spec.nodeName") == "node-1")
            .count();
        assert!(on_node_1 > 0, "the sample must put something on node-1");

        let mut paused = 0;
        let report = drain(&cluster, &nodes, &pods, "node-1", |_| paused += 1).unwrap();

        // Cordoned.
        let node = cluster.get(&nodes, None, "node-1").unwrap();
        assert!(node.bool_at("spec.unschedulable"));
        // Every pod on the node is accounted for.
        assert_eq!(report.pods.len(), on_node_1);
        // The sample's pods are all ReplicaSet-owned or bare; nothing failed.
        assert_eq!(report.failed(), 0);
        assert_eq!(paused, 0, "nothing in the sample has a budget");
        // What was evicted is gone.
        let after = cluster.list(&pods, None).unwrap();
        let evicted: Vec<&str> = report
            .pods
            .iter()
            .filter(|(_, outcome)| *outcome == Outcome::Evicted)
            .map(|(label, _)| label.rsplit('/').next().unwrap())
            .collect();
        for name in &evicted {
            assert!(
                !after.items.iter().any(|pod| pod.meta.name == *name),
                "{name} should have been evicted"
            );
        }
        // And what was skipped is still there.
        let skipped: Vec<&str> = report
            .pods
            .iter()
            .filter(|(_, outcome)| matches!(outcome, Outcome::Skipped(_)))
            .map(|(label, _)| label.rsplit('/').next().unwrap())
            .collect();
        for name in &skipped {
            assert!(after.items.iter().any(|pod| pod.meta.name == *name));
        }
    }

    #[test]
    fn a_budget_that_keeps_saying_no_is_reported_not_forced() {
        let cluster = Scripted::sample().with_protected_pod("shop", "api-7d9f8c-2xk4t");
        let (nodes, pods) = resources(&cluster);
        let mut paused = 0;
        let report = drain(&cluster, &nodes, &pods, "node-1", |_| paused += 1).unwrap();
        let protected = report
            .pods
            .iter()
            .find(|(label, _)| label == "shop/api-7d9f8c-2xk4t")
            .unwrap();
        assert_eq!(protected.1, Outcome::Skipped(Skip::Budget));
        assert_eq!(paused, ATTEMPTS - 1);
        assert!(!report.is_clean());
        assert!(report.summary().contains("skipped"), "{}", report.summary());
        // And it is still there.
        assert!(cluster.get(&pods, Some("shop"), "api-7d9f8c-2xk4t").is_ok());
    }

    #[test]
    fn a_node_that_will_not_cordon_is_a_drain_that_never_started() {
        let cluster = Scripted::sample();
        let (nodes, pods) = resources(&cluster);
        let before = cluster.list(&pods, None).unwrap().items.len();
        assert!(matches!(
            drain(&cluster, &nodes, &pods, "no-such-node", |_| {}),
            Err(Error::NotFound(_))
        ));
        assert_eq!(cluster.list(&pods, None).unwrap().items.len(), before);
    }

    #[test]
    fn the_summary_is_one_line_and_says_only_what_happened() {
        let mut report = Report::default();
        report.pods.push(("a/b".into(), Outcome::Evicted));
        assert_eq!(report.summary(), "1 evicted");
        report
            .pods
            .push(("a/c".into(), Outcome::Skipped(Skip::DaemonSet)));
        report
            .pods
            .push(("a/d".into(), Outcome::Failed("no".into())));
        assert_eq!(report.summary(), "1 evicted · 1 skipped · 1 failed");
        assert!(!report.is_clean());
    }
}
