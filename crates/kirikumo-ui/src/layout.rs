//! Which columns are open, and how wide they are.
//!
//! Each panel toggles independently, a closed panel remembers the width it
//! had, and the arrangement survives a restart. The centre column is not a
//! panel — it can never be closed, so there is no arrangement that leaves the
//! window empty.

use crate::settings::AppSettings;
use gpui::{Pixels, px};

/// How tall the strip across the top of each column is.
///
/// There is no window-wide title bar (`docs/ui.md` §3.1): each column paints
/// itself to the top of the window and carries its own controls. The strips
/// share this height so their contents sit on one line.
pub const HEADER_HEIGHT: Pixels = px(44.);

/// What macOS reserves at the leading edge for the traffic lights.
///
/// Whichever column is leftmost has to leave this much room before its own
/// controls start, so it moves from the sidebar to the centre column when the
/// sidebar is closed.
pub const TRAFFIC_LIGHT_INSET: Pixels = px(78.);

/// The columns the user can open and close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    /// The cluster and its resources, on the left.
    Sidebar,
    /// The object being looked at, on the right.
    RightPanel,
}

impl Panel {
    /// Both, in render order.
    pub const ALL: &'static [Panel] = &[Panel::Sidebar, Panel::RightPanel];

    /// The locale key for the panel's name.
    pub fn label_key(self) -> &'static str {
        match self {
            Self::Sidebar => "panel.sidebar",
            Self::RightPanel => "panel.right",
        }
    }
}

/// The arrangement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    sidebar_open: bool,
    right_open: bool,
    sidebar_width: f32,
    right_width: f32,
}

impl Layout {
    /// Read the arrangement out of the settings.
    pub fn from_settings(settings: &AppSettings) -> Self {
        Self {
            sidebar_open: settings.sidebar_open,
            right_open: settings.right_panel_open,
            sidebar_width: settings.sidebar_width,
            right_width: settings.right_panel_width,
        }
    }

    /// Write it back, leaving the other fields untouched.
    pub fn write_into(&self, settings: &mut AppSettings) {
        settings.sidebar_open = self.sidebar_open;
        settings.right_panel_open = self.right_open;
        settings.sidebar_width = self.sidebar_width;
        settings.right_panel_width = self.right_width;
    }

    /// Whether a panel is showing.
    pub fn is_open(&self, panel: Panel) -> bool {
        match panel {
            Panel::Sidebar => self.sidebar_open,
            Panel::RightPanel => self.right_open,
        }
    }

    /// Flip a panel.
    pub fn toggle(&mut self, panel: Panel) {
        self.set_open(panel, !self.is_open(panel));
    }

    /// Open or close a panel.
    pub fn set_open(&mut self, panel: Panel, open: bool) {
        match panel {
            Panel::Sidebar => self.sidebar_open = open,
            Panel::RightPanel => self.right_open = open,
        }
    }

    /// Record a drag-resize. Kept while the panel is closed, so reopening
    /// restores what the user had rather than a default.
    pub fn set_size(&mut self, panel: Panel, size: Pixels) {
        let value = f32::from(size);
        match panel {
            Panel::Sidebar => self.sidebar_width = value,
            Panel::RightPanel => self.right_width = value,
        }
    }

    /// What occupies each slot of the horizontal group, in render order.
    ///
    /// `None` is the centre column, which has no stored size. Closing a panel
    /// shifts every slot after it, so the resize callback must consult this
    /// rather than assume index 0 is the sidebar.
    pub fn columns(&self) -> Vec<Option<Panel>> {
        let mut slots = Vec::with_capacity(3);
        if self.sidebar_open {
            slots.push(Some(Panel::Sidebar));
        }
        slots.push(None);
        if self.right_open {
            slots.push(Some(Panel::RightPanel));
        }
        slots
    }

    /// Record the sizes a drag produced, ignoring the centre column.
    ///
    /// Extra sizes are ignored rather than trusted: a mismatch means the
    /// toolkit's state and our slot map disagree, and guessing would write
    /// one panel's size over another's.
    pub fn record_sizes(&mut self, slots: &[Option<Panel>], sizes: &[Pixels]) {
        for (slot, size) in slots.iter().zip(sizes) {
            if let Some(panel) = slot {
                self.set_size(*panel, *size);
            }
        }
    }

    /// A panel's width.
    pub fn size(&self, panel: Panel) -> Pixels {
        px(match panel {
            Panel::Sidebar => self.sidebar_width,
            Panel::RightPanel => self.right_width,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_settings() {
        let mut settings = AppSettings::default();
        let mut layout = Layout::from_settings(&settings);
        layout.toggle(Panel::RightPanel);
        layout.set_size(Panel::Sidebar, px(320.));
        layout.write_into(&mut settings);
        let restored = Layout::from_settings(&settings);
        assert_eq!(restored, layout);
        assert!(!restored.is_open(Panel::RightPanel));
        assert_eq!(restored.size(Panel::Sidebar), px(320.));
    }

    #[test]
    fn a_closed_panel_keeps_its_size() {
        let mut layout = Layout::from_settings(&AppSettings::default());
        layout.set_size(Panel::Sidebar, px(320.));
        layout.toggle(Panel::Sidebar);
        layout.toggle(Panel::Sidebar);
        assert_eq!(layout.size(Panel::Sidebar), px(320.));
    }

    #[test]
    fn slots_track_which_panels_are_open() {
        let mut layout = Layout::from_settings(&AppSettings::default());
        assert_eq!(
            layout.columns(),
            vec![Some(Panel::Sidebar), None, Some(Panel::RightPanel)]
        );
        layout.set_open(Panel::Sidebar, false);
        assert_eq!(layout.columns(), vec![None, Some(Panel::RightPanel)]);
    }

    #[test]
    fn a_resize_with_the_sidebar_closed_does_not_write_the_centre_width_into_it() {
        let mut layout = Layout::from_settings(&AppSettings::default());
        let original = layout.size(Panel::Sidebar);
        layout.set_open(Panel::Sidebar, false);
        layout.record_sizes(&layout.columns(), &[px(900.), px(500.)]);
        assert_eq!(layout.size(Panel::Sidebar), original);
        assert_eq!(layout.size(Panel::RightPanel), px(500.));
    }

    #[test]
    fn record_sizes_ignores_a_length_mismatch_rather_than_guessing() {
        let mut layout = Layout::from_settings(&AppSettings::default());
        let right = layout.size(Panel::RightPanel);
        layout.record_sizes(&layout.columns(), &[px(300.), px(900.)]);
        assert_eq!(layout.size(Panel::Sidebar), px(300.));
        assert_eq!(layout.size(Panel::RightPanel), right);
    }
}
