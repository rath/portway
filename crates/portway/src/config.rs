//! File configuration and command-line precedence.
use crate::cli::{Args, Mode};
use portway_core::{ForwarderConfig, Router, receiver::ReceiverConfig};
use serde::Deserialize;
use std::sync::Arc;
use std::{collections::BTreeMap, path::PathBuf};
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
}
pub type Prices = BTreeMap<String, Price>;
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub upstream: Option<String>,
    pub models: BTreeMap<String, String>,
    pub compression: ForwarderConfig,
    pub receiver: ReceiverConfig,
    pub prices: Prices,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8787,
            upstream: None,
            models: BTreeMap::new(),
            compression: ForwarderConfig::default(),
            receiver: ReceiverConfig::default(),
            prices: Prices::new(),
        }
    }
}
impl Config {
    pub fn load(args: &Args) -> Result<Self, String> {
        let path = args
            .config
            .clone()
            .unwrap_or_else(|| PathBuf::from("portway.toml"));
        let mut config = match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str::<Self>(&text).map_err(|e| format!("{}: {e}", path.display()))?
            }
            Err(e) if args.config.is_none() && e.kind() == std::io::ErrorKind::NotFound => {
                Self::default()
            }
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        if let Some(url) = &args.upstream {
            config.upstream = Some(url.clone());
            config.models.clear();
        }
        if let Some(host) = &args.host {
            config.host = host.clone();
        }
        if let Some(port) = args.port {
            config.port = port;
        }
        args.apply_compression(&mut config.compression);
        if args.tui && config.upstream.is_none() && config.models.is_empty() {
            config.compression.validate()?;
            config.receiver.validate()?;
        } else {
            config.validate(args.mode)?;
        }
        Ok(config)
    }
    pub fn validate(&self, mode: Mode) -> Result<(), String> {
        self.compression.validate()?;
        self.receiver.validate()?;
        if self.upstream.is_some() == !self.models.is_empty() {
            return Err("configure either upstream or [models], exclusively".into());
        }
        if mode == Mode::Receive && self.upstream.is_none() {
            return Err("receive requires a single upstream".into());
        }
        for price in self.prices.values() {
            if [price.input, price.output, price.cache_read]
                .iter()
                .any(|v| !v.is_finite() || *v < 0.0)
            {
                return Err("prices must be finite nonnegative USD per million tokens".into());
            }
        }
        Ok(())
    }
    pub fn router(&self, mode: Mode) -> Result<Arc<Router>, String> {
        let mut compression = self.compression.clone();
        if mode == Mode::Receive {
            compression.coding = portway_core::CodingPreference::Off;
            compression.dict = portway_core::DictionaryPreference::Off;
        }
        if let Some(url) = &self.upstream {
            Router::single(&compression, url, None)
        } else {
            Router::build(
                &compression,
                &self
                    .models
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Vec<_>>(),
                None,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    #[test]
    fn explicit_cli_values_override_files_without_default_flags_erasing_them() {
        let path = std::env::temp_dir().join(format!("portway-config-{}.toml", std::process::id()));
        std::fs::write(&path,"upstream='http://example.test/api'\nport=9000\n[compression]\nlevel=3\nmin_bytes=4096\n[prices.sample]\ninput=1.0\noutput=2.0\ncache_read=0.1\n").unwrap();
        let args = Args::parse_from([
            "portway",
            "--config",
            path.to_str().unwrap(),
            "--port",
            "9001",
        ]);
        let config = Config::load(&args).unwrap();
        assert_eq!(config.port, 9001);
        assert_eq!(config.compression.level, 3);
        assert_eq!(config.compression.min_bytes, 4096);
        assert_eq!(config.prices["sample"].input, 1.0);
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn route_modes_and_prices_are_validated_without_inventing_defaults() {
        assert!(Config::default().validate(Mode::Forward).is_err());
        let mut config = Config {
            upstream: Some("http://example.test".into()),
            ..Config::default()
        };
        assert!(config.prices.is_empty());
        assert!(config.validate(Mode::Forward).is_ok());
        config
            .models
            .insert("sample".into(), "http://example.test".into());
        assert!(config.validate(Mode::Forward).is_err());
        config.upstream = None;
        assert!(config.validate(Mode::Forward).is_ok());
        assert!(config.validate(Mode::Receive).is_err());
        config.prices.insert(
            "sample".into(),
            Price {
                input: f64::NAN,
                output: 1.0,
                cache_read: 0.0,
            },
        );
        assert!(config.validate(Mode::Forward).is_err());
        assert!(toml::from_str::<Config>("unknown_option=1").is_err());
        assert!(
            toml::from_str::<Config>("[models]\na='http://a.test'\na='http://b.test'").is_err()
        );
    }
    #[test]
    fn an_explicit_missing_file_is_an_error() {
        let args = Args::parse_from(["portway", "--config", "/no-such-portway-config.toml"]);
        assert!(Config::load(&args).is_err());
    }
}
