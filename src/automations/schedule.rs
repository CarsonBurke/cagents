//! Cron matches civil time; cagents owns the explicit gap/fold policy.

use croner::{
    Cron,
    parser::{CronParser, Seconds},
};
use jiff::{
    Timestamp,
    tz::{AmbiguousOffset, TimeZone},
};

#[cfg(feature = "host")]
use crate::automations::MISFIRE_GRACE_MS;
use crate::automations::{Occurrence, Schedule};

pub struct Compiled {
    cron: Cron,
    timezone: TimeZone,
}

#[cfg(feature = "host")]
#[derive(Debug, PartialEq, Eq)]
pub struct Due {
    pub fire: Option<Occurrence>,
    pub next: Occurrence,
    /// At least one older due occurrence was omitted. A single history entry
    /// reports the missed interval rather than allocating a row per minute.
    pub missed: bool,
}

impl Compiled {
    pub fn new(schedule: &Schedule) -> Result<Self, String> {
        let cron = CronParser::builder()
            .seconds(Seconds::Disallowed)
            .build()
            .parse(&schedule.pattern.expression()?)
            .map_err(|error| format!("Invalid cron schedule: {error}"))?;
        let timezone = jiff::tz::db().get(&schedule.timezone).map_err(|error| {
            format!(
                "Unknown or unavailable timezone {}: {error}",
                schedule.timezone
            )
        })?;
        Ok(Self { cron, timezone })
    }

    /// Strictly after `after_ms`. Nonexistent civil minutes are skipped;
    /// repeated minutes use only their first instant for every pattern.
    pub fn next_after(&self, after_ms: u64) -> Result<Occurrence, String> {
        let after = timestamp(after_ms)?;
        let mut civil = after.to_zoned(self.timezone.clone()).datetime();
        // Modern timezone gaps/folds can span an entire calendar day. Bound
        // exceptional candidate work, while allowing more than two such days.
        for _ in 0..4096 {
            civil = self
                .cron
                .find_next_occurrence(&civil, false)
                .map_err(|error| format!("Schedule has no next occurrence: {error}"))?;
            let ambiguous = self.timezone.to_ambiguous_zoned(civil);
            if matches!(ambiguous.offset(), AmbiguousOffset::Gap { .. }) {
                continue;
            }
            let instant = ambiguous
                .earlier()
                .map_err(|error| error.to_string())?
                .timestamp();
            if instant <= after {
                // During the second pass through a fold, its first pass is
                // already past. Advance the civil cursor instead of repeating.
                continue;
            }
            return Ok(Occurrence {
                due_ms: u64::try_from(instant.as_millisecond())
                    .map_err(|_| "Schedule predates the supported clock")?,
                local: civil.strftime("%Y-%m-%dT%H:%M").to_string(),
            });
        }
        Err("Schedule exceeded the bounded timezone-transition search".into())
    }

    #[cfg(feature = "host")]
    /// At most one catch-up firing: the latest due minute inside the grace
    /// window. Looking only inside that window bounds work after long downtime.
    pub fn due(&self, persisted: &Occurrence, now_ms: u64) -> Result<Due, String> {
        self.due_with_policy(persisted, now_ms, &crate::automations::CatchUp::default())
    }

    #[cfg(feature = "host")]
    pub fn due_with_policy(
        &self,
        persisted: &Occurrence,
        now_ms: u64,
        policy: &crate::automations::CatchUp,
    ) -> Result<Due, String> {
        use crate::automations::CatchUp;
        if persisted.due_ms <= now_ms {
            match policy {
                CatchUp::OnceOnWake => {
                    return Ok(Due {
                        fire: Some(persisted.clone()),
                        next: self.next_after(now_ms)?,
                        missed: false,
                    });
                }
                CatchUp::Skip if now_ms.saturating_sub(persisted.due_ms) >= 60_000 => {
                    return Ok(Due {
                        fire: None,
                        next: self.next_after(now_ms)?,
                        missed: true,
                    });
                }
                _ => {}
            }
        }
        let grace_ms = match policy {
            CatchUp::WithinMinutes(minutes) => u64::from(*minutes) * 60_000,
            CatchUp::Skip => 60_000,
            CatchUp::OnceOnWake => MISFIRE_GRACE_MS,
        };
        if persisted.due_ms > now_ms {
            return Ok(Due {
                fire: None,
                next: persisted.clone(),
                missed: false,
            });
        }
        let lower = now_ms.saturating_sub(grace_ms).max(persisted.due_ms);
        let mut cursor = lower.saturating_sub(1);
        let mut latest = None;
        let mut missed = persisted.due_ms < lower;
        for _ in 0..=grace_ms / 60_000 + 1 {
            let next = self.next_after(cursor)?;
            if next.due_ms > now_ms {
                return Ok(Due {
                    fire: latest,
                    next,
                    missed,
                });
            }
            missed |= latest.is_some();
            cursor = next.due_ms;
            latest = Some(next);
        }
        Err("Schedule produced more than one occurrence per civil minute".into())
    }
}

fn timestamp(milliseconds: u64) -> Result<Timestamp, String> {
    Timestamp::from_millisecond(
        i64::try_from(milliseconds).map_err(|_| "Clock is outside the supported range")?,
    )
    .map_err(|error| error.to_string())
}

#[cfg(feature = "ui")]
/// The system's IANA timezone, which a new schedule starts in. A system
/// configured without one, or with only a fixed offset, has no name to save.
pub fn system_timezone() -> Option<String> {
    TimeZone::system().iana_name().map(str::to_owned)
}

#[cfg(feature = "ui")]
/// Every IANA timezone name the database knows, in its own order.
pub fn timezones() -> Vec<String> {
    jiff::tz::db()
        .available()
        .map(|name| name.as_str().to_owned())
        .collect()
}

#[cfg(feature = "ui")]
/// When `schedule` next runs after `now_ms`, in its own timezone, followed by
/// the same instant in `reader`'s timezone where that zone differs.
pub fn describe_next(
    schedule: &Schedule,
    now_ms: u64,
    reader: &TimeZone,
) -> Result<String, String> {
    let compiled = Compiled::new(schedule)?;
    let instant = timestamp(compiled.next_after(now_ms)?.due_ms)?;
    let there = instant.to_zoned(compiled.timezone.clone());
    let mut text = format!(
        "{} {}",
        there.strftime("%a %b %-d, %H:%M"),
        schedule.timezone
    );
    let here = instant.to_zoned(reader.clone());
    if reader.iana_name() != Some(schedule.timezone.as_str()) && here.offset() != there.offset() {
        let day = if here.date() == there.date() {
            ""
        } else {
            "%a "
        };
        text.push_str(&format!(
            " ({} your time)",
            here.strftime(&format!("{day}%H:%M"))
        ));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automations::SchedulePattern;

    fn ms(value: &str) -> u64 {
        value.parse::<Timestamp>().unwrap().as_millisecond() as u64
    }
    fn cron(expression: &str, timezone: &str) -> Compiled {
        Compiled::new(&Schedule {
            pattern: SchedulePattern::Cron {
                expression: expression.into(),
            },
            timezone: timezone.into(),
        })
        .unwrap()
    }

    #[cfg(feature = "host")]
    #[test]
    fn catch_up_policies_bound_or_resume_a_long_gap() {
        use crate::automations::CatchUp;
        let schedule = cron("0 9 * * *", "UTC");
        let persisted = schedule.next_after(ms("2026-10-01T00:00:00Z")).unwrap();
        let wake = ms("2026-10-04T09:20:00Z");
        assert!(
            schedule
                .due_with_policy(&persisted, wake, &CatchUp::Skip)
                .unwrap()
                .fire
                .is_none()
        );
        assert!(
            schedule
                .due_with_policy(&persisted, wake, &CatchUp::WithinMinutes(15))
                .unwrap()
                .fire
                .is_none()
        );
        let within = schedule
            .due_with_policy(&persisted, wake, &CatchUp::WithinMinutes(30))
            .unwrap();
        assert_eq!(within.fire.unwrap().due_ms, ms("2026-10-04T09:00:00Z"));
        let once = schedule
            .due_with_policy(&persisted, wake, &CatchUp::OnceOnWake)
            .unwrap();
        assert_eq!(once.fire, Some(persisted));
        assert!(once.next.due_ms > wake);
    }

    #[test]
    fn spring_gap_is_skipped_instead_of_moving_a_fixed_job() {
        let schedule = cron("30 2 * * *", "America/Vancouver");
        let next = schedule.next_after(ms("2026-03-08T09:59:00Z")).unwrap();
        assert_eq!(next.local, "2026-03-09T02:30");
        assert_eq!(next.due_ms, ms("2026-03-09T09:30:00Z"));
    }

    #[test]
    fn repeated_minutes_fire_once_including_wildcard_patterns() {
        let fixed = cron("30 1 * * *", "America/Vancouver");
        let first = fixed.next_after(ms("2025-11-02T07:00:00Z")).unwrap();
        assert_eq!(first.due_ms, ms("2025-11-02T08:30:00Z"));
        assert_eq!(
            fixed.next_after(first.due_ms).unwrap().local,
            "2025-11-03T01:30"
        );
        assert_eq!(
            fixed.next_after(ms("2025-11-02T09:10:00Z")).unwrap().local,
            "2025-11-03T01:30"
        );
        let wildcard = cron("* * * * *", "America/Vancouver");
        assert_eq!(
            wildcard
                .next_after(ms("2025-11-02T08:59:00Z"))
                .unwrap()
                .due_ms,
            ms("2025-11-02T10:00:00Z")
        );
        assert_eq!(
            wildcard
                .next_after(ms("2025-11-02T09:10:00Z"))
                .unwrap()
                .due_ms,
            ms("2025-11-02T10:00:00Z")
        );
    }

    #[cfg(feature = "ui")]
    #[test]
    fn next_run_reads_in_the_schedule_zone_and_the_readers() {
        let schedule = Schedule {
            pattern: SchedulePattern::Weekdays { hour: 9, minute: 0 },
            timezone: "America/Vancouver".into(),
        };
        // Saturday 2026-09-26 in Vancouver: the next weekday run is Monday.
        let now = ms("2026-09-26T19:00:00Z");
        let vancouver = jiff::tz::db().get("America/Vancouver").unwrap();
        assert_eq!(
            describe_next(&schedule, now, &vancouver).unwrap(),
            "Mon Sep 28, 09:00 America/Vancouver"
        );
        let berlin = jiff::tz::db().get("Europe/Berlin").unwrap();
        assert_eq!(
            describe_next(&schedule, now, &berlin).unwrap(),
            "Mon Sep 28, 09:00 America/Vancouver (18:00 your time)"
        );
        let tokyo = jiff::tz::db().get("Asia/Tokyo").unwrap();
        assert_eq!(
            describe_next(&schedule, now, &tokyo).unwrap(),
            "Mon Sep 28, 09:00 America/Vancouver (Tue 01:00 your time)"
        );
        let unknown = Schedule {
            timezone: "Mars/Olympus".into(),
            ..schedule
        };
        assert!(
            describe_next(&unknown, now, &berlin)
                .unwrap_err()
                .contains("Mars/Olympus")
        );
        assert!(timezones().iter().any(|name| name == "America/Vancouver"));
    }

    #[cfg(feature = "host")]
    #[test]
    fn long_downtime_catches_up_only_latest_due_minute() {
        let schedule = cron("* * * * *", "UTC");
        let old = schedule.next_after(ms("2020-01-01T00:00:00Z")).unwrap();
        let due = schedule.due(&old, ms("2026-09-26T12:34:42Z")).unwrap();
        assert!(due.missed);
        assert_eq!(due.fire.unwrap().local, "2026-09-26T12:34");
        assert_eq!(due.next.local, "2026-09-26T12:35");
    }

    #[cfg(feature = "host")]
    #[test]
    fn clock_rollback_does_not_reopen_persisted_future_occurrence() {
        let schedule = cron("0 * * * *", "UTC");
        let next = schedule.next_after(ms("2026-09-26T12:00:00Z")).unwrap();
        assert_eq!(
            schedule.due(&next, ms("2026-09-26T11:00:00Z")).unwrap(),
            Due {
                fire: None,
                next,
                missed: false
            }
        );
    }
}
