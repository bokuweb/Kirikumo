# CLAUDE.md

**This project's agent instructions live in [`AGENTS.md`](AGENTS.md). Read it first, then [`docs/roadmap.md`](docs/roadmap.md) — and [`docs/ui.md`](docs/ui.md) for anything that renders.**

Quick orientation:

- **Kirikumo** is a native Kubernetes viewer in Rust on **GPUI** — Lens's job, as a single binary that installs nothing in the cluster. Everything in it is written here; Lens, k9s, Headlamp and `kubectl` are read for behaviour and never copied.
- It is the third window in a family that becomes **one application**: [`ginka`](https://github.com/bokuweb/ginka) is the host, [`e1`](https://github.com/bokuweb/e1) (GitHub) is a surface in it, and this is another. The six constraints that follow are `docs/roadmap.md` §4.3, as **K1–K6** — a library of views, `Arc<dyn Cluster>` as the only path to the network, one reactor unless a second is recorded and sealed behind that trait, Ginka's locked `gpui` rev, Ginka's tokens, and no write without two gestures.
- Architecture in one line: one process, no daemon of our own; blocking `ureq` on GPUI's background executor, a watch on a thread of its own, and `kirikumo-views → kirikumo-ui → kirikumo-kube`.
- The model is **generic on purpose**: an object is `serde_json::Value` plus its `ObjectMeta`, and the resource tree comes from API discovery — so a CRD gets a table without a release. Never add a typed model per kind.
- **`kube-rs` is not a dependency today**, because it is built on tokio and the read path it would replace is already written and tested. It is not forbidden: K3 asks that a second runtime be *recorded* and sealed behind the `Cluster` implementation, and the question is asked once more at M5, where exec and port-forward make it worth its cost (roadmap Q2). Do not add it before then, and do not add it without a decision-log entry.
- **Nothing about a cluster is written to disk** — no cache, no snapshot, no credential. Settings are all this app writes.
- Everything written into this repository is **English** — code, rustdoc comments (`///` on every public item), docs, commit messages and PR titles/descriptions.

When a change alters architecture, data model or scope, update `docs/roadmap.md` — including its decision log — in the same change.
