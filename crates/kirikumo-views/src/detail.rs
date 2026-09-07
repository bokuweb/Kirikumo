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

use crate::store::{ObjectKey, Store, StoreEvent};
use chrono::Utc;
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::{Icon, StyledExt as _, h_flex, v_flex};
use kirikumo_kube::{LogRequest, Object, yaml};
use kirikumo_ui::assets::icon;
use kirikumo_ui::{Tokens, detail, time};
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
}

impl Tab {
    /// The locale key for the tab's name.
    fn label_key(self) -> &'static str {
        match self {
            Self::Overview => "detail.overview",
            Self::Events => "detail.events",
            Self::Yaml => "detail.yaml",
            Self::Logs => "detail.logs",
        }
    }
}

/// The right panel.
pub struct Detail {
    store: Entity<Store>,
    key: Option<ObjectKey>,
    tab: Tab,
    /// Which container's log is showing, when the object has more than one.
    container: Option<String>,
}

impl Detail {
    /// A panel over a store, showing nothing until told what to.
    pub fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        cx.subscribe(&store, |_, _, _: &StoreEvent, cx| cx.notify())
            .detach();
        Self {
            store,
            key: None,
            tab: Tab::Overview,
            container: None,
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
        self.ensure(cx);
        cx.notify();
    }

    /// Nothing is open.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.key = None;
        cx.notify();
    }

    /// Fetch again whatever the open tab is showing.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if let (Tab::Logs, Some(request)) = (self.tab, self.log_request(cx)) {
            self.store
                .update(cx, |store, cx| store.load_logs(request, cx));
        }
        self.ensure(cx);
    }

    fn set_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        self.ensure(cx);
        cx.notify();
    }

    /// Ask for whatever the open tab needs and does not have.
    fn ensure(&mut self, cx: &mut Context<Self>) {
        let Some(object) = self.object(cx) else {
            return;
        };
        match self.tab {
            Tab::Events => {
                let uid = object.meta.uid.clone();
                let namespace = object.meta.namespace.clone();
                self.store
                    .update(cx, |store, cx| store.ensure_events(uid, namespace, cx));
            }
            Tab::Logs => {
                if let Some(request) = self.log_request(cx) {
                    self.store
                        .update(cx, |store, cx| store.ensure_logs(request, cx));
                }
            }
            Tab::Overview | Tab::Yaml => {}
        }
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
        Some(LogRequest::new(namespace, object.meta.name.clone()).container(container))
    }

    /// The name, the kind and the health, across the top.
    fn header(&self, object: &Object, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let tokens = Tokens::global(cx).clone();
        let kind = self
            .key
            .as_ref()
            .map(|(kind, _, _)| kind.kind.clone())
            .unwrap_or_default();
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
            true => vec![Tab::Overview, Tab::Events, Tab::Yaml, Tab::Logs],
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
        let kind = self
            .key
            .as_ref()
            .map(|(kind, _, _)| kind.kind.clone())
            .unwrap_or_default();
        let overview = detail::overview(&kind, object, Utc::now());

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
                    .children(section.facts.into_iter().map(|fact| {
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
                                    .flex_1()
                                    .text_size(px(12.))
                                    .when(fact.mono, |this| this.font_family("monospace"))
                                    .text_color(tokens.colors().text_secondary)
                                    .child(fact.value),
                            )
                    }))
            }))
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

    /// The YAML tab: the object as the apiserver holds it, virtualized.
    fn yaml(&self, object: &Object, cx: &mut Context<Self>) -> AnyElement {
        let tokens = Tokens::global(cx).clone();
        let lines: Vec<SharedString> = yaml::to_yaml(&object.raw)
            .lines()
            .map(|line| SharedString::from(line.to_string()))
            .collect();
        let gutter = px(44.);

        uniform_list("yaml", lines.len(), move |range, _window, _cx| {
            range
                .map(|index| {
                    let line = lines.get(index).cloned().unwrap_or_default();
                    h_flex()
                        .w_full()
                        .h(LINE_HEIGHT)
                        .px_3()
                        .gap_2()
                        .items_center()
                        .child(
                            div()
                                .w(gutter)
                                .flex_shrink_0()
                                .text_right()
                                .text_size(px(11.))
                                .font_family("monospace")
                                .text_color(tokens.colors().text_muted)
                                .child((index + 1).to_string()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_size(px(12.))
                                .font_family("monospace")
                                .text_color(tokens.colors().text_secondary)
                                .child(line),
                        )
                })
                .collect()
        })
        .size_full()
        .into_any_element()
    }

    /// The Logs tab: the container picker, then the lines.
    fn logs(&self, cx: &mut Context<Self>) -> AnyElement {
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
        let text = fetch.as_ref().and_then(|fetch| fetch.value()).cloned();
        let error = fetch
            .as_ref()
            .and_then(|fetch| fetch.error())
            .map(str::to_string);
        let loading = fetch.as_ref().is_some_and(|fetch| fetch.is_loading());

        // The picker, when there is a choice to make.
        let picker = (containers.len() > 1).then(|| {
            h_flex()
                .w_full()
                .px_3()
                .py_1p5()
                .gap_1()
                .flex_shrink_0()
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
                            this.ensure(cx);
                            cx.notify();
                        }))
                }))
                .into_any_element()
        });

        let body: AnyElement = match (text, error, loading) {
            (Some(text), _, _) if !text.trim().is_empty() => {
                let lines: Vec<SharedString> = text
                    .lines()
                    .map(|line| SharedString::from(line.to_string()))
                    .collect();
                let colors = *tokens.colors();
                uniform_list("log", lines.len(), move |range, _window, _cx| {
                    range
                        .map(|index| {
                            let line = lines.get(index).cloned().unwrap_or_default();
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
                .size_full()
                .into_any_element()
            }
            (_, Some(error), _) => self.notice(error, true, cx),
            (_, None, true) => crate::skeleton::detail(cx),
            _ => self.notice(rust_i18n::t!("detail.logs_empty").to_string(), false, cx),
        };

        v_flex()
            .size_full()
            .bg(tokens.colors().bg_terminal)
            .children(picker)
            .child(div().flex_1().min_h_0().w_full().child(body))
            .into_any_element()
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
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            Tab::Yaml => self.yaml(&object, cx),
            Tab::Logs => self.logs(cx),
        };
        v_flex()
            .size_full()
            .child(header)
            .child(tabs)
            .child(div().flex_1().min_h_0().w_full().child(body))
    }
}
