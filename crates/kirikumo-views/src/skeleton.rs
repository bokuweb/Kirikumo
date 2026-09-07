//! What a column shows while its first answer is on the way.
//!
//! Bars in the shape of what is coming, rather than a word: a reader who sees
//! the shape of a table knows a table is coming and where to look for it, and
//! *Loading…* tells them only that they are waiting. These stand in for a
//! *first* load only — a refresh keeps the old value on screen
//! (`kirikumo_ui::Fetch`) and needs nothing here.

use gpui::*;
use gpui_component::skeleton::Skeleton;
use gpui_component::{h_flex, v_flex};
use kirikumo_ui::Tokens;

/// A bar `width` wide and `height` tall, rounded like a control.
fn bar(width: Pixels, height: Pixels, cx: &App) -> Skeleton {
    let tokens = Tokens::global(cx);
    Skeleton::new()
        .w(width)
        .h(height)
        .rounded(px(tokens.radius.control()))
}

/// The shape of `count` table rows: a mark, a name, and a few columns.
pub fn rows(count: usize, height: Pixels, cx: &App) -> AnyElement {
    v_flex()
        .w_full()
        .children((0..count).map(|index| {
            // Names vary; the bars do too, or the table reads as a grid.
            let name = px([0.42, 0.30, 0.38, 0.26, 0.35, 0.33][index % 6] * 520.);
            h_flex()
                .w_full()
                .h(height)
                .px_2()
                .gap_2()
                .items_center()
                .child(Skeleton::new().size(px(8.)).rounded_full().flex_shrink_0())
                .child(bar(name, px(9.), cx))
                .child(bar(px(46.), px(9.), cx).secondary())
                .child(bar(px(70.), px(9.), cx).secondary())
        }))
        .into_any_element()
}

/// The shape of an object being read: a name, a line of facts, some rows.
pub fn detail(cx: &App) -> AnyElement {
    v_flex()
        .w_full()
        .px_4()
        .py_3()
        .gap_3()
        .child(bar(px(220.), px(14.), cx))
        .child(
            h_flex()
                .gap_2()
                .child(bar(px(60.), px(10.), cx).secondary())
                .child(bar(px(96.), px(10.), cx).secondary()),
        )
        .child(div().h_2())
        .children([0.9, 0.7, 0.8, 0.55, 0.75].into_iter().map(|fraction| {
            Skeleton::new()
                .w(relative(fraction))
                .h(px(10.))
                .rounded(px(3.))
                .secondary()
        }))
        .into_any_element()
}
