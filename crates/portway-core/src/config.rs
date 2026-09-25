//! Explicit configuration for embedded forwarders.
use crate::telemetry::Telemetry;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

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
    pub coding: CodingPreference,
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
            coding: CodingPreference::Auto,
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
