//! A cluster that is not there.
//!
//! Two uses, and both matter. Tests run against this rather than against a
//! live apiserver, so a test suite needs no cluster and asserts on data it
//! chose. And `KIRIKUMO_DEMO=1` opens the window over it, which is how the
//! layout is worked on, how a screenshot is taken, and how someone tries the
//! app before pointing it at anything real.
//!
//! The sample is deliberately unhealthy: a crash loop, a pending pod, a
//! cordoned node, a suspended cron job and a custom resource. A demo where
//! everything is green exercises none of the code worth looking at.
//!
//! It accepts writes. A delete removes, a patch merges (RFC 7386 — see
//! [`Cluster::patch`] on this type for what that means for a *strategic*
//! merge), and every write hands out a new `resourceVersion` the way the
//! apiserver does, so the whole M4 flow — the two gestures, the request, the
//! row changing under the reader — can be seen with no cluster to break.

use crate::actions::merge_patch;
use crate::error::{Error, Result};
use crate::logs::LogStream;
use crate::model::{
    ApiResource, Catalogue, ClusterVersion, EventRecord, LogRequest, Metrics, Object, ObjectList,
    Patch, ResourceKey,
};
use crate::watch::{WatchEvent, WatchStream};
use crate::{Cluster, discovery};
use chrono::{Duration, Utc};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration as Wait;

/// An in-memory cluster.
pub struct Scripted {
    version: ClusterVersion,
    catalogue: Catalogue,
    namespaces: Vec<String>,
    /// Behind a lock because the trait is `&self` and the scripted cluster
    /// accepts writes: a demo delete or scale is real, so the flow can be
    /// seen end to end without a cluster to break.
    objects: Mutex<HashMap<ResourceKey, Vec<Object>>>,
    /// The version the next write hands out. A live apiserver bumps
    /// `resourceVersion` on every write, and the table relies on that to
    /// know a row has to be drawn again.
    next_version: Mutex<u64>,
    /// Events by the uid of the object they are about.
    events: HashMap<String, Vec<EventRecord>>,
    node_metrics: Vec<Metrics>,
    pod_metrics: Vec<Metrics>,
    logs: String,
    /// How long the scripted watch waits between events. Real seconds in a
    /// demo, zero in a test.
    watch_delay: Wait,
}

impl Scripted {
    /// A cluster with nothing in it.
    ///
    /// What the window holds before a context is chosen, so that no view has
    /// to cope with the absence of a cluster as well as with an empty one.
    pub fn empty() -> Self {
        Self {
            version: ClusterVersion::default(),
            catalogue: Catalogue::default(),
            namespaces: Vec::new(),
            objects: Mutex::new(HashMap::new()),
            next_version: Mutex::new(1),
            events: HashMap::new(),
            node_metrics: Vec::new(),
            pod_metrics: Vec::new(),
            logs: String::new(),
            watch_delay: Wait::ZERO,
        }
    }

    /// How long the scripted watch waits between events.
    pub fn with_watch_delay(mut self, delay: Wait) -> Self {
        self.watch_delay = delay;
        self
    }

    /// A small cluster with something wrong in it.
    pub fn sample() -> Self {
        let now = Utc::now();
        let ago = |minutes: i64| (now - Duration::minutes(minutes)).to_rfc3339();

        let resources = vec![
            resource("", "v1", "Node", "nodes", false),
            resource("", "v1", "Namespace", "namespaces", false),
            resource("", "v1", "Event", "events", true),
            resource("", "v1", "Pod", "pods", true),
            resource("apps", "v1", "Deployment", "deployments", true),
            resource("apps", "v1", "ReplicaSet", "replicasets", true),
            resource("batch", "v1", "CronJob", "cronjobs", true),
            resource("", "v1", "ConfigMap", "configmaps", true),
            resource("", "v1", "Service", "services", true),
            resource(
                "",
                "v1",
                "PersistentVolumeClaim",
                "persistentvolumeclaims",
                true,
            ),
            resource("argoproj.io", "v1alpha1", "Rollout", "rollouts", true),
        ];
        let catalogue = discovery::catalogue(vec![resources]);

        let mut objects: HashMap<ResourceKey, Vec<Object>> = HashMap::new();

        objects.insert(
            ResourceKey::new("", "Node"),
            parse(vec![
                node("node-1", true, false, &ago(60 * 24 * 40)),
                node("node-2", true, true, &ago(60 * 24 * 40)),
                node("node-3", false, false, &ago(60 * 24 * 3)),
            ]),
        );

        objects.insert(
            ResourceKey::new("", "Namespace"),
            parse(
                ["default", "kube-system", "observability", "shop"]
                    .into_iter()
                    .map(|name| {
                        json!({
                            "apiVersion": "v1", "kind": "Namespace",
                            "metadata": {"name": name, "uid": format!("ns-{name}"),
                                         "creationTimestamp": ago(60 * 24 * 40)},
                            "status": {"phase": "Active"}
                        })
                    })
                    .collect(),
            ),
        );

        objects.insert(
            ResourceKey::new("", "Pod"),
            parse(vec![
                pod_running("api-7d9f8c-2xk4t", "shop", "node-1", 2, 2, &ago(180)),
                pod_running("api-7d9f8c-9qdlm", "shop", "node-2", 2, 2, &ago(180)),
                pod_running("web-6b4c5d-lm2zp", "shop", "node-1", 1, 2, &ago(12)),
                pod_waiting(
                    "jobrunner-xk4qq",
                    "shop",
                    "node-3",
                    "CrashLoopBackOff",
                    17,
                    &ago(400),
                ),
                pod_waiting(
                    "importer-9j2dd",
                    "shop",
                    "node-3",
                    "ImagePullBackOff",
                    0,
                    &ago(6),
                ),
                pod_succeeded("backup-28471-tqzn", "observability", &ago(90)),
                pod_running(
                    "prometheus-0",
                    "observability",
                    "node-2",
                    2,
                    2,
                    &ago(60 * 24 * 5),
                ),
                pod_running(
                    "grafana-5f7d9-bbn8k",
                    "observability",
                    "node-1",
                    1,
                    1,
                    &ago(60 * 24 * 5),
                ),
                pod_running(
                    "coredns-7db6d-4v2xn",
                    "kube-system",
                    "node-1",
                    1,
                    1,
                    &ago(60 * 24 * 40),
                ),
                pod_running(
                    "coredns-7db6d-t9mzq",
                    "kube-system",
                    "node-2",
                    1,
                    1,
                    &ago(60 * 24 * 40),
                ),
            ]),
        );

        objects.insert(
            ResourceKey::new("apps", "Deployment"),
            parse(vec![
                deployment("api", "shop", 2, 2, &ago(60 * 24 * 12)),
                deployment("web", "shop", 2, 1, &ago(60 * 24 * 12)),
                deployment("grafana", "observability", 1, 1, &ago(60 * 24 * 30)),
                deployment("importer", "shop", 1, 0, &ago(30)),
            ]),
        );

        objects.insert(
            ResourceKey::new("apps", "ReplicaSet"),
            parse(vec![json!({
                "apiVersion": "apps/v1", "kind": "ReplicaSet",
                "metadata": {"name": "api-7d9f8c", "namespace": "shop", "uid": "rs-api",
                             "creationTimestamp": ago(60 * 24 * 12),
                             "ownerReferences": [{"apiVersion": "apps/v1", "kind": "Deployment",
                                                  "name": "api", "uid": "dep-api",
                                                  "controller": true}]},
                "spec": {"replicas": 2},
                "status": {"replicas": 2, "readyReplicas": 2}
            })]),
        );

        objects.insert(
            ResourceKey::new("batch", "CronJob"),
            parse(vec![
                json!({
                    "apiVersion": "batch/v1", "kind": "CronJob",
                    "metadata": {"name": "backup", "namespace": "observability", "uid": "cj-backup",
                                 "creationTimestamp": ago(60 * 24 * 20)},
                    "spec": {"schedule": "0 2 * * *", "suspend": false},
                    "status": {"lastScheduleTime": ago(700)}
                }),
                json!({
                    "apiVersion": "batch/v1", "kind": "CronJob",
                    "metadata": {"name": "reindex", "namespace": "shop", "uid": "cj-reindex",
                                 "creationTimestamp": ago(60 * 24 * 20)},
                    "spec": {"schedule": "*/15 * * * *", "suspend": true},
                    "status": {}
                }),
            ]),
        );

        objects.insert(
            ResourceKey::new("", "Service"),
            parse(vec![
                json!({
                    "apiVersion": "v1", "kind": "Service",
                    "metadata": {"name": "api", "namespace": "shop", "uid": "svc-api",
                                 "creationTimestamp": ago(60 * 24 * 12)},
                    "spec": {"type": "ClusterIP", "clusterIP": "10.96.14.2",
                             "selector": {"app": "api"},
                             "ports": [{"port": 80, "targetPort": 8080, "protocol": "TCP"}]},
                    "status": {}
                }),
                json!({
                    "apiVersion": "v1", "kind": "Service",
                    "metadata": {"name": "web", "namespace": "shop", "uid": "svc-web",
                                 "creationTimestamp": ago(60 * 24 * 12)},
                    "spec": {"type": "LoadBalancer", "clusterIP": "10.96.14.9",
                             "selector": {"app": "web"},
                             "ports": [{"port": 443, "targetPort": 8443, "protocol": "TCP"}]},
                    "status": {}
                }),
            ]),
        );

        objects.insert(
            ResourceKey::new("", "ConfigMap"),
            parse(vec![json!({
                "apiVersion": "v1", "kind": "ConfigMap",
                "metadata": {"name": "api-config", "namespace": "shop", "uid": "cm-api",
                             "creationTimestamp": ago(60 * 24 * 12)},
                "data": {"LOG_LEVEL": "info", "TIMEOUT": "30s"}
            })]),
        );

        objects.insert(
            ResourceKey::new("", "PersistentVolumeClaim"),
            parse(vec![
                json!({
                    "apiVersion": "v1", "kind": "PersistentVolumeClaim",
                    "metadata": {"name": "prometheus-data", "namespace": "observability",
                                 "uid": "pvc-prom", "creationTimestamp": ago(60 * 24 * 30)},
                    "spec": {"storageClassName": "standard",
                             "resources": {"requests": {"storage": "50Gi"}}},
                    "status": {"phase": "Bound", "capacity": {"storage": "50Gi"}}
                }),
                json!({
                    "apiVersion": "v1", "kind": "PersistentVolumeClaim",
                    "metadata": {"name": "importer-scratch", "namespace": "shop",
                                 "uid": "pvc-scratch", "creationTimestamp": ago(30)},
                    "spec": {"storageClassName": "fast",
                             "resources": {"requests": {"storage": "10Gi"}}},
                    "status": {"phase": "Pending"}
                }),
            ]),
        );

        // A custom resource, to prove the sidebar and the table need no code
        // for one (`AGENTS.md` rule 8).
        objects.insert(
            ResourceKey::new("argoproj.io", "Rollout"),
            parse(vec![json!({
                "apiVersion": "argoproj.io/v1alpha1", "kind": "Rollout",
                "metadata": {"name": "web", "namespace": "shop", "uid": "ro-web",
                             "creationTimestamp": ago(60 * 24 * 4)},
                "spec": {"replicas": 2},
                "status": {"readyReplicas": 1,
                           "conditions": [{"type": "Available", "status": "True"}]}
            })]),
        );

        let mut events = HashMap::new();
        events.insert(
            "pod-jobrunner-xk4qq".to_string(),
            vec![
                EventRecord {
                    kind: "Warning".into(),
                    reason: "BackOff".into(),
                    message: "Back-off restarting failed container jobrunner".into(),
                    source: "kubelet".into(),
                    count: 42,
                    last: Some(now - Duration::minutes(2)),
                },
                EventRecord {
                    kind: "Normal".into(),
                    reason: "Pulled".into(),
                    message: "Container image \"ghcr.io/shop/jobrunner:2.1\" already present"
                        .into(),
                    source: "kubelet".into(),
                    count: 42,
                    last: Some(now - Duration::minutes(3)),
                },
            ],
        );
        events.insert(
            "pod-importer-9j2dd".to_string(),
            vec![EventRecord {
                kind: "Warning".into(),
                reason: "Failed".into(),
                message: "Failed to pull image \"ghcr.io/shop/importer:next\": not found".into(),
                source: "kubelet".into(),
                count: 5,
                last: Some(now - Duration::minutes(1)),
            }],
        );

        Self {
            version: ClusterVersion {
                major: "1".into(),
                minor: "31".into(),
                git_version: "v1.31.2".into(),
                platform: "linux/arm64".into(),
            },
            catalogue,
            namespaces: vec![
                "default".into(),
                "kube-system".into(),
                "observability".into(),
                "shop".into(),
            ],
            objects: Mutex::new(objects),
            next_version: Mutex::new(5000),
            events,
            node_metrics: vec![
                Metrics {
                    name: "node-1".into(),
                    namespace: None,
                    cpu_milli: 1420,
                    memory_bytes: 6_012_338_176,
                },
                Metrics {
                    name: "node-2".into(),
                    namespace: None,
                    cpu_milli: 890,
                    memory_bytes: 4_884_901_888,
                },
                Metrics {
                    name: "node-3".into(),
                    namespace: None,
                    cpu_milli: 120,
                    memory_bytes: 1_073_741_824,
                },
            ],
            pod_metrics: vec![
                Metrics {
                    name: "api-7d9f8c-2xk4t".into(),
                    namespace: Some("shop".into()),
                    cpu_milli: 143,
                    memory_bytes: 268_435_456,
                },
                Metrics {
                    name: "api-7d9f8c-9qdlm".into(),
                    namespace: Some("shop".into()),
                    cpu_milli: 138,
                    memory_bytes: 260_046_848,
                },
                Metrics {
                    name: "prometheus-0".into(),
                    namespace: Some("observability".into()),
                    cpu_milli: 612,
                    memory_bytes: 2_147_483_648,
                },
            ],
            logs: SAMPLE_LOG.to_string(),
            // Slow enough to watch happen, quick enough to see within a
            // minute of opening the window.
            watch_delay: Wait::from_secs(3),
        }
    }

    /// The objects of a kind, whatever namespace they are in.
    fn all(&self, key: &ResourceKey) -> Vec<Object> {
        self.objects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key)
            .cloned()
            .unwrap_or_default()
    }

    /// Where an object sits among its kind, or a not-found the way the
    /// apiserver would word it.
    fn position(
        held: &[Object],
        resource: &ApiResource,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<usize> {
        held.iter()
            .position(|object| {
                object.meta.name == name
                    && match namespace.filter(|_| resource.namespaced) {
                        Some(namespace) => object.meta.namespace.as_deref() == Some(namespace),
                        None => true,
                    }
            })
            .ok_or_else(|| Error::NotFound(format!("{} {name:?}", resource.kind)))
    }

    /// A fresh `resourceVersion`, as a write would be given one.
    fn bump(&self) -> String {
        let mut next = self
            .next_version
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *next += 1;
        next.to_string()
    }
}

impl Cluster for Scripted {
    fn version(&self) -> Result<ClusterVersion> {
        Ok(self.version.clone())
    }

    fn catalogue(&self) -> Result<Catalogue> {
        Ok(self.catalogue.clone())
    }

    fn namespaces(&self) -> Result<Vec<String>> {
        Ok(self.namespaces.clone())
    }

    fn list(&self, resource: &ApiResource, namespace: Option<&str>) -> Result<ObjectList> {
        let namespace = namespace.filter(|namespace| resource.namespaced && !namespace.is_empty());
        let items = self
            .all(&resource.key())
            .into_iter()
            .filter(|object| match namespace {
                Some(namespace) => object.meta.namespace.as_deref() == Some(namespace),
                None => true,
            })
            .collect();
        Ok(ObjectList {
            items,
            resource_version: "1".into(),
            next: None,
        })
    }

    fn get(&self, resource: &ApiResource, namespace: Option<&str>, name: &str) -> Result<Object> {
        let held = self.all(&resource.key());
        let index = Self::position(&held, resource, namespace, name)?;
        Ok(held[index].clone())
    }

    fn delete(&self, resource: &ApiResource, namespace: Option<&str>, name: &str) -> Result<()> {
        let mut objects = self
            .objects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let held = objects.entry(resource.key()).or_default();
        let index = Self::position(held, resource, namespace, name)?;
        held.remove(index);
        Ok(())
    }

    /// A patch, applied the way the apiserver would apply the patches this
    /// app sends.
    ///
    /// A merge patch is RFC 7386, exactly. A *strategic* merge patch is
    /// applied with the same rule, which is right for every patch this app
    /// makes — none of them touch a list — and wrong for one that does: a
    /// strategic merge of a containers list merges by name, and this would
    /// replace it. A JSON patch is refused, because this app never sends
    /// one and a fake that accepted it would be pretending.
    fn patch(
        &self,
        resource: &ApiResource,
        namespace: Option<&str>,
        name: &str,
        patch: Patch,
    ) -> Result<Object> {
        let version = self.bump();
        let mut objects = self
            .objects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let held = objects.entry(resource.key()).or_default();
        let index = Self::position(held, resource, namespace, name)?;
        let mut raw = held[index].raw.clone();
        match &patch {
            Patch::Merge(change) | Patch::Strategic(change) => merge_patch(&mut raw, change),
            Patch::Replace(whole) => raw = whole.clone(),
            Patch::Json(_) => return Err(Error::Unsupported),
        }
        raw["metadata"]["resourceVersion"] = json!(version);
        let updated = Object::new(raw)?;
        held[index] = updated.clone();
        Ok(updated)
    }

    fn events_for(&self, uid: &str, _namespace: Option<&str>) -> Result<Vec<EventRecord>> {
        Ok(self.events.get(uid).cloned().unwrap_or_default())
    }

    fn logs(&self, request: &LogRequest) -> Result<String> {
        match self.logs.is_empty() {
            true => Err(Error::Unsupported),
            false => Ok(format!(
                "# {}/{}\n{}",
                request.namespace, request.pod, self.logs
            )),
        }
    }

    fn follow_logs(&self, request: &LogRequest) -> Result<Box<dyn LogStream>> {
        if self.logs.is_empty() {
            return Err(Error::Unsupported);
        }
        Ok(Box::new(ScriptedLog {
            pod: request.pod.clone(),
            written: 0,
            delay: self.watch_delay,
        }))
    }

    fn node_metrics(&self) -> Result<Vec<Metrics>> {
        match self.node_metrics.is_empty() {
            true => Err(Error::Unsupported),
            false => Ok(self.node_metrics.clone()),
        }
    }

    fn watch(
        &self,
        resource: &ApiResource,
        namespace: Option<&str>,
        _from: &str,
    ) -> Result<Box<dyn WatchStream>> {
        if self.catalogue.get(&resource.key()).is_none() {
            return Err(Error::NotFound(resource.kind.clone()));
        }
        Ok(Box::new(ScriptedWatch::new(
            &resource.kind,
            namespace.map(str::to_string),
            self.watch_delay,
        )))
    }

    fn pod_metrics(&self, namespace: Option<&str>) -> Result<Vec<Metrics>> {
        if self.pod_metrics.is_empty() {
            return Err(Error::Unsupported);
        }
        Ok(self
            .pod_metrics
            .iter()
            .filter(|metrics| match namespace.filter(|name| !name.is_empty()) {
                Some(namespace) => metrics.namespace.as_deref() == Some(namespace),
                None => true,
            })
            .cloned()
            .collect())
    }
}

/// A resource for the sample catalogue: everything a viewer may do, nothing
/// it may not, because the scripted cluster refuses writes by inheriting the
/// trait's defaults.
fn resource(group: &str, version: &str, kind: &str, name: &str, namespaced: bool) -> ApiResource {
    ApiResource {
        group: group.into(),
        version: version.into(),
        kind: kind.into(),
        name: name.into(),
        singular: kind.to_lowercase(),
        namespaced,
        verbs: vec!["get".into(), "list".into(), "watch".into()],
        short_names: Vec::new(),
        categories: Vec::new(),
    }
}

fn parse(values: Vec<Value>) -> Vec<Object> {
    values
        .into_iter()
        .map(|value| Object::new(value).expect("the sample cluster is well formed"))
        .collect()
}

fn node(name: &str, ready: bool, cordoned: bool, created: &str) -> Value {
    json!({
        "apiVersion": "v1", "kind": "Node",
        "metadata": {"name": name, "uid": format!("node-{name}"),
                     "creationTimestamp": created,
                     "labels": {"kubernetes.io/os": "linux"}},
        "spec": {"unschedulable": cordoned},
        "status": {
            "conditions": [{"type": "Ready", "status": if ready { "True" } else { "False" }}],
            "capacity": {"cpu": "4", "memory": "8Gi", "pods": "110"},
            "allocatable": {"cpu": "3800m", "memory": "7.5Gi", "pods": "110"},
            "nodeInfo": {"kubeletVersion": "v1.31.2", "osImage": "Debian GNU/Linux 12",
                         "containerRuntimeVersion": "containerd://1.7.18"},
            "addresses": [{"type": "InternalIP", "address": "10.244.0.1"}]
        }
    })
}

fn pod_running(
    name: &str,
    namespace: &str,
    node: &str,
    ready: usize,
    total: usize,
    created: &str,
) -> Value {
    // A container short of ready is *running* and not waiting: its readiness
    // probe has not passed yet. That is the state a `Running 1/2` row is in,
    // and the one that has to read as attention rather than as an error.
    let statuses: Vec<Value> = (0..total)
        .map(|index| {
            json!({
                "name": format!("c{index}"),
                "ready": index < ready,
                "restartCount": 0,
                "state": {"running": {"startedAt": created}}
            })
        })
        .collect();
    json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": name, "namespace": namespace, "uid": format!("pod-{name}"),
                     "creationTimestamp": created,
                     "labels": {"app": name.split('-').next().unwrap_or(name)}},
        "spec": {"nodeName": node,
                 "containers": (0..total).map(|index| json!({
                     "name": format!("c{index}"),
                     "image": "ghcr.io/shop/service:1.4"
                 })).collect::<Vec<_>>()},
        "status": {"phase": "Running", "podIP": "10.244.1.7", "hostIP": "10.244.0.1",
                   "qosClass": "Burstable",
                   "containerStatuses": statuses,
                   "conditions": [{"type": "Ready",
                                   "status": if ready == total { "True" } else { "False" }}]}
    })
}

fn pod_waiting(
    name: &str,
    namespace: &str,
    node: &str,
    reason: &str,
    restarts: i64,
    created: &str,
) -> Value {
    json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": name, "namespace": namespace, "uid": format!("pod-{name}"),
                     "creationTimestamp": created,
                     "labels": {"app": name.split('-').next().unwrap_or(name)}},
        "spec": {"nodeName": node,
                 "containers": [{"name": "app", "image": "ghcr.io/shop/importer:next"}]},
        "status": {"phase": "Running", "podIP": "10.244.2.9", "hostIP": "10.244.0.3",
                   "qosClass": "BestEffort",
                   "containerStatuses": [{
                       "name": "app", "ready": false, "restartCount": restarts,
                       "state": {"waiting": {"reason": reason,
                                             "message": "back-off 5m0s restarting failed container"}}
                   }],
                   "conditions": [{"type": "Ready", "status": "False"}]}
    })
}

fn pod_succeeded(name: &str, namespace: &str, created: &str) -> Value {
    json!({
        "apiVersion": "v1", "kind": "Pod",
        "metadata": {"name": name, "namespace": namespace, "uid": format!("pod-{name}"),
                     "creationTimestamp": created},
        "spec": {"nodeName": "node-2",
                 "containers": [{"name": "backup", "image": "ghcr.io/ops/backup:3"}]},
        "status": {"phase": "Succeeded",
                   "containerStatuses": [{"name": "backup", "ready": false, "restartCount": 0,
                                          "state": {"terminated": {"exitCode": 0,
                                                                   "reason": "Completed"}}}]}
    })
}

fn deployment(name: &str, namespace: &str, replicas: i64, ready: i64, created: &str) -> Value {
    json!({
        "apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"name": name, "namespace": namespace, "uid": format!("dep-{name}"),
                     "creationTimestamp": created, "labels": {"app": name}},
        "spec": {"replicas": replicas,
                 "strategy": {"type": "RollingUpdate"},
                 "selector": {"matchLabels": {"app": name}},
                 "template": {"spec": {"containers": [
                     {"name": name, "image": format!("ghcr.io/shop/{name}:1.4")}
                 ]}}},
        "status": {"replicas": replicas, "readyReplicas": ready,
                   "updatedReplicas": replicas, "availableReplicas": ready,
                   "conditions": [{"type": "Available",
                                   "status": if ready >= replicas { "True" } else { "False" },
                                   "reason": "MinimumReplicasAvailable"}]}
    })
}

/// A watch over the scripted cluster.
///
/// Exists for two reasons. It is the only way to exercise the whole live path
/// — thread, channel, `watch::apply`, row reuse — without an apiserver. And
/// it makes `KIRIKUMO_DEMO=1` demonstrate what the app is *for*: a pod
/// restarting, one appearing, one going away, without anybody pressing
/// refresh.
///
/// After its script it sends a bookmark for ever. That is not filler: it is
/// what a real idle watch does, and it is what keeps the reading thread
/// parked in `next_event`, which is where a stop flag can reach it.
pub struct ScriptedWatch {
    /// Which kind is being followed; only pods have a script.
    kind: String,
    /// The namespace the table is scoped to, so an event lands in it.
    namespace: Option<String>,
    /// How far through the script we are.
    step: usize,
    /// How long to wait before each event.
    delay: Wait,
    /// The version handed out with the last event.
    version: u64,
}

impl ScriptedWatch {
    /// A watch on a kind.
    pub fn new(kind: &str, namespace: Option<String>, delay: Wait) -> Self {
        Self {
            kind: kind.to_string(),
            namespace,
            step: 0,
            delay,
            version: 1000,
        }
    }

    /// The namespace a scripted pod lives in: the table's, or the one the
    /// sample puts its interesting pods in.
    fn namespace(&self) -> String {
        self.namespace.clone().unwrap_or_else(|| "shop".to_string())
    }
}

impl WatchStream for ScriptedWatch {
    fn next_event(&mut self) -> Option<WatchEvent> {
        std::thread::sleep(self.delay);
        self.step += 1;
        self.version += 1;
        let version = self.version.to_string();
        let namespace = self.namespace();
        if self.kind != "Pod" {
            return Some(WatchEvent::Bookmark(version));
        }
        let now = Utc::now().to_rfc3339();
        let at = |mut value: Value| {
            value["metadata"]["resourceVersion"] = json!(version);
            Object::new(value).expect("the script is well formed")
        };
        match self.step {
            // The crash loop gets worse, which is what a person watching this
            // table is watching for.
            1 => Some(WatchEvent::Modified(at(pod_waiting(
                "jobrunner-xk4qq",
                &namespace,
                "node-3",
                "CrashLoopBackOff",
                18,
                &now,
            )))),
            // A new pod is scheduled…
            2 => Some(WatchEvent::Added(at(pod_waiting(
                "web-6b4c5d-nn7kq",
                &namespace,
                "node-1",
                "ContainerCreating",
                0,
                &now,
            )))),
            // …and comes up.
            3 => Some(WatchEvent::Modified(at(pod_running(
                "web-6b4c5d-nn7kq",
                &namespace,
                "node-1",
                2,
                2,
                &now,
            )))),
            // The one that could not pull its image is given up on.
            4 => Some(WatchEvent::Deleted(at(pod_waiting(
                "importer-9j2dd",
                &namespace,
                "node-3",
                "ImagePullBackOff",
                0,
                &now,
            )))),
            _ => Some(WatchEvent::Bookmark(version)),
        }
    }
}

/// A container that keeps talking.
///
/// The scripted log for `KIRIKUMO_DEMO=1`: the sample lines first, then one
/// more every few seconds for ever, so the Logs tab can be seen following
/// something with no cluster anywhere.
pub struct ScriptedLog {
    /// Which pod, so the lines name it.
    pod: String,
    /// How many lines have been written.
    written: usize,
    /// How long to wait before each one after the sample.
    delay: Wait,
}

impl LogStream for ScriptedLog {
    fn next_line(&mut self) -> Option<std::result::Result<String, Error>> {
        let sample: Vec<&str> = SAMPLE_LOG.lines().collect();
        if self.written < sample.len() {
            let line = sample[self.written].to_string();
            self.written += 1;
            return Some(Ok(line));
        }
        // Past the sample: one line at a time, at the pace of the demo's
        // watch, which is what makes *following* visible rather than just
        // asserted.
        std::thread::sleep(self.delay);
        self.written += 1;
        Some(Ok(format!(
            "{} INFO  GET /healthz 200 0.3ms pod={} line={}",
            Utc::now().to_rfc3339(),
            self.pod,
            self.written
        )))
    }
}

/// A few lines that look like something, for the Logs tab.
const SAMPLE_LOG: &str = "\
2026-09-07T09:14:02.118Z INFO  starting, version=1.4.0 commit=9f2c1ab
2026-09-07T09:14:02.204Z INFO  connected to postgres host=db.shop.svc pool=10
2026-09-07T09:14:02.331Z INFO  listening addr=0.0.0.0:8080
2026-09-07T09:14:19.882Z INFO  GET /healthz 200 0.4ms
2026-09-07T09:15:41.005Z WARN  slow query 1.82s SELECT * FROM orders WHERE status = $1
2026-09-07T09:15:41.006Z INFO  GET /orders 200 1841ms
2026-09-07T09:16:02.774Z ERROR upstream timeout service=inventory after=2s
2026-09-07T09:16:02.775Z INFO  GET /cart 503 2003ms
2026-09-07T09:16:33.410Z INFO  GET /healthz 200 0.3ms
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::{self, Level};

    fn resource_for(cluster: &Scripted, group: &str, kind: &str) -> ApiResource {
        cluster
            .catalogue()
            .unwrap()
            .get(&ResourceKey::new(group, kind))
            .cloned()
            .unwrap_or_else(|| panic!("the sample serves no {kind}"))
    }

    #[test]
    fn the_empty_cluster_answers_everything_with_nothing() {
        let cluster = Scripted::empty();
        assert!(cluster.catalogue().unwrap().resources.is_empty());
        assert!(cluster.namespaces().unwrap().is_empty());
        assert_eq!(cluster.version().unwrap().label(), "");
        assert!(cluster.node_metrics().is_err());
    }

    #[test]
    fn a_list_is_scoped_to_a_namespace_and_a_cluster_scoped_kind_ignores_one() {
        let cluster = Scripted::sample();
        let pods = resource_for(&cluster, "", "Pod");
        let all = cluster.list(&pods, None).unwrap();
        let shop = cluster.list(&pods, Some("shop")).unwrap();
        assert!(shop.items.len() < all.items.len());
        assert!(
            shop.items
                .iter()
                .all(|pod| pod.meta.namespace.as_deref() == Some("shop"))
        );

        let nodes = resource_for(&cluster, "", "Node");
        assert_eq!(cluster.list(&nodes, Some("shop")).unwrap().items.len(), 3);
    }

    #[test]
    fn the_sample_has_something_wrong_in_it_so_the_marks_are_exercised() {
        let cluster = Scripted::sample();
        let pods = resource_for(&cluster, "", "Pod");
        let levels: Vec<Level> = cluster
            .list(&pods, None)
            .unwrap()
            .items
            .iter()
            .map(|pod| health::of("Pod", pod).level)
            .collect();
        assert!(levels.contains(&Level::Ok));
        assert!(levels.contains(&Level::Attention));
        assert!(levels.contains(&Level::Error));
    }

    #[test]
    fn the_sample_serves_a_custom_resource_nobody_wrote_code_for() {
        let cluster = Scripted::sample();
        let rollouts = resource_for(&cluster, "argoproj.io", "Rollout");
        assert_eq!(discovery::group_of(&rollouts), crate::model::Group::Custom);
        assert_eq!(cluster.list(&rollouts, None).unwrap().items.len(), 1);
    }

    #[test]
    fn getting_something_that_is_not_there_says_so() {
        let cluster = Scripted::sample();
        let pods = resource_for(&cluster, "", "Pod");
        assert!(cluster.get(&pods, Some("shop"), "api-7d9f8c-2xk4t").is_ok());
        assert!(matches!(
            cluster.get(&pods, Some("shop"), "nope"),
            Err(Error::NotFound(_))
        ));
        // Right name, wrong namespace.
        assert!(
            cluster
                .get(&pods, Some("default"), "api-7d9f8c-2xk4t")
                .is_err()
        );
    }

    #[test]
    fn events_are_found_by_the_uid_of_what_they_are_about() {
        let cluster = Scripted::sample();
        let events = cluster
            .events_for("pod-jobrunner-xk4qq", Some("shop"))
            .unwrap();
        assert!(events.iter().any(EventRecord::is_warning));
        assert!(
            cluster
                .events_for("pod-api-7d9f8c-2xk4t", None)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_delete_is_real_and_the_list_no_longer_has_it() {
        let cluster = Scripted::sample();
        let pods = resource_for(&cluster, "", "Pod");
        let before = cluster.list(&pods, Some("shop")).unwrap().items.len();
        cluster
            .delete(&pods, Some("shop"), "api-7d9f8c-2xk4t")
            .unwrap();
        let after = cluster.list(&pods, Some("shop")).unwrap();
        assert_eq!(after.items.len(), before - 1);
        assert!(
            cluster
                .get(&pods, Some("shop"), "api-7d9f8c-2xk4t")
                .is_err()
        );
        // Deleting it again is a not-found, as the apiserver would say.
        assert!(matches!(
            cluster.delete(&pods, Some("shop"), "api-7d9f8c-2xk4t"),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn a_patch_merges_and_hands_out_a_new_version() {
        let cluster = Scripted::sample();
        let deployments = resource_for(&cluster, "apps", "Deployment");
        let before = cluster.get(&deployments, Some("shop"), "api").unwrap();
        let after = cluster
            .patch(&deployments, Some("shop"), "api", crate::actions::scale(5))
            .unwrap();
        assert_eq!(crate::actions::current_replicas(&after), 5);
        assert_ne!(after.meta.resource_version, before.meta.resource_version);
        // The rest of the object survived the merge.
        assert_eq!(after.str_at("spec.strategy.type"), "RollingUpdate");
        // And the change is what a later list sees.
        let listed = cluster.get(&deployments, Some("shop"), "api").unwrap();
        assert_eq!(crate::actions::current_replicas(&listed), 5);
    }

    #[test]
    fn a_replacement_replaces_and_a_json_patch_is_refused() {
        let cluster = Scripted::sample();
        let configmaps = resource_for(&cluster, "", "ConfigMap");
        let replaced = cluster
            .patch(
                &configmaps,
                Some("shop"),
                "api-config",
                crate::Patch::Replace(json!({
                    "apiVersion": "v1", "kind": "ConfigMap",
                    "metadata": {"name": "api-config", "namespace": "shop"},
                    "data": {"ONLY": "this"}
                })),
            )
            .unwrap();
        assert_eq!(replaced.str_at("data.ONLY"), "this");
        assert_eq!(replaced.str_at("data.LOG_LEVEL"), "");
        assert!(matches!(
            cluster.patch(
                &configmaps,
                Some("shop"),
                "api-config",
                crate::Patch::Json(json!([]))
            ),
            Err(Error::Unsupported)
        ));
    }

    #[test]
    fn the_scripted_watch_makes_the_sample_change_under_the_reader() {
        use crate::watch::{Applied, apply};
        let cluster = Scripted::sample().with_watch_delay(std::time::Duration::ZERO);
        let pods = resource_for(&cluster, "", "Pod");
        let mut list = cluster.list(&pods, Some("shop")).unwrap();
        let before = list.items.len();
        let mut stream = cluster
            .watch(&pods, Some("shop"), &list.resource_version)
            .unwrap();

        // The crash loop gets worse, in place.
        let restarted = stream.next_event().unwrap();
        assert!(matches!(apply(&mut list, restarted), Applied::Changed(_)));
        let jobrunner = list
            .items
            .iter()
            .find(|pod| pod.meta.name == "jobrunner-xk4qq")
            .unwrap();
        assert_eq!(health::restarts(jobrunner), 18);

        // One appears…
        assert!(matches!(
            apply(&mut list, stream.next_event().unwrap()),
            Applied::Added(_)
        ));
        assert_eq!(list.items.len(), before + 1);
        // …and comes up where it already was, rather than moving.
        assert!(matches!(
            apply(&mut list, stream.next_event().unwrap()),
            Applied::Changed(_)
        ));
        assert_eq!(
            health::of("Pod", list.items.last().unwrap()).level,
            Level::Ok
        );
        // One goes away.
        assert!(matches!(
            apply(&mut list, stream.next_event().unwrap()),
            Applied::Removed(_)
        ));
        assert_eq!(list.items.len(), before);
        assert!(
            !list
                .items
                .iter()
                .any(|pod| pod.meta.name == "importer-9j2dd")
        );
    }

    #[test]
    fn an_idle_scripted_watch_keeps_saying_where_to_resume() {
        let cluster = Scripted::sample().with_watch_delay(std::time::Duration::ZERO);
        let configmaps = resource_for(&cluster, "", "ConfigMap");
        let mut stream = cluster.watch(&configmaps, None, "1").unwrap();
        let mut versions = Vec::new();
        for _ in 0..3 {
            let event = stream.next_event().unwrap();
            assert!(matches!(event, WatchEvent::Bookmark(_)));
            versions.push(event.resource_version().unwrap().to_string());
        }
        // Each bookmark moves the version on, which is what makes a reconnect
        // after an idle hour free.
        assert!(versions[0] < versions[1] && versions[1] < versions[2]);
    }

    #[test]
    fn a_kind_the_cluster_does_not_serve_cannot_be_watched() {
        let cluster = Scripted::sample();
        let absent = resource("example.com", "v1", "Nothing", "nothings", true);
        assert!(matches!(
            cluster.watch(&absent, None, "1"),
            Err(Error::NotFound(_))
        ));
    }

    #[test]
    fn a_scripted_log_replays_the_sample_and_then_keeps_talking() {
        let cluster = Scripted::sample().with_watch_delay(std::time::Duration::ZERO);
        let mut stream = cluster
            .follow_logs(&LogRequest::new("shop", "api-7d9f8c-2xk4t"))
            .unwrap();
        let sample = SAMPLE_LOG.lines().count();
        for expected in SAMPLE_LOG.lines() {
            assert_eq!(stream.next_line().unwrap().unwrap(), expected);
        }
        // And then it does not end, which is the point of following.
        let next = stream.next_line().unwrap().unwrap();
        assert!(next.contains("api-7d9f8c-2xk4t"), "{next}");
        assert!(next.contains(&format!("line={}", sample + 1)), "{next}");
    }

    #[test]
    fn a_cluster_with_no_logs_cannot_be_followed() {
        let cluster = Scripted::empty();
        assert!(matches!(
            cluster.follow_logs(&LogRequest::new("a", "b")),
            Err(Error::Unsupported)
        ));
    }

    #[test]
    fn pod_metrics_are_scoped_the_way_a_list_is() {
        let cluster = Scripted::sample();
        let all = cluster.pod_metrics(None).unwrap();
        let shop = cluster.pod_metrics(Some("shop")).unwrap();
        assert!(shop.len() < all.len());
        assert!(shop.iter().all(|m| m.namespace.as_deref() == Some("shop")));
    }
}
