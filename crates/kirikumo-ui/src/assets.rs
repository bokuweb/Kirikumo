//! Asset source, and the icon names the views address.
//!
//! Ours first, the toolkit's second. The five health marks are ours because
//! they have to read as *one set* at 8 px — a filled dot, a ring, a triangle,
//! a cross and a dashed ring, all on the same optical weight — and a set
//! assembled from a general-purpose icon library does not.
//!
//! Health is a mark *and* a colour, never a colour alone (`AGENTS.md`
//! conventions): that is what [`icon::health`] is for.

use gpui::{AssetSource, Result, SharedString};
use std::borrow::Cow;

/// The asset source the window is opened with.
pub struct Assets;

/// Icons this app ships. Paths match what `Icon::path` is given.
const ICONS: &[(&str, &str)] = &[
    (
        icon::KIRIKUMO,
        include_str!("../../../assets/icons/kirikumo.svg"),
    ),
    (
        icon::HEALTH_OK,
        include_str!("../../../assets/icons/health-ok.svg"),
    ),
    (
        icon::HEALTH_WORKING,
        include_str!("../../../assets/icons/health-working.svg"),
    ),
    (
        icon::HEALTH_ATTENTION,
        include_str!("../../../assets/icons/health-attention.svg"),
    ),
    (
        icon::HEALTH_ERROR,
        include_str!("../../../assets/icons/health-error.svg"),
    ),
    (
        icon::HEALTH_UNKNOWN,
        include_str!("../../../assets/icons/health-unknown.svg"),
    ),
    (
        icon::SUN_MOON,
        include_str!("../../../assets/icons/sun-moon.svg"),
    ),
];

/// Icon paths, the way [`gpui_component::Icon::path`] expects them.
pub mod icon {
    use kirikumo_kube::Level;

    /// The app's own mark, painted in [`crate::Tokens::logo`].
    pub const KIRIKUMO: &str = "icons/kirikumo.svg";
    /// Healthy: a filled dot.
    pub const HEALTH_OK: &str = "icons/health-ok.svg";
    /// On its way somewhere: a ring.
    pub const HEALTH_WORKING: &str = "icons/health-working.svg";
    /// Not as intended: a triangle.
    pub const HEALTH_ATTENTION: &str = "icons/health-attention.svg";
    /// Broken: a cross.
    pub const HEALTH_ERROR: &str = "icons/health-error.svg";
    /// Nothing to read: a dashed ring.
    pub const HEALTH_UNKNOWN: &str = "icons/health-unknown.svg";
    /// Follow the system's appearance: half sun, half moon.
    pub const SUN_MOON: &str = "icons/sun-moon.svg";

    /// The toolkit's: the light theme.
    pub const SUN: &str = "icons/sun.svg";
    /// The toolkit's: the dark theme.
    pub const MOON: &str = "icons/moon.svg";
    /// The toolkit's: the Cluster group.
    pub const GLOBE: &str = "icons/globe.svg";
    /// The toolkit's: the Workloads group.
    pub const LAYOUT: &str = "icons/layout-dashboard.svg";
    /// The toolkit's: the Config group.
    pub const SETTINGS: &str = "icons/settings.svg";
    /// The toolkit's: the Network group.
    pub const NETWORK: &str = "icons/network.svg";
    /// The toolkit's: the Storage group.
    pub const DISK: &str = "icons/hard-drive.svg";
    /// The toolkit's: the Access Control group.
    pub const USER: &str = "icons/user.svg";
    /// The toolkit's: the Custom Resources group.
    pub const FRAME: &str = "icons/frame.svg";
    /// The toolkit's: refresh.
    pub const REFRESH: &str = "icons/rotate-cw.svg";
    /// The toolkit's: a group that is folded away.
    pub const CHEVRON_RIGHT: &str = "icons/chevron-right.svg";
    /// The toolkit's: a group that is showing.
    pub const CHEVRON_DOWN: &str = "icons/chevron-down.svg";
    /// The toolkit's: the current item in a picker.
    pub const CHECK: &str = "icons/check.svg";
    /// The toolkit's: something the reader should be warned about.
    pub const WARNING: &str = "icons/triangle-alert.svg";
    /// The toolkit's: a container's log.
    pub const TERMINAL: &str = "icons/square-terminal.svg";

    /// The mark for a health level.
    pub fn health(level: Level) -> &'static str {
        match level {
            Level::Ok => HEALTH_OK,
            Level::Working => HEALTH_WORKING,
            Level::Attention => HEALTH_ATTENTION,
            Level::Error => HEALTH_ERROR,
            Level::Unknown => HEALTH_UNKNOWN,
        }
    }
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, contents)) = ICONS.iter().find(|(name, _)| *name == path) {
            return Ok(Some(Cow::Borrowed(contents.as_bytes())));
        }
        gpui_component_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut listed: Vec<SharedString> = ICONS
            .iter()
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| SharedString::from(*name))
            .collect();
        listed.extend(gpui_component_assets::Assets.list(path)?);
        Ok(listed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kirikumo_kube::Level;

    #[test]
    fn our_icons_load() {
        for (path, _) in ICONS {
            let loaded = Assets
                .load(path)
                .unwrap()
                .unwrap_or_else(|| panic!("{path} is not embedded"));
            assert!(String::from_utf8_lossy(&loaded).contains("<svg"), "{path}");
        }
    }

    #[test]
    fn every_health_level_has_a_mark_of_its_own() {
        let levels = [
            Level::Ok,
            Level::Working,
            Level::Attention,
            Level::Error,
            Level::Unknown,
        ];
        let mut marks: Vec<&str> = levels.iter().map(|level| icon::health(*level)).collect();
        marks.sort_unstable();
        marks.dedup();
        assert_eq!(marks.len(), levels.len(), "two levels share a mark");
        for level in levels {
            assert!(Assets.load(icon::health(level)).unwrap().is_some());
        }
    }

    #[test]
    fn the_toolkit_icons_we_lean_on_are_really_there() {
        for path in [
            icon::SUN,
            icon::MOON,
            icon::GLOBE,
            icon::LAYOUT,
            icon::SETTINGS,
            icon::NETWORK,
            icon::DISK,
            icon::USER,
            icon::FRAME,
            icon::REFRESH,
            icon::CHEVRON_RIGHT,
            icon::CHEVRON_DOWN,
            icon::CHECK,
            icon::WARNING,
            icon::TERMINAL,
        ] {
            assert!(Assets.load(path).unwrap().is_some(), "{path}");
        }
    }
}
