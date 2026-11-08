//! Configuration from command line flags, with environment variables as the fallback.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};

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
  --update-schedule CRON   LRCLIB_MIRROR_UPDATE_SCHEDULE
                                                        when to check for a new dump, besides at start: a
                                                        cron expression (minute hour day month weekday) in
                                                        the time zone of TZ (else the system's); off: never
                                                        [34 2 * * *, every day at 02:34]
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
    pub update_schedule: Option<Schedule>,
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
    #[error("{old} was replaced by {new}")]
    Replaced {
        old: &'static str,
        new: &'static str,
    },
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
        if args.contains("--update-interval") {
            return Err(ConfigError::Replaced {
                old: "--update-interval",
                new: "--update-schedule",
            });
        }
        if std::env::var_os("LRCLIB_MIRROR_UPDATE_INTERVAL").is_some() {
            return Err(ConfigError::Replaced {
                old: "LRCLIB_MIRROR_UPDATE_INTERVAL",
                new: "LRCLIB_MIRROR_UPDATE_SCHEDULE",
            });
        }
        let cron: Cron = value(
            args,
            "--update-schedule",
            "LRCLIB_MIRROR_UPDATE_SCHEDULE",
            Cron(Some(default_schedule())),
        )?;
        let update_schedule = match cron.0 {
            Some(cron) => Some(Schedule {
                cron,
                tz: time_zone()?,
            }),
            None => None,
        };
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
            update_schedule,
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

/// The default schedule: every day at 02:34.
pub const DEFAULT_UPDATE_SCHEDULE: &str = "34 2 * * *";

/// When to check for a new dump: a cron expression in a time zone.
#[derive(Debug, Clone)]
pub struct Schedule {
    cron: croner::Cron,
    tz: TimeZone,
}

impl Schedule {
    /// A cron expression (`minute hour day month weekday`, or a nickname like `@daily`) in the time zone `tz`.
    pub fn parse(expr: &str, tz: TimeZone) -> Result<Self, croner::errors::CronError> {
        Ok(Self {
            cron: expr.parse()?,
            tz,
        })
    }

    /// The first time after `now` the schedule fires (`None` for one that never does, like `0 0 30 2 *`).
    #[must_use]
    pub fn next_after(&self, now: Timestamp) -> Option<Zoned> {
        self.cron
            .find_next_occurrence(&now.to_zoned(self.tz.clone()), false)
            .ok()
    }

    /// The next time the schedule fires, and how long until then.
    #[must_use]
    pub fn next(&self) -> Option<(Zoned, Duration)> {
        let now = Timestamp::now();
        let next = self.next_after(now)?;
        let wait = Duration::try_from(now.duration_until(next.timestamp())).unwrap_or_default();
        Some((next, wait))
    }
}

impl Default for Schedule {
    /// Every day at 02:34 UTC.
    fn default() -> Self {
        Self {
            cron: default_schedule(),
            tz: TimeZone::UTC,
        }
    }
}

impl std::fmt::Display for Schedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tz = self.tz.iana_name().unwrap_or("local time");
        write!(f, "{} ({tz})", self.cron.pattern.as_str())
    }
}

/// A cron expression; `off` is none.
struct Cron(Option<croner::Cron>);

fn default_schedule() -> croner::Cron {
    // A test checks that it parses.
    DEFAULT_UPDATE_SCHEDULE
        .parse()
        .unwrap_or_else(|_| unreachable!("the default schedule parses"))
}

impl FromStr for Cron {
    type Err = croner::errors::CronError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.trim().eq_ignore_ascii_case("off") {
            return Ok(Self(None));
        }
        s.parse().map(|c| Self(Some(c)))
    }
}

/// The time zone of `TZ`, else the system's, else UTC; a `TZ` that names no time zone is an error.
fn time_zone() -> Result<TimeZone, ConfigError> {
    match TimeZone::try_system() {
        Ok(tz) => Ok(tz),
        Err(_) => match std::env::var("TZ") {
            Ok(tz) if !tz.is_empty() => Err(ConfigError::Invalid {
                name: "TZ".to_owned(),
                value: tz,
            }),
            _ => Ok(TimeZone::UTC),
        },
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
            "--update-schedule",
            "off",
            "--dump-url",
            "http://h/",
        ])
        .unwrap();
        assert_eq!(c.data_dir, PathBuf::from("/x"));
        assert!(c.update_schedule.is_none());
        assert_eq!(c.dump_url, "http://h");
        assert_eq!(c.listen.port(), 3300);
    }

    #[test]
    fn rejects_unknown_and_invalid() {
        assert!(matches!(parse(&["--nope"]), Err(ConfigError::Unknown(_))));
        assert!(matches!(
            parse(&["--update-schedule", "x"]),
            Err(ConfigError::Invalid { .. })
        ));
        assert!(matches!(
            parse(&["--update-schedule", "61 * * * *"]),
            Err(ConfigError::Invalid { .. })
        ));
        assert!(matches!(
            parse(&["--update-interval", "24h"]),
            Err(ConfigError::Replaced { .. })
        ));
    }

    #[test]
    fn schedules() {
        let at = |cron: &str, tz: &str, now: &str| {
            Schedule::parse(cron, jiff::tz::db().get(tz).unwrap())
                .unwrap()
                .next_after(now.parse().unwrap())
                .unwrap()
                .timestamp()
                .to_string()
        };
        // the default: every day at 02:34
        let daily = DEFAULT_UPDATE_SCHEDULE;
        assert_eq!(
            at(daily, "UTC", "2026-11-08T01:00:00Z"),
            "2026-11-08T02:34:00Z"
        );
        assert_eq!(
            at(daily, "UTC", "2026-11-08T02:34:00Z"),
            "2026-11-09T02:34:00Z"
        );
        // in the time zone given, across a daylight saving change
        assert_eq!(
            at(daily, "Europe/Riga", "2026-10-24T12:00:00Z"),
            "2026-10-24T23:34:00Z"
        );
        assert_eq!(
            at(daily, "Europe/Riga", "2026-10-25T12:00:00Z"),
            "2026-10-26T00:34:00Z"
        );
        assert_eq!(
            at("0 */6 * * *", "UTC", "2026-11-08T07:00:00Z"),
            "2026-11-08T12:00:00Z"
        );
        assert_eq!(
            at("@weekly", "UTC", "2026-11-08T07:00:00Z"),
            "2026-11-15T00:00:00Z"
        );
        assert!("Off".parse::<Cron>().unwrap().0.is_none());
        assert_eq!(
            parse(&[])
                .unwrap()
                .update_schedule
                .unwrap()
                .cron
                .pattern
                .as_str(),
            DEFAULT_UPDATE_SCHEDULE
        );
    }
}
