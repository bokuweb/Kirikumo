//! The views.
//!
//! A library, so a host window can mount them (`docs/roadmap.md` K1): the
//! standalone binary mounts [`Shell`], which is the three-column window;
//! Ginka will mount the pieces under it — [`sidebar::Sidebar`],
//! [`table::ResourceTable`] and [`detail::Detail`] — over its own
//! [`store::Store`]. Every view reaches the cluster through the store, and
//! the store through an `Arc<dyn Cluster>`, which is the whole of the
//! network's surface (K2).
//!
//! No tests here, by construction: `rustc` overflows its stack expanding
//! `#[test]` next to the toolkit's builder chains (`AGENTS.md` rule 6). The
//! decisions these views draw are tested in `kirikumo-ui` and
//! `kirikumo-kube`.

rust_i18n::i18n!("../../locales", fallback = "en");

pub mod detail;
pub mod shell;
pub mod sidebar;
pub mod skeleton;
pub mod store;
pub mod table;

pub use shell::Shell;
pub use store::Store;

use gpui::App;

/// Bind the keys the views answer to. Call once, after `gpui_component::init`.
pub fn init(cx: &mut App) {
    shell::init(cx);
}
