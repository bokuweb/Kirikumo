//! Which language the window speaks.
//!
//! `en` and `ja` are both maintained from the first commit, because
//! retrofitting localisation is expensive. The strings live in
//! `locales/app.yml` with both languages side by side, so a missing
//! translation is visible in review rather than in a running window.

/// The language to use, in order of authority.
///
/// An explicit choice in `app.json` wins. Failing that the environment is
/// asked, because a user whose desktop is in Japanese has already said what
/// they want. Anything unrecognised falls back to English rather than to an
/// empty window.
pub fn resolve(preference: Option<&str>, environment: Option<&str>) -> String {
    preference
        .and_then(normalize)
        .or_else(|| environment.and_then(normalize))
        .unwrap_or_else(|| "en".to_string())
}

/// Reduce a locale tag to one this app has strings for.
///
/// `LANG` arrives as `ja_JP.UTF-8` and a BCP-47 preference as `ja-JP`; both
/// are the same language to us. `C` and `POSIX` mean "no preference stated".
fn normalize(tag: &str) -> Option<String> {
    let language = tag
        .split(['_', '.', '-', '@'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match language.as_str() {
        "ja" => Some("ja".to_string()),
        "en" => Some("en".to_string()),
        _ => None,
    }
}

/// The locale the environment asks for, if it says.
pub fn from_environment() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok())
        .filter(|value| !value.is_empty())
}

/// Resolve, apply to every string in this process, and report what was chosen.
pub fn init(preference: Option<&str>) -> String {
    let locale = resolve(preference, from_environment().as_deref());
    rust_i18n::set_locale(&locale);
    locale
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_choice_wins_over_the_environment() {
        assert_eq!(resolve(Some("ja"), Some("en_US.UTF-8")), "ja");
    }

    #[test]
    fn the_environment_is_used_when_nothing_was_chosen() {
        assert_eq!(resolve(None, Some("ja_JP.UTF-8")), "ja");
        assert_eq!(resolve(None, Some("en-GB")), "en");
    }

    #[test]
    fn anything_unknown_falls_back_to_english() {
        assert_eq!(resolve(Some("fr"), Some("C")), "en");
        assert_eq!(resolve(None, None), "en");
    }
}
