//! Fluent-based localisation. Translations live in `crates/core/locales/<lang>.ftl`
//! and are embedded at compile time; missing keys fall back to English, then to the key.

pub use fluent_bundle::FluentArgs;
use fluent_bundle::{FluentBundle, FluentResource};
use unic_langid::LanguageIdentifier;

const EN_US: &str = include_str!("../locales/en-US.ftl");

/// `(tag, display name, source)` for every bundled language.
pub const LANGUAGES: &[(&str, &str, &str)] = &[("en-US", "English", EN_US)];

pub struct I18n {
    primary: FluentBundle<FluentResource>,
    fallback: FluentBundle<FluentResource>,
    lang: String,
}

fn bundle(tag: &str, source: &str) -> FluentBundle<FluentResource> {
    let id: LanguageIdentifier = tag.parse().unwrap_or_default();
    let mut bundle = FluentBundle::new(vec![id]);
    bundle.set_use_isolating(false);
    let res = FluentResource::try_new(source.to_owned()).unwrap_or_else(|(res, errs)| {
        tracing::warn!("{} parse errors in {tag} locale", errs.len());
        res
    });
    if let Err(errs) = bundle.add_resource(res) {
        tracing::warn!("{} duplicate keys in {tag} locale", errs.len());
    }
    bundle
}

impl I18n {
    pub fn new(lang: &str) -> Self {
        let (tag, source) = LANGUAGES
            .iter()
            .find(|(t, _, _)| t.eq_ignore_ascii_case(lang))
            .map(|(t, _, s)| (*t, *s))
            .unwrap_or(("en-US", EN_US));
        Self {
            primary: bundle(tag, source),
            fallback: bundle("en-US", EN_US),
            lang: tag.to_owned(),
        }
    }

    pub fn lang(&self) -> &str {
        &self.lang
    }

    pub fn tr(&self, key: &str) -> String {
        self.tr_args(key, None)
    }

    pub fn tr_args(&self, key: &str, args: Option<&FluentArgs<'_>>) -> String {
        for bundle in [&self.primary, &self.fallback] {
            if let Some(pattern) = bundle.get_message(key).and_then(|m| m.value()) {
                let mut errors = vec![];
                return bundle.format_pattern(pattern, args, &mut errors).into_owned();
            }
        }
        key.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_and_unknown_keys() {
        let i18n = I18n::new("en-US");
        assert_eq!(i18n.tr("category-cleaner"), "Cleaner");
        assert_eq!(i18n.tr("no-such-key"), "no-such-key");
    }

    #[test]
    fn args_and_unknown_language_fallback() {
        let i18n = I18n::new("xx-YY");
        assert_eq!(i18n.lang(), "en-US");
        let mut args = FluentArgs::new();
        args.set("count", 3);
        assert_eq!(i18n.tr_args("journal-count", Some(&args)), "3 recorded changes");
    }
}
