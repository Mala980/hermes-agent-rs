//! Unified schedule parser for cron jobs.
//!
//! Accepted formats — fixed and documented; full NLP (e.g. "every monday at
//! 9am") is out of scope:
//!
//! - 6-field cron: `"0 30 9 * * *"` (sec min hour day month weekday).
//!   Stored byte-identical as typed.
//! - 5-field cron: `"30 9 * * *"` — seconds assumed `0`, normalized to
//!   `"0 30 9 * * *"`. Detected by field count (5 vs 6).
//! - Interval: `"every 15m"`, `"every 1h"`, `"every 30s"` (units: `s` seconds,
//!   `m` minutes, `h` hours, `d` days, `w` weeks; case-insensitive).
//!   Repeats every duration from the last tick.
//! - One-shot: `"at 2026-10-22T09:00:00+08:00"` (ISO-8601/RFC3339).
//!   Without an offset (`"at 2026-10-22T09:00:00"`) it is read as wall-clock
//!   in the job's timezone. Times in the past are rejected at parse time.
//!   A one-shot job is **automatically removed from the store** after firing
//!   exactly once (see [`crate::scheduler`]); if the scheduler was down past
//!   its time, the job still fires once (late) on the next tick, then is
//!   removed.
//! - Keywords (case-insensitive):
//!   `hourly` → every hour on the hour (`0 0 * * * *`),
//!   `daily`/`midnight` → every midnight (`0 0 0 * * *`),
//!   `weekly` → every Sunday midnight (`0 0 0 * * 1`; the `cron` crate uses
//!   1=Sunday … 7=Saturday, not 0=Sunday like UNIX cron),
//!   `noon` → every midday (`0 0 12 * * *`).
//!
//! Every path — [`crate::job::CronJob::new`], [`crate::job::CronJob::validate`]
//! and the scheduler's `next_run` recomputation — uses the same
//! [`parse_schedule`]. Unparseable input produces an error naming every
//! accepted format.

use std::str::FromStr;

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};

/// Schedule-format help; embedded in every parse error.
const SCHEDULE_HELP: &str = "Accepted schedule formats:\n  \
    - 6-field cron: \"0 30 9 * * *\" (sec min hour day month weekday)\n  \
    - 5-field cron: \"30 9 * * *\" (seconds assumed 0)\n  \
    - interval: \"every 15m\" / \"every 1h\" / \"every 30s\" (units: s, m, h, d, w)\n  \
    - one-shot: \"at 2026-10-22T09:00:00+08:00\" (ISO-8601/RFC3339; without offset = wall-clock in the job's timezone)\n  \
    - keywords: hourly, daily, weekly, midnight, noon";

/// A parsed and normalized schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedSchedule {
    /// Recurring cron; `expr` is 6/7 fields and validated by the `cron` crate.
    Cron { expr: String },
    /// Recurring interval in seconds (anchored at the last tick).
    Interval { secs: u64 },
    /// One-shot at a single point in time.
    Once { at: OnceTime },
}

/// One-shot point in time: an absolute instant, or a naive wall-clock that is
/// interpreted in the job's timezone when `next_run` is computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnceTime {
    /// Absolute instant (RFC3339 with offset / `Z`).
    Instant(DateTime<Utc>),
    /// Naive wall-clock (`YYYY-MM-DDTHH:MM:SS` without offset).
    WallClock(NaiveDateTime),
}

impl ParsedSchedule {
    /// Canonical form for storing in [`crate::job::CronJob`]'s `schedule`
    /// field. Parsing the canonical form always succeeds and is equivalent.
    pub fn canonical(&self) -> String {
        match self {
            ParsedSchedule::Cron { expr } => expr.clone(),
            ParsedSchedule::Interval { secs } => format!("every {secs}s"),
            ParsedSchedule::Once { at } => match at {
                OnceTime::Instant(dt) => format!("at {}", dt.to_rfc3339()),
                OnceTime::WallClock(naive) => {
                    format!("at {}", naive.format("%Y-%m-%dT%H:%M:%S"))
                }
            },
        }
    }

    /// True when this is a one-shot schedule.
    pub fn is_oneshot(&self) -> bool {
        matches!(self, ParsedSchedule::Once { .. })
    }

    /// Next occurrence strictly after `from`, or `None` when there is none
    /// (a one-shot in the past, or a one-shot that already fired —
    /// `last_run` is set).
    ///
    /// Pure/deterministic in `from` and `last_run`: it does not read the wall
    /// clock.
    pub fn next_run_after(
        &self,
        tz: chrono_tz::Tz,
        from: DateTime<Utc>,
        last_run: Option<DateTime<Utc>>,
    ) -> Option<DateTime<Utc>> {
        match self {
            ParsedSchedule::Cron { expr } => {
                let after = from.with_timezone(&tz);
                cron::Schedule::from_str(expr)
                    .ok()?
                    .after(&after)
                    .next()
                    .map(|dt| dt.with_timezone(&Utc))
            }
            ParsedSchedule::Interval { secs } => {
                let secs = i64::try_from(*secs).ok()?;
                Some(from + chrono::Duration::seconds(secs))
            }
            ParsedSchedule::Once { at } => {
                // A one-shot that already fired is never scheduled again.
                if last_run.is_some() {
                    return None;
                }
                let instant = match at {
                    OnceTime::Instant(dt) => *dt,
                    OnceTime::WallClock(naive) => tz
                        .from_local_datetime(naive)
                        .earliest()?
                        .with_timezone(&Utc),
                };
                (instant > from).then_some(instant)
            }
        }
    }
}

/// Parse a schedule string into a [`ParsedSchedule`]. Errors always name the
/// accepted formats.
pub fn parse_schedule(input: &str) -> Result<ParsedSchedule, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err(format!("empty schedule: `{input}`\n{SCHEDULE_HELP}"));
    }
    let lower = s.to_ascii_lowercase();

    // 1. Natural-language-lite keywords (single token).
    let keyword = match lower.as_str() {
        "hourly" => Some("0 0 * * * *"),
        "daily" | "midnight" => Some("0 0 0 * * *"),
        // The `cron` crate: 1=Sunday … 7=Saturday (not 0=Sunday like UNIX cron).
        "weekly" => Some("0 0 0 * * 1"),
        "noon" => Some("0 0 12 * * *"),
        _ => None,
    };
    if let Some(expr) = keyword {
        return Ok(ParsedSchedule::Cron {
            expr: expr.to_string(),
        });
    }

    // 2/3. Single-word prefixes: "every <duration>" or "at <datetime>".
    // A first token that is not a keyword can never be a valid cron field,
    // so there is no ambiguity with the cron path below.
    let mut tokens = s.split_whitespace();
    let first = tokens.next().unwrap_or("").to_ascii_lowercase();
    if first == "every" {
        let rest: Vec<&str> = tokens.collect();
        let dur = rest
            .first()
            .filter(|_| rest.len() == 1)
            .copied()
            .unwrap_or("");
        let secs = parse_duration(dur).ok_or_else(|| {
            format!(
                "invalid interval: `{input}`. Use `every <duration>`, e.g. \
                 `every 15m`, `every 1h`, `every 30s` (units: s seconds, m minutes, \
                 h hours, d days, w weeks).\n{SCHEDULE_HELP}"
            )
        })?;
        return Ok(ParsedSchedule::Interval { secs });
    }
    if first == "at" {
        let rest: Vec<&str> = tokens.collect();
        if rest.is_empty() {
            return Err(format!(
                "empty one-shot time: `{input}`. Use `at <ISO-8601 datetime>`, \
                 e.g. `at 2026-10-22T09:00:00+08:00`.\n{SCHEDULE_HELP}"
            ));
        }
        // Re-join so "at 2026-10-22 09:00:00" (space separator) still parses.
        return parse_once(&rest.join(" "), input);
    }

    // 4. Cron expression: 5 fields → seconds assumed 0; otherwise pass
    // through as-is (6/7 fields), exactly like the previous behavior.
    let expr = if s.split_whitespace().count() == 5 {
        format!("0 {s}")
    } else {
        s.to_string()
    };
    cron::Schedule::from_str(&expr)
        .map(|_| ParsedSchedule::Cron { expr })
        .map_err(|e| format!("invalid cron expression: {e}\n{SCHEDULE_HELP}"))
}

/// Parse `<n><unit>`; units: s/m/h/d/w (+ common aliases). Upper bound: 10 years.
fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let cut = s.find(|c: char| !c.is_ascii_digit())?;
    let (num, unit) = s.split_at(cut);
    let n: u64 = num.parse().ok()?;
    if n == 0 {
        return None;
    }
    let mult: u64 = match unit {
        "s" | "sec" | "secs" | "second" | "seconds" => 1,
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86_400,
        "w" | "week" | "weeks" => 604_800,
        _ => return None,
    };
    let secs = n.checked_mul(mult)?;
    // Sanity bound: 10 years.
    (secs <= 10 * 366 * 86_400).then_some(secs)
}

/// Parse the body of `at <datetime>`: RFC3339 with offset first, then naive forms.
/// Times in the past are rejected (best-effort check using the system-local
/// zone for naive input — the job's final timezone is only known later).
fn parse_once(dt_str: &str, original: &str) -> Result<ParsedSchedule, String> {
    let now = Utc::now();
    if let Ok(dt) = DateTime::parse_from_rfc3339(dt_str) {
        let at = dt.with_timezone(&Utc);
        if at <= now {
            return Err(format!(
                "one-shot time `{original}` is in the past; use a future time.\n{SCHEDULE_HELP}"
            ));
        }
        return Ok(ParsedSchedule::Once {
            at: OnceTime::Instant(at),
        });
    }
    let naive = [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ]
    .iter()
    .find_map(|fmt| NaiveDateTime::parse_from_str(dt_str, fmt).ok())
    .or_else(|| {
        NaiveDate::parse_from_str(dt_str, "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
    });
    match naive {
        Some(naive) => {
            // Best-effort past check: interpret in the system-local zone.
            let probe = system_local_tz()
                .from_local_datetime(&naive)
                .earliest()
                .map(|dt| dt.with_timezone(&Utc));
            if probe.map(|at| at <= now).unwrap_or(false) {
                return Err(format!(
                    "one-shot time `{original}` is in the past; use a future time.\n{SCHEDULE_HELP}"
                ));
            }
            Ok(ParsedSchedule::Once {
                at: OnceTime::WallClock(naive),
            })
        }
        None => Err(format!(
            "invalid one-shot time: `{original}`. Use `at <ISO-8601 datetime>`, \
             e.g. `at 2026-10-22T09:00:00+08:00` or `at 2026-10-22T09:00:00` \
             (without offset = wall-clock in the job's timezone).\n{SCHEDULE_HELP}"
        )),
    }
}

/// The system-local timezone as a [`chrono_tz::Tz`]; UTC when undetectable.
pub fn system_local_tz() -> chrono_tz::Tz {
    iana_time_zone::get_timezone()
        .ok()
        .and_then(|name| name.parse::<chrono_tz::Tz>().ok())
        .unwrap_or(chrono_tz::UTC)
}

/// Validate an IANA timezone name. `Ok(())` for valid names; an error message
/// naming the problem otherwise.
pub fn validate_timezone(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("timezone must not be empty (use an IANA name like \"Asia/Makassar\")".into());
    }
    name.parse::<chrono_tz::Tz>().map(|_| ()).map_err(|_| {
        format!(
            "invalid IANA timezone: `{name}` (e.g. \"Asia/Makassar\", \"UTC\", \"America/New_York\")"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, mo, d, h, mi, s).single().unwrap()
    }

    #[test]
    fn five_field_cron_assumes_seconds_zero() {
        let p = parse_schedule("30 9 * * *").unwrap();
        assert_eq!(
            p,
            ParsedSchedule::Cron {
                expr: "0 30 9 * * *".into()
            }
        );
        assert_eq!(p.canonical(), "0 30 9 * * *");
        // Deterministic next-run: from 2026-10-21T00:00:00Z → 09:30 that day.
        let next = p
            .next_run_after(chrono_tz::UTC, utc(2026, 10, 21, 0, 0, 0), None)
            .unwrap();
        assert_eq!(next, utc(2026, 10, 21, 9, 30, 0));
    }

    #[test]
    fn six_field_cron_stored_verbatim() {
        let p = parse_schedule("0 30 9 * * *").unwrap();
        assert_eq!(p.canonical(), "0 30 9 * * *");
        // 7 fields (with year) also pass through as before.
        assert!(parse_schedule("0 0 0 1 1 * 2030").is_ok());
    }

    #[test]
    fn invalid_cron_error_names_expected_formats() {
        // Invalid 6-field: the old-style message is kept as a prefix…
        let err = parse_schedule("0 0 25 * * *").unwrap_err();
        assert!(err.contains("invalid cron expression"), "message: {err}");
        // …but now names every accepted format.
        for needle in [
            "6-field cron",
            "5-field cron",
            "every 15m",
            "at 2026-10-22T09:00:00+08:00",
            "hourly",
        ] {
            assert!(err.contains(needle), "error must name `{needle}`: {err}");
        }
        // Not cron at all → the error also names the formats.
        let err2 = parse_schedule("whenever").unwrap_err();
        assert!(err2.contains("5-field cron"), "message: {err2}");
        assert!(parse_schedule("").is_err());
        assert!(parse_schedule("   ").is_err());
    }

    #[test]
    fn interval_parses_units_case_insensitive() {
        let cases = [
            ("every 15m", 900),
            ("every 1h", 3600),
            ("every 30s", 30),
            ("EVERY 2H", 7200),
            ("every 1d", 86_400),
            ("every 1w", 604_800),
            ("every 90s", 90),
        ];
        for (input, secs) in cases {
            assert_eq!(
                parse_schedule(input).unwrap(),
                ParsedSchedule::Interval { secs },
                "{input}"
            );
        }
    }

    #[test]
    fn interval_next_run_is_from_plus_duration() {
        let p = parse_schedule("every 15m").unwrap();
        assert_eq!(p.canonical(), "every 900s");
        let from = utc(2026, 10, 21, 12, 0, 0);
        let next = p.next_run_after(chrono_tz::UTC, from, None).unwrap();
        assert_eq!(next, from + chrono::Duration::minutes(15));
        // Round-trip the canonical form.
        assert_eq!(parse_schedule(&p.canonical()).unwrap(), p);
    }

    #[test]
    fn interval_rejects_garbage() {
        for bad in [
            "every",
            "every xyz",
            "every -5m",
            "every 0m",
            "every 1.5h",
            "every 15m 2h",
            "every 9999999999999w",
        ] {
            let err = parse_schedule(bad).unwrap_err();
            assert!(
                err.contains("invalid interval"),
                "`{bad}` must be an interval error: {err}"
            );
        }
    }

    #[test]
    fn once_with_offset_resolves_to_instant() {
        // 09:00 +08:00 == 01:00Z. (Far-future date keeps the test stable.)
        let p = parse_schedule("at 2030-06-15T09:00:00+08:00").unwrap();
        let next = p
            .next_run_after(chrono_tz::UTC, utc(2030, 6, 14, 0, 0, 0), None)
            .unwrap();
        assert_eq!(next, utc(2030, 6, 15, 1, 0, 0));
        // After its time → no further occurrences.
        assert!(p
            .next_run_after(chrono_tz::UTC, utc(2030, 6, 16, 0, 0, 0), None)
            .is_none());
        // A one-shot that already fired never reschedules.
        assert!(p
            .next_run_after(
                chrono_tz::UTC,
                utc(2030, 6, 14, 0, 0, 0),
                Some(utc(2030, 6, 15, 1, 0, 0))
            )
            .is_none());
        assert_eq!(p.canonical(), "at 2030-06-15T01:00:00+00:00");
        assert_eq!(parse_schedule(&p.canonical()).unwrap(), p);
    }

    #[test]
    fn once_accepts_z_suffix_and_space_separator() {
        let p = parse_schedule("at 2030-06-15T01:00:00Z").unwrap();
        assert!(matches!(
            p,
            ParsedSchedule::Once {
                at: OnceTime::Instant(_)
            }
        ));
        let p2 = parse_schedule("at 2030-06-15 01:00:00").unwrap();
        assert!(matches!(
            p2,
            ParsedSchedule::Once {
                at: OnceTime::WallClock(_)
            }
        ));
    }

    #[test]
    fn once_naive_uses_job_timezone() {
        let p = parse_schedule("at 2030-06-15T09:00:00").unwrap();
        assert!(matches!(
            p,
            ParsedSchedule::Once {
                at: OnceTime::WallClock(_)
            }
        ));
        assert_eq!(p.canonical(), "at 2030-06-15T09:00:00");
        // Asia/Makassar = UTC+8, no DST: 09:00 local == 01:00Z.
        let next = p
            .next_run_after(chrono_tz::Asia::Makassar, utc(2030, 6, 14, 0, 0, 0), None)
            .unwrap();
        assert_eq!(next, utc(2030, 6, 15, 1, 0, 0));
    }

    #[test]
    fn cron_daily_respects_job_timezone() {
        // "daily" = local midnight. From 2026-10-21T00:00:00Z (08:00 in
        // Asia/Makassar, UTC+8) the next local midnight is 2026-10-21T16:00Z.
        let p = parse_schedule("daily").unwrap();
        let next = p
            .next_run_after(chrono_tz::Asia::Makassar, utc(2026, 10, 21, 0, 0, 0), None)
            .unwrap();
        assert_eq!(next, utc(2026, 10, 21, 16, 0, 0));
    }

    #[test]
    fn once_past_times_rejected_at_parse() {
        let err = parse_schedule("at 2000-01-01T00:00:00Z").unwrap_err();
        assert!(err.contains("in the past"), "message: {err}");
        assert!(parse_schedule("at 2000-01-01 00:00:00").is_err());
    }

    #[test]
    fn once_rejects_garbage() {
        for bad in [
            "at",
            "at tomorrow morning",
            "at 2026-13-45T99:99:99",
            "at noonish",
        ] {
            assert!(parse_schedule(bad).is_err(), "`{bad}` must error");
        }
    }

    #[test]
    fn keywords_map_to_cron_equivalents() {
        let cases = [
            ("hourly", "0 0 * * * *"),
            ("daily", "0 0 0 * * *"),
            ("midnight", "0 0 0 * * *"),
            ("weekly", "0 0 0 * * 1"),
            ("noon", "0 0 12 * * *"),
            ("DAILY", "0 0 0 * * *"),
            ("  Hourly  ", "0 0 * * * *"),
        ];
        for (kw, expr) in cases {
            assert_eq!(
                parse_schedule(kw).unwrap(),
                ParsedSchedule::Cron {
                    expr: expr.to_string()
                },
                "{kw}"
            );
        }
    }

    #[test]
    fn daily_next_run_from_fixed_instant() {
        let p = parse_schedule("daily").unwrap();
        // From 2026-10-21T00:00:01Z → the next midnight.
        let next = p
            .next_run_after(chrono_tz::UTC, utc(2026, 10, 21, 0, 0, 1), None)
            .unwrap();
        assert_eq!(next, utc(2026, 10, 22, 0, 0, 0));
    }

    #[test]
    fn parse_is_case_insensitive_for_prefixes() {
        assert!(parse_schedule("Every 15m").is_ok());
        assert!(parse_schedule("AT 2030-06-15T01:00:00Z").is_ok());
    }

    #[test]
    fn validate_timezone_accepts_iana_rejects_garbage() {
        assert!(validate_timezone("Asia/Makassar").is_ok());
        assert!(validate_timezone("UTC").is_ok());
        assert!(validate_timezone("America/New_York").is_ok());
        let err = validate_timezone("Mars/Olympus").unwrap_err();
        assert!(err.contains("IANA"), "message: {err}");
        assert!(validate_timezone("").is_err());
    }

    #[test]
    fn system_local_tz_returns_a_tz() {
        // Must not panic; result is a real Tz (UTC fallback at worst).
        let tz = system_local_tz();
        let _ = tz;
    }
}
