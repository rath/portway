//! Explicit configuration for embedded forwarders.
use crate::telemetry::Telemetry;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::{collections::BTreeMap, time::Duration};

/// Optional time-to-headers budgets, keyed by the router's route names.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct TimeoutConfig {
    pub routes: BTreeMap<String, RouteTimeouts>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouteTimeouts {
    pub response_headers_seconds: Option<u64>,
}

impl TimeoutConfig {
    pub fn validate(&self) -> Result<(), String> {
        for (name, route) in &self.routes {
            if let Some(seconds) = route.response_headers_seconds
                && (seconds == 0
                    || std::time::Instant::now()
                        .checked_add(Duration::from_secs(seconds))
                        .is_none())
            {
                return Err(format!(
                    "timeouts.routes.{name}.response_headers_seconds must be positive and representable"
                ));
            }
        }
        Ok(())
    }

    pub fn validate_routes<'a>(
        &self,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), String> {
        let names: Vec<_> = names.into_iter().collect();
        for name in self.routes.keys() {
            if !names.contains(&name.as_str()) {
                return Err(format!("timeouts names unknown route {name:?}"));
            }
        }
        Ok(())
    }

    pub(crate) fn response_headers(&self, name: &str) -> Option<Duration> {
        self.routes
            .get(name)?
            .response_headers_seconds
            .map(Duration::from_secs)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CodingPreference {
    #[default]
    Auto,
    Zstd,
    Gzip,
    Off,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DictionaryPreference {
    #[default]
    Auto,
    Off,
}
impl std::str::FromStr for CodingPreference {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "auto" => Ok(Self::Auto),
            "zstd" => Ok(Self::Zstd),
            "gzip" => Ok(Self::Gzip),
            "off" => Ok(Self::Off),
            _ => Err("expected auto, zstd, gzip or off".into()),
        }
    }
}
impl std::str::FromStr for DictionaryPreference {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "auto" => Ok(Self::Auto),
            "off" => Ok(Self::Off),
            _ => Err("expected auto or off".into()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ForwarderConfig {
    /// Standalone configuration supplies this from the top-level [timeouts].
    #[serde(skip)]
    pub timeouts: TimeoutConfig,
    pub coding: CodingPreference,
    /// Origin policy is selected by receiver configuration, not sender TOML.
    #[serde(skip)]
    pub origin_compression: crate::origin::OriginCompressionConfig,
    pub dict: DictionaryPreference,
    pub level: i32,
    pub min_bytes: usize,
    pub max_body_bytes: usize,
    /// Absolute fallback path. Probing always tries Portway capabilities first.
    pub probe_path: Option<String>,
    #[serde(skip)]
    pub telemetry: Arc<Telemetry>,
}
impl Default for ForwarderConfig {
    fn default() -> Self {
        Self {
            timeouts: TimeoutConfig::default(),
            coding: CodingPreference::Auto,
            origin_compression: Default::default(),
            dict: DictionaryPreference::Auto,
            level: 11,
            min_bytes: 1024,
            max_body_bytes: 256 << 20,
            probe_path: None,
            telemetry: Arc::default(),
        }
    }
}
impl ForwarderConfig {
    pub fn validate(&self) -> Result<(), String> {
        self.timeouts.validate()?;
        if !(1..=19).contains(&self.level) {
            return Err("compression level must be 1..19".into());
        }
        if self.max_body_bytes == 0 {
            return Err("max_body_bytes must be positive".into());
        }
        if let Some(path) = &self.probe_path {
            let uri: http::Uri = path.parse().map_err(|_| "invalid probe_path")?;
            if !path.starts_with('/')
                || path.starts_with("//")
                || uri.authority().is_some()
                || uri.query().is_some()
            {
                return Err("probe_path must be an absolute origin path without a query".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_timeouts_reject_unrepresentable_durations() {
        let config = TimeoutConfig {
            routes: BTreeMap::from([(
                "upstream".into(),
                RouteTimeouts {
                    response_headers_seconds: Some(u64::MAX),
                },
            )]),
        };
        assert!(config.validate().is_err());
    }
}
