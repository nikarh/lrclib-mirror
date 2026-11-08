//! Configuration from command line flags, with environment variables as the fallback.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

pub const DEFAULT_LISTING_URL: &str = "https://lrclib-db-dumps.bu3nnyut4y9jfkdg.workers.dev";
pub const DEFAULT_DUMP_URL: &str = "https://db-dumps.lrclib.net";

pub const HELP: &str = "\
lrclib-mirror: the LRCLIB API (read-only) and web search on a local copy of LRCLIB's public database dump

Usage: lrclib-mirror [serve] [OPTIONS]
       lrclib-mirror import DUMP OUT [--import-threads N] [--import-memory MB]
       lrclib-mirror healthcheck [--listen ADDR]

  import                   builds a store in the new directory OUT from DUMP (an LRCLIB dump: .sqlite3,
                           .sqlite3.gz, or - for standard input); run again, an interrupted import continues

Options (each has an environment variable fallback):
  --data-dir DIR           LRCLIB_MIRROR_DATA_DIR       the stores, and dumps to import [/data]
  --listen ADDR            LRCLIB_MIRROR_LISTEN         HTTP address [0.0.0.0:3300]
  --update-interval TIME   LRCLIB_MIRROR_UPDATE_INTERVAL
                                                        how often to check for a new dump (30m, 24h, 7d);
                                                        0: never [24h]
  --min-free-gb GB         LRCLIB_MIRROR_MIN_FREE_GB    free space an import needs to start [60]
  --import-threads N       LRCLIB_MIRROR_IMPORT_THREADS threads importing a dump [the number of CPUs]
  --import-memory MB       LRCLIB_MIRROR_IMPORT_MEMORY  memory an import may take, roughly (at least 768)
                                                        [60% of the container's limit or the machine's
                                                        memory, at most 4096]
  --listing-url URL        LRCLIB_MIRROR_LISTING_URL    the dump listing [LRCLIB's]
  --dump-url URL           LRCLIB_MIRROR_DUMP_URL       where dumps are downloaded from [LRCLIB's]
  -h, --help               this help
  -V, --version            the version and the upstream commit of the web search

Logging: RUST_LOG (default info,tantivy=warn).";

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub listen: SocketAddr,
    /// `None`: never check for a new dump (the newest dump in the data directory is served).
    pub update_interval: Option<Duration>,
    pub min_free_bytes: u64,
    pub import_threads: usize,
    /// `None`: from the memory available.
    pub import_memory_bytes: Option<u64>,
    pub listing_url: String,
    pub dump_url: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{name}: invalid value {value:?}")]
    Invalid { name: String, value: String },
    #[error(transparent)]
    Args(#[from] pico_args::Error),
    #[error("unknown arguments: {0}")]
    Unknown(String),
}

impl Config {
    pub fn parse(args: &mut pico_args::Arguments) -> Result<Self, ConfigError> {
        let data_dir: String = value(
            args,
            "--data-dir",
            "LRCLIB_MIRROR_DATA_DIR",
            "/data".to_owned(),
        )?;
        let listen = value(
            args,
            "--listen",
            "LRCLIB_MIRROR_LISTEN",
            SocketAddr::from(([0, 0, 0, 0], 3300)),
        )?;
        let interval: Interval = value(
            args,
            "--update-interval",
            "LRCLIB_MIRROR_UPDATE_INTERVAL",
            Interval(Some(Duration::from_hours(24))),
        )?;
        let min_free_gb: u64 = value(args, "--min-free-gb", "LRCLIB_MIRROR_MIN_FREE_GB", 60)?;
        let import_threads: usize = value(
            args,
            "--import-threads",
            "LRCLIB_MIRROR_IMPORT_THREADS",
            std::thread::available_parallelism().map_or(2, std::num::NonZero::get),
        )?;
        let import_memory_mb: Option<u64> = value::<Auto>(
            args,
            "--import-memory",
            "LRCLIB_MIRROR_IMPORT_MEMORY",
            Auto(None),
        )?
        .0;
        if import_threads == 0 {
            return Err(ConfigError::Invalid {
                name: "--import-threads".to_owned(),
                value: "0".to_owned(),
            });
        }
        let listing_url = value(
            args,
            "--listing-url",
            "LRCLIB_MIRROR_LISTING_URL",
            DEFAULT_LISTING_URL.to_owned(),
        )?;
        let dump_url = value(
            args,
            "--dump-url",
            "LRCLIB_MIRROR_DUMP_URL",
            DEFAULT_DUMP_URL.to_owned(),
        )?;

        Ok(Self {
            data_dir: PathBuf::from(data_dir),
            listen,
            update_interval: interval.0,
            min_free_bytes: min_free_gb.saturating_mul(1 << 30),
            import_threads,
            import_memory_bytes: import_memory_mb.map(|mb| mb.saturating_mul(1 << 20)),
            listing_url,
            dump_url: dump_url.trim_end_matches('/').to_owned(),
        })
    }

    /// Fails on arguments left over.
    pub fn finish(args: pico_args::Arguments) -> Result<(), ConfigError> {
        let rest = args.finish();
        if !rest.is_empty() {
            let rest: Vec<_> = rest
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            return Err(ConfigError::Unknown(rest.join(" ")));
        }
        Ok(())
    }
}

/// A number, or nothing (the default) when not given.
struct Auto(Option<u64>);

impl FromStr for Auto {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse().map(|n| Self(Some(n)))
    }
}

/// A duration like `90s`, `30m`, `24h` or `7d`; `0` is none.
struct Interval(Option<Duration>);

impl FromStr for Interval {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, ()> {
        if s == "0" {
            return Ok(Self(None));
        }
        let split = s.find(|c: char| !c.is_ascii_digit()).ok_or(())?;
        let (number, unit) = s.split_at(split);
        let number: u64 = number.parse().map_err(|_| ())?;
        let seconds = match unit {
            "s" => 1,
            "m" => 60,
            "h" => 3600,
            "d" => 86400,
            _ => return Err(()),
        };
        match number.checked_mul(seconds) {
            Some(0) => Ok(Self(None)),
            Some(secs) => Ok(Self(Some(Duration::from_secs(secs)))),
            None => Err(()),
        }
    }
}

/// The flag's value, else the environment variable's, else the default.
fn value<T: FromStr>(
    args: &mut pico_args::Arguments,
    flag: &'static str,
    env: &str,
    default: T,
) -> Result<T, ConfigError> {
    let raw = match args.opt_value_from_str::<_, String>(flag)? {
        Some(v) => (flag, v),
        None => match std::env::var(env) {
            Ok(v) if !v.is_empty() => (env, v),
            _ => return Ok(default),
        },
    };
    raw.1.parse().map_err(|_| ConfigError::Invalid {
        name: raw.0.to_owned(),
        value: raw.1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Config, ConfigError> {
        let mut args = pico_args::Arguments::from_vec(args.iter().map(Into::into).collect());
        let config = Config::parse(&mut args)?;
        Config::finish(args)?;
        Ok(config)
    }

    #[test]
    fn flags_override_defaults() {
        let c = parse(&[
            "--data-dir",
            "/x",
            "--update-interval",
            "0",
            "--dump-url",
            "http://h/",
        ])
        .unwrap();
        assert_eq!(c.data_dir, PathBuf::from("/x"));
        assert_eq!(c.update_interval, None);
        assert_eq!(c.dump_url, "http://h");
        assert_eq!(c.listen.port(), 3300);
    }

    #[test]
    fn rejects_unknown_and_invalid() {
        assert!(matches!(parse(&["--nope"]), Err(ConfigError::Unknown(_))));
        assert!(matches!(
            parse(&["--update-interval", "x"]),
            Err(ConfigError::Invalid { .. })
        ));
        // a bare number has no unit
        assert!(matches!(
            parse(&["--update-interval", "24"]),
            Err(ConfigError::Invalid { .. })
        ));
    }

    #[test]
    fn intervals() {
        let interval = |s: &str| parse(&["--update-interval", s]).unwrap().update_interval;
        assert_eq!(interval("90s"), Some(Duration::from_secs(90)));
        assert_eq!(interval("30m"), Some(Duration::from_mins(30)));
        assert_eq!(interval("7d"), Some(Duration::from_hours(7 * 24)));
        assert_eq!(interval("0"), None);
        assert_eq!(
            parse(&[]).unwrap().update_interval,
            Some(Duration::from_hours(24))
        );
    }
}
