use std::error::Error;
use std::path::PathBuf;

use chrono::{Datelike, FixedOffset, TimeZone, Utc, Weekday};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::memory::now_iso;

type BoxError = Box<dyn Error + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Schedule {
    pub id: Uuid,
    pub description: String,
    pub repeat: RepeatSpec,
    pub enabled: bool,
    pub created: String,
    pub last_run: Option<String>,
    pub last_status: Option<String>,
    pub frontend: String,
    pub channel_id: Option<u64>,
    pub timezone_offset_hours: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RepeatSpec {
    Daily {
        hour: u8,
        minute: u8,
    },
    EveryNHours {
        interval: u16,
    },
    Weekly {
        day: WeekdaySpec,
        hour: u8,
        minute: u8,
    },
    Once {
        at: String,
    },
}

/// Weekday wrapper for clean serde.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WeekdaySpec {
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
    Sunday,
}

impl WeekdaySpec {
    pub fn to_chrono(&self) -> Weekday {
        match self {
            Self::Monday => Weekday::Mon,
            Self::Tuesday => Weekday::Tue,
            Self::Wednesday => Weekday::Wed,
            Self::Thursday => Weekday::Thu,
            Self::Friday => Weekday::Fri,
            Self::Saturday => Weekday::Sat,
            Self::Sunday => Weekday::Sun,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "monday" | "mon" => Some(Self::Monday),
            "tuesday" | "tue" => Some(Self::Tuesday),
            "wednesday" | "wed" => Some(Self::Wednesday),
            "thursday" | "thu" => Some(Self::Thursday),
            "friday" | "fri" => Some(Self::Friday),
            "saturday" | "sat" => Some(Self::Saturday),
            "sunday" | "sun" => Some(Self::Sunday),
            _ => None,
        }
    }
}

impl std::fmt::Display for WeekdaySpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Monday => write!(f, "Monday"),
            Self::Tuesday => write!(f, "Tuesday"),
            Self::Wednesday => write!(f, "Wednesday"),
            Self::Thursday => write!(f, "Thursday"),
            Self::Friday => write!(f, "Friday"),
            Self::Saturday => write!(f, "Saturday"),
            Self::Sunday => write!(f, "Sunday"),
        }
    }
}

impl RepeatSpec {
    /// Human-readable summary, e.g. "daily at 08:00" or "every 2 hours".
    pub fn display(&self) -> String {
        match self {
            Self::Daily { hour, minute } => format!("daily at {hour:02}:{minute:02}"),
            Self::EveryNHours { interval } => {
                if *interval == 1 {
                    "every hour".to_string()
                } else {
                    format!("every {interval} hours")
                }
            }
            Self::Weekly { day, hour, minute } => {
                format!("weekly on {day} at {hour:02}:{minute:02}")
            }
            Self::Once { at } => format!("once at {at}"),
        }
    }
}

impl Schedule {
    pub fn new(
        description: impl Into<String>,
        repeat: RepeatSpec,
        frontend: impl Into<String>,
        channel_id: Option<u64>,
        timezone_offset_hours: i32,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            description: description.into(),
            repeat,
            enabled: true,
            created: now_iso(),
            last_run: None,
            last_status: None,
            frontend: frontend.into(),
            channel_id,
            timezone_offset_hours,
        }
    }
}

/// Check whether a schedule is due for execution.
pub fn is_due(schedule: &Schedule, now_utc: &chrono::DateTime<Utc>) -> bool {
    if !schedule.enabled {
        return false;
    }

    let offset = FixedOffset::east_opt(schedule.timezone_offset_hours * 3600)
        .unwrap_or(FixedOffset::east_opt(0).unwrap());
    let now_local = now_utc.with_timezone(&offset);

    let last_run = schedule.last_run.as_ref().and_then(|s| {
        chrono::DateTime::parse_from_rfc3339(s)
            .or_else(|_| {
                // Try parsing our now_iso format (ends with Z, no timezone offset)
                chrono::DateTime::parse_from_rfc3339(&format!("{}+00:00", s.trim_end_matches('Z')))
            })
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    });

    match &schedule.repeat {
        RepeatSpec::Daily { hour, minute } => {
            let scheduled_today = offset
                .with_ymd_and_hms(
                    now_local.year(),
                    now_local.month(),
                    now_local.day(),
                    *hour as u32,
                    *minute as u32,
                    0,
                )
                .single();

            let Some(scheduled_today) = scheduled_today else {
                return false;
            };
            let scheduled_utc = scheduled_today.with_timezone(&Utc);

            // Due if the scheduled time has passed and we haven't run since then
            *now_utc >= scheduled_utc && last_run.is_none_or(|lr| lr < scheduled_utc)
        }

        RepeatSpec::EveryNHours { interval } => {
            let interval_dur = chrono::Duration::hours(*interval as i64);
            match last_run {
                Some(lr) => *now_utc >= lr + interval_dur,
                None => true, // Never run — fire immediately
            }
        }

        RepeatSpec::Weekly { day, hour, minute } => {
            let target_weekday = day.to_chrono();
            if now_local.weekday() != target_weekday {
                return false;
            }

            let scheduled_today = offset
                .with_ymd_and_hms(
                    now_local.year(),
                    now_local.month(),
                    now_local.day(),
                    *hour as u32,
                    *minute as u32,
                    0,
                )
                .single();

            let Some(scheduled_today) = scheduled_today else {
                return false;
            };
            let scheduled_utc = scheduled_today.with_timezone(&Utc);

            *now_utc >= scheduled_utc && last_run.is_none_or(|lr| lr < scheduled_utc)
        }

        RepeatSpec::Once { at } => {
            let target = chrono::DateTime::parse_from_rfc3339(at)
                .or_else(|_| {
                    chrono::DateTime::parse_from_rfc3339(&format!(
                        "{}+00:00",
                        at.trim_end_matches('Z')
                    ))
                })
                .ok()
                .map(|dt| dt.with_timezone(&Utc));

            let Some(target) = target else {
                return false;
            };

            *now_utc >= target && last_run.is_none()
        }
    }
}

/// Compute the next fire time as an ISO 8601 string, for display purposes.
pub fn next_run(schedule: &Schedule) -> Option<String> {
    let offset = FixedOffset::east_opt(schedule.timezone_offset_hours * 3600)
        .unwrap_or(FixedOffset::east_opt(0).unwrap());
    let now_utc = Utc::now();
    let now_local = now_utc.with_timezone(&offset);

    match &schedule.repeat {
        RepeatSpec::Daily { hour, minute } => {
            let today = offset
                .with_ymd_and_hms(
                    now_local.year(),
                    now_local.month(),
                    now_local.day(),
                    *hour as u32,
                    *minute as u32,
                    0,
                )
                .single()?;

            if now_local < today {
                Some(today.to_rfc3339())
            } else {
                Some((today + chrono::Duration::days(1)).to_rfc3339())
            }
        }
        RepeatSpec::EveryNHours { interval } => {
            let last = schedule.last_run.as_ref().and_then(|s| {
                chrono::DateTime::parse_from_rfc3339(s)
                    .or_else(|_| {
                        chrono::DateTime::parse_from_rfc3339(&format!(
                            "{}+00:00",
                            s.trim_end_matches('Z')
                        ))
                    })
                    .ok()
            });
            match last {
                Some(lr) => {
                    let next = lr + chrono::Duration::hours(*interval as i64);
                    Some(next.to_rfc3339())
                }
                None => Some(now_utc.to_rfc3339()),
            }
        }
        RepeatSpec::Weekly { day, hour, minute } => {
            let target_weekday = day.to_chrono();
            let days_until = (target_weekday.num_days_from_monday() as i64
                - now_local.weekday().num_days_from_monday() as i64
                + 7)
                % 7;

            let candidate_date = now_local.date_naive() + chrono::Duration::days(days_until);
            let candidate = offset
                .with_ymd_and_hms(
                    candidate_date.year(),
                    candidate_date.month(),
                    candidate_date.day(),
                    *hour as u32,
                    *minute as u32,
                    0,
                )
                .single()?;

            if now_local < candidate {
                Some(candidate.to_rfc3339())
            } else {
                Some((candidate + chrono::Duration::weeks(1)).to_rfc3339())
            }
        }
        RepeatSpec::Once { at } => {
            if schedule.last_run.is_some() {
                None // Already fired
            } else {
                Some(at.clone())
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// ScheduleStore
// ────────────────────────────────────────────────────────────────────────────

pub struct ScheduleStore {
    dir: PathBuf,
}

impl ScheduleStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn ensure_dir(&self) -> Result<(), BoxError> {
        std::fs::create_dir_all(&self.dir)?;
        Ok(())
    }

    pub fn save(&self, schedule: &Schedule) -> Result<(), BoxError> {
        self.ensure_dir()?;
        let path = self.dir.join(format!("{}.json", schedule.id));
        let json = serde_json::to_string_pretty(schedule)?;
        std::fs::write(path, json)?;
        Ok(())
    }

    pub fn load(&self, id: Uuid) -> Result<Schedule, BoxError> {
        let path = self.dir.join(format!("{id}.json"));
        let data = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&data)?)
    }

    pub fn delete(&self, id: Uuid) -> Result<(), BoxError> {
        let path = self.dir.join(format!("{id}.json"));
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }

    pub fn update(&self, schedule: &Schedule) -> Result<(), BoxError> {
        self.save(schedule)
    }

    pub fn list(&self) -> Result<Vec<Schedule>, BoxError> {
        let mut schedules = Vec::new();
        if !self.dir.exists() {
            return Ok(schedules);
        }

        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "json") {
                let data = std::fs::read_to_string(&path)?;
                if let Ok(s) = serde_json::from_str::<Schedule>(&data) {
                    schedules.push(s);
                }
            }
        }

        Ok(schedules)
    }

    pub fn list_enabled(&self) -> Result<Vec<Schedule>, BoxError> {
        Ok(self.list()?.into_iter().filter(|s| s.enabled).collect())
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new().prefix(name).tempdir().unwrap()
    }

    #[test]
    fn schedule_store_crud() {
        let dir = temp_dir("marrow_sched");
        let store = ScheduleStore::new(dir.path());

        let schedule = Schedule::new(
            "check calendar",
            RepeatSpec::Daily { hour: 8, minute: 0 },
            "cli",
            None,
            0,
        );
        let id = schedule.id;

        store.save(&schedule).unwrap();
        let loaded = store.load(id).unwrap();
        assert_eq!(loaded.description, "check calendar");
        assert!(loaded.enabled);

        let all = store.list().unwrap();
        assert_eq!(all.len(), 1);

        store.delete(id).unwrap();
        assert!(store.load(id).is_err());
        assert_eq!(store.list().unwrap().len(), 0);
    }

    #[test]
    fn schedule_store_update() {
        let dir = temp_dir("marrow_sched");
        let store = ScheduleStore::new(dir.path());

        let mut schedule = Schedule::new(
            "test task",
            RepeatSpec::EveryNHours { interval: 2 },
            "discord",
            Some(12345),
            1,
        );
        store.save(&schedule).unwrap();

        schedule.enabled = false;
        schedule.last_status = Some("failed".to_string());
        store.update(&schedule).unwrap();

        let loaded = store.load(schedule.id).unwrap();
        assert!(!loaded.enabled);
        assert_eq!(loaded.last_status.as_deref(), Some("failed"));
    }

    #[test]
    fn list_enabled_filters_disabled() {
        let dir = temp_dir("marrow_sched");
        let store = ScheduleStore::new(dir.path());

        let s1 = Schedule::new(
            "enabled task",
            RepeatSpec::Daily { hour: 9, minute: 0 },
            "cli",
            None,
            0,
        );
        let mut s2 = Schedule::new(
            "disabled task",
            RepeatSpec::Daily {
                hour: 10,
                minute: 0,
            },
            "cli",
            None,
            0,
        );
        s2.enabled = false;

        store.save(&s1).unwrap();
        store.save(&s2).unwrap();

        assert_eq!(store.list().unwrap().len(), 2);
        assert_eq!(store.list_enabled().unwrap().len(), 1);
    }

    #[test]
    fn is_due_daily_fires_after_scheduled_time() {
        let schedule = Schedule {
            id: Uuid::new_v4(),
            description: "test".to_string(),
            repeat: RepeatSpec::Daily { hour: 8, minute: 0 },
            enabled: true,
            created: "2026-01-01T00:00:00Z".to_string(),
            last_run: None,
            last_status: None,
            frontend: "cli".to_string(),
            channel_id: None,
            timezone_offset_hours: 0,
        };

        // 09:00 UTC — should be due (past 08:00, never run)
        let now = Utc.with_ymd_and_hms(2026, 4, 22, 9, 0, 0).unwrap();
        assert!(is_due(&schedule, &now));

        // 07:00 UTC — not yet due
        let now = Utc.with_ymd_and_hms(2026, 4, 22, 7, 0, 0).unwrap();
        assert!(!is_due(&schedule, &now));
    }

    #[test]
    fn is_due_daily_does_not_refire_same_day() {
        let schedule = Schedule {
            id: Uuid::new_v4(),
            description: "test".to_string(),
            repeat: RepeatSpec::Daily { hour: 8, minute: 0 },
            enabled: true,
            created: "2026-01-01T00:00:00Z".to_string(),
            last_run: Some("2026-04-22T08:01:00+00:00".to_string()),
            last_status: Some("succeeded".to_string()),
            frontend: "cli".to_string(),
            channel_id: None,
            timezone_offset_hours: 0,
        };

        // Same day, 09:00 — already ran today
        let now = Utc.with_ymd_and_hms(2026, 4, 22, 9, 0, 0).unwrap();
        assert!(!is_due(&schedule, &now));

        // Next day, 09:00 — should fire again
        let now = Utc.with_ymd_and_hms(2026, 4, 23, 9, 0, 0).unwrap();
        assert!(is_due(&schedule, &now));
    }

    #[test]
    fn is_due_every_n_hours() {
        let schedule = Schedule {
            id: Uuid::new_v4(),
            description: "test".to_string(),
            repeat: RepeatSpec::EveryNHours { interval: 2 },
            enabled: true,
            created: "2026-01-01T00:00:00Z".to_string(),
            last_run: Some("2026-04-22T08:00:00+00:00".to_string()),
            last_status: None,
            frontend: "cli".to_string(),
            channel_id: None,
            timezone_offset_hours: 0,
        };

        // 1 hour later — not due
        let now = Utc.with_ymd_and_hms(2026, 4, 22, 9, 0, 0).unwrap();
        assert!(!is_due(&schedule, &now));

        // 2 hours later — due
        let now = Utc.with_ymd_and_hms(2026, 4, 22, 10, 0, 0).unwrap();
        assert!(is_due(&schedule, &now));
    }

    #[test]
    fn is_due_every_n_hours_fires_immediately_when_never_run() {
        let schedule = Schedule {
            id: Uuid::new_v4(),
            description: "test".to_string(),
            repeat: RepeatSpec::EveryNHours { interval: 4 },
            enabled: true,
            created: "2026-01-01T00:00:00Z".to_string(),
            last_run: None,
            last_status: None,
            frontend: "cli".to_string(),
            channel_id: None,
            timezone_offset_hours: 0,
        };

        let now = Utc::now();
        assert!(is_due(&schedule, &now));
    }

    #[test]
    fn is_due_weekly() {
        // 2026-04-22 is a Wednesday
        let schedule = Schedule {
            id: Uuid::new_v4(),
            description: "test".to_string(),
            repeat: RepeatSpec::Weekly {
                day: WeekdaySpec::Wednesday,
                hour: 9,
                minute: 0,
            },
            enabled: true,
            created: "2026-01-01T00:00:00Z".to_string(),
            last_run: None,
            last_status: None,
            frontend: "cli".to_string(),
            channel_id: None,
            timezone_offset_hours: 0,
        };

        // Wednesday 10:00 — due
        let now = Utc.with_ymd_and_hms(2026, 4, 22, 10, 0, 0).unwrap();
        assert!(is_due(&schedule, &now));

        // Thursday 10:00 — wrong day
        let now = Utc.with_ymd_and_hms(2026, 4, 23, 10, 0, 0).unwrap();
        assert!(!is_due(&schedule, &now));
    }

    #[test]
    fn is_due_once_fires_once() {
        let schedule = Schedule {
            id: Uuid::new_v4(),
            description: "test".to_string(),
            repeat: RepeatSpec::Once {
                at: "2026-04-22T15:00:00+00:00".to_string(),
            },
            enabled: true,
            created: "2026-01-01T00:00:00Z".to_string(),
            last_run: None,
            last_status: None,
            frontend: "cli".to_string(),
            channel_id: None,
            timezone_offset_hours: 0,
        };

        // Before target — not due
        let now = Utc.with_ymd_and_hms(2026, 4, 22, 14, 0, 0).unwrap();
        assert!(!is_due(&schedule, &now));

        // After target, never run — due
        let now = Utc.with_ymd_and_hms(2026, 4, 22, 16, 0, 0).unwrap();
        assert!(is_due(&schedule, &now));
    }

    #[test]
    fn is_due_once_does_not_refire() {
        let schedule = Schedule {
            id: Uuid::new_v4(),
            description: "test".to_string(),
            repeat: RepeatSpec::Once {
                at: "2026-04-22T15:00:00+00:00".to_string(),
            },
            enabled: true,
            created: "2026-01-01T00:00:00Z".to_string(),
            last_run: Some("2026-04-22T15:01:00+00:00".to_string()),
            last_status: Some("succeeded".to_string()),
            frontend: "cli".to_string(),
            channel_id: None,
            timezone_offset_hours: 0,
        };

        let now = Utc.with_ymd_and_hms(2026, 4, 22, 16, 0, 0).unwrap();
        assert!(!is_due(&schedule, &now));
    }

    #[test]
    fn is_due_disabled_schedule() {
        let schedule = Schedule {
            id: Uuid::new_v4(),
            description: "test".to_string(),
            repeat: RepeatSpec::Daily { hour: 8, minute: 0 },
            enabled: false,
            created: "2026-01-01T00:00:00Z".to_string(),
            last_run: None,
            last_status: None,
            frontend: "cli".to_string(),
            channel_id: None,
            timezone_offset_hours: 0,
        };

        let now = Utc.with_ymd_and_hms(2026, 4, 22, 9, 0, 0).unwrap();
        assert!(!is_due(&schedule, &now));
    }

    #[test]
    fn is_due_with_timezone_offset() {
        // Schedule daily at 08:00 in UTC+2
        let schedule = Schedule {
            id: Uuid::new_v4(),
            description: "test".to_string(),
            repeat: RepeatSpec::Daily { hour: 8, minute: 0 },
            enabled: true,
            created: "2026-01-01T00:00:00Z".to_string(),
            last_run: None,
            last_status: None,
            frontend: "cli".to_string(),
            channel_id: None,
            timezone_offset_hours: 2,
        };

        // 06:01 UTC = 08:01 UTC+2 — should be due
        let now = Utc.with_ymd_and_hms(2026, 4, 22, 6, 1, 0).unwrap();
        assert!(is_due(&schedule, &now));

        // 05:00 UTC = 07:00 UTC+2 — not yet due
        let now = Utc.with_ymd_and_hms(2026, 4, 22, 5, 0, 0).unwrap();
        assert!(!is_due(&schedule, &now));
    }

    #[test]
    fn repeat_spec_display() {
        assert_eq!(
            RepeatSpec::Daily { hour: 8, minute: 0 }.display(),
            "daily at 08:00"
        );
        assert_eq!(
            RepeatSpec::EveryNHours { interval: 1 }.display(),
            "every hour"
        );
        assert_eq!(
            RepeatSpec::EveryNHours { interval: 3 }.display(),
            "every 3 hours"
        );
        assert_eq!(
            RepeatSpec::Weekly {
                day: WeekdaySpec::Monday,
                hour: 9,
                minute: 30
            }
            .display(),
            "weekly on Monday at 09:30"
        );
    }

    #[test]
    fn weekday_spec_from_str() {
        assert!(matches!(
            WeekdaySpec::parse("monday"),
            Some(WeekdaySpec::Monday)
        ));
        assert!(matches!(
            WeekdaySpec::parse("Mon"),
            Some(WeekdaySpec::Monday)
        ));
        assert!(matches!(
            WeekdaySpec::parse("FRIDAY"),
            Some(WeekdaySpec::Friday)
        ));
        assert!(WeekdaySpec::parse("invalid").is_none());
    }
}
