//! Presentation names only: never routing, pricing, or storage identities.
use serde::{Deserialize, Deserializer};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

pub type ModelAliases = BTreeMap<String, String>;

pub fn display<'a>(aliases: &'a ModelAliases, id: &'a str) -> &'a str {
    aliases.get(id).map(String::as_str).unwrap_or(id)
}

pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<ModelAliases, D::Error> {
    let aliases = ModelAliases::deserialize(deserializer)?;
    for (id, name) in &aliases {
        if [id, name]
            .iter()
            .any(|s| s.trim().is_empty() || s.chars().any(char::is_control))
        {
            return Err(serde::de::Error::custom(
                "model_aliases keys and names must be nonempty and contain no control characters",
            ));
        }
    }
    Ok(aliases)
}

/// Small immutable maps shared by the listener and synchronous dashboard threads.
#[derive(Clone, Default)]
pub struct Shared(Arc<RwLock<Arc<ModelAliases>>>);

impl Shared {
    pub fn new(aliases: ModelAliases) -> Self {
        Self(Arc::new(RwLock::new(Arc::new(aliases))))
    }
    pub fn get(&self) -> Arc<ModelAliases> {
        Arc::clone(&self.0.read().expect("model aliases"))
    }
    pub fn set(&self, aliases: ModelAliases) {
        *self.0.write().expect("model aliases") = Arc::new(aliases);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn config_names_are_exact_display_only_and_not_recursive() {
        let config: Config = toml::from_str(
            r#"
[model_aliases]
"vendor/model" = "short"
short = "other"
"historical/model" = "short"
"#,
        )
        .unwrap();
        assert_eq!(display(&config.model_aliases, "vendor/model"), "short");
        assert_eq!(
            display(&config.model_aliases, "Vendor/model"),
            "Vendor/model"
        );
        assert_eq!(display(&config.model_aliases, ""), "");
        assert!(config.models.is_empty());
        assert!(config.prices.is_empty());
        for entry in [
            r#"" = "name""#,
            r#"id = "  ""#,
            r#"id = "line\nnext""#,
            r#""bad\tkey" = "name""#,
        ] {
            assert!(
                toml::from_str::<Config>(&format!("[model_aliases]\n{entry}")).is_err(),
                "{entry}"
            );
        }
        assert!(
            toml::from_str::<Config>("")
                .unwrap()
                .model_aliases
                .is_empty()
        );
    }
}
