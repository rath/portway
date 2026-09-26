//! Portway's application arguments.
#[cfg(feature = "tui")]
use crate::tui::state::{COLUMNS, Column};
use clap::{Parser, ValueEnum};
pub use portway_core::config::{CodingPreference as CodingArg, DictionaryPreference as DictArg};
use std::{path::PathBuf, time::Duration};
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    #[default]
    Forward,
    Receive,
}
#[derive(Debug, Clone, Parser)]
#[command(
    name = "portway",
    version = crate::VERSION,
    about = "A compression-first HTTP forwarder"
)]
pub struct Args {
    /// Forward requests, or receive compressed requests in front of an HTTP service.
    #[arg(value_enum, default_value = "forward")]
    pub mode: Mode,
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
    #[arg(long, value_name = "URL")]
    pub upstream: Option<String>,
    #[arg(long)]
    pub host: Option<String>,
    #[arg(long)]
    pub port: Option<u16>,
    #[arg(long)]
    pub coding: Option<CodingArg>,
    #[arg(long,value_parser=level_in_range)]
    pub level: Option<i32>,
    #[arg(long)]
    pub dict: Option<DictArg>,
    #[arg(long)]
    pub min_bytes: Option<usize>,
    #[arg(long)]
    pub max_body_bytes: Option<usize>,
    #[arg(long)]
    pub probe_path: Option<String>,
    #[cfg_attr(feature = "tui", arg(long, group = "operation"))]
    #[cfg_attr(not(feature = "tui"), arg(skip))]
    pub tui: bool,
    #[cfg(feature = "tui")]
    #[arg(long,value_name="LIST",value_delimiter=',',value_parser=column,requires="tui")]
    pub event_columns: Option<Vec<Column>>,
    /// Serve the dashboard in a browser, and open it in the default one.
    /// Attaches to a forwarder already on the port, like --tui, and combines
    /// with --daemon, which only prints the link.
    #[cfg_attr(
        feature = "web",
        arg(long, conflicts_with_all = ["stop", "reload", "status", "report"])
    )]
    #[cfg_attr(all(feature = "web", feature = "tui"), arg(conflicts_with = "tui"))]
    #[cfg_attr(not(feature = "web"), arg(skip))]
    pub web: bool,
    #[cfg(feature = "web")]
    #[arg(
        long,
        value_name = "HOST",
        default_value = "127.0.0.1",
        requires = "web"
    )]
    pub web_host: String,
    #[cfg(feature = "web")]
    #[arg(long, value_name = "PORT", default_value_t = crate::web::DEFAULT_PORT, requires = "web")]
    pub web_port: u16,
    /// A name the console answers to besides localhost and IP addresses, such
    /// as the machine's host name; repeat or separate with commas.
    #[cfg(feature = "web")]
    #[arg(
        long,
        value_name = "NAME",
        value_delimiter = ',',
        value_parser = crate::web::address::host_name,
        requires = "web"
    )]
    pub web_allow_host: Vec<String>,
    /// Print the console's link without opening a browser.
    #[cfg(feature = "web")]
    #[arg(long, requires = "web")]
    pub no_open: bool,
    #[arg(long, value_name = "PATH")]
    pub data_dir: Option<PathBuf>,
    #[arg(long, group = "operation")]
    pub daemon: bool,
    #[arg(long, group = "operation")]
    pub stop: bool,
    #[arg(long, group = "operation")]
    pub reload: bool,
    #[arg(long, group = "operation")]
    pub status: bool,
    #[arg(long, group = "operation")]
    pub report: bool,
    #[arg(long,value_name="SPAN",default_value="24h",requires="report",value_parser=parse_span)]
    pub since: Duration,
    #[arg(long, value_name = "NAME", requires = "report")]
    pub model: Option<String>,
    #[arg(long, value_name = "DAYS", default_value_t = 0)]
    pub retention_days: u32,
}
impl Default for Args {
    fn default() -> Self {
        Self::parse_from(["portway"])
    }
}
impl Args {
    pub fn forwarder_config(&self) -> portway_core::ForwarderConfig {
        let mut config = portway_core::ForwarderConfig::default();
        self.apply_compression(&mut config);
        config
    }
    pub fn apply_compression(&self, config: &mut portway_core::ForwarderConfig) {
        if let Some(v) = self.coding {
            config.coding = v;
        }
        if let Some(v) = self.level {
            config.level = v;
        }
        if let Some(v) = self.dict {
            config.dict = v;
        }
        if let Some(v) = self.min_bytes {
            config.min_bytes = v;
        }
        if let Some(v) = self.max_body_bytes {
            config.max_body_bytes = v;
        }
        if let Some(v) = &self.probe_path {
            config.probe_path = Some(v.clone());
        }
        config.telemetry = crate::telemetry::core();
    }
}
#[cfg(feature = "tui")]
fn column(raw: &str) -> Result<Column, String> {
    Column::from_name(raw).ok_or_else(|| {
        format!(
            "unknown column {raw:?}: {}",
            COLUMNS
                .into_iter()
                .map(Column::name)
                .collect::<Vec<_>>()
                .join(", ")
        )
    })
}
pub fn parse_span(raw: &str) -> Result<Duration, String> {
    const BAD: &str = "--since must look like 90s, 30m, 24h or 7d";
    let (count, scale) = match raw.as_bytes().last() {
        Some(b's') => (&raw[..raw.len() - 1], 1),
        Some(b'm') => (&raw[..raw.len() - 1], 60),
        Some(b'h') => (&raw[..raw.len() - 1], 3600),
        Some(b'd') => (&raw[..raw.len() - 1], 86400),
        _ => return Err(BAD.into()),
    };
    let count: u64 = count.parse().map_err(|_| BAD)?;
    count
        .checked_mul(scale)
        .filter(|n| *n > 0)
        .map(Duration::from_secs)
        .ok_or_else(|| BAD.into())
}
fn level_in_range(raw: &str) -> Result<i32, String> {
    raw.parse::<i32>()
        .ok()
        .filter(|v| (1..=19).contains(v))
        .ok_or_else(|| "--level must be 1..19".into())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn modes_and_operations_are_exclusive() {
        assert!(Args::try_parse_from(["portway", "--stop", "--daemon"]).is_err());
        #[cfg(feature = "tui")]
        assert!(Args::try_parse_from(["portway", "--daemon", "--tui"]).is_err());
        #[cfg(not(feature = "tui"))]
        assert!(Args::try_parse_from(["portway", "--tui"]).is_err());
        assert_eq!(
            Args::parse_from(["portway", "receive", "--upstream", "http://localhost:8000"]).mode,
            Mode::Receive
        );
    }
    #[test]
    fn the_console_is_a_way_to_run_not_a_one_shot_command() {
        #[cfg(feature = "web")]
        {
            let args = Args::parse_from(["portway", "--web", "--daemon", "--web-port", "0"]);
            assert!(args.web && args.daemon);
            assert_eq!((args.web_host.as_str(), args.web_port), ("127.0.0.1", 0));
            assert_eq!(Args::parse_from(["portway", "--web"]).web_port, 8790);
            for other in ["--stop", "--reload", "--status", "--report"] {
                assert!(
                    Args::try_parse_from(["portway", "--web", other]).is_err(),
                    "{other}"
                );
            }
            assert!(Args::try_parse_from(["portway", "--web-port", "1"]).is_err());
            assert!(Args::parse_from(["portway", "--web", "--no-open"]).no_open);
            assert!(!Args::parse_from(["portway", "--web"]).no_open);
            let args = Args::parse_from([
                "portway",
                "--web",
                "--web-allow-host",
                "Sender-Host,box.lan",
                "--web-allow-host",
                "other",
            ]);
            assert_eq!(args.web_allow_host, ["sender-host", "box.lan", "other"]);
            assert!(
                Args::try_parse_from(["portway", "--web", "--web-allow-host", "sender-host:8790"])
                    .is_err()
            );
            assert!(Args::try_parse_from(["portway", "--web-allow-host", "sender-host"]).is_err());
            assert!(Args::try_parse_from(["portway", "--no-open"]).is_err());
            #[cfg(feature = "tui")]
            assert!(Args::try_parse_from(["portway", "--web", "--tui"]).is_err());
        }
        #[cfg(not(feature = "web"))]
        assert!(Args::try_parse_from(["portway", "--web"]).is_err());
    }
    #[test]
    fn report_window_is_bounded_and_positive() {
        assert_eq!(
            Args::parse_from(["portway", "--report", "--since", "7d"]).since,
            Duration::from_secs(604800)
        );
        for bad in [
            "0s",
            "24",
            "h",
            "-1h",
            "1.5h",
            "10x",
            "18446744073709551615d",
        ] {
            assert!(Args::try_parse_from(["portway", "--report", "--since", bad]).is_err());
        }
    }
    #[test]
    fn retention_keeps_everything_by_default() {
        assert_eq!(Args::default().retention_days, 0);
    }
}
