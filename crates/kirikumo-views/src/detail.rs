//! The right panel: `docs/ui.md` §3.4.
//!
//! Four tabs over one object — Overview, Events, YAML, and Logs for anything
//! with containers. What each of them says is decided in `kirikumo_ui`
//! (`detail::overview`) or in `kirikumo_kube` (`yaml::to_yaml`); this file
//! draws it.
//!
//! The object itself is read out of the list the table is already showing
//! rather than fetched again: it arrived a moment ago, and a `GET` for it
//! would put a spinner over data the window already has.

use crate::store::{ObjectKey, Store, StoreEvent, Write};
use chrono::Utc;
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_component::tooltip::Tooltip;
use gpui_component::{Icon, StyledExt as _, h_flex, v_flex};
use kirikumo_kube::{Action, ExecRequest, LogRequest, Object, actions, yaml};
use kirikumo_ui::actions::{Pending, confirm_label};
use kirikumo_ui::assets::icon;
use kirikumo_ui::detail::Target;
use kirikumo_ui::{Tokens, detail, logs, time};
use serde_json::Value;

/// How tall one line of YAML or of a log is.
const LINE_HEIGHT: Pixels = px(17.);

/// Which tab is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    /// The facts.
    Overview,
    /// What has happened to it.
    Events,
    /// The object as the apiserver holds it.
    Yaml,
    /// A container's output.
    Logs,
    /// A command, run in a container.
    Run,
}

impl Tab {
    /// The locale key for the tab's name.
    fn label_key(self) -> &'static str {
        match self {
            Self::Overview => "detail.overview",
            Self::Events => "detail.events",
            Self::Yaml => "detail.yaml",
            Self::Logs => "detail.logs",
            Self::Run => "detail.run",
        }
    }
}

/// What the reader did in the panel.
pub enum DetailEvent {
    /// Go to something this object points at: up to its controller, or down
    /// to what its selector selects.
    Navigate(Target),
}

impl EventEmitter<DetailEvent> for Detail {}

/// The right panel.
pub struct Detail {
    store: Entity<Store>,
    key: Option<ObjectKey>,
    tab: Tab,
    /// Which container's log is showing, when the object has more than one.
    container: Option<String>,
    /// Whether the log is being followed as the container writes.
    following: bool,
    /// Whether to read the *previous* instance's log, which is the only place
    /// a crash loop's reason survives.
    previous: bool,
    /// The find box over the log.
    find: Entity<InputState>,
    scroll: UniformListScrollHandle,
    /// How many lines the log had at the last frame, so that following can
    /// tell "something arrived" from "nothing did".
    last_lines: usize,
    /// A write between its first gesture and its second.
    pending: Pending,
    /// The replica count field, for a scale.
    replicas: Entity<InputState>,
    /// The YAML tab: the toolkit's editor, read-only until *Edit*.
    editor: Entity<EditorState>,
    /// Whether the YAML is being edited, which is when the editor's text is
    /// the reader's and must not be replaced by the object's.
    editing: bool,
    /// Which object and version the editor was last filled from, so it is
    /// refilled when either changes and left alone otherwise.
    editor_holds: Option<(ObjectKey, String)>,
    /// Why an edited manifest was refused before it was sent, if it was.
    apply_error: Option<String>,
    /// Why the last forward could not be started, if it could not.
    forward_error: Option<String>,
    /// The command field on the Run tab.
    command: Entity<InputState>,
}

impl Detail {
    /// A panel over a store, showing nothing until told what to.
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let find = cx.new(|cx| {
            InputState::new(window, cx).placeholder(rust_i18n::t!("detail.find").to_string())
        });
        cx.subscribe(&find, |_, _, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                cx.notify();
            }
        })
        .detach();
        let replicas = cx.new(|cx| {
            InputState::new(window, cx).placeholder(rust_i18n::t!("action.replicas").to_string())
        });
        cx.subscribe(&replicas, |this, replicas, event: &InputEvent, cx| {
            if let InputEvent::Change = event
                && let Pending::Armed {
                    action: Action::Scale,
                    replicas: typed,
                } = &mut this.pending
            {
                *typed = replicas.read(cx).value().to_string();
                cx.notify();
            }
        })
        .detach();
        let editor = cx.new(|cx| EditorState::new(window, cx).language("yaml"));
        let command = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rust_i18n::t!("detail.run_placeholder").to_string())
        });
        cx.subscribe(&command, |this, _, event: &InputEvent, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.run_command(cx);
            }
        })
        .detach();
        // Asked again on every change, not just when a tab is opened: an
        // object reached by a link is drawn before the list it lives in has
        // landed, so the moment it does the tab has to ask for its events.
        // `ensure` is idempotent — it starts a fetch only when one is idle.
        cx.subscribe(&store, |this: &mut Self, _, _: &StoreEvent, cx| {
            this.ensure(cx);
            cx.notify();
        })
        .detach();
        Self {
            store,
            key: None,
            tab: Tab::Overview,
            container: None,
            // Following is what a person opening a log wants; a tail that
            // stops the moment it is drawn is a screenshot.
            following: true,
            previous: false,
            find,
            scroll: UniformListScrollHandle::new(),
            last_lines: 0,
            pending: Pending::Idle,
            replicas,
            editor,
            editing: false,
            editor_holds: None,
            apply_error: None,
            forward_error: None,
            command,
        }
    }

    /// Show an object.
    pub fn show(&mut self, key: ObjectKey, cx: &mut Context<Self>) {
        if self.key.as_ref() != Some(&key) {
            // A different object: the tab stays, because a reader stepping
            // down a list of pods with the YAML tab open wants the next
            // pod's YAML — but the container does not, since it named one
            // of the last object's.
            self.container = None;
        }
        self.key = Some(key);
        // A write armed on one object must not fire on the next.
        self.pending = Pending::Idle;
        self.editing = false;
        self.apply_error = None;
        self.forward_error = None;
        self.stop_following(cx);
        self.ensure(cx);
        cx.notify();
    }

    /// Nothing is open.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.key = None;
        self.stop_following(cx);
        cx.notify();
    }

    /// Stop reading a log, which every move away from one has to do: a
    /// followed log is a thread and a connection, and nothing else on screen
    /// needs either.
    fn stop_following(&mut self, cx: &mut Context<Self>) {
        self.store.update(cx, |store, _| store.stop_following_log());
        self.last_lines = 0;
    }

    /// Fetch again whatever the open tab is showing.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if self.tab == Tab::Logs {
            self.reload_log(cx);
            return;
        }
        self.ensure(cx);
    }

    fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        if self.tab == Tab::Logs && tab != Tab::Logs {
            self.stop_following(cx);
        }
        let entering_logs = tab == Tab::Logs && self.tab != Tab::Logs;
        self.tab = tab;
        // Coming *back* to a log has to start following again, and `ensure`
        // will not: it asks only for what has never been asked for, and this
        // log's lines are still here from last time.
        match entering_logs {
            true => self.reload_log(cx),
            false => self.ensure(cx),
        }
        cx.notify();
    }

    /// Open a tab by name. For demos and screenshots
    /// (`KIRIKUMO_DEMO_OPEN=…#yaml`); an unknown name is the Overview.
    pub fn show_tab_named(&mut self, name: &str, cx: &mut Context<Self>) {
        let tab = match name {
            "events" => Tab::Events,
            "yaml" => Tab::Yaml,
            "logs" => Tab::Logs,
            "run" => Tab::Run,
            _ => Tab::Overview,
        };
        self.set_tab(tab, cx);
    }

    /// Ask for the log again under whatever the toggles now say.
    fn reload_log(&mut self, cx: &mut Context<Self>) {
        let Some(request) = self.log_request(cx) else {
            return;
        };
        let following = self.following;
        self.last_lines = 0;
        self.store
            .update(cx, |store, cx| store.show_log(request, following, cx));
        cx.notify();
    }

    /// Ask for whatever the open tab needs and does not have.
    fn ensure(&mut self, cx: &mut Context<Self>) {
        let Some(object) = self.object(cx) else {
            return;
        };
        // Usage is on the Overview, which is the tab this panel opens on, and
        // it is one request per namespace rather than per object.
        let kind = self.kind();
        let namespace = object.meta.namespace.clone();
        self.store.update(cx, |store, cx| {
            store.ensure_metrics(&kind, namespace.as_deref(), cx)
        });
        // And whether this login may do each thing the footer offers, so a
        // button is lit or grey before it is pressed rather than after.
        if let Some((key, resource)) =
            self.key
                .as_ref()
                .map(|(key, _, _)| key.clone())
                .and_then(|key| {
                    let resource = self.store.read(cx).resource(&key).cloned()?;
                    Some((key, resource))
                })
        {
            let verbs: Vec<&'static str> = actions::available(&resource, &object)
                .into_iter()
                .map(Action::verb)
                .collect();
            let namespace = namespace.filter(|_| resource.namespaced);
            self.store.update(cx, |store, cx| {
                for verb in verbs {
                    store.ensure_permission(key.clone(), namespace.clone(), verb, cx);
                }
            });
        }
        match self.tab {
            Tab::Events => {
                let uid = object.meta.uid.clone();
                let namespace = object.meta.namespace.clone();
                self.store
                    .update(cx, |store, cx| store.ensure_events(uid, namespace, cx));
            }
            Tab::Logs => {
                if let Some(request) = self.log_request(cx) {
                    let following = self.following;
                    // Only when nothing has been asked for yet: `ensure` runs
                    // on every store change, and restarting a followed log on
                    // each of its own lines would be a loop.
                    let asked = self
                        .store
                        .read(cx)
                        .logs(&Store::log_key(&request))
                        .is_some_and(|fetch| !fetch.is_idle());
                    if !asked {
                        self.store
                            .update(cx, |store, cx| store.show_log(request, following, cx));
                    }
                }
            }
            Tab::Run => {
                let namespace = object.meta.namespace.clone();
                self.store
                    .update(cx, |store, cx| store.ensure_exec_permission(namespace, cx));
            }
            Tab::Overview | Tab::Yaml => {}
        }
    }

    /// The kind being shown.
    fn kind(&self) -> String {
        self.key
            .as_ref()
            .map(|(kind, _, _)| kind.kind.clone())
            .unwrap_or_default()
    }

    /// The object being shown, out of the store's lists.
    fn object(&self, cx: &App) -> Option<Object> {
        let key = self.key.as_ref()?;
        self.store.read(cx).object(key).cloned()
    }

    /// The containers this object has, if it is the kind that has any.
    fn containers(&self, cx: &App) -> Vec<String> {
        self.object(cx)
            .map(|object| {
                object
                    .array_at("spec.containers")
                    .iter()
                    .filter_map(|container| container.get("name").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// What to ask for on the Logs tab.
    fn log_request(&self, cx: &App) -> Option<LogRequest> {
        let object = self.object(cx)?;
        let namespace = object.meta.namespace.clone()?;
        let containers = self.containers(cx);
        let container = self
            .container
            .clone()
            .or_else(|| containers.first().cloned())?;
        Some(
            LogRequest::new(namespace, object.meta.name.clone())
                .container(container)
                .previous(self.previous),
        )
    }

    /// The name, the kind and the health, across the top.
    fn header(&self, object: &Object, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let tokens = Tokens::global(cx).clone();
        let kind = self.kind();
        let health = kirikumo_kube::health::of(&kind, object);
        let mut where_it_is = kind.clone();
        if let Some(namespace) = &object.meta.namespace {
            where_it_is.push_str(" · ");
            where_it_is.push_str(namespace);
        }

        v_flex()
            .w_full()
            .px_4()
            .py_3()
            .gap_1p5()
            .border_b_1()
            .border_color(tokens.colors().border_subtle)
            .child(
                div()
                    .text_size(px(15.))
                    .font_medium()
                    .font_family("monospace")
                    .text_color(tokens.colors().text_primary)
                    .child(object.meta.name.clone()),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(tokens.colors().text_muted)
                            .child(where_it_is),
                    )
                    .when(!health.word.is_empty(), |this| {
                        this.child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .child(
                                    Icon::empty()
                                        .path(icon::health(health.level))
                                        .size(px(8.))
                                        .text_color(tokens.colors().health(health.level)),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.5))
                                        .text_color(tokens.colors().text_secondary)
                                        .child(health.word.clone()),
                                ),
                        )
                    }),
            )
    }

    /// The tab chips.
    fn tabs(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let tokens = Tokens::global(cx).clone();
        let has_containers = !self.containers(cx).is_empty();
        let tabs: Vec<Tab> = match has_containers {
            true => vec![Tab::Overview, Tab::Events, Tab::Yaml, Tab::Logs, Tab::Run],
            false => vec![Tab::Overview, Tab::Events, Tab::Yaml],
        };
        h_flex()
            .w_full()
            .px_3()
            .py_2()
            .gap_1()
            .children(tabs.into_iter().enumerate().map(|(index, tab)| {
                let selected = tab == self.tab;
                div()
                    .id(("tab", index))
                    .px_2p5()
                    .py_1()
                    .rounded(px(tokens.radius.control()))
                    .cursor_pointer()
                    .text_size(px(11.5))
                    .when(selected, |this| {
                        this.bg(tokens.colors().row_active())
                            .text_color(tokens.colors().text_primary)
                    })
                    .when(!selected, |this| {
                        this.text_color(tokens.colors().text_muted)
                    })
                    .hover(|this| this.bg(tokens.colors().row_hover()))
                    .child(rust_i18n::t!(tab.label_key()).to_string())
                    .on_click(cx.listener(move |this, _, _, cx| this.set_tab(tab, cx)))
            }))
    }

    /// The Overview tab.
    fn overview(&self, object: &Object, cx: &mut Context<Self>) -> AnyElement {
        let tokens = Tokens::global(cx).clone();
        let kind = self.kind();
        let usage = self
            .store
            .read(cx)
            .metrics_for(&kind, object.meta.namespace.as_deref(), &object.meta.name)
            .cloned();
        let overview = detail::overview(&kind, object, usage.as_ref(), Utc::now());

        v_flex()
            .id("overview")
            .size_full()
            .px_4()
            .py_3()
            .gap_4()
            .overflow_y_scroll()
            .children(overview.sections.into_iter().map(|section| {
                v_flex()
                    .w_full()
                    .gap_1p5()
                    .children(section.title.map(|title| {
                        div()
                            .text_size(px(10.5))
                            .text_color(tokens.colors().text_muted)
                            .child(title)
                    }))
                    .children(section.facts.into_iter().enumerate().map(|(index, fact)| {
                        let link = fact.link.clone();
                        h_flex()
                            .w_full()
                            .gap_3()
                            .items_start()
                            .child(
                                div()
                                    .w(px(104.))
                                    .flex_shrink_0()
                                    .text_size(px(11.5))
                                    .text_color(tokens.colors().text_muted)
                                    .child(fact.label),
                            )
                            .child(
                                div()
                                    .id(("fact", index))
                                    .flex_1()
                                    .text_size(px(12.))
                                    .when(fact.mono, |this| this.font_family("monospace"))
                                    // A fact that goes somewhere is drawn as a
                                    // link, in the accent, and never as an
                                    // underline in the muted grey that reads
                                    // as struck out.
                                    .when(link.is_some(), |this| {
                                        this.cursor_pointer()
                                            .text_color(tokens.colors().accent)
                                            .hover(|this| {
                                                this.text_color(tokens.colors().accent.opacity(0.8))
                                            })
                                    })
                                    .when(link.is_none(), |this| {
                                        this.text_color(tokens.colors().text_secondary)
                                    })
                                    .child(fact.value)
                                    .on_click(cx.listener(move |_, _, _, cx| {
                                        if let Some(target) = link.clone() {
                                            cx.emit(DetailEvent::Navigate(target));
                                        }
                                    })),
                            )
                    }))
            }))
            .when(kind == "Pod", |this| this.child(self.forwards(object, cx)))
            .when(!overview.conditions.is_empty(), |this| {
                this.child(
                    v_flex()
                        .w_full()
                        .gap_1p5()
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(tokens.colors().text_muted)
                                .child(rust_i18n::t!("detail.conditions").to_string()),
                        )
                        .children(overview.conditions.into_iter().map(|condition| {
                            h_flex()
                                .w_full()
                                .gap_2()
                                .items_center()
                                .child(
                                    Icon::empty()
                                        .path(icon::health(condition.level))
                                        .size(px(8.))
                                        .text_color(tokens.colors().health(condition.level)),
                                )
                                .child(
                                    div()
                                        .w(px(140.))
                                        .flex_shrink_0()
                                        .text_size(px(12.))
                                        .font_family("monospace")
                                        .text_color(tokens.colors().text_secondary)
                                        .truncate()
                                        .child(condition.kind),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .text_size(px(11.5))
                                        .text_color(tokens.colors().text_muted)
                                        .truncate()
                                        .child(match condition.reason.is_empty() {
                                            true => condition.status,
                                            false => condition.reason,
                                        }),
                                )
                        })),
                )
            })
            .into_any_element()
    }

    /// Start a forward, remembering why it could not be if it could not.
    fn start_forward(&mut self, remote: u16, cx: &mut Context<Self>) {
        let Some(key) = self.key.clone() else {
            return;
        };
        self.forward_error = self
            .store
            .update(cx, |store, cx| store.start_forward(key, remote, cx))
            .err();
        cx.notify();
    }

    /// The pod's ports, and the forwards open to them.
    ///
    /// Not a write in K6's sense — nothing in the cluster changes — but it
    /// opens a port on this machine, so it is one deliberate click on a
    /// chip that names the port, and the strip says what is open.
    fn forwards(&self, object: &Object, cx: &mut Context<Self>) -> AnyElement {
        let tokens = Tokens::global(cx).clone();
        let key = self.key.clone();
        let ports = detail::container_ports(object);
        let active: Vec<(u16, u16, usize, Option<String>)> = key
            .as_ref()
            .map(|key| {
                self.store
                    .read(cx)
                    .forwards_for(key)
                    .map(|forward| {
                        (
                            forward.remote,
                            forward.local(),
                            forward.forwarder.open_connections(),
                            forward.forwarder.last_error(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();

        v_flex()
            .w_full()
            .gap_1p5()
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(tokens.colors().text_muted)
                    .child(rust_i18n::t!("detail.forward").to_string()),
            )
            .when(ports.is_empty(), |this| {
                this.child(
                    div()
                        .text_size(px(11.5))
                        .text_color(tokens.colors().text_muted)
                        .child(rust_i18n::t!("detail.forward_none").to_string()),
                )
            })
            .when(!ports.is_empty(), |this| {
                this.child(h_flex().gap_1().flex_wrap().children(ports.into_iter().map(
                    |(remote, label)| {
                        let forwarding = active.iter().any(|(port, ..)| *port == remote);
                        self.button(
                            "forward-port",
                            label,
                            !forwarding,
                            false,
                            cx,
                            move |this, _, cx| this.start_forward(remote, cx),
                        )
                    },
                )))
            })
            .children(active.into_iter().map(|(remote, local, open, error)| {
                let address = format!("localhost:{local}");
                let copied = address.clone();
                h_flex()
                    .w_full()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(12.))
                            .font_family("monospace")
                            .text_color(tokens.colors().text_secondary)
                            .truncate()
                            .child(format!("{address} → {remote}")),
                    )
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(match error.is_some() {
                                true => tokens.colors().status_error,
                                false => tokens.colors().text_muted,
                            })
                            .child(match error {
                                Some(error) => error,
                                None => {
                                    rust_i18n::t!("detail.forward_open", count = open).to_string()
                                }
                            }),
                    )
                    .child(self.button(
                        "forward-copy",
                        rust_i18n::t!("detail.forward_copy").to_string(),
                        true,
                        false,
                        cx,
                        move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(copied.clone()));
                        },
                    ))
                    .child(self.button(
                        "forward-stop",
                        rust_i18n::t!("detail.forward_stop").to_string(),
                        true,
                        false,
                        cx,
                        move |this, _, cx| {
                            if let Some(key) = this.key.clone() {
                                this.store
                                    .update(cx, |store, cx| store.stop_forward(&key, remote, cx));
                            }
                        },
                    ))
            }))
            .children(self.forward_error.clone().map(|error| {
                div()
                    .text_size(px(11.5))
                    .text_color(tokens.colors().status_error)
                    .child(error)
            }))
            .into_any_element()
    }

    /// The Events tab.
    fn events(&self, object: &Object, cx: &mut Context<Self>) -> AnyElement {
        let tokens = Tokens::global(cx).clone();
        let now = Utc::now();
        let fetch = self.store.read(cx).events(&object.meta.uid);
        let events = fetch.and_then(|fetch| fetch.value()).cloned();
        let loading = fetch.is_some_and(|fetch| fetch.is_loading());
        let error = fetch.and_then(|fetch| fetch.error()).map(str::to_string);

        let Some(events) = events.filter(|events| !events.is_empty()) else {
            return match (error, loading) {
                (Some(error), _) => self.notice(error, true, cx),
                (None, true) => crate::skeleton::detail(cx),
                (None, false) => {
                    self.notice(rust_i18n::t!("detail.events_empty").to_string(), false, cx)
                }
            };
        };

        v_flex()
            .id("events")
            .size_full()
            .px_4()
            .py_3()
            .gap_2p5()
            .overflow_y_scroll()
            .children(events.into_iter().map(|event| {
                let level = match event.is_warning() {
                    true => kirikumo_kube::Level::Attention,
                    false => kirikumo_kube::Level::Ok,
                };
                v_flex()
                    .w_full()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                Icon::empty()
                                    .path(icon::health(level))
                                    .size(px(8.))
                                    .text_color(tokens.colors().health(level)),
                            )
                            .child(
                                div()
                                    .text_size(px(11.5))
                                    .font_family("monospace")
                                    .text_color(tokens.colors().text_secondary)
                                    .child(event.reason.clone()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(11.))
                                    .text_color(tokens.colors().text_muted)
                                    .child(match event.count > 1 {
                                        true => format!(
                                            "{} · ×{}",
                                            time::age(event.last, now),
                                            event.count
                                        ),
                                        false => time::age(event.last, now),
                                    }),
                            ),
                    )
                    .child(
                        div()
                            .pl(px(16.))
                            .text_size(px(12.))
                            .text_color(tokens.colors().text_secondary)
                            .child(event.message.clone()),
                    )
            }))
            .into_any_element()
    }

    /// The YAML tab: the object as the apiserver holds it, in the toolkit's
    /// editor — read-only until *Edit*, and then the reader's until *Apply*
    /// or *Cancel*.
    fn yaml(&mut self, object: &Object, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let tokens = Tokens::global(cx).clone();
        let Some(key) = self.key.clone() else {
            return div().into_any_element();
        };
        // Refilled when the object or its version changes, and never while
        // the text is the reader's: a watch event landing mid-edit must not
        // throw their edit away.
        let holds = (key.clone(), object.meta.resource_version.clone());
        if !self.editing && self.editor_holds.as_ref() != Some(&holds) {
            let text = yaml::to_yaml(&object.raw);
            self.editor
                .update(cx, |editor, cx| editor.set_value(text, window, cx));
            self.editor_holds = Some(holds);
        }

        let editing = self.editing;
        let armed = self.pending.action() == Some(Action::Apply);
        let may_apply = self.permission(Action::Apply, cx) == Some(true);
        let writing = self
            .store
            .read(cx)
            .write(&key)
            .is_some_and(|w| w.is_loading());
        let name = object.meta.name.clone();

        let strip = h_flex()
            .w_full()
            .px_3()
            .py_1p5()
            .gap_1()
            .flex_shrink_0()
            .items_center()
            .justify_end()
            .when(!editing, |this| {
                this.child(self.button(
                    "edit",
                    rust_i18n::t!("action.edit").to_string(),
                    may_apply && !writing,
                    false,
                    cx,
                    |this, _, cx| {
                        this.editing = true;
                        cx.notify();
                    },
                ))
            })
            .when(editing && !armed, |this| {
                this.child(self.button(
                    "apply",
                    rust_i18n::t!("action.apply").to_string(),
                    !writing,
                    false,
                    cx,
                    |this, _, cx| this.arm(Action::Apply, cx),
                ))
            })
            .when(editing && armed, |this| {
                this.child(self.button(
                    "confirm-apply",
                    confirm_label(Action::Apply, &name, None),
                    !writing,
                    false,
                    cx,
                    |this, window, cx| this.confirm(window, cx),
                ))
            })
            .when(editing, |this| {
                this.child(self.button(
                    "cancel-edit",
                    rust_i18n::t!("action.cancel").to_string(),
                    true,
                    false,
                    cx,
                    |this, _, cx| {
                        this.editing = false;
                        this.pending = Pending::Idle;
                        // Forget what the editor holds, so the next frame
                        // refills it from the object.
                        this.editor_holds = None;
                        cx.notify();
                    },
                ))
            });

        v_flex()
            .size_full()
            .child(strip)
            .child(
                div().flex_1().min_h_0().w_full().px_2().pb_2().child(
                    Editor::new(&self.editor)
                        .readonly(!editing)
                        .bordered(editing)
                        .h(relative(1.))
                        .text_size(px(12.)),
                ),
            )
            .when(!editing, |this| {
                // A hint that the text is the apiserver's, not the reader's.
                this.child(
                    div()
                        .px_3()
                        .pb_1p5()
                        .text_size(px(10.5))
                        .text_color(tokens.colors().text_muted)
                        .child(rust_i18n::t!("detail.yaml_readonly").to_string()),
                )
            })
            .into_any_element()
    }

    /// Whether this login may do an action on the object shown, if the
    /// cluster has said.
    fn permission(&self, action: Action, cx: &App) -> Option<bool> {
        let (key, namespace, _) = self.key.as_ref()?;
        let store = self.store.read(cx);
        let namespaced = store
            .resource(key)
            .is_none_or(|resource| resource.namespaced);
        let namespace = namespace.as_deref().filter(|_| namespaced);
        store.permission(key, namespace, action.verb())
    }

    /// The first gesture.
    fn arm(&mut self, action: Action, cx: &mut Context<Self>) {
        let current = self
            .object(cx)
            .map(|object| actions::current_replicas(&object))
            .unwrap_or(1);
        self.pending = Pending::arm(action, current);
        cx.notify();
    }

    /// Back to nothing armed.
    fn disarm(&mut self, cx: &mut Context<Self>) {
        self.pending = Pending::Idle;
        cx.notify();
    }

    /// The second gesture: build the write and send it.
    fn confirm(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(action) = self.pending.action() else {
            return;
        };
        if !self.pending.can_confirm() {
            return;
        }
        let Some(key) = self.key.clone() else {
            return;
        };
        let write = match action {
            Action::Delete => Write::Delete,
            Action::Scale => match self.pending.replicas() {
                Some(count) => Write::Patch(actions::scale(count)),
                None => return,
            },
            Action::Restart => Write::Patch(actions::restart(Utc::now())),
            Action::Cordon => Write::Patch(actions::schedulable(true)),
            Action::Uncordon => Write::Patch(actions::schedulable(false)),
            Action::Drain => Write::Drain,
            Action::Apply => {
                let text = self.editor.read(cx).value().to_string();
                match actions::apply(&text) {
                    Ok(patch) => Write::Patch(patch),
                    Err(error) => {
                        // Refused here, before anything is sent: the reason
                        // goes where the apiserver's would.
                        self.apply_error = Some(kirikumo_ui::fetch::describe(&error));
                        self.pending = Pending::Idle;
                        cx.notify();
                        return;
                    }
                }
            }
        };
        self.apply_error = None;
        self.pending = Pending::Idle;
        // A cluster-scoped kind is written without a namespace, whatever the
        // detail was opened with.
        let namespaced = self
            .store
            .read(cx)
            .resource(&key.0)
            .is_none_or(|resource| resource.namespaced);
        let target = (
            key.0.clone(),
            key.1.clone().filter(|_| namespaced),
            key.2.clone(),
        );
        self.editing = false;
        self.editor_holds = None;
        self.store
            .update(cx, |store, cx| store.perform(target, write, cx));
        cx.notify();
    }

    /// A button in the footer or the YAML strip.
    ///
    /// Greyed rather than hidden when it cannot be pressed: a control that
    /// vanishes leaves the reader wondering whether the thing can be done at
    /// all, and one that is grey with a tooltip says exactly why not.
    fn button(
        &self,
        id: &'static str,
        label: String,
        enabled: bool,
        destructive: bool,
        cx: &mut Context<Self>,
        act: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let tokens = Tokens::global(cx).clone();
        let tip = rust_i18n::t!("action.forbidden").to_string();
        div()
            .id(id)
            .px_2p5()
            .py_1()
            .flex_shrink_0()
            .rounded(px(tokens.radius.control()))
            .text_size(px(11.5))
            .when(enabled && destructive, |this| {
                this.cursor_pointer()
                    .bg(tokens.colors().status_error.opacity(0.18))
                    .text_color(tokens.colors().status_error)
                    .hover(|this| this.bg(tokens.colors().status_error.opacity(0.3)))
            })
            .when(enabled && !destructive, |this| {
                this.cursor_pointer()
                    .bg(tokens.colors().bg_surface)
                    .text_color(tokens.colors().text_primary)
                    .hover(|this| this.bg(tokens.colors().surface_hover()))
            })
            .when(!enabled, |this| {
                this.bg(tokens.colors().bg_surface.opacity(0.5))
                    .text_color(tokens.colors().text_muted)
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
            })
            .child(label)
            .when(enabled, |this| {
                this.on_click(cx.listener(move |this, _, window, cx| act(this, window, cx)))
            })
    }

    /// The actions strip at the foot of the panel: the first gesture, then
    /// the second, then what the apiserver said.
    fn footer(&self, object: &Object, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tokens = Tokens::global(cx).clone();
        let key = self.key.clone()?;
        let resource = self.store.read(cx).resource(&key.0).cloned()?;
        // Apply lives on the YAML tab, beside the text it applies.
        let available: Vec<Action> = actions::available(&resource, object)
            .into_iter()
            .filter(|action| *action != Action::Apply)
            .collect();
        if available.is_empty() {
            return None;
        }
        let write = self.store.read(cx).write(&key).cloned();
        let working = write.as_ref().is_some_and(|write| write.is_loading());
        // What a landed write had to say: nothing for most, a drain's
        // `3 evicted · 1 skipped` for a drain.
        let landed = write
            .as_ref()
            .and_then(|write| write.value())
            .filter(|line| !line.is_empty())
            .cloned();
        let refused = self.apply_error.clone().or_else(|| {
            write
                .as_ref()
                .and_then(|write| write.error())
                .map(str::to_string)
        });
        let name = object.meta.name.clone();
        let armed = self.pending.action();

        let controls: Vec<AnyElement> = match armed {
            None => available
                .iter()
                .map(|action| {
                    let action = *action;
                    let allowed = self.permission(action, cx) == Some(true);
                    self.button(
                        match action {
                            Action::Scale => "act-scale",
                            Action::Restart => "act-restart",
                            Action::Cordon => "act-cordon",
                            Action::Uncordon => "act-uncordon",
                            Action::Drain => "act-drain",
                            Action::Apply => "act-apply",
                            Action::Delete => "act-delete",
                        },
                        rust_i18n::t!(action.label_key()).to_string(),
                        allowed && !working,
                        action.is_destructive(),
                        cx,
                        move |this, _, cx| this.arm(action, cx),
                    )
                    .into_any_element()
                })
                .collect(),
            Some(action) => {
                let mut controls = Vec::new();
                if action == Action::Scale {
                    controls.push(
                        div()
                            .w(px(72.))
                            .flex_shrink_0()
                            .child(Input::new(&self.replicas))
                            .into_any_element(),
                    );
                }
                controls.push(
                    self.button(
                        "confirm",
                        confirm_label(action, &name, self.pending.replicas()),
                        self.pending.can_confirm() && !working,
                        action.is_destructive(),
                        cx,
                        |this, window, cx| this.confirm(window, cx),
                    )
                    .into_any_element(),
                );
                controls.push(
                    self.button(
                        "cancel",
                        rust_i18n::t!("action.cancel").to_string(),
                        true,
                        false,
                        cx,
                        |this, _, cx| this.disarm(cx),
                    )
                    .into_any_element(),
                );
                controls
            }
        };

        Some(
            v_flex()
                .w_full()
                .flex_shrink_0()
                .border_t_1()
                .border_color(tokens.colors().border_subtle)
                .child(
                    h_flex()
                        .w_full()
                        .px_3()
                        .py_2()
                        .gap_1p5()
                        .items_center()
                        .flex_wrap()
                        .children(controls)
                        .when(working, |this| {
                            this.child(
                                div()
                                    .text_size(px(11.5))
                                    .text_color(tokens.colors().text_muted)
                                    .child(rust_i18n::t!("action.working").to_string()),
                            )
                        }),
                )
                .children(refused.map(|error| {
                    div()
                        .px_3()
                        .pb_2()
                        .text_size(px(11.5))
                        .text_color(tokens.colors().status_error)
                        .child(error)
                }))
                .children(landed.map(|line| {
                    div()
                        .px_3()
                        .pb_2()
                        .text_size(px(11.5))
                        .text_color(tokens.colors().text_secondary)
                        .child(line)
                }))
                .into_any_element(),
        )
    }

    /// Run what is in the command field, in the chosen container.
    ///
    /// The command is the reader's own words, typed, which is the deliberate
    /// act; the run itself is one press. It is not a K6 write to the cluster
    /// — nothing in the apiserver changes — but it can change a container,
    /// so the review for `pods/exec` gates it like any write.
    fn run_command(&mut self, cx: &mut Context<Self>) {
        let line = self.command.read(cx).value().trim().to_string();
        if line.is_empty() {
            return;
        }
        let Some(key) = self.key.clone() else {
            return;
        };
        let Some(object) = self.object(cx) else {
            return;
        };
        let Some(namespace) = object.meta.namespace.clone() else {
            return;
        };
        if self.store.read(cx).exec_permission(Some(&namespace)) != Some(true) {
            return;
        }
        let containers = self.containers(cx);
        let mut request = ExecRequest::shell(namespace, object.meta.name.clone(), &line);
        if let Some(container) = self
            .container
            .clone()
            .or_else(|| containers.first().cloned())
        {
            request = request.container(container);
        }
        self.store
            .update(cx, |store, cx| store.exec(key, request, cx));
        cx.notify();
    }

    /// The Run tab: a command, and what it said.
    fn run_tab(&mut self, object: &Object, cx: &mut Context<Self>) -> AnyElement {
        let tokens = Tokens::global(cx).clone();
        let containers = self.containers(cx);
        let current = self
            .container
            .clone()
            .or_else(|| containers.first().cloned())
            .unwrap_or_default();
        let allowed = self
            .store
            .read(cx)
            .exec_permission(object.meta.namespace.as_deref())
            == Some(true);
        let run = self
            .key
            .as_ref()
            .and_then(|key| self.store.read(cx).run(key).cloned());
        let working = run.as_ref().is_some_and(|run| run.is_loading());
        let refused = run.as_ref().and_then(|run| run.error()).map(str::to_string);
        let output = run.as_ref().and_then(|run| run.value()).cloned();

        let toolbar = h_flex()
            .w_full()
            .px_3()
            .py_1p5()
            .gap_1()
            .flex_shrink_0()
            .items_center()
            .children(containers.into_iter().enumerate().map(|(index, name)| {
                let selected = name == current;
                let picked = name.clone();
                div()
                    .id(("run-container", index))
                    .px_2()
                    .py_0p5()
                    .rounded(px(tokens.radius.control()))
                    .cursor_pointer()
                    .text_size(px(11.))
                    .font_family("monospace")
                    .when(selected, |this| {
                        this.bg(tokens.colors().row_active())
                            .text_color(tokens.colors().text_primary)
                    })
                    .when(!selected, |this| {
                        this.text_color(tokens.colors().text_muted)
                    })
                    .hover(|this| this.bg(tokens.colors().row_hover()))
                    .child(name)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.container = Some(picked.clone());
                        cx.notify();
                    }))
            }))
            .child(div().flex_1().child(Input::new(&self.command)))
            .child(self.button(
                "run",
                rust_i18n::t!("detail.run_button").to_string(),
                allowed && !working,
                false,
                cx,
                |this, _, cx| this.run_command(cx),
            ));

        // stdout, then stderr, then the status: the two streams are separate
        // channels on the wire and the apiserver does not order them against
        // each other, so they are not interleaved here either.
        let mut lines: Vec<(SharedString, bool)> = Vec::new();
        if let Some(output) = &output {
            lines.extend(
                output
                    .stdout
                    .lines()
                    .map(|line| (SharedString::from(line.to_string()), false)),
            );
            if !output.stderr.trim().is_empty() {
                lines.push((
                    SharedString::from(format!("— {} —", rust_i18n::t!("detail.run_stderr"))),
                    true,
                ));
                lines.extend(
                    output
                        .stderr
                        .lines()
                        .map(|line| (SharedString::from(line.to_string()), true)),
                );
            }
        }
        let status: Option<(String, bool)> = output.as_ref().map(|output| match &output.failure {
            Some(failure) => (failure.clone(), true),
            None => match output.exit_code {
                Some(code) => (
                    rust_i18n::t!("detail.run_exit", code = code).to_string(),
                    code != 0,
                ),
                None => (rust_i18n::t!("detail.run_no_exit").to_string(), true),
            },
        });

        let body: AnyElement = if working {
            crate::skeleton::detail(cx)
        } else if let Some(error) = refused {
            self.notice(error, true, cx)
        } else if lines.is_empty() && output.is_some() {
            self.notice(rust_i18n::t!("detail.run_empty").to_string(), false, cx)
        } else if lines.is_empty() {
            div().into_any_element()
        } else {
            let colors = *tokens.colors();
            uniform_list("run-output", lines.len(), move |range, _window, _cx| {
                range
                    .map(|index| {
                        let (line, is_err) = lines
                            .get(index)
                            .cloned()
                            .unwrap_or((SharedString::default(), false));
                        div()
                            .w_full()
                            .h(LINE_HEIGHT)
                            .px_3()
                            .text_size(px(12.))
                            .font_family("monospace")
                            .text_color(match is_err {
                                true => colors.status_error,
                                false => colors.text_secondary,
                            })
                            .child(line)
                    })
                    .collect()
            })
            .size_full()
            .into_any_element()
        };

        v_flex()
            .size_full()
            .bg(tokens.colors().bg_terminal)
            .child(toolbar)
            .when(!allowed, |this| {
                this.child(
                    div()
                        .px_3()
                        .pb_1()
                        .text_size(px(11.))
                        .text_color(tokens.colors().text_muted)
                        .child(rust_i18n::t!("action.forbidden").to_string()),
                )
            })
            .child(div().flex_1().min_h_0().w_full().child(body))
            .children(status.map(|(text, bad)| {
                div()
                    .px_3()
                    .py_1p5()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .font_family("monospace")
                    .text_color(match bad {
                        true => tokens.colors().status_error,
                        false => tokens.colors().text_muted,
                    })
                    .child(text)
            }))
            .into_any_element()
    }

    /// The Logs tab: what to read, and then the reading of it.
    fn logs(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let tokens = Tokens::global(cx).clone();
        let containers = self.containers(cx);
        let current = self
            .container
            .clone()
            .or_else(|| containers.first().cloned())
            .unwrap_or_default();
        let fetch = self
            .log_request(cx)
            .map(|request| Store::log_key(&request))
            .and_then(|key| self.store.read(cx).logs(&key).cloned());
        let lines = fetch
            .as_ref()
            .and_then(|fetch| fetch.value())
            .cloned()
            .unwrap_or_default();
        let error = fetch
            .as_ref()
            .and_then(|fetch| fetch.error())
            .map(str::to_string);
        let loading = fetch.as_ref().is_some_and(|fetch| fetch.is_loading());
        let query = self.find.read(cx).value().to_string();
        let visible = logs::matching(&lines, &query);

        // Following means the end stays in view. Only when something actually
        // arrived: scrolling on every frame would fight the reader the moment
        // they touched the wheel.
        if self.following && lines.len() != self.last_lines && !visible.is_empty() {
            self.scroll
                .scroll_to_item(visible.len() - 1, ScrollStrategy::Top);
        }
        self.last_lines = lines.len();

        let toolbar = h_flex()
            .w_full()
            .px_3()
            .py_1p5()
            .gap_1()
            .flex_shrink_0()
            .items_center()
            .children(containers.into_iter().enumerate().map(|(index, name)| {
                let selected = name == current;
                let picked = name.clone();
                div()
                    .id(("container", index))
                    .px_2()
                    .py_0p5()
                    .rounded(px(tokens.radius.control()))
                    .cursor_pointer()
                    .text_size(px(11.))
                    .font_family("monospace")
                    .when(selected, |this| {
                        this.bg(tokens.colors().row_active())
                            .text_color(tokens.colors().text_primary)
                    })
                    .when(!selected, |this| {
                        this.text_color(tokens.colors().text_muted)
                    })
                    .hover(|this| this.bg(tokens.colors().row_hover()))
                    .child(name)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.container = Some(picked.clone());
                        this.reload_log(cx);
                    }))
            }))
            .child(div().flex_1())
            .child(self.toggle(
                "follow",
                rust_i18n::t!("detail.follow").to_string(),
                self.following,
                cx,
                |this, cx| {
                    this.following = !this.following;
                    this.reload_log(cx);
                },
            ))
            .child(self.toggle(
                "previous",
                rust_i18n::t!("detail.previous").to_string(),
                self.previous,
                cx,
                |this, cx| {
                    this.previous = !this.previous;
                    this.reload_log(cx);
                },
            ))
            .child(
                div()
                    .w(px(150.))
                    .child(Input::new(&self.find).cleanable(true)),
            )
            // How much of the log the find box is hiding, which is the one
            // number that stops a filtered log being mistaken for a short one.
            .when(!query.trim().is_empty(), |this| {
                this.child(
                    div()
                        .flex_shrink_0()
                        .text_size(px(10.5))
                        .text_color(tokens.colors().text_muted)
                        .child(format!("{}/{}", visible.len(), lines.len())),
                )
            });

        let body: AnyElement = if !visible.is_empty() {
            let colors = *tokens.colors();
            // Cloned into the closure rather than read from the store on each
            // frame: the closure outlives this borrow, and a log's lines are
            // `Arc`-free `String`s the list only ever reads.
            let all = lines.clone();
            let indices = visible.clone();
            uniform_list("log", indices.len(), move |range, _window, _cx| {
                range
                    .map(|position| {
                        let line = indices
                            .get(position)
                            .and_then(|index| all.get(*index))
                            .cloned()
                            .unwrap_or_default();
                        div()
                            .w_full()
                            .h(LINE_HEIGHT)
                            .px_3()
                            .text_size(px(12.))
                            .font_family("monospace")
                            .text_color(colors.text_secondary)
                            .child(line)
                    })
                    .collect()
            })
            .track_scroll(&self.scroll)
            .size_full()
            .into_any_element()
        } else if let Some(error) = error {
            self.notice(error, true, cx)
        } else if loading {
            crate::skeleton::detail(cx)
        } else if !query.trim().is_empty() && !lines.is_empty() {
            self.notice(rust_i18n::t!("table.no_matches").to_string(), false, cx)
        } else {
            self.notice(rust_i18n::t!("detail.logs_empty").to_string(), false, cx)
        };

        v_flex()
            .size_full()
            .bg(tokens.colors().bg_terminal)
            .child(toolbar)
            .child(div().flex_1().min_h_0().w_full().child(body))
            .into_any_element()
    }

    /// A small on/off chip in the log's toolbar.
    fn toggle(
        &self,
        id: &'static str,
        label: String,
        on: bool,
        cx: &mut Context<Self>,
        act: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> Stateful<Div> {
        let tokens = Tokens::global(cx).clone();
        div()
            .id(id)
            .px_2()
            .py_0p5()
            .flex_shrink_0()
            .rounded(px(tokens.radius.control()))
            .cursor_pointer()
            .text_size(px(11.))
            .when(on, |this| {
                this.bg(tokens.colors().row_active())
                    .text_color(tokens.colors().text_primary)
            })
            .when(!on, |this| this.text_color(tokens.colors().text_muted))
            .hover(|this| this.bg(tokens.colors().row_hover()))
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| act(this, cx)))
    }

    /// One muted or red line.
    fn notice(&self, text: String, bad: bool, cx: &mut Context<Self>) -> AnyElement {
        let tokens = Tokens::global(cx);
        div()
            .w_full()
            .px_4()
            .py_3()
            .text_size(px(12.))
            .text_color(match bad {
                true => tokens.colors().status_error,
                false => tokens.colors().text_muted,
            })
            .child(text)
            .into_any_element()
    }
}

impl Render for Detail {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(object) = self.object(cx) else {
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .child(self.notice(rust_i18n::t!("detail.empty").to_string(), false, cx));
        };
        let header = self.header(&object, cx).into_any_element();
        let tabs = self.tabs(cx).into_any_element();
        let body = match self.tab {
            Tab::Overview => self.overview(&object, cx),
            Tab::Events => self.events(&object, cx),
            Tab::Yaml => self.yaml(&object, window, cx),
            Tab::Logs => self.logs(cx),
            Tab::Run => self.run_tab(&object, cx),
        };
        let footer = self.footer(&object, cx);
        v_flex()
            .size_full()
            .child(header)
            .child(tabs)
            .child(div().flex_1().min_h_0().w_full().child(body))
            .children(footer)
    }
}
