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
use kirikumo_kube::watch::Backoff;
use kirikumo_kube::{
    ApiResource, Applied, Catalogue, Cluster, ClusterVersion, ContextRef, EventRecord, LogRequest,
    Object, ObjectList, ResourceKey, WatchEvent, watch,
};
use kirikumo_ui::Fetch;
use kirikumo_ui::fetch::describe;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
    /// The list the window is looking at, and so the only one worth
    /// following. A cluster with ten thousand pods must not be streamed
    /// because the sidebar mentions pods (roadmap §4.7).
    followed: Option<ListKey>,
    /// The watch that is running, if one is.
    watch: Option<WatchHandle>,
    /// Bumped for every watch started, so an event from one that has been
    /// abandoned is recognised and dropped.
    generation: u64,
}

/// A watch that is running, and the way to tell it to stop.
struct WatchHandle {
    /// Which list it follows.
    key: ListKey,
    /// Which generation it belongs to.
    generation: u64,
    /// Set when it is abandoned.
    ///
    /// The reader is a blocking read on a thread of its own and cannot be
    /// interrupted from here, so it notices at its next event or at the
    /// apiserver's timeout — five minutes at the outside
    /// (`kirikumo_kube::rest`). Until then it is a parked thread and a socket,
    /// and its events are dropped by generation.
    stop: Arc<AtomicBool>,
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
            followed: None,
            watch: None,
            generation: 0,
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
        self.stop_watch();
        self.followed = None;
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
            move |this, result, cx| {
                let landed = result.is_ok();
                this.lists.entry(stored.clone()).or_default().finish(result);
                // A watch resumes from the version the list came back with,
                // so it can only start once there is a list.
                if landed && this.followed.as_ref() == Some(&stored) {
                    this.start_watch(cx);
                }
            },
        );
    }

    /// Follow one list, and stop following whatever was followed before.
    ///
    /// One watch at a time, because one table is on screen at a time. The
    /// watch starts when a list for this key has landed — it resumes from
    /// that list's `resourceVersion`, so there is nothing to resume from
    /// before then.
    pub fn follow(&mut self, key: Option<ListKey>, cx: &mut Context<Self>) {
        if self.followed == key {
            return;
        }
        self.stop_watch();
        self.followed = key;
        self.start_watch(cx);
    }

    /// Whether the list on screen is being followed rather than refreshed.
    pub fn is_live(&self) -> bool {
        self.watch.is_some()
    }

    /// Start a watch on the followed list, if there is one to start.
    fn start_watch(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.followed.clone() else {
            return;
        };
        if self.watch.as_ref().is_some_and(|watch| watch.key == key) {
            return;
        }
        let Some(resource) = self.resource(&key.0).cloned() else {
            return;
        };
        // A cluster may serve a kind it will not let anyone follow, and RBAC
        // may allow `list` and refuse `watch`. Either way the table stays
        // correct; it is refreshed rather than live.
        if !resource.supports("watch") {
            tracing::debug!(kind = %resource.kind, "no watch for this kind");
            return;
        }
        let Some(version) = self
            .lists
            .get(&key)
            .and_then(Fetch::value)
            .map(|list| list.resource_version.clone())
            .filter(|version| !version.is_empty())
        else {
            return;
        };

        self.stop_watch();
        self.generation += 1;
        let generation = self.generation;
        let stop = Arc::new(AtomicBool::new(false));
        self.watch = Some(WatchHandle {
            key: key.clone(),
            generation,
            stop: stop.clone(),
        });

        let (sender, receiver) = async_channel::unbounded::<WatchEvent>();
        let cluster = self.cluster.clone();
        let namespace = key.1.clone();
        let kind = resource.kind.clone();
        if let Err(error) = std::thread::Builder::new()
            .name(format!("kirikumo-watch-{kind}"))
            .spawn(move || pump(cluster, resource, namespace, version, stop, sender))
        {
            tracing::warn!(%error, %kind, "could not start a watch");
            self.watch = None;
            return;
        }

        cx.spawn(async move |this, cx| {
            while let Ok(event) = receiver.recv().await {
                let carry_on = this
                    .update(cx, |this, cx| this.on_watch(generation, event, cx))
                    .unwrap_or(false);
                if !carry_on {
                    break;
                }
            }
        })
        .detach();
    }

    /// Tell the running watch to stop, and stop believing it.
    fn stop_watch(&mut self) {
        if let Some(watch) = self.watch.take() {
            watch.stop.store(true, Ordering::Relaxed);
        }
    }

    /// Apply one watch event, and say whether the watch should carry on.
    fn on_watch(&mut self, generation: u64, event: WatchEvent, cx: &mut Context<Self>) -> bool {
        // An event from a watch we have abandoned: its thread has not
        // noticed yet, and its list may not even be here any more.
        let Some(key) = self
            .watch
            .as_ref()
            .filter(|watch| watch.generation == generation)
            .map(|watch| watch.key.clone())
        else {
            return false;
        };
        let Some(list) = self.lists.get_mut(&key).and_then(Fetch::value_mut) else {
            return false;
        };
        match watch::apply(list, event) {
            applied @ (Applied::Added(_) | Applied::Changed(_) | Applied::Removed(_)) => {
                // At `trace` rather than `debug`: one line per object per
                // event is the right grain for diagnosing a table that will
                // not settle, and far too much for anything else.
                tracing::trace!(?applied, kind = %key.0.kind, "a watch event landed");
                cx.emit(StoreEvent::Changed);
                cx.notify();
                true
            }
            // A bookmark: nothing on screen moved, and the version it left
            // behind is the thread's business, not ours.
            Applied::Version => true,
            Applied::Restart => {
                // The version we were resuming from has aged out of the
                // apiserver's window. List again; that lands a new version
                // and starts a new watch.
                tracing::debug!("the watch aged out; listing again");
                self.stop_watch();
                self.load_list(key.0, key.1.as_deref(), cx);
                false
            }
            Applied::Failed(error) => {
                // Logged, not shown: the table is still correct, it has just
                // stopped being live, and blanking a good list over it would
                // be worse than the loss.
                tracing::warn!(%error, "the watch stopped");
                self.stop_watch();
                false
            }
        }
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

/// One watch, read to its end on a thread of its own.
///
/// The retry policy lives here rather than in the store because it is about a
/// connection, not about a window: an apiserver closes an idle watch on a
/// timeout of its own, which is routine and reconnects at once, while a
/// transport failure backs off (roadmap §4.7). What the store decides is the
/// one thing a connection cannot: that a `410 Gone` means list again.
///
/// Returns when it is told to stop, when the channel is closed — which is
/// what dropping the receiving task does — or when the failure is one that
/// reconnecting cannot fix.
fn pump(
    cluster: Arc<dyn Cluster>,
    resource: ApiResource,
    namespace: Option<String>,
    mut version: String,
    stop: Arc<AtomicBool>,
    sender: async_channel::Sender<WatchEvent>,
) {
    let mut backoff = Backoff::new();
    while !stop.load(Ordering::Relaxed) {
        match cluster.watch(&resource, namespace.as_deref(), &version) {
            Ok(mut stream) => {
                backoff.reset();
                while let Some(event) = stream.next_event() {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    // Remembered before the event is given away, so a
                    // reconnect resumes from the last thing we saw rather
                    // than from the list.
                    if let Some(seen) = event.resource_version() {
                        version = seen.to_string();
                    }
                    let terminal = matches!(event, WatchEvent::Failed(_));
                    if sender.send_blocking(event).is_err() || terminal {
                        return;
                    }
                }
                // The stream ended without saying anything: the apiserver's
                // own idle timeout. Reconnect immediately, from where we got
                // to — this is the common case and must cost nothing.
            }
            Err(error) => {
                let retry = error.is_retryable();
                if sender.send_blocking(WatchEvent::Failed(error)).is_err() || !retry {
                    return;
                }
                std::thread::sleep(backoff.next_delay());
            }
        }
    }
}
