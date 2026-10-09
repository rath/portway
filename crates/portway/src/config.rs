//! File configuration and command-line precedence.
use crate::cli::{Args, Mode};
use portway_core::{ForwarderConfig, Router, receiver::ReceiverConfig};
use serde::Deserialize;
use std::sync::Arc;
use std::{collections::BTreeMap, path::PathBuf};
/// One set of rates, in USD per million tokens.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    /// What a request that outgrows the vendor's long-context line is billed
    /// at instead, for all of its tokens.
    #[serde(default)]
    pub long_context: Option<LongContext>,
}
/// The rates a vendor bills a whole request at once its prompt, cache reads
/// included, passes `above` tokens.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct LongContext {
    pub above: u64,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
}
impl LongContext {
    /// The rates themselves, with no further line of their own.
    pub fn rates(&self) -> Rates {
        Rates {
            input: self.input,
            output: self.output,
            cache_read: self.cache_read,
            long_context: None,
        }
    }
}
/// A model's rates: the standard ones, for a request that names no
/// tier, and one set for each tier the vendor bills apart.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    #[serde(default)]
    pub long_context: Option<LongContext>,
    /// Keyed by the tier exactly as requests send it: their `speed`, or
    /// else their `service_tier`.
    #[serde(default)]
    pub tiers: BTreeMap<String, Rates>,
}
impl Price {
    /// The rates a request is billed at: the standard ones when it named no
    /// tier, that tier's own when the table has them, and `None` when it has
    /// not. A faster class billed at the standard rates would be a cost quietly
    /// too low, so a tier is never priced as another one.
    pub fn rates(&self, tier: Option<&str>) -> Option<Rates> {
        match tier {
            None => Some(Rates {
                input: self.input,
                output: self.output,
                cache_read: self.cache_read,
                long_context: self.long_context,
            }),
            Some(tier) => self.tiers.get(tier).copied(),
        }
    }
}
pub type Prices = BTreeMap<String, Price>;
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub upstream: Option<String>,
    /// Named upstreams, each answering under `/<name>/`.
    pub upstreams: BTreeMap<String, String>,
    /// Upstreams chosen by the JSON `model` field at the root.
    pub models: BTreeMap<String, String>,
    pub compression: ForwarderConfig,
    pub receiver: ReceiverConfig,
    pub prices: Prices,
    #[serde(default, deserialize_with = "crate::aliases::deserialize")]
    pub model_aliases: crate::aliases::ModelAliases,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 8787,
            upstream: None,
            upstreams: BTreeMap::new(),
            models: BTreeMap::new(),
            compression: ForwarderConfig::default(),
            receiver: ReceiverConfig::default(),
            prices: Prices::new(),
            model_aliases: Default::default(),
        }
    }
}
/// First existing candidate, or `None` when none of them is there. Pure, so
/// the search order is testable without a filesystem the test did not make.
fn default_config_path(candidates: &[PathBuf]) -> Option<PathBuf> {
    candidates
        .iter()
        .find(|p| !p.as_os_str().is_empty() && p.is_file())
        .cloned()
}

impl Config {
    /// The file `load` reads, or `None` for the built-in default. An explicit
    /// --config wins outright. Without one, look in the current directory
    /// first (project-local settings), then in the data directory: --data-dir
    /// when given, else $XDG_CONFIG_HOME/portway or ~/.config/portway. An
    /// instance given its own --data-dir never falls back to the default
    /// instance's file, and a dashboard given the same --data-dir (or none)
    /// finds the file the daemon keeps beside its database.
    pub fn path(args: &Args) -> Option<PathBuf> {
        match &args.config {
            Some(path) => Some(path.clone()),
            None => default_config_path(&[
                PathBuf::from("portway.toml"),
                crate::store::data_dir(args.data_dir.as_deref())
                    .map(|dir| dir.join("portway.toml"))
                    .unwrap_or_default(),
            ]),
        }
    }
    pub fn load(args: &Args) -> Result<Self, String> {
        let path = Self::path(args);
        let mut config = match path.as_deref().map(std::fs::read_to_string) {
            Some(Ok(text)) => toml::from_str::<Self>(&text)
                .map_err(|e| format!("{}: {e}", path.as_ref().unwrap().display()))?,
            Some(Err(e)) if args.config.is_none() && e.kind() == std::io::ErrorKind::NotFound => {
                Self::default()
            }
            Some(Err(e)) => return Err(format!("{}: {e}", path.as_ref().unwrap().display())),
            None => Self::default(),
        };
        if let Some(url) = &args.upstream {
            config.upstream = Some(url.clone());
            config.upstreams.clear();
            config.models.clear();
        }
        if let Some(host) = &args.host {
            config.host = host.clone();
        }
        if let Some(port) = args.port {
            config.port = port;
        }
        args.apply_compression(&mut config.compression);
        if (args.tui || args.web) && !config.has_destination() {
            config.compression.validate()?;
            config.receiver.validate()?;
        } else {
            config.validate(args.mode)?;
        }
        Ok(config)
    }
    /// Whether the file names somewhere to send requests. A dashboard that
    /// only attaches to a running instance needs none.
    pub fn has_destination(&self) -> bool {
        self.upstream.is_some() || !self.upstreams.is_empty() || !self.models.is_empty()
    }
    pub fn validate(&self, mode: Mode) -> Result<(), String> {
        self.compression.validate()?;
        self.receiver.validate()?;
        if self.upstream.is_some() && (!self.upstreams.is_empty() || !self.models.is_empty()) {
            return Err(
                "upstream is the one destination for everything: remove it to route by \
                 [upstreams] or [models]"
                    .into(),
            );
        }
        if !self.has_destination() {
            return Err(match mode {
                Mode::Receive => {
                    "receive requires an upstream, an [upstreams] table or a [models] table"
                }
                Mode::Forward => "configure an upstream, an [upstreams] table or a [models] table",
            }
            .into());
        }
        // Every set of rates the table holds, each tier's and each
        // long-context set included.
        let rates = self
            .prices
            .values()
            .flat_map(|price| {
                price
                    .rates(None)
                    .into_iter()
                    .chain(price.tiers.values().copied())
            })
            .flat_map(|rates| [Some(rates), rates.long_context.map(|long| long.rates())])
            .flatten();
        for rates in rates {
            if [rates.input, rates.output, rates.cache_read]
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
            compression.origin_compression = self.receiver.origin_compression.clone();
            compression.coding = portway_core::CodingPreference::Off;
            compression.dict = portway_core::DictionaryPreference::Off;
        }
        if let Some(url) = &self.upstream {
            Router::single(&compression, url, None)
        } else {
            let pairs = |table: &BTreeMap<String, String>| {
                table
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<Vec<_>>()
            };
            Router::build(
                &compression,
                &pairs(&self.upstreams),
                &pairs(&self.models),
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
    fn a_tier_is_priced_by_its_own_rates_and_never_by_another() {
        let config: Config = toml::from_str(
            "upstream='http://example.test'\n\
             [prices.'model-a']\ninput=1.0\noutput=2.0\ncache_read=0.1\n\
             [prices.'model-a'.tiers.'tier-a']\ninput=2.0\noutput=4.0\ncache_read=0.2\n",
        )
        .unwrap();
        assert!(config.validate(Mode::Forward).is_ok());
        let price = &config.prices["model-a"];
        let standard = Rates {
            input: 1.0,
            output: 2.0,
            cache_read: 0.1,
            long_context: None,
        };
        let faster = Rates {
            input: 2.0,
            output: 4.0,
            cache_read: 0.2,
            long_context: None,
        };
        assert_eq!(price.rates(None), Some(standard));
        assert_eq!(price.rates(Some("tier-a")), Some(faster));
        assert_eq!(price.rates(Some("tier-b")), None, "no fallback to standard");
        // A tier is a full set of rates: a partial one is refused, not
        // completed from the standard rates.
        assert!(
            toml::from_str::<Config>(
                "[prices.m]\ninput=1.0\noutput=2.0\ncache_read=0.1\n\
                 [prices.m.tiers.t]\ninput=2.0\n"
            )
            .is_err()
        );
        assert!(
            toml::from_str::<Config>(
                "[prices.m]\ninput=1.0\noutput=2.0\ncache_read=0.1\n\
                 [prices.m.tiers.t]\ninput=2.0\noutput=4.0\ncache_read=0.2\nmultiplier=2\n"
            )
            .is_err()
        );
    }
    /// A set of rates may carry the vendor's long-context line: a threshold
    /// and the rates a request past it is billed at, as an inline table that a
    /// tier's rates can carry too. It is held to the same rules as any rates.
    #[test]
    fn long_context_rates_ride_inside_a_set_of_rates() {
        let config: Config = toml::from_str(
            "upstream='http://example.test'\n\
             [prices.'model-a']\ninput=1.0\noutput=2.0\ncache_read=0.1\n\
             long_context={above=1000,input=2.0,output=3.0,cache_read=0.2}\n\
             [prices.'model-a'.tiers.'tier-a']\ninput=2.0\noutput=4.0\ncache_read=0.2\n\
             long_context={above=1000,input=4.0,output=6.0,cache_read=0.4}\n\
             [prices.'model-b']\ninput=1.0\noutput=2.0\ncache_read=0.1\n",
        )
        .unwrap();
        assert!(config.validate(Mode::Forward).is_ok());
        let long = |model: &str, tier: Option<&str>| {
            config.prices[model].rates(tier).unwrap().long_context
        };
        assert_eq!(
            long("model-a", None),
            Some(LongContext {
                above: 1000,
                input: 2.0,
                output: 3.0,
                cache_read: 0.2,
            })
        );
        assert_eq!(long("model-a", Some("tier-a")).unwrap().input, 4.0);
        assert_eq!(long("model-b", None), None, "a vendor without the line");

        let refused = |table: &str| {
            let file = format!(
                "upstream='http://example.test'\n[prices.m]\ninput=1.0\noutput=2.0\ncache_read=0.1\n{table}\n"
            );
            match toml::from_str::<Config>(&file) {
                Ok(config) => config.validate(Mode::Forward).is_err(),
                Err(_) => true,
            }
        };
        assert!(refused(
            "long_context={above=1000,input=2.0,output=-3.0,cache_read=0.2}"
        ));
        assert!(refused("long_context={above=1000,input=2.0,output=3.0}"));
        assert!(refused(
            "long_context={above=-1,input=2.0,output=3.0,cache_read=0.2}"
        ));
        assert!(refused(
            "long_context={above=1000,input=2.0,output=3.0,cache_read=0.2,factor=2}"
        ));
        assert!(!refused(
            "long_context={above=1000,input=2.0,output=3.0,cache_read=0.2}"
        ));
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
        // A [models] table serves both modes: a receiver routes each request
        // to the origin configured for its JSON model.
        assert!(config.validate(Mode::Receive).is_ok());
        assert!(Config::default().validate(Mode::Receive).is_err());
        // Mounts coexist with models and serve both modes too; only the
        // single upstream excludes them.
        config
            .upstreams
            .insert("vendor".into(), "http://example.test".into());
        assert!(config.validate(Mode::Forward).is_ok());
        assert!(config.validate(Mode::Receive).is_ok());
        config.models.clear();
        assert!(config.validate(Mode::Forward).is_ok());
        config.upstream = Some("http://example.test".into());
        assert!(config.validate(Mode::Forward).is_err());
        config.upstream = None;
        assert!(config.router(Mode::Forward).is_ok());
        config.prices.insert(
            "sample".into(),
            Price {
                input: f64::NAN,
                output: 1.0,
                cache_read: 0.0,
                long_context: None,
                tiers: BTreeMap::new(),
            },
        );
        assert!(config.validate(Mode::Forward).is_err());
        // A tier's rates are held to the same rule as the standard ones.
        let negative = Rates {
            input: 1.0,
            output: -1.0,
            cache_read: 0.0,
            long_context: None,
        };
        config.prices.insert(
            "sample".into(),
            Price {
                input: 1.0,
                output: 1.0,
                cache_read: 0.0,
                long_context: None,
                tiers: BTreeMap::from([("tier-a".into(), negative)]),
            },
        );
        assert!(config.validate(Mode::Forward).is_err());
        assert!(toml::from_str::<Config>("unknown_option=1").is_err());
        assert!(
            toml::from_str::<Config>("[models]\na='http://a.test'\na='http://b.test'").is_err()
        );
    }
    #[test]
    fn upstreams_mount_by_name_and_the_cli_upstream_clears_both_tables() {
        let config: Config = toml::from_str(
            "[upstreams]\nanthropic='https://api.example.test'\n[models]\n'model-a'='http://a.test'\n",
        )
        .unwrap();
        assert_eq!(config.upstreams["anthropic"], "https://api.example.test");
        assert!(config.validate(Mode::Forward).is_ok());
        let names: Vec<String> = config
            .router(Mode::Forward)
            .unwrap()
            .routes()
            .iter()
            .map(|(name, _)| name.clone())
            .collect();
        assert_eq!(names, ["anthropic", "model-a"]);
        // A name the router refuses is refused here too, before serving.
        let bad: Config =
            toml::from_str("[upstreams]\n'a/b'='https://api.example.test'\n").unwrap();
        assert!(bad.router(Mode::Forward).is_err());

        let path = std::env::temp_dir().join(format!("portway-mounts-{}.toml", std::process::id()));
        std::fs::write(&path, "[upstreams]\nanthropic='https://api.example.test'\n").unwrap();
        let args = Args::parse_from([
            "portway",
            "--config",
            path.to_str().unwrap(),
            "--upstream",
            "http://single.test",
        ]);
        let config = Config::load(&args).unwrap();
        assert_eq!(config.upstream.as_deref(), Some("http://single.test"));
        assert!(config.upstreams.is_empty());
        std::fs::remove_file(path).unwrap();
    }
    #[test]
    fn an_explicit_missing_file_is_an_error() {
        let args = Args::parse_from(["portway", "--config", "/no-such-portway-config.toml"]);
        assert!(Config::load(&args).is_err());
    }

    #[test]
    fn the_search_order_prefers_earlier_candidates() {
        let base = std::env::temp_dir().join(format!("portway-search-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let first = base.join("first.toml");
        let second = base.join("second.toml");
        assert_eq!(default_config_path(&[first.clone(), second.clone()]), None);
        std::fs::write(&second, "").unwrap();
        assert_eq!(
            default_config_path(&[first.clone(), second.clone()]),
            Some(second.clone())
        );
        std::fs::write(&first, "").unwrap();
        assert_eq!(
            default_config_path(&[first.clone(), second.clone()]),
            Some(first.clone())
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn a_candidate_with_an_empty_path_is_skipped() {
        // data_dir errors yield an empty PathBuf as a placeholder; it must
        // not shadow an earlier real file.
        let base = std::env::temp_dir().join(format!("portway-empty-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let real = base.join("real.toml");
        std::fs::write(&real, "").unwrap();
        assert_eq!(
            default_config_path(&[PathBuf::new(), real.clone()]),
            Some(real)
        );
        std::fs::remove_dir_all(&base).unwrap();
    }
}
