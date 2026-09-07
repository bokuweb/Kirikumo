# Kirikumo Roadmap

> The authoritative document for this repository. `AGENTS.md` is the short version; `docs/ui.md` says what it looks like.
> Last updated: 2026-09-07

## 1. Vision

**Kirikumo is a native Kubernetes viewer, written in Rust on GPUI, built to stand on its own and to be mounted inside [Ginka](https://github.com/bokuweb/ginka).**

It answers the questions a person opens Lens for — what is running, what is unhealthy, what did that pod log, what does this object actually look like — in one window, over any cluster their kubeconfig can reach, with no agent installed in the cluster and no account anywhere.

It is the third window in a family that becomes one application:

| Repository | What it shows | What it becomes |
| --- | --- | --- |
| [`ginka`](https://github.com/bokuweb/ginka) | coding agents across worktrees | the host: the window, the daemon, the theme |
| [`e1`](https://github.com/bokuweb/e1) | GitHub: inbox, pulls, issues, diffs | a surface in that window |
| **`kirikumo`** | Kubernetes: workloads, config, logs | a surface in that window |

So every decision here is made twice: once for the standalone window, and once for the day these three are one binary. §4.3 is where that second answer is written down, and it is the reason this repository looks the way it does.

## 2. What we take from the references

- **[Lens](https://k8slens.dev/)** — the shape of the thing: a cluster picked at the top, a resource tree grouped into Workloads / Config / Network / Storage / Access Control / Custom Resources, a table in the middle, an object drawer with Overview, Events, YAML and Logs.
- **[k9s](https://k9scli.io/)** — its discipline about *what a row is worth*: the columns are the ones `kubectl get` prints, health is one mark at the head of the row, and everything is one keystroke away.
- **[Headlamp](https://headlamp.dev/)** — generic-first rendering: the resource catalogue comes from API discovery, so a CRD gets a table without anyone writing code for it.
- **`kubectl`** — the wire behaviour: discovery, `?watch=true` bookmarks and `410 Gone`, `metrics.k8s.io` being optional, exec credential plugins.

**Kirikumo is written here.** These are read for behaviour and for the edge cases they have already hit; no code is copied, ported or paraphrased in. Lens is closed-source above its core, k9s and Headlamp are Apache-2.0, and the rule holds for all of them: an implementation we did not write is one we cannot debug.

## 3. Scope

### 3.1 v1.0 definition of done

1. Open on the context the kubeconfig says is current, and switch between contexts without restarting.
2. List **any** resource the cluster serves, including custom resources, from API discovery — not a hardcoded list.
3. A table per kind with the columns `kubectl get` prints, a health mark per row, a namespace filter and a fuzzy filter over the rows.
4. An object detail with Overview, Events, YAML, and — for anything with containers — Logs, following.
5. Live: what is on screen is watched, and a change in the cluster reaches the row without a refresh.
6. Node and pod resource use, when `metrics.k8s.io` is installed, and silence when it is not.
7. The destructive actions a viewer still needs — delete, scale, restart, cordon — behind a confirmation and greyed out when RBAC says no.
8. Read-only by default over any auth a kubeconfig can express: client certificates, tokens, token files and exec credential plugins.

### 3.2 Non-goals for v1

- **No cluster install.** Nothing is deployed into the cluster to make this work — no agent, no metrics collector, no dashboards.
- **No editor.** YAML is shown and can be applied, but authoring manifests is what an editor is for. (`kubectl apply` from a file, and Ginka's own code surface, are the answer.)
- **No Helm, no charts, no marketplace.** Lens's chart catalogue is a package manager wearing a viewer's clothes.
- **No multi-cluster aggregation.** One context at a time; switching is cheap.
- **No metrics history and no Prometheus.** Instantaneous use from `metrics.k8s.io`, nothing stored.
- **No port-forward, no exec** until M5, and neither in the definition of done above until the embedding question (§7 Q2) is settled.

## 4. Architecture

### 4.1 Process model

One process, and no daemon of our own. The cluster is the remote and the kubeconfig is the only state we read; the window holds view state and in-memory caches, so closing it loses nothing.

```
┌─────────────────────────────────────────────────┐  HTTPS   ┌──────────────────┐
│  kirikumo (GPUI app)                            │  (ureq,  │  kube-apiserver  │
│  src/main.rs        opens the window            │◄────────►│                  │
│  kirikumo-views     Shell / Sidebar / Table /   │  mTLS or │  /api, /apis     │
│                     Detail, over Arc<dyn Cluster>│  bearer │  ?watch=true     │
│  kirikumo-ui        tokens, settings, view models│         │  metrics.k8s.io  │
│  kirikumo-kube      Cluster trait, kubeconfig,  │         └──────────────────┘
│                     discovery, REST, Scripted   │
└─────────────────────────────────────────────────┘
```

Every request is blocking and runs on GPUI's background executor. A watch is a blocking read of a chunked response on a thread of its own, which posts each event back to the window; that is the same shape a driver's reader has in Ginka, and it is why no async runtime appears in the graph (§4.3, K3).

When embedded, the same views sit in Ginka's window and the `Arc<dyn Cluster>` they are given proxies through Ginka's daemon, so Ginka's rule that the daemon owns all state holds without any view knowing.

### 4.2 Crate layout

```
kirikumo/
├─ Cargo.toml              # workspace root; the `kirikumo` binary, deliberately thin
├─ src/main.rs             # paths, settings, locale, logging, the window, Shell
├─ crates/
│  ├─ kirikumo-kube/       # kubeconfig, auth, discovery, the model, the Cluster
│  │                       # trait, the REST client, watches, the Scripted fake.
│  │                       # No GPUI.
│  ├─ kirikumo-ui/         # Tokens + theme apply, Assets, Layout, AppSettings,
│  │                       # Paths, i18n, logging, the sidebar tree, the table's
│  │                       # columns, row filtering, Fetch
│  └─ kirikumo-views/      # Store, Shell, Sidebar, ResourceTable, Detail. A
│                          # library, with no tests (AGENTS.md rule 6).
├─ locales/app.yml
├─ assets/themes/{dark,light}.json     # Ginka's tokens, byte for byte
├─ assets/icons/*.svg
└─ docs/{roadmap,ui}.md
```

Dependency direction: `kirikumo-views → kirikumo-ui → kirikumo-kube`. `kirikumo-kube` knows nothing about the UI; `kirikumo-ui` knows the model but not the views; `kirikumo-views` is the only crate that touches `gpui-component`'s render chains.

### 4.3 The embedding contract

Ginka will add `kirikumo-views` to its workspace and mount `kirikumo_views::ClusterPanel` — the table and the detail, without the window's own chrome — as a surface in its right panel, with the resource tree folded into its sidebar. For that to be a mount rather than a rewrite, six things are held constant from now on. They are e1's E1–E5 with one addition, and they are numbered K so the two lists can be compared line by line.

| # | Constraint | Why it is decided now |
| --- | --- | --- |
| K1 | **Views are a library crate.** `src/main.rs` opens a window and nothing more. | A view in a binary cannot be linked. |
| K2 | **`Arc<dyn Cluster>` is the only way a view reaches a cluster.** The trait is blocking, `Send + Sync`, and called on the background executor. | Ginka's daemon owns state; its implementation will answer over its RPC. A blocking trait can be implemented over a channel with `block_on`; an async trait would fix the executor. |
| K3 | **One reactor by default; a second one is a recorded decision, confined behind `Cluster`.** Today that means `ureq` over rustls, a watch on a thread of its own, and no tokio in the graph at all. | Ginka's own rule is that a second runtime needs an entry in its decision log — not that it is forbidden. Two runtimes coexist perfectly well as separate thread pools; the hazard is narrower and nastier than that. A `hyper` future polled from `smol` panics at *run* time rather than failing to compile, so whichever runtime a client needs has to be sealed inside the implementation of the trait. `Cluster` is where that seal is, which is what keeps this reversible. See §8 and Q2. |
| K4 | **`gpui-component` and `gpui` at Ginka's locked revs**, and no other GPUI library. | Two revs of `gpui` are two unrelated `App`, `Window`, `Element` types. `Cargo.lock` was seeded from e1's, which was seeded from Ginka's. |
| K5 | **The token schema is Ginka's.** `kirikumo_ui::Tokens` deserialises the same JSON, and `assets/themes/*.json` is Ginka's file unchanged. | A view written against `text.secondary` renders correctly under any of the three apps' themes. |
| K6 | **Nothing writes to a cluster without an explicit, per-action confirmation, and no write is reachable from a key chord alone.** | The other two apps act on a working copy; this one acts on production. When all three are one window, a muscle-memory keystroke must not be able to delete a StatefulSet. |

What is *not* held constant: the window's own header strips, its traffic-light inset, the appearance control and settings persistence. Those live in `Shell`, and `Shell` is the one view Ginka will not mount.

### 4.4 Data model

Kubernetes already has a data model, and it is `unstructured`: every object is JSON with `apiVersion`, `kind`, `metadata`, and whatever else its schema says. Typing each kind here would mean a code change for every CRD, which is exactly what §2 says not to do. So the model is thin and generic, in `kirikumo_kube::model`:

| Type | What it is | Where it comes from |
| --- | --- | --- |
| `ContextRef` | one entry in the kubeconfig: name, cluster, user, default namespace | `~/.kube/config`, or every file in `KUBECONFIG` |
| `ClusterAccess` | what it takes to reach one cluster: server URL, roots, client cert or token, and how to refresh it | a `ContextRef` resolved against its cluster and user |
| `ApiResource` | one thing the cluster serves: group, version, kind, plural name, namespaced or not, verbs, short names, categories | `/api`, `/apis`, `/apis/{group}/{version}` |
| `Catalogue` | every `ApiResource`, deduplicated to one preferred version per kind, sorted into the sidebar's groups | discovery, once per connection |
| `Object` | one resource: the raw JSON, plus the `ObjectMeta` every object has | any list or get |
| `ObjectMeta` | name, namespace, uid, resourceVersion, creation time, labels, annotations, owner references | `metadata`, which is the one schema every kind shares |
| `ObjectList` | a page of objects and the `resourceVersion` a watch continues from | a list |
| `Health` | one of `Ok`, `Working`, `Attention`, `Error`, `Unknown`, with a word for it | computed per kind in `kirikumo_kube::health` |
| `WatchEvent` | `Added`, `Modified`, `Deleted`, `Bookmark`, or `Error` with a status | `?watch=true`, one JSON object per line |
| `EventRecord` | one `v1.Event`: type, reason, message, count, when | `/api/v1/events?fieldSelector=involvedObject.uid=…` |
| `Metrics` | CPU in milli-cores and memory in bytes, per node or per pod | `metrics.k8s.io/v1beta1` |
| `ClusterVersion` | what the apiserver says it is | `/version` |

`Object` keeps the raw `serde_json::Value`, and everything drawn from it — a column, a health mark, a detail row — is a function of that value written once and tested against real payloads. This is the single most important shape in the repository: it is what makes a CRD free.

### 4.5 The `Cluster` trait

```rust
pub trait Cluster: Send + Sync {
    fn version(&self) -> Result<ClusterVersion>;
    fn catalogue(&self) -> Result<Catalogue>;
    fn namespaces(&self) -> Result<Vec<String>>;
    fn list(&self, resource: &ApiResource, namespace: Option<&str>) -> Result<ObjectList>;
    fn get(&self, resource: &ApiResource, namespace: Option<&str>, name: &str) -> Result<Object>;
    fn events_for(&self, uid: &str, namespace: Option<&str>) -> Result<Vec<EventRecord>>;
    fn logs(&self, request: &LogRequest) -> Result<String>;
    fn node_metrics(&self) -> Result<Vec<Metrics>>;   // defaults to Unsupported
    fn pod_metrics(&self, namespace: Option<&str>) -> Result<Vec<Metrics>>;  // ditto
    fn watch(&self, resource: &ApiResource, namespace: Option<&str>, from: &str)
        -> Result<Box<dyn WatchStream>>;              // ditto
    fn delete(&self, resource: &ApiResource, namespace: Option<&str>, name: &str) -> Result<()>;
    fn patch(&self, resource: &ApiResource, namespace: Option<&str>, name: &str,
             patch: Patch) -> Result<Object>;         // both default to Unsupported
    fn can_i(&self, resource: &ApiResource, namespace: Option<&str>, verb: &str) -> Result<bool>;
}
```

Small on purpose: every method is one screen's question, and a `list` that takes an `ApiResource` is what lets one table draw every kind. Writes arrive in M4 as methods with a default `Err(Unsupported)`, so a host implementation that cannot do them yet still compiles — and so a cluster that refuses them degrades to a viewer rather than to an error.

Two implementations ship: `Rest` (ureq over a per-cluster agent carrying the kubeconfig's TLS) and `Scripted` (in-memory, with a sample cluster used by tests and by `KIRIKUMO_DEMO=1`).

### 4.6 Kubeconfig and authentication

`KUBECONFIG` is a `:`-separated list and it *merges*: the first file to name a context, cluster or user wins, and `current-context` comes from the first file that sets one. With no `KUBECONFIG`, `~/.kube/config`. `KIRIKUMO_KUBECONFIG` overrides both, which is how a test keeps out of the real one.

A context resolves to a `ClusterAccess` carrying one of:

- **Client certificates** — `client-certificate[-data]` and `client-key[-data]`, PEM, handed to `ureq` as a `ClientCert`. This is what `kind`, `minikube` and most bare clusters use.
- **A bearer token** — `token`, or `tokenFile` re-read on every request because a projected service-account token rotates.
- **An exec credential plugin** — `user.exec`, the way EKS, GKE and AKS all authenticate now: run the command, parse the `ExecCredential` it prints, use its `status.token` or its client certificate, and re-run it when `expirationTimestamp` has passed. The command is run with its `env` added to ours, never with a shell.
- **Basic auth** — `username`/`password`, still present in old files, supported and never stored.

The server's roots come from `certificate-authority[-data]`; `insecure-skip-tls-verify: true` is honoured because a homelab cluster is a real thing, and the window says so in the header when it is on, because a viewer that hides that is worse than one that refuses.

Nothing is written back: this app never edits a kubeconfig, and never keeps a credential on disk. A token an exec plugin produced lives in memory until it expires.

### 4.7 Watching, and what is cached

There is no disk cache. Kubernetes answers are large, short-lived and often secret, and `304` is not part of the apiserver's vocabulary the way it is GitHub's; a `store.json` full of pod specs would be a liability with no speed to show for it. What we keep is in memory, for as long as the window is open.

A watch is the mechanism, not a refresh loop:

1. `list` a kind, note the `resourceVersion` of the list.
2. `GET …?watch=true&resourceVersion=…&allowWatchBookmarks=true` and read newline-delimited JSON on a thread of its own.
3. Apply each event to the table's rows, and remember the bookmark's version so a reconnect resumes where we were.
4. On `410 Gone` — the version has aged out — list again and start over. On a transport failure, back off (1 s, doubling to 30 s) and retry.

Only what is on screen is watched, and a watch is dropped when its table is. A cluster with ten thousand pods must not be streamed because the sidebar mentions pods.

### 4.8 UI stack

`gpui-component` over the `gpui` rev it owns, at exactly Ginka's and e1's lock (K4). Used from it: `Root`, `Icon`, `Input`, `Tooltip`, `TextView::markdown`, and `gpui::uniform_list` for every table. Built here: the header strips, the health marks, the table's own header and cells, the log view, the YAML view.

Before writing a widget, check `gpui-component`'s gallery for an existing one.

## 5. Milestones

**M0 and M1 have landed; M2 is under way.** What works today:

- The **window** opens frameless over a blurred desktop, with the three columns resizable and their arrangement, the appearance, the context, the kind and the namespace all remembered across launches.
- The **kubeconfig layer** reads and merges `KUBECONFIG` first-wins, resolves a context into a connection, and authenticates with client certificates, a token, a token file, basic auth or an **exec credential plugin** whose answer is cached until it expires.
- **Discovery** builds the sidebar: every listable kind the apiserver serves, one preferred version per kind, sorted into the seven groups, with custom resources under their API group. Nothing in the tree is written in the source.
- **One table draws every kind**, virtualized, with `kubectl get`'s columns per kind and Name/Namespace/Age for everything else, a health mark per row, sortable headings, a namespace picker and a fuzzy filter over every cell and label.
- The **detail panel** has Overview (per-kind facts plus conditions), Events, YAML (rendered locally from the object already on screen) and, for anything with containers, Logs with a container picker.
- **The table is live.** The list on screen — and only that one — is watched: a thread reads the stream, bookmarks keep the resume point moving while nothing happens, a `410 Gone` re-lists and starts again, and a dropped connection backs off from a second to thirty. A change reaches the row without anybody pressing refresh, and only the rows whose objects moved are formatted again.
- **A log can be followed.** The Logs tab tails a container and keeps reading, with a container picker, a *Previous* toggle for the instance before this one, and a literal find that filters and counts. The scrollback is bounded; the reading is a thread, like a watch's.
- **The detail panel goes places.** A pod's controller and its node are links; a controller's and a service's selector is a link *down* to the pods it selects. And when `metrics.k8s.io` is installed, a pod or a node says what it is using — a node as a share of its allocatable.
- **`⌘K` reaches everything by name** — every kind, every namespace, every context, and the four commands that are none of those — ranked by score, and holding nothing that can destroy anything.
- **Switching context** rebuilds the connection and clears everything the last cluster said. An insecure connection says so in the sidebar.
- **English and Japanese.** Every user-visible string is in `locales/app.yml` in both.
- `KIRIKUMO_DEMO=1` runs the whole window over a scripted cluster with something wrong in it, and no network — including a scripted *watch*, so the demo shows a pod restarting, one arriving and one going away without a cluster anywhere.

What is *not* there yet: any write at all, which is M4.

| # | Name | What lands | Owes |
| --- | --- | --- | --- |
| **M0** | The window | Workspace, tokens, locales, the three-column shell with no title bar, kubeconfig parsing, context list, `/version` handshake, `Scripted` and `KIRIKUMO_DEMO=1` | **Landed.** Visual sign-off against `docs/ui.md` |
| **M1** | Everything is a table | API discovery, the resource tree in the sidebar, one virtualized table for every kind with `kubectl`'s columns, the namespace picker, health marks, the detail's Overview, Events, YAML and Logs | **Landed.** Column sets beyond the built-in kinds — see Q5, which may make that debt disappear rather than be paid; sticky header under scroll |
| **M2** | Live | Watches wired to the store with bookmarks and re-list, rows updated in place rather than rebuilt, `⌘F`/`⌘L`/`⌘K` | **Landed.** Backoff tuning against a real flaky apiserver rather than a scripted one |
| **M3** | Pods in depth | Logs following, with find and the previous instance; `metrics.k8s.io` for nodes and pods; owner/child navigation | **Landed.** Wrapping long log lines (needs a variable-height virtualized list); dropping follow when the reader scrolls up; metrics in the *table* as well as the panel, which needs a column set that depends on what the cluster can answer |
| **M4** | Acting | Delete, scale, restart, cordon/uncordon/drain, apply an edited YAML — each behind a confirmation, each greyed out when `SelfSubjectAccessReview` says no (K6) | |
| **M5** | In Ginka's window | `ClusterPanel` mounted as a surface, the `Cluster` implementation that proxies through Ginka's daemon, and exec/port-forward over WebSocket if Q2 says yes | |

## 6. Quality bars

- **A table of 10 000 rows scrolls at 60 fps.** Every list is one `uniform_list`, and every cell is a string formatted when the object landed, never during a scroll.
- **A watch reconnect is invisible.** A dropped connection must not blank a table or renumber it.
- **No request blocks the window.** Everything through the trait runs on the background executor; a cluster that has gone away shows the last answer and an error line, never a frozen frame.
- **a11y is a rule, not a polish pass.** Every control reachable by mouse is reachable by keyboard with visible focus; health is an icon *and* a colour, never a colour alone.
- **A destructive action takes two deliberate gestures**, and the second one names the object.

## 7. Open questions

| # | Question | Status |
| --- | --- | --- |
| **Q1** | Does the sidebar's resource tree get folded into Ginka's sidebar, or does the cluster surface carry its own tree in the right panel? | Open. The tree is deep enough that Ginka's sidebar may not want it; decide before M5, not during it. |
| **Q2** | Exec and port-forward: WebSocket (`v5.channel.k8s.io`, apiserver ≥ 1.30) with `tungstenite`, or `kube-rs` — and with it tokio? | Open, and deliberately deferred to M5. This is the one place `kube-rs` earns its cost: the channel protocol and the SPDY upgrade are exactly what is miserable to write by hand, and by then the read path will have been in use long enough to say what it is actually missing. Decide *before* M5 starts, not during it. |
| **Q5** | Do the table's columns come from the apiserver instead of from a list in the source? `Accept: application/json;as=Table;g=meta.k8s.io;v=v1` makes the server print `kubectl get`'s columns — including a CRD's own `additionalPrinterColumns`. | Open, and promising: it would turn M1's remaining column debt from work into a deletion, and it needs no new dependency. Against it: the answer is rows of pre-rendered cells rather than objects, so the health mark, the detail panel and the watch still need the objects — which means asking for both, or asking for `Table` only for the kinds we have no columns of our own for. |
| **Q3** | Licence | Open, as in Ginka. Nothing copied in, so nothing is settled by accident. |
| **Q4** | Does a shared `glass-tokens` crate get extracted for the three apps, or does each keep its own copy of `Tokens`? | Extract at unification, not before: three copies of a 400-line file that must stay identical is a smell, but a shared crate before there is a host is speculative. |

## 8. Decision log

| Date | Decision | Why |
| --- | --- | --- |
| 2026-09-07 | Kirikumo is a third window in the Ginka family, and every interface decision is made for the embedded case too (§4.3, K1–K6) | The three become one application. A viewer designed only to stand alone would have to be rewritten to be mounted; the constraints cost nothing now and everything later. |
| 2026-09-07 | **`kube-rs` is not used.** The apiserver is reached with `ureq` and a hand-written discovery/watch layer | `kube` is built on `tokio` through `hyper`, and K3 forbids a second reactor in the process Ginka will host. The parts of `kube` we would use — discovery, unstructured lists, watch framing — are a few hundred lines each against a stable, documented API, and writing them keeps the graph free of a runtime. The cost is real: `kube` handles conformance details we will meet one at a time. |
| 2026-09-07 | Objects are `serde_json::Value` plus a parsed `ObjectMeta`, never generated types | A typed model means a code change per CRD, and CRDs are most of what makes a cluster interesting. `metadata` is the one schema every kind shares, so it is the one thing worth typing. |
| 2026-09-07 | The resource catalogue comes from discovery, not from a list in the source | Same reason. A cluster that serves `argoproj.io/Rollout` gets a table without a release here. |
| 2026-09-07 | No disk cache; nothing about a cluster is written to disk | Kubernetes payloads are large, short-lived and frequently secret, and the apiserver does not do `ETag`s. e1 caches to save GitHub's rate limit, which has no analogue here. Settings are the only thing this app writes. |
| 2026-09-07 | Read-only by default, and every write takes two gestures (K6) | The other two apps in the family act on a working copy. This one acts on production, and it will one day share a window — and a key map — with them. |
| 2026-09-07 | `assets/themes/*.json` is Ginka's file byte for byte, not a Kubernetes-flavoured palette | K5. A surface that brings its own colours is a surface that looks bolted on. |
| 2026-09-07 | **K3 is relaxed**: a second async runtime is allowed when it is recorded here and sealed behind the `Cluster` implementation | The original wording — no second reactor, ever — was stronger than Ginka's own rule, which asks for a decision-log entry rather than abstinence. The real constraint is not "one runtime per process" but "a future must be polled by the runtime it was built for", and that is a property of an implementation, not of a workspace. Writing it as a ban would have made a reasonable future choice look like a violation. |
| 2026-09-07 | **`kube-rs` is revisited at M5, and not before** | The value of adopting it is very unevenly distributed. For the read path it is near zero *now*: discovery, unstructured lists, watch framing and the kubeconfig layer are written, tested and about 1,900 lines, and swapping them would not change a pixel. For exec and port-forward it is very high, and that is M5 (Q2). So the read path stays as it is, the question is asked once, at the point where the answer matters, and `Cluster` keeps the swap contained to one implementation if the answer is yes. |
| 2026-09-07 | One watch at a time, for the list on screen, and its retry policy lives on the reading thread | Only one table is showing, so following anything else is streaming a cluster nobody is looking at (§4.7). Putting the reconnect on the thread keeps the store's part down to the one thing a connection cannot decide — that a `410 Gone` means list again — and the thread is told to stop by a flag it can only notice at its next event, which is why the apiserver's watch timeout was cut from half an hour to five minutes. |
| 2026-09-07 | A row is reused when its object's `resourceVersion` has not moved | A watch event changes one object. Rebuilding four thousand rows for it — seven formatted cells each, tens of times a second on a busy namespace — is how a live table becomes a space heater. The version is the cheapest possible proof that nothing changed. |
| 2026-09-07 | Going *down* from a controller is the filter box, not a query | "Which pods does this select?" has no apiserver endpoint — a label selector is a list parameter, not a resource — and building one into the table would be a second query language beside the filter that is already there. Putting the controller's first selector label into the filter box answers the question the way a person would, and leaves them holding the thing that narrowed the table so they can clear it. The cost is that a multi-label selector becomes its first label, which in practice narrows a namespace just as far. |
| 2026-09-07 | The log's find box is literal, not fuzzy — the only box in the window that is | Fuzzy matching over a hundred thousand lines finds every line containing those letters in that order, which is every line. What people do to a log is `grep`, and a filter with a `matched/total` count beside it is that. Highlighting and stepping through matches was the alternative; it needs styled runs inside a line and scroll targeting, and answers a question — "where is the next one?" — that filtering makes not arise. |
| 2026-09-07 | No wrap toggle for logs until the list can have rows of different heights | A wrapped line is a taller row, and the whole reason a fifty-thousand-line scrollback is affordable is that every row is the same height (rule 7). Wrapping by dropping virtualization for the wrapped case would put four thousand elements on screen, which is the thing rule 7 exists to prevent. So: a long line scrolls sideways, and wrapping waits for a variable-height list. |
| 2026-09-07 | A log's scrollback is bounded at 50 000 lines, oldest first | A pod that has been logging for a week would otherwise be held whole in a window nobody closed. Oldest first because the reason anyone follows a log is what happens next. The same shape as a terminal's scrollback in Ginka. |
