//! Everything fetched, and the fetching of it.
//!
//! One entity holds every answer the cluster has given this window, each as a
//! [`Fetch`] so a refresh keeps the old value on screen. Every call goes to
//! the background executor and comes back through `this.update`; nothing here
//! blocks the UI thread, and nothing but this file calls the trait
//! (`AGENTS.md` rule 2).
//!
//! Nothing is written to disk (rule 10). Unlike this app's siblings there is
//! no snapshot and no HTTP cache: Kubernetes payloads are large, short-lived
//! and frequently secret, and the apiserver has no `ETag` to make a cheap
//! revalidation out of.

use gpui::{AppContext as _, Context, EventEmitter};
use kirikumo_kube::{
    ApiResource, Catalogue, Cluster, ClusterVersion, ContextRef, EventRecord, LogRequest, Object,
    ObjectList, ResourceKey,
};
use kirikumo_ui::Fetch;
use kirikumo_ui::fetch::describe;
use std::collections::HashMap;
use std::sync::Arc;

/// Which list: a kind, scoped to a namespace or to all of them.
pub type ListKey = (ResourceKey, Option<String>);

/// Which object: a kind, a namespace and a name.
pub type ObjectKey = (ResourceKey, Option<String>, String);

/// Emitted whenever an answer lands.
pub enum StoreEvent {
    /// Something changed; views re-read what they show.
    Changed,
}

/// The window's memory of the cluster.
pub struct Store {
    cluster: Arc<dyn Cluster>,
    /// The contexts the kubeconfig offers, and which one is connected.
    contexts: Vec<ContextRef>,
    current: Option<String>,
    /// Whether the connected context skips certificate verification, which
    /// the sidebar says out loud (`docs/ui.md` §3.2).
    insecure: bool,
    version: Fetch<ClusterVersion>,
    catalogue: Fetch<Catalogue>,
    namespaces: Fetch<Vec<String>>,
    lists: HashMap<ListKey, Fetch<ObjectList>>,
    events: HashMap<String, Fetch<Vec<EventRecord>>>,
    logs: HashMap<String, Fetch<String>>,
}

impl EventEmitter<StoreEvent> for Store {}

impl Store {
    /// A store over a cluster. Nothing is fetched until asked.
    pub fn new(cluster: Arc<dyn Cluster>) -> Self {
        Self {
            cluster,
            contexts: Vec::new(),
            current: None,
            insecure: false,
            version: Fetch::Idle,
            catalogue: Fetch::Idle,
            namespaces: Fetch::Idle,
            lists: HashMap::new(),
            events: HashMap::new(),
            logs: HashMap::new(),
        }
    }

    /// Tell the store which contexts exist and which one it is connected to.
    pub fn with_contexts(mut self, contexts: Vec<ContextRef>, current: Option<String>) -> Self {
        self.insecure = contexts
            .iter()
            .find(|context| Some(&context.name) == current.as_ref())
            .is_some_and(|context| context.insecure);
        self.contexts = contexts;
        self.current = current;
        self
    }

    /// The contexts the kubeconfig offers.
    pub fn contexts(&self) -> &[ContextRef] {
        &self.contexts
    }

    /// The context this store is connected to.
    pub fn current_context(&self) -> Option<&str> {
        self.current.as_deref()
    }

    /// The server of the connected context, for the insecure warning.
    pub fn current_server(&self) -> Option<&str> {
        self.contexts
            .iter()
            .find(|context| Some(&context.name) == self.current.as_ref())
            .map(|context| context.server.as_str())
    }

    /// Whether certificate verification is off on this connection.
    pub fn is_insecure(&self) -> bool {
        self.insecure
    }

    /// Point the store at another cluster, forgetting everything the last one
    /// said.
    ///
    /// Everything: a list of pods from one cluster drawn under another
    /// cluster's name is the single worst thing a viewer like this can do.
    pub fn connect(
        &mut self,
        cluster: Arc<dyn Cluster>,
        context: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.cluster = cluster;
        self.insecure = self
            .contexts
            .iter()
            .find(|entry| Some(&entry.name) == context.as_ref())
            .is_some_and(|entry| entry.insecure);
        self.current = context;
        self.version = Fetch::Idle;
        self.catalogue = Fetch::Idle;
        self.namespaces = Fetch::Idle;
        self.lists.clear();
        self.events.clear();
        self.logs.clear();
        cx.emit(StoreEvent::Changed);
        cx.notify();
        self.refresh_all(cx);
    }

    /// What the apiserver says it is.
    pub fn version(&self) -> &Fetch<ClusterVersion> {
        &self.version
    }

    /// Everything the cluster serves.
    pub fn catalogue(&self) -> &Fetch<Catalogue> {
        &self.catalogue
    }

    /// Every namespace.
    pub fn namespaces(&self) -> &Fetch<Vec<String>> {
        &self.namespaces
    }

    /// The resource for a kind, if discovery has landed and the cluster
    /// serves it.
    pub fn resource(&self, key: &ResourceKey) -> Option<&ApiResource> {
        self.catalogue
            .value()
            .and_then(|catalogue| catalogue.get(key))
    }

    /// The key a list is stored under.
    ///
    /// A cluster-scoped kind is stored once, whatever namespace is selected,
    /// so moving between namespaces does not re-fetch the nodes.
    pub fn list_key(&self, key: &ResourceKey, namespace: Option<&str>) -> ListKey {
        let namespaced = self
            .resource(key)
            .is_some_and(|resource| resource.namespaced);
        let namespace = namespace
            .filter(|namespace| namespaced && !namespace.is_empty())
            .map(str::to_string);
        (key.clone(), namespace)
    }

    /// A list, if it has ever been asked for.
    pub fn list(&self, key: &ResourceKey, namespace: Option<&str>) -> Option<&Fetch<ObjectList>> {
        self.lists.get(&self.list_key(key, namespace))
    }

    /// Fetch a list only if it never has been.
    pub fn ensure_list(
        &mut self,
        key: ResourceKey,
        namespace: Option<&str>,
        cx: &mut Context<Self>,
    ) {
        let stored = self.list_key(&key, namespace);
        if self.lists.get(&stored).is_none_or(Fetch::is_idle) {
            self.load_list(key, namespace, cx);
        }
    }

    /// Fetch a list.
    pub fn load_list(&mut self, key: ResourceKey, namespace: Option<&str>, cx: &mut Context<Self>) {
        let Some(resource) = self.resource(&key).cloned() else {
            // Discovery has not landed, or this cluster does not serve the
            // kind the settings remembered. Either way there is nothing to
            // ask for; the sidebar will not have drawn a row for it.
            return;
        };
        let stored = self.list_key(&key, namespace);
        self.lists.entry(stored.clone()).or_default().begin();
        let scope = stored.1.clone();
        self.fetch(
            cx,
            move |cluster| cluster.list(&resource, scope.as_deref()),
            move |this, result, _| {
                this.lists.entry(stored).or_default().finish(result);
            },
        );
    }

    /// One object out of a list that has already landed.
    ///
    /// The detail panel reads from the list rather than fetching again: the
    /// object it wants arrived a moment ago, and a `GET` for it would show
    /// the reader a spinner over data the window already has.
    pub fn object(&self, key: &ObjectKey) -> Option<&Object> {
        let (kind, namespace, name) = key;
        self.lists
            .iter()
            .filter(|((stored, _), _)| stored == kind)
            .filter_map(|(_, list)| list.value())
            .flat_map(|list| list.items.iter())
            .find(|object| {
                &object.meta.name == name
                    && (namespace.is_none() || object.meta.namespace == *namespace)
            })
    }

    /// The events about an object, if they have ever been asked for.
    pub fn events(&self, uid: &str) -> Option<&Fetch<Vec<EventRecord>>> {
        self.events.get(uid)
    }

    /// Fetch an object's events only if they never have been.
    pub fn ensure_events(
        &mut self,
        uid: String,
        namespace: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if uid.is_empty() || self.events.get(&uid).is_some_and(|fetch| !fetch.is_idle()) {
            return;
        }
        self.events.entry(uid.clone()).or_default().begin();
        let key = uid.clone();
        self.fetch(
            cx,
            move |cluster| cluster.events_for(&uid, namespace.as_deref()),
            move |this, result, _| {
                this.events.entry(key).or_default().finish(result);
            },
        );
    }

    /// A container's log, if it has ever been asked for.
    pub fn logs(&self, key: &str) -> Option<&Fetch<String>> {
        self.logs.get(key)
    }

    /// The key a log is stored under: the pod, and the container within it.
    pub fn log_key(request: &LogRequest) -> String {
        format!(
            "{}/{}:{}",
            request.namespace,
            request.pod,
            request.container.as_deref().unwrap_or_default()
        )
    }

    /// Fetch a log only if it never has been.
    pub fn ensure_logs(&mut self, request: LogRequest, cx: &mut Context<Self>) {
        let key = Self::log_key(&request);
        if self.logs.get(&key).is_some_and(|fetch| !fetch.is_idle()) {
            return;
        }
        self.load_logs(request, cx);
    }

    /// Fetch a log.
    pub fn load_logs(&mut self, request: LogRequest, cx: &mut Context<Self>) {
        let key = Self::log_key(&request);
        self.logs.entry(key.clone()).or_default().begin();
        self.fetch(
            cx,
            move |cluster| cluster.logs(&request),
            move |this, result, _| {
                this.logs.entry(key).or_default().finish(result);
            },
        );
    }

    /// Ask the cluster who it is, what it serves and what namespaces it has.
    ///
    /// The three questions every other question depends on, asked once per
    /// connection.
    pub fn refresh_all(&mut self, cx: &mut Context<Self>) {
        self.version.begin();
        self.fetch(
            cx,
            |cluster| cluster.version(),
            |this, result, _| this.version.finish(result),
        );
        self.catalogue.begin();
        self.fetch(
            cx,
            |cluster| cluster.catalogue(),
            |this, result, _| this.catalogue.finish(result),
        );
        self.namespaces.begin();
        self.fetch(
            cx,
            |cluster| cluster.namespaces(),
            |this, result, _| this.namespaces.finish(result),
        );
    }

    /// Fetch every list that is already on screen again.
    pub fn refresh_lists(&mut self, cx: &mut Context<Self>) {
        let keys: Vec<ListKey> = self.lists.keys().cloned().collect();
        for (key, namespace) in keys {
            self.load_list(key, namespace.as_deref(), cx);
        }
    }

    /// Run one call on the background executor and apply its answer here.
    ///
    /// The whole of the threading model: the trait is blocking, the executor
    /// is GPUI's, and the answer lands back on the UI thread through
    /// `this.update`. No reactor appears anywhere (`AGENTS.md` rule 3).
    fn fetch<T, W, A>(&self, cx: &mut Context<Self>, work: W, apply: A)
    where
        T: Send + 'static,
        W: FnOnce(&dyn Cluster) -> kirikumo_kube::Result<T> + Send + 'static,
        A: FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static,
    {
        cx.notify();
        let cluster = self.cluster.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    work(cluster.as_ref()).map_err(|error| {
                        tracing::warn!(%error, "a request to the cluster failed");
                        describe(&error)
                    })
                })
                .await;
            this.update(cx, |this, cx| {
                apply(this, result, cx);
                cx.emit(StoreEvent::Changed);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}
