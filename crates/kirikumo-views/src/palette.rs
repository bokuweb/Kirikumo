//! `⌘K`: everything the window can be pointed at, by name.
//!
//! An overlay over the centre column — a field, and under it every kind the
//! cluster serves, every namespace, every context and the few commands that
//! are none of those. What is in the list and how it ranks is
//! `kirikumo_ui::palette`, where it can be tested; this file is the drawing
//! and the keyboard.
//!
//! Unlike the table's filter, this list *is* ranked by score: a table is read
//! down a column and must not reorder as you type, a palette is read from the
//! top and must.

use crate::store::Store;
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::{Icon, h_flex, v_flex};
use kirikumo_ui::assets::icon;
use kirikumo_ui::palette::{self, Action, Entry, Here};
use kirikumo_ui::{Filter, Tokens};

actions!(kirikumo_palette, [SelectNext, SelectPrevious, Dismiss]);

/// The key context the palette's own chords are bound in.
pub const CONTEXT: &str = "KirikumoPalette";

/// How tall one row is.
const ROW_HEIGHT: Pixels = px(34.);

/// How many rows are shown before the list scrolls.
const VISIBLE_ROWS: usize = 9;

/// Bind the palette's keys. Called from the shell's own `init`.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("down", SelectNext, Some(CONTEXT)),
        KeyBinding::new("up", SelectPrevious, Some(CONTEXT)),
        KeyBinding::new("ctrl-n", SelectNext, Some(CONTEXT)),
        KeyBinding::new("ctrl-p", SelectPrevious, Some(CONTEXT)),
        KeyBinding::new("escape", Dismiss, Some(CONTEXT)),
    ]);
}

/// What the reader did with the palette.
pub enum PaletteEvent {
    /// They chose something.
    Chose(Action),
    /// They closed it without choosing.
    Dismissed,
}

impl EventEmitter<PaletteEvent> for Palette {}

/// The palette.
pub struct Palette {
    store: Entity<Store>,
    input: Entity<InputState>,
    /// Everything reachable, rebuilt when the palette is opened.
    entries: Vec<Entry>,
    /// What each entry is matched against, in step with `entries`.
    haystacks: Vec<String>,
    /// The indices of the entries that match, best first.
    visible: Vec<usize>,
    /// Which of `visible` is highlighted.
    selected: usize,
    filter: Filter,
    scroll: UniformListScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl Palette {
    /// A palette over a store. It holds nothing until it is opened.
    pub fn new(store: Entity<Store>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(rust_i18n::t!("palette.placeholder").to_string())
        });
        let subscriptions =
            vec![
                cx.subscribe(&input, |this, input, event: &InputEvent, cx| match event {
                    InputEvent::Change => {
                        let query = input.read(cx).value().to_string();
                        this.refilter(&query, cx);
                    }
                    InputEvent::PressEnter { .. } => this.choose(cx),
                    _ => {}
                }),
            ];
        Self {
            store,
            input,
            entries: Vec::new(),
            haystacks: Vec::new(),
            visible: Vec::new(),
            selected: 0,
            filter: Filter::new(),
            scroll: UniformListScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    /// Open it: gather what is reachable, empty the field, take focus.
    ///
    /// Gathered on every open rather than kept: the catalogue and the
    /// namespaces change under a live window, and a palette offering a kind
    /// the cluster stopped serving is worse than one that takes a millisecond
    /// to build.
    pub fn open(&mut self, here: Here, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let catalogue = store.catalogue().value().cloned().unwrap_or_default();
        let namespaces = store.namespaces().value().cloned().unwrap_or_default();
        let contexts = store.contexts().to_vec();
        self.entries = palette::entries(&catalogue, &namespaces, &contexts, &here);
        tracing::debug!(entries = self.entries.len(), "the palette opened");
        self.haystacks = self.entries.iter().map(Entry::haystack).collect();
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.refilter("", cx);
        // The caret goes in the field, and the field's context is where the
        // arrow keys are bound.
        self.input.read(cx).focus_handle(cx).focus(window, cx);
        cx.notify();
    }

    /// How many entries are showing.
    pub fn len(&self) -> usize {
        self.visible.len()
    }

    /// Whether nothing matches what has been typed.
    pub fn is_empty(&self) -> bool {
        self.visible.is_empty()
    }

    fn refilter(&mut self, query: &str, cx: &mut Context<Self>) {
        self.visible = self.filter.rank(query, &self.haystacks);
        // Back to the top on every keystroke: the best match is the point.
        self.selected = 0;
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn move_by(&mut self, delta: isize, cx: &mut Context<Self>) {
        if self.visible.is_empty() {
            return;
        }
        let last = self.visible.len() - 1;
        // Wrapping, because a palette is a ring: pressing up on the first row
        // to reach the last is faster than nine downs.
        self.selected = match delta {
            delta if delta < 0 && self.selected == 0 => last,
            delta if delta < 0 => self.selected - 1,
            _ if self.selected >= last => 0,
            _ => self.selected + 1,
        };
        self.scroll
            .scroll_to_item(self.selected, ScrollStrategy::Top);
        cx.notify();
    }

    fn choose(&mut self, cx: &mut Context<Self>) {
        let Some(entry) = self
            .visible
            .get(self.selected)
            .and_then(|index| self.entries.get(*index))
        else {
            return;
        };
        cx.emit(PaletteEvent::Chose(entry.action.clone()));
    }

    fn choose_at(&mut self, position: usize, cx: &mut Context<Self>) {
        self.selected = position;
        self.choose(cx);
    }

    fn on_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.move_by(1, cx);
    }

    fn on_previous(&mut self, _: &SelectPrevious, _: &mut Window, cx: &mut Context<Self>) {
        self.move_by(-1, cx);
    }

    fn on_dismiss(&mut self, _: &Dismiss, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(PaletteEvent::Dismissed);
    }

    /// One row.
    fn row(&self, position: usize, cx: &mut Context<Self>) -> AnyElement {
        let tokens = Tokens::global(cx).clone();
        let Some(entry) = self
            .visible
            .get(position)
            .and_then(|index| self.entries.get(*index))
        else {
            return div().h(ROW_HEIGHT).into_any_element();
        };
        let selected = position == self.selected;
        h_flex()
            .id(("entry", position))
            .w_full()
            .h(ROW_HEIGHT)
            .px_2p5()
            .gap_2p5()
            .items_center()
            .rounded(px(tokens.radius.row))
            .cursor_pointer()
            .when(selected, |this| this.bg(tokens.colors().row_active()))
            .hover(|this| this.bg(tokens.colors().row_hover()))
            .on_click(cx.listener(move |this, _, _, cx| this.choose_at(position, cx)))
            .child(
                Icon::empty()
                    .path(entry.icon)
                    .size_3p5()
                    .flex_shrink_0()
                    .text_color(tokens.colors().text_muted),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(px(13.))
                    .text_color(tokens.colors().text_primary)
                    .truncate()
                    .child(entry.title.clone()),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .max_w(px(200.))
                    .text_size(px(11.))
                    .text_color(tokens.colors().text_muted)
                    .truncate()
                    .child(entry.hint.clone()),
            )
            // Where the window already is, marked the way the pickers mark it.
            .when(entry.current, |this| {
                this.child(
                    Icon::empty()
                        .path(icon::CHECK)
                        .size_3()
                        .flex_shrink_0()
                        .text_color(tokens.colors().accent),
                )
            })
            .into_any_element()
    }
}

impl Render for Palette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = Tokens::global(cx).clone();
        let shown = self.visible.len().clamp(1, VISIBLE_ROWS);
        let this = cx.entity();

        v_flex()
            .key_context(CONTEXT)
            .on_action(cx.listener(Self::on_next))
            .on_action(cx.listener(Self::on_previous))
            .on_action(cx.listener(Self::on_dismiss))
            .w(px(560.))
            .p_1p5()
            .gap_1()
            .rounded(px(tokens.radius.card))
            .bg(tokens.colors().bg_raised)
            .border_1()
            .border_color(tokens.colors().border_strong)
            .child(div().px_1().child(Input::new(&self.input).cleanable(true)))
            .when(self.visible.is_empty(), |this| {
                this.child(
                    div()
                        .px_3()
                        .py_2()
                        .text_size(px(12.))
                        .text_color(tokens.colors().text_muted)
                        .child(rust_i18n::t!("palette.empty").to_string()),
                )
            })
            .when(!self.visible.is_empty(), |element| {
                element.child(
                    uniform_list("palette", self.visible.len(), move |range, _window, cx| {
                        this.update(cx, |this, cx| {
                            range.map(|position| this.row(position, cx)).collect()
                        })
                    })
                    .track_scroll(&self.scroll)
                    .h(ROW_HEIGHT * shown),
                )
            })
    }
}
