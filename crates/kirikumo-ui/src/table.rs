//! The centre column's columns.
//!
//! One table draws every kind (`AGENTS.md` rule 8), so the per-kind knowledge
//! is *data*: a list of columns, each paired with the thing to read out of
//! the object. Titles and cells come from the same list, which is what makes
//! it impossible for them to drift apart — the failure mode of every table
//! that keeps its headers in one place and its rows in another.
//!
//! The column sets are `kubectl get`'s, in `kubectl`'s order, because the
//! value of a column called `READY` showing `2/2` is that the reader has
//! already learnt it somewhere else. A kind with no set of its own gets Name,
//! Namespace and Age — which is what `kubectl` prints for a custom resource
//! too.

use chrono::{DateTime, Utc};
use kirikumo_kube::{ApiResource, Health, Level, Object, health, quantity};
use serde_json::Value;

/// How a column takes its share of the table's width.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Width {
    /// A share of what is left after the fixed columns.
    Flex(f32),
    /// Exactly this many pixels: the narrow columns whose contents are one
    /// or two characters and which must not stretch.
    Fixed(f32),
}

/// One column.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    /// The heading, in `kubectl`'s spelling.
    pub title: &'static str,
    /// How wide.
    pub width: Width,
    /// Whether the cells are identifiers, which are drawn in the mono family
    /// so a column of them lines up (`docs/ui.md` §2).
    pub mono: bool,
    /// Whether the cells are numbers, which are drawn right-aligned.
    pub numeric: bool,
}

impl Column {
    const fn text(title: &'static str, flex: f32) -> Self {
        Self {
            title,
            width: Width::Flex(flex),
            mono: false,
            numeric: false,
        }
    }

    const fn id(title: &'static str, flex: f32) -> Self {
        Self {
            title,
            width: Width::Flex(flex),
            mono: true,
            numeric: false,
        }
    }

    const fn num(title: &'static str, pixels: f32) -> Self {
        Self {
            title,
            width: Width::Fixed(pixels),
            mono: true,
            numeric: true,
        }
    }

    const fn fixed(title: &'static str, pixels: f32) -> Self {
        Self {
            title,
            width: Width::Fixed(pixels),
            mono: true,
            numeric: false,
        }
    }
}

/// What to read out of an object for one column.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Cell {
    /// `metadata.name`.
    Name,
    /// `metadata.namespace`.
    Namespace,
    /// How long ago it was created, `kubectl`'s way.
    Age,
    /// The word beside the health mark.
    Status,
    /// A pod's ready containers over its total.
    Ready,
    /// A pod's restarts, summed.
    Restarts,
    /// A string at a dotted path.
    Str(&'static str),
    /// A number at a dotted path, printed as `0` when absent — which is what
    /// the apiserver means by omitting it.
    Int(&'static str),
    /// How many entries an object or array at a path has.
    Count(&'static str),
    /// A node's roles, from its `node-role.kubernetes.io/*` labels.
    NodeRoles,
    /// A service's ports, as `80:30001/TCP`.
    ServicePorts,
    /// A service's external address, or `<none>`/`<pending>`.
    ServiceExternalIp,
    /// An ingress's hosts, comma-separated.
    IngressHosts,
    /// The images a pod or a controller's template runs.
    Images,
    /// A job's completions, as `1/1`.
    JobCompletions,
    /// A cron job's `spec.suspend`, as `True`/`False`, which is how
    /// `kubectl` prints it.
    Suspend,
    /// A timestamp at a path, as an age.
    Since(&'static str),
    /// A quantity at a path, printed in binary units.
    Bytes(&'static str),
    /// An autoscaler's target, as `Deployment/api`.
    ScaleTarget,
    /// A binding's role, as `ClusterRole/view`.
    RoleRef,
    /// An endpoints object's addresses.
    Endpoints,
}

/// The columns for one kind, and what fills them.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnSet {
    kind: String,
    columns: Vec<(Column, Cell)>,
    /// Which column the table sorts by when nothing has been clicked.
    default_sort: usize,
    /// Whether that default is ascending.
    default_ascending: bool,
}

/// What `kubectl` prints where a field is empty.
const NONE: &str = "<none>";

impl ColumnSet {
    /// The columns for a kind.
    ///
    /// `show_namespace` is true when the table is listing every namespace at
    /// once, which is the only time a NAMESPACE column earns its width.
    pub fn for_kind(kind: &str, namespaced: bool, show_namespace: bool) -> Self {
        let mut columns: Vec<(Column, Cell)> = vec![(Column::id("NAME", 3.0), Cell::Name)];
        if namespaced && show_namespace {
            columns.push((Column::id("NAMESPACE", 1.4), Cell::Namespace));
        }
        // Age descending for the kinds that churn — the newest pod is the one
        // that just crashed — and name ascending for the ones that do not.
        let mut sort_by_age = false;
        match kind {
            "Pod" => {
                columns.extend([
                    (Column::num("READY", 62.), Cell::Ready),
                    (Column::text("STATUS", 1.3), Cell::Status),
                    (Column::num("RESTARTS", 76.), Cell::Restarts),
                    (Column::id("IP", 1.0), Cell::Str("status.podIP")),
                    (Column::id("NODE", 1.2), Cell::Str("spec.nodeName")),
                ]);
                sort_by_age = true;
            }
            "Deployment" | "StatefulSet" => columns.extend([
                (Column::num("READY", 62.), Cell::Status),
                (
                    Column::num("UP-TO-DATE", 92.),
                    Cell::Int("status.updatedReplicas"),
                ),
                (
                    Column::num("AVAILABLE", 82.),
                    Cell::Int("status.availableReplicas"),
                ),
                (Column::id("IMAGES", 2.0), Cell::Images),
            ]),
            "ReplicaSet" => columns.extend([
                (Column::num("DESIRED", 72.), Cell::Int("spec.replicas")),
                (Column::num("CURRENT", 72.), Cell::Int("status.replicas")),
                (Column::num("READY", 62.), Cell::Int("status.readyReplicas")),
            ]),
            "DaemonSet" => columns.extend([
                (
                    Column::num("DESIRED", 72.),
                    Cell::Int("status.desiredNumberScheduled"),
                ),
                (Column::num("READY", 62.), Cell::Int("status.numberReady")),
                (
                    Column::num("UP-TO-DATE", 92.),
                    Cell::Int("status.updatedNumberScheduled"),
                ),
                (
                    Column::num("AVAILABLE", 82.),
                    Cell::Int("status.numberAvailable"),
                ),
            ]),
            "Job" => {
                columns.extend([
                    (Column::num("COMPLETIONS", 100.), Cell::JobCompletions),
                    (Column::text("STATUS", 1.0), Cell::Status),
                ]);
                sort_by_age = true;
            }
            "CronJob" => columns.extend([
                (Column::id("SCHEDULE", 1.2), Cell::Str("spec.schedule")),
                (Column::fixed("SUSPEND", 74.), Cell::Suspend),
                (Column::num("ACTIVE", 62.), Cell::Count("status.active")),
                (
                    Column::fixed("LAST SCHEDULE", 104.),
                    Cell::Since("status.lastScheduleTime"),
                ),
            ]),
            "Node" => columns.extend([
                (Column::text("STATUS", 1.2), Cell::Status),
                (Column::text("ROLES", 1.0), Cell::NodeRoles),
                (
                    Column::id("VERSION", 1.0),
                    Cell::Str("status.nodeInfo.kubeletVersion"),
                ),
            ]),
            "Namespace" => columns.push((Column::text("STATUS", 1.0), Cell::Status)),
            "Service" => columns.extend([
                (Column::text("TYPE", 1.0), Cell::Str("spec.type")),
                (Column::id("CLUSTER-IP", 1.1), Cell::Str("spec.clusterIP")),
                (Column::id("EXTERNAL-IP", 1.1), Cell::ServiceExternalIp),
                (Column::id("PORTS", 1.3), Cell::ServicePorts),
            ]),
            "Endpoints" => columns.push((Column::id("ENDPOINTS", 3.0), Cell::Endpoints)),
            "Ingress" => columns.extend([
                (
                    Column::text("CLASS", 0.9),
                    Cell::Str("spec.ingressClassName"),
                ),
                (Column::id("HOSTS", 2.0), Cell::IngressHosts),
            ]),
            "ConfigMap" => columns.push((Column::num("DATA", 62.), Cell::Count("data"))),
            "Secret" => columns.extend([
                (Column::text("TYPE", 1.4), Cell::Str("type")),
                (Column::num("DATA", 62.), Cell::Count("data")),
            ]),
            "PersistentVolumeClaim" => columns.extend([
                (Column::text("STATUS", 0.9), Cell::Status),
                (Column::id("VOLUME", 1.6), Cell::Str("spec.volumeName")),
                (
                    Column::num("CAPACITY", 82.),
                    Cell::Bytes("status.capacity.storage"),
                ),
                (
                    Column::text("STORAGECLASS", 1.1),
                    Cell::Str("spec.storageClassName"),
                ),
            ]),
            "PersistentVolume" => columns.extend([
                (
                    Column::num("CAPACITY", 82.),
                    Cell::Bytes("spec.capacity.storage"),
                ),
                (Column::text("STATUS", 0.9), Cell::Status),
                (Column::id("CLAIM", 1.6), Cell::Str("spec.claimRef.name")),
                (
                    Column::text("STORAGECLASS", 1.1),
                    Cell::Str("spec.storageClassName"),
                ),
            ]),
            "StorageClass" => {
                columns.push((Column::id("PROVISIONER", 2.0), Cell::Str("provisioner")))
            }
            "ServiceAccount" => columns.push((Column::num("SECRETS", 72.), Cell::Count("secrets"))),
            "HorizontalPodAutoscaler" => columns.extend([
                (Column::id("REFERENCE", 1.6), Cell::ScaleTarget),
                (Column::num("MINPODS", 76.), Cell::Int("spec.minReplicas")),
                (Column::num("MAXPODS", 76.), Cell::Int("spec.maxReplicas")),
                (
                    Column::num("REPLICAS", 82.),
                    Cell::Int("status.currentReplicas"),
                ),
            ]),
            "RoleBinding" | "ClusterRoleBinding" => {
                columns.push((Column::id("ROLE", 1.8), Cell::RoleRef))
            }
            "Event" => {
                columns.extend([
                    (Column::text("TYPE", 0.7), Cell::Str("type")),
                    (Column::id("REASON", 1.0), Cell::Str("reason")),
                    (Column::id("OBJECT", 1.4), Cell::Str("involvedObject.name")),
                    (Column::text("MESSAGE", 3.0), Cell::Str("message")),
                ]);
                sort_by_age = true;
            }
            _ => {}
        }
        columns.push((Column::fixed("AGE", 74.), Cell::Age));
        let age = columns.len() - 1;
        Self {
            kind: kind.to_string(),
            columns,
            default_sort: if sort_by_age { age } else { 0 },
            // Ascending either way, and it means the useful thing in both:
            // A→Z by name, and *youngest first* by age, because a smaller
            // age is a newer object and the newest pod is the one that just
            // crashed.
            default_ascending: true,
        }
    }

    /// The columns for a resource, given whether the table is showing every
    /// namespace.
    pub fn for_resource(resource: &ApiResource, show_namespace: bool) -> Self {
        Self::for_kind(&resource.kind, resource.namespaced, show_namespace)
    }

    /// The kind these columns are for.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// The headings, in order.
    pub fn columns(&self) -> impl Iterator<Item = &Column> {
        self.columns.iter().map(|(column, _)| column)
    }

    /// How many columns there are.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Whether there are no columns, which cannot happen — NAME and AGE are
    /// always there — but which clippy asks about.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }

    /// The column and direction the table sorts by until the reader says
    /// otherwise.
    pub fn default_sort(&self) -> (usize, bool) {
        (self.default_sort, self.default_ascending)
    }

    /// Whether a column holds an age, which sorts by the underlying
    /// timestamp rather than by the string: `9m59s` is younger than `10m`,
    /// and no string comparison will ever say so.
    pub fn is_age(&self, column: usize) -> bool {
        matches!(self.columns.get(column), Some((_, Cell::Age)))
    }

    /// One object's cells, in column order.
    pub fn cells(&self, object: &Object, now: DateTime<Utc>) -> Vec<String> {
        self.columns
            .iter()
            .map(|(_, cell)| render(*cell, &self.kind, object, now))
            .collect()
    }

    /// One row, ready to draw.
    pub fn row(&self, object: &Object, now: DateTime<Utc>) -> Row {
        let cells = self.cells(object, now);
        // What the filter box matches against: every cell, plus the labels,
        // because "app=api" is a thing people type.
        let mut haystack = cells.join(" ");
        for (name, value) in &object.meta.labels {
            haystack.push(' ');
            haystack.push_str(name);
            haystack.push('=');
            haystack.push_str(value);
        }
        Row {
            key: row_key(object),
            name: object.meta.name.clone(),
            namespace: object.meta.namespace.clone(),
            health: health::of(&self.kind, object),
            created: object.meta.created,
            cells,
            haystack: haystack.to_lowercase(),
        }
    }
}

/// The identity of a row.
///
/// The uid, because a deleted-and-recreated object with the same name is a
/// different object and must not inherit the old one's selection. Falling
/// back to `namespace/name` covers the answers that omit the uid, which some
/// aggregated apiservers do.
pub fn row_key(object: &Object) -> String {
    if !object.meta.uid.is_empty() {
        return object.meta.uid.clone();
    }
    match &object.meta.namespace {
        Some(namespace) => format!("{namespace}/{}", object.meta.name),
        None => object.meta.name.clone(),
    }
}

/// One object as the table draws it.
///
/// Everything is a string by the time it gets here: a cell is formatted once,
/// when the object lands, and never during a scroll (`AGENTS.md` rule 7).
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// The object's identity.
    pub key: String,
    /// Its name, for the detail panel.
    pub name: String,
    /// Its namespace, for the detail panel.
    pub namespace: Option<String>,
    /// The mark and the word.
    pub health: Health,
    /// The cells, one per column.
    pub cells: Vec<String>,
    /// When it was created, which is what an AGE column sorts by.
    pub created: Option<DateTime<Utc>>,
    /// Every cell and label, lowercased, for the filter box.
    pub haystack: String,
}

impl Row {
    /// Whether this row is one the reader should look at.
    pub fn is_bad(&self) -> bool {
        self.health.level.is_bad()
    }
}

/// Sort rows by a column.
///
/// Ages sort by their timestamp, numbers by their value, and everything else
/// by its text, case-insensitively. A cell that is `<none>` sorts last
/// whichever way the column is pointing, because an absence is not a value.
pub fn sort(rows: &mut [Row], columns: &ColumnSet, column: usize, ascending: bool) {
    if columns.is_age(column) {
        // A newer object has a larger timestamp and a *smaller* age, so
        // "ascending age" is descending time.
        rows.sort_by(|a, b| match ascending {
            true => b.created.cmp(&a.created),
            false => a.created.cmp(&b.created),
        });
        return;
    }
    rows.sort_by(|a, b| {
        let left = a.cells.get(column).map(String::as_str).unwrap_or_default();
        let right = b.cells.get(column).map(String::as_str).unwrap_or_default();
        let order = match (numeric(left), numeric(right)) {
            (Some(left), Some(right)) => left.total_cmp(&right),
            _ => left.to_lowercase().cmp(&right.to_lowercase()),
        };
        match ascending {
            true => order,
            false => order.reverse(),
        }
    });
}

/// A cell that is only a number, for sorting.
fn numeric(cell: &str) -> Option<f64> {
    cell.parse().ok()
}

/// Read one cell out of an object.
fn render(cell: Cell, kind: &str, object: &Object, now: DateTime<Utc>) -> String {
    match cell {
        Cell::Name => object.meta.name.clone(),
        Cell::Namespace => object.meta.namespace.clone().unwrap_or_default(),
        Cell::Age => crate::time::age(object.meta.created, now),
        Cell::Status => health::of(kind, object).word,
        Cell::Ready => {
            let (ready, total) = health::ready_containers(object);
            format!("{ready}/{total}")
        }
        Cell::Restarts => health::restarts(object).to_string(),
        Cell::Str(path) => or_none(object.str_at(path)),
        Cell::Int(path) => object.int_at(path).to_string(),
        Cell::Count(path) => match object.at(path) {
            Some(Value::Object(map)) => map.len().to_string(),
            Some(Value::Array(items)) => items.len().to_string(),
            _ => "0".to_string(),
        },
        Cell::NodeRoles => node_roles(object),
        Cell::ServicePorts => service_ports(object),
        Cell::ServiceExternalIp => service_external_ip(object),
        Cell::IngressHosts => ingress_hosts(object),
        Cell::Images => images(object),
        Cell::JobCompletions => {
            let completions = object
                .at("spec.completions")
                .and_then(Value::as_i64)
                .unwrap_or(1);
            format!("{}/{completions}", object.int_at("status.succeeded"))
        }
        // `kubectl` prints a boolean as `True`/`False` here, not `true`.
        Cell::Suspend => match object.bool_at("spec.suspend") {
            true => "True".to_string(),
            false => "False".to_string(),
        },
        Cell::Since(path) => match object
            .at(path)
            .and_then(Value::as_str)
            .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        {
            Some(time) => crate::time::age(Some(time.with_timezone(&Utc)), now),
            None => NONE.to_string(),
        },
        Cell::Bytes(path) => match object.str_at(path) {
            // The quantity is shown as written — `50Gi`, not `53687091200` —
            // because that is the number in the manifest.
            "" => NONE.to_string(),
            quantity => quantity.to_string(),
        },
        Cell::ScaleTarget => {
            let kind = object.str_at("spec.scaleTargetRef.kind");
            let name = object.str_at("spec.scaleTargetRef.name");
            match kind.is_empty() {
                true => NONE.to_string(),
                false => format!("{kind}/{name}"),
            }
        }
        Cell::RoleRef => {
            let kind = object.str_at("roleRef.kind");
            let name = object.str_at("roleRef.name");
            match kind.is_empty() {
                true => NONE.to_string(),
                false => format!("{kind}/{name}"),
            }
        }
        Cell::Endpoints => endpoints(object),
    }
}

/// An empty string as `kubectl` prints one.
fn or_none(value: &str) -> String {
    match value.is_empty() {
        true => NONE.to_string(),
        false => value.to_string(),
    }
}

/// A node's roles, from the labels the control plane puts on it.
fn node_roles(object: &Object) -> String {
    const PREFIX: &str = "node-role.kubernetes.io/";
    let mut roles: Vec<&str> = object
        .meta
        .labels
        .keys()
        .filter_map(|label| label.strip_prefix(PREFIX))
        .filter(|role| !role.is_empty())
        .collect();
    roles.sort_unstable();
    match roles.is_empty() {
        true => NONE.to_string(),
        false => roles.join(","),
    }
}

/// A service's ports, as `kubectl` writes them: `80:30001/TCP`.
fn service_ports(object: &Object) -> String {
    let ports: Vec<String> = object
        .array_at("spec.ports")
        .iter()
        .map(|port| {
            let number = port.get("port").and_then(Value::as_i64).unwrap_or_default();
            let protocol = port
                .get("protocol")
                .and_then(Value::as_str)
                .unwrap_or("TCP");
            match port.get("nodePort").and_then(Value::as_i64) {
                Some(node_port) => format!("{number}:{node_port}/{protocol}"),
                None => format!("{number}/{protocol}"),
            }
        })
        .collect();
    match ports.is_empty() {
        true => NONE.to_string(),
        false => ports.join(","),
    }
}

/// A service's external address.
///
/// `<pending>` for a load balancer that has not been given one, which is the
/// single most-asked question about a `Service` and deserves its own word.
fn service_external_ip(object: &Object) -> String {
    let ingress: Vec<String> = object
        .array_at("status.loadBalancer.ingress")
        .iter()
        .filter_map(|entry| {
            entry
                .get("ip")
                .or_else(|| entry.get("hostname"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    if !ingress.is_empty() {
        return ingress.join(",");
    }
    let external: Vec<String> = object
        .array_at("spec.externalIPs")
        .iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect();
    if !external.is_empty() {
        return external.join(",");
    }
    match object.str_at("spec.type") {
        "LoadBalancer" => "<pending>".to_string(),
        _ => NONE.to_string(),
    }
}

/// An ingress's hosts.
fn ingress_hosts(object: &Object) -> String {
    let hosts: Vec<String> = object
        .array_at("spec.rules")
        .iter()
        .filter_map(|rule| rule.get("host").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    match hosts.is_empty() {
        true => "*".to_string(),
        false => hosts.join(","),
    }
}

/// The images an object runs, from a pod spec or from a controller's
/// template.
fn images(object: &Object) -> String {
    let containers = match object.at("spec.template.spec.containers") {
        Some(_) => object.array_at("spec.template.spec.containers"),
        None => object.array_at("spec.containers"),
    };
    let images: Vec<String> = containers
        .iter()
        .filter_map(|container| container.get("image").and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    match images.is_empty() {
        true => NONE.to_string(),
        false => images.join(","),
    }
}

/// An endpoints object's addresses, as `10.0.0.1:8080,10.0.0.2:8080`.
fn endpoints(object: &Object) -> String {
    let mut listed = Vec::new();
    for subset in object.array_at("subsets") {
        let ports: Vec<String> = subset
            .get("ports")
            .and_then(Value::as_array)
            .map(|ports| {
                ports
                    .iter()
                    .filter_map(|port| port.get("port").and_then(Value::as_i64))
                    .map(|port| port.to_string())
                    .collect()
            })
            .unwrap_or_default();
        for address in subset
            .get("addresses")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let Some(ip) = address.get("ip").and_then(Value::as_str) else {
                continue;
            };
            match ports.first() {
                Some(port) => listed.push(format!("{ip}:{port}")),
                None => listed.push(ip.to_string()),
            }
        }
    }
    match listed.is_empty() {
        true => NONE.to_string(),
        false => listed.join(","),
    }
}

/// A quantity as a byte count, for the places a number reads better than the
/// manifest's own spelling — a node's memory beside what it is using.
pub fn as_bytes(quantity_text: &str) -> Option<String> {
    quantity::bytes(quantity_text).map(crate::time::bytes)
}

/// The share of a capacity something is using, as a percentage, when both are
/// known. `None` rather than zero when either is missing: a bar drawn at zero
/// because a number was absent is a lie.
pub fn percent(used: u64, capacity: u64) -> Option<f32> {
    (capacity > 0).then(|| (used as f32 / capacity as f32 * 100.0).min(999.0))
}

/// The health of a whole table, for the sidebar's count: how many rows are
/// worth looking at.
pub fn bad_rows(rows: &[Row]) -> usize {
    rows.iter().filter(|row| row.is_bad()).count()
}

/// The worst level in a table, for a summary mark.
pub fn worst(rows: &[Row]) -> Level {
    rows.iter()
        .map(|row| row.health.level)
        .max_by_key(|level| match level {
            Level::Error => 4,
            Level::Attention => 3,
            Level::Working => 2,
            Level::Ok => 1,
            Level::Unknown => 0,
        })
        .unwrap_or(Level::Unknown)
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

    fn pod() -> Object {
        object(json!({
            "metadata": {"name": "api-7d9f8c-2xk", "namespace": "shop", "uid": "u1",
                         "creationTimestamp": "2026-09-07T09:00:00Z",
                         "labels": {"app": "api"}},
            "spec": {"nodeName": "node-1",
                     "containers": [{"name": "api", "image": "ghcr.io/x/api:1.4"}]},
            "status": {"phase": "Running", "podIP": "10.244.1.7",
                       "containerStatuses": [{"ready": true, "restartCount": 3,
                                              "state": {"running": {}}}]}
        }))
    }

    #[test]
    fn every_kind_has_as_many_cells_as_columns() {
        // The property the whole design exists for: titles and cells come
        // from one list, so they cannot disagree.
        let kinds = [
            "Pod",
            "Deployment",
            "StatefulSet",
            "ReplicaSet",
            "DaemonSet",
            "Job",
            "CronJob",
            "Node",
            "Namespace",
            "Service",
            "Endpoints",
            "Ingress",
            "ConfigMap",
            "Secret",
            "PersistentVolumeClaim",
            "PersistentVolume",
            "StorageClass",
            "ServiceAccount",
            "HorizontalPodAutoscaler",
            "RoleBinding",
            "Event",
            "Rollout",
        ];
        let empty = object(json!({"metadata": {"name": "x", "namespace": "n"}}));
        for kind in kinds {
            for show_namespace in [false, true] {
                let columns = ColumnSet::for_kind(kind, true, show_namespace);
                assert_eq!(
                    columns.cells(&empty, now()).len(),
                    columns.len(),
                    "{kind}, namespace column {show_namespace}"
                );
            }
        }
    }

    #[test]
    fn every_kind_starts_with_a_name_and_ends_with_an_age() {
        for kind in ["Pod", "Node", "Rollout", "Event"] {
            let columns = ColumnSet::for_kind(kind, true, true);
            let titles: Vec<&str> = columns.columns().map(|column| column.title).collect();
            assert_eq!(titles.first(), Some(&"NAME"), "{kind}");
            assert_eq!(titles.last(), Some(&"AGE"), "{kind}");
        }
    }

    #[test]
    fn a_kind_nobody_wrote_columns_for_still_gets_a_table() {
        let columns = ColumnSet::for_kind("Rollout", true, true);
        let titles: Vec<&str> = columns.columns().map(|column| column.title).collect();
        assert_eq!(titles, vec!["NAME", "NAMESPACE", "AGE"]);
    }

    #[test]
    fn the_namespace_column_only_appears_when_it_earns_its_width() {
        assert!(
            !ColumnSet::for_kind("Pod", true, false)
                .columns()
                .any(|column| column.title == "NAMESPACE")
        );
        assert!(
            ColumnSet::for_kind("Pod", true, true)
                .columns()
                .any(|column| column.title == "NAMESPACE")
        );
        // A cluster-scoped kind never gets one, however the table is scoped.
        assert!(
            !ColumnSet::for_kind("Node", false, true)
                .columns()
                .any(|column| column.title == "NAMESPACE")
        );
    }

    #[test]
    fn a_pod_row_is_the_one_kubectl_prints() {
        let columns = ColumnSet::for_kind("Pod", true, false);
        let cells = columns.cells(&pod(), now());
        assert_eq!(
            cells,
            vec![
                "api-7d9f8c-2xk",
                "1/1",
                "Running",
                "3",
                "10.244.1.7",
                "node-1",
                "3h"
            ]
        );
    }

    #[test]
    fn a_row_carries_its_health_and_a_haystack_the_filter_can_match_labels_in() {
        let row = ColumnSet::for_kind("Pod", true, false).row(&pod(), now());
        assert_eq!(row.key, "u1");
        assert_eq!(row.namespace.as_deref(), Some("shop"));
        assert_eq!(row.health.level, Level::Ok);
        assert!(row.haystack.contains("app=api"));
        assert!(row.haystack.contains("node-1"));
        assert!(!row.is_bad());
    }

    #[test]
    fn a_row_without_a_uid_is_keyed_by_where_it_lives() {
        let anonymous = object(json!({"metadata": {"name": "a", "namespace": "n"}}));
        assert_eq!(row_key(&anonymous), "n/a");
        let cluster_scoped = object(json!({"metadata": {"name": "node-1"}}));
        assert_eq!(row_key(&cluster_scoped), "node-1");
    }

    #[test]
    fn an_absent_field_reads_as_kubectl_writes_it() {
        let columns = ColumnSet::for_kind("Pod", true, false);
        let unscheduled = object(json!({
            "metadata": {"name": "p"},
            "spec": {"containers": [{"name": "c"}]},
            "status": {"phase": "Pending"}
        }));
        let cells = columns.cells(&unscheduled, now());
        assert!(cells.contains(&NONE.to_string()), "{cells:?}");
    }

    #[test]
    fn a_service_says_pending_while_its_load_balancer_has_no_address() {
        let columns = ColumnSet::for_kind("Service", true, false);
        let pending = object(json!({
            "metadata": {"name": "web"},
            "spec": {"type": "LoadBalancer", "clusterIP": "10.96.0.9",
                     "ports": [{"port": 443, "protocol": "TCP"}]},
            "status": {}
        }));
        let cells = columns.cells(&pending, now());
        assert!(cells.contains(&"<pending>".to_string()), "{cells:?}");
        assert!(cells.contains(&"443/TCP".to_string()), "{cells:?}");

        let assigned = object(json!({
            "metadata": {"name": "web"},
            "spec": {"type": "LoadBalancer", "ports": [{"port": 443, "nodePort": 31000}]},
            "status": {"loadBalancer": {"ingress": [{"hostname": "a.elb.example"}]}}
        }));
        let cells = columns.cells(&assigned, now());
        assert!(cells.contains(&"a.elb.example".to_string()), "{cells:?}");
        assert!(cells.contains(&"443:31000/TCP".to_string()), "{cells:?}");
    }

    #[test]
    fn a_nodes_roles_come_from_its_labels_and_sort() {
        let node = object(json!({"metadata": {"name": "n", "labels": {
            "node-role.kubernetes.io/worker": "",
            "node-role.kubernetes.io/control-plane": "",
            "kubernetes.io/os": "linux"
        }}}));
        assert_eq!(node_roles(&node), "control-plane,worker");
        let plain = object(json!({"metadata": {"name": "n"}}));
        assert_eq!(node_roles(&plain), NONE);
    }

    #[test]
    fn images_come_from_a_controllers_template_when_it_has_one() {
        let deployment = object(json!({
            "metadata": {"name": "api"},
            "spec": {"template": {"spec": {"containers": [
                {"name": "api", "image": "ghcr.io/x/api:1.4"},
                {"name": "proxy", "image": "envoy:1.31"}
            ]}}}
        }));
        assert_eq!(images(&deployment), "ghcr.io/x/api:1.4,envoy:1.31");
        assert_eq!(images(&pod()), "ghcr.io/x/api:1.4");
    }

    #[test]
    fn a_cron_job_prints_its_suspension_the_way_kubectl_does() {
        let columns = ColumnSet::for_kind("CronJob", true, false);
        let suspended = object(json!({
            "metadata": {"name": "reindex"},
            "spec": {"schedule": "*/15 * * * *", "suspend": true},
            "status": {}
        }));
        let cells = columns.cells(&suspended, now());
        assert!(cells.contains(&"True".to_string()), "{cells:?}");
        assert!(cells.contains(&"*/15 * * * *".to_string()), "{cells:?}");
        // Never scheduled, so there is no last schedule.
        assert!(cells.contains(&NONE.to_string()), "{cells:?}");
    }

    #[test]
    fn the_kinds_that_churn_sort_newest_first() {
        for kind in ["Pod", "Event", "Job"] {
            let (column, ascending) = ColumnSet::for_kind(kind, true, false).default_sort();
            assert!(
                ColumnSet::for_kind(kind, true, false).is_age(column),
                "{kind} should sort by age"
            );
            assert!(ascending, "{kind} should sort youngest first");
        }
        let (column, ascending) = ColumnSet::for_kind("ConfigMap", true, false).default_sort();
        assert_eq!(column, 0);
        assert!(ascending);
    }

    #[test]
    fn sorting_by_age_uses_the_timestamp_and_not_the_string() {
        let columns = ColumnSet::for_kind("Pod", true, false);
        let make = |name: &str, created: &str| {
            columns.row(
                &object(json!({"metadata": {"name": name, "creationTimestamp": created}})),
                now(),
            )
        };
        // `9m59s` and `10m` compare the wrong way round as text.
        let mut rows = vec![
            make("older", "2026-09-07T11:50:00Z"),
            make("newer", "2026-09-07T11:50:01Z"),
        ];
        let (age, _) = columns.default_sort();
        sort(&mut rows, &columns, age, true);
        assert_eq!(rows[0].name, "newer");
        sort(&mut rows, &columns, age, false);
        assert_eq!(rows[0].name, "older");
    }

    #[test]
    fn sorting_a_number_column_compares_numbers() {
        let columns = ColumnSet::for_kind("Pod", true, false);
        let make = |name: &str, restarts: i64| {
            columns.row(
                &object(json!({
                    "metadata": {"name": name},
                    "status": {"phase": "Running",
                               "containerStatuses": [{"ready": true, "restartCount": restarts}]}
                })),
                now(),
            )
        };
        let restarts = columns
            .columns()
            .position(|column| column.title == "RESTARTS")
            .unwrap();
        let mut rows = vec![make("nine", 9), make("ten", 10), make("two", 2)];
        sort(&mut rows, &columns, restarts, true);
        let order: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
        assert_eq!(order, vec!["two", "nine", "ten"]);
    }

    #[test]
    fn a_table_reports_what_is_worth_looking_at() {
        let columns = ColumnSet::for_kind("Pod", true, false);
        let healthy = columns.row(&pod(), now());
        let broken = columns.row(
            &object(json!({
                "metadata": {"name": "b"},
                "status": {"phase": "Running", "containerStatuses": [
                    {"ready": false, "state": {"waiting": {"reason": "CrashLoopBackOff"}}}
                ]}
            })),
            now(),
        );
        let rows = vec![healthy, broken];
        assert_eq!(bad_rows(&rows), 1);
        assert_eq!(worst(&rows), Level::Error);
        assert_eq!(worst(&[]), Level::Unknown);
    }

    #[test]
    fn a_share_of_nothing_is_not_zero_percent() {
        assert_eq!(percent(50, 100), Some(50.0));
        assert_eq!(percent(1, 0), None);
    }

    #[test]
    fn a_capacity_is_shown_as_the_manifest_wrote_it() {
        let columns = ColumnSet::for_kind("PersistentVolumeClaim", true, false);
        let claim = object(json!({
            "metadata": {"name": "data"},
            "spec": {"storageClassName": "standard"},
            "status": {"phase": "Bound", "capacity": {"storage": "50Gi"}}
        }));
        assert!(columns.cells(&claim, now()).contains(&"50Gi".to_string()));
        assert_eq!(as_bytes("50Gi").as_deref(), Some("50Gi"));
    }
}
