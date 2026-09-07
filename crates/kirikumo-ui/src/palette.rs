//! Everything the window can be pointed at, by name.
//!
//! `⌘K` (`docs/ui.md` §5). One list holding every kind the cluster serves,
//! every namespace, every context in the kubeconfig and the handful of
//! commands that are not any of those — so that a person who knows the word
//! never has to know where the control is.
//!
//! Built here rather than in the view because deciding *what is reachable*
//! is a decision, and a decision belongs where it can be tested
//! (`AGENTS.md` rule 6). It is also the only list in the app ranked by score
//! rather than left in its own order: a table is read down a column and must
//! not reorder as you type, and a palette is read from the top and must.

use crate::assets::icon;
use crate::nav;
use kirikumo_kube::{Catalogue, ContextRef, ResourceKey};

/// What choosing an entry does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// List a kind in the centre column.
    Kind(ResourceKey),
    /// Scope the table to a namespace, or to every one.
    Namespace(Option<String>),
    /// Connect to another context.
    Context(String),
    /// Everything that is not a place to go.
    Command(Command),
}

/// The commands the palette offers.
///
/// Deliberately short, and deliberately containing nothing destructive: no
/// palette entry may delete, scale or evict, for the same reason no key chord
/// may (`AGENTS.md` rule 9). A destructive action is reached from the object
/// it acts on, and confirmed there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Fetch again what is on screen.
    Refresh,
    /// Show or hide the navigation column.
    ToggleSidebar,
    /// Show or hide the detail column.
    ToggleRightPanel,
    /// Dark, light, or the system's.
    CycleAppearance,
}

impl Command {
    /// Every command, in the order the palette lists them.
    pub const ALL: &'static [Command] = &[
        Command::Refresh,
        Command::ToggleSidebar,
        Command::ToggleRightPanel,
        Command::CycleAppearance,
    ];

    /// The locale key for the command's name.
    pub fn label_key(self) -> &'static str {
        match self {
            Self::Refresh => "table.refresh",
            Self::ToggleSidebar => "panel.sidebar",
            Self::ToggleRightPanel => "panel.right",
            Self::CycleAppearance => "sidebar.appearance",
        }
    }
}

/// One row of the palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// What the row says.
    pub title: String,
    /// What it says underneath, muted: which group a kind is in, which server
    /// a context points at, what kind of thing this is.
    pub hint: String,
    /// The icon it carries.
    pub icon: &'static str,
    /// What choosing it does.
    pub action: Action,
    /// Whether it is where the window already is.
    pub current: bool,
}

impl Entry {
    /// What a query is matched against: the title and the hint together, so
    /// `shop` finds the namespace and `apps` finds every kind in that API
    /// group.
    pub fn haystack(&self) -> String {
        format!("{} {}", self.title, self.hint).to_lowercase()
    }
}

/// What the window is currently showing, so the palette can mark it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Here {
    /// The kind the table is listing.
    pub kind: Option<ResourceKey>,
    /// The namespace it is scoped to; `None` is every namespace.
    pub namespace: Option<String>,
    /// The context that is connected.
    pub context: Option<String>,
}

/// Everything reachable, in the order an empty query lists it.
///
/// Kinds first, because that is what nine of ten `⌘K`s are for; then
/// namespaces, then contexts, then the commands. Within each, the order they
/// already have elsewhere — the sidebar's for kinds, the picker's for the
/// rest — so that muscle memory built in one place works in the other.
pub fn entries(
    catalogue: &Catalogue,
    namespaces: &[String],
    contexts: &[ContextRef],
    here: &Here,
) -> Vec<Entry> {
    let mut entries = Vec::new();

    for resource in &catalogue.resources {
        let group = nav::group_of(resource);
        let mut hint = rust_i18n::t!(group.label_key()).to_string();
        // A custom resource says whose it is: two clusters' CRDs collide on
        // names far more often than built-in kinds do.
        if !resource.group.is_empty() {
            hint = format!("{hint} · {}", resource.group);
        }
        entries.push(Entry {
            title: nav::title(resource),
            hint,
            icon: nav::group_icon(group),
            current: here.kind.as_ref() == Some(&resource.key()),
            action: Action::Kind(resource.key()),
        });
    }

    let all = rust_i18n::t!("table.all_namespaces").to_string();
    entries.push(Entry {
        title: all,
        hint: rust_i18n::t!("table.namespace").to_string(),
        icon: icon::FRAME,
        current: here.namespace.is_none(),
        action: Action::Namespace(None),
    });
    for namespace in namespaces {
        entries.push(Entry {
            title: namespace.clone(),
            hint: rust_i18n::t!("table.namespace").to_string(),
            icon: icon::FRAME,
            current: here.namespace.as_deref() == Some(namespace.as_str()),
            action: Action::Namespace(Some(namespace.clone())),
        });
    }

    for context in contexts {
        entries.push(Entry {
            title: context.name.clone(),
            hint: context.server.clone(),
            icon: icon::GLOBE,
            current: here.context.as_deref() == Some(context.name.as_str()),
            action: Action::Context(context.name.clone()),
        });
    }

    for command in Command::ALL {
        entries.push(Entry {
            title: rust_i18n::t!(command.label_key()).to_string(),
            hint: rust_i18n::t!("palette.command").to_string(),
            icon: icon::SETTINGS,
            current: false,
            action: Action::Command(*command),
        });
    }

    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use kirikumo_kube::{ApiResource, discovery};

    fn resource(group: &str, kind: &str, name: &str) -> ApiResource {
        ApiResource {
            group: group.into(),
            version: "v1".into(),
            kind: kind.into(),
            name: name.into(),
            singular: String::new(),
            namespaced: true,
            verbs: vec!["list".into()],
            short_names: Vec::new(),
            categories: Vec::new(),
        }
    }

    fn context(name: &str, server: &str) -> ContextRef {
        ContextRef {
            name: name.into(),
            cluster: name.into(),
            user: None,
            namespace: None,
            server: server.into(),
            insecure: false,
        }
    }

    fn sample(here: Here) -> Vec<Entry> {
        rust_i18n::set_locale("en");
        let catalogue = discovery::catalogue(vec![vec![
            resource("", "Pod", "pods"),
            resource("apps", "Deployment", "deployments"),
            resource("argoproj.io", "Rollout", "rollouts"),
        ]]);
        entries(
            &catalogue,
            &["default".to_string(), "shop".to_string()],
            &[context("kind-dev", "https://127.0.0.1:6443")],
            &here,
        )
    }

    fn titles(entries: &[Entry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.title.as_str()).collect()
    }

    #[test]
    fn kinds_come_first_because_that_is_what_the_palette_is_for() {
        let entries = sample(Here::default());
        let titles = titles(&entries);
        assert_eq!(&titles[..3], &["Pods", "Deployments", "Rollouts"]);
        assert!(titles.contains(&"shop"));
        assert!(titles.contains(&"kind-dev"));
        assert!(titles.contains(&"Refresh"));
    }

    #[test]
    fn a_custom_resource_says_whose_it_is() {
        let entries = sample(Here::default());
        let rollout = entries
            .iter()
            .find(|entry| entry.title == "Rollouts")
            .unwrap();
        assert!(rollout.hint.contains("argoproj.io"), "{}", rollout.hint);
        // A built-in one has no group to name, so it only says its section.
        let pods = entries.iter().find(|entry| entry.title == "Pods").unwrap();
        assert_eq!(pods.hint, "Workloads");
    }

    #[test]
    fn every_namespace_is_reachable_and_so_is_every_namespace_at_once() {
        let entries = sample(Here::default());
        assert!(
            entries
                .iter()
                .any(|entry| entry.action == Action::Namespace(None))
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry.action == Action::Namespace(Some("shop".into())))
        );
    }

    #[test]
    fn where_the_window_already_is_gets_marked() {
        let here = Here {
            kind: Some(ResourceKey::new("apps", "Deployment")),
            namespace: Some("shop".into()),
            context: Some("kind-dev".into()),
        };
        let entries = sample(here);
        let current: Vec<&str> = entries
            .iter()
            .filter(|entry| entry.current)
            .map(|entry| entry.title.as_str())
            .collect();
        assert_eq!(current, vec!["Deployments", "shop", "kind-dev"]);
    }

    #[test]
    fn all_namespaces_is_the_current_one_when_nothing_is_scoped() {
        let entries = sample(Here::default());
        let all = entries
            .iter()
            .find(|entry| entry.action == Action::Namespace(None))
            .unwrap();
        assert!(all.current);
    }

    #[test]
    fn a_haystack_matches_the_hint_as_well_as_the_title() {
        let entries = sample(Here::default());
        let rollout = entries
            .iter()
            .find(|entry| entry.title == "Rollouts")
            .unwrap();
        assert!(rollout.haystack().contains("argoproj.io"));
        assert!(rollout.haystack().contains("rollouts"));
    }

    #[test]
    fn nothing_in_the_palette_can_destroy_anything() {
        // The palette is a way to *go* somewhere. A destructive action is
        // reached from the object it acts on, and confirmed there
        // (`AGENTS.md` rule 9).
        for command in Command::ALL {
            assert!(matches!(
                command,
                Command::Refresh
                    | Command::ToggleSidebar
                    | Command::ToggleRightPanel
                    | Command::CycleAppearance
            ));
        }
    }

    #[test]
    fn a_cluster_with_nothing_in_it_still_offers_its_commands() {
        rust_i18n::set_locale("en");
        let entries = entries(&Catalogue::default(), &[], &[], &Here::default());
        assert!(
            entries
                .iter()
                .any(|entry| entry.action == Action::Command(Command::Refresh))
        );
    }
}
