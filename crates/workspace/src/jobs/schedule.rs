use chrono::{Datelike, Duration, Local, NaiveTime, TimeZone};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, ts_rs::TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleMode {
    /// Every `minutes`.
    #[default]
    Interval,
    /// At `at` (local "HH:MM") on `weekdays` (0 = Sunday; empty = every day).
    Daily,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct JobSchedule {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub mode: ScheduleMode,
    #[serde(default = "default_minutes")]
    pub minutes: u32,
    #[serde(default = "default_at")]
    pub at: String,
    #[serde(default)]
    pub weekdays: Vec<u8>,
}

fn default_minutes() -> u32 {
    60
}
fn default_at() -> String {
    "08:00".into()
}

impl Default for JobSchedule {
    fn default() -> Self {
        Self { enabled: false, mode: ScheduleMode::Interval, minutes: default_minutes(), at: default_at(), weekdays: Vec::new() }
    }
}

impl JobSchedule {
    /// First run time strictly after `after_ms` (epoch ms), `None` when off
    /// or invalid. Daily times are local time.
    pub fn next_after(&self, after_ms: i64) -> Option<i64> {
        if !self.enabled {
            return None;
        }
        match self.mode {
            ScheduleMode::Interval => {
                let step = i64::from(self.minutes.max(1)) * 60_000;
                // Aligned to the interval (every 15 min → :00, :15, …), local time.
                let offset = Local.timestamp_millis_opt(after_ms).single()?.offset().local_minus_utc() as i64 * 1000;
                let local = after_ms + offset;
                Some((local / step + 1) * step - offset)
            }
            ScheduleMode::Daily => {
                let time = NaiveTime::parse_from_str(self.at.trim(), "%H:%M").ok()?;
                let start = Local.timestamp_millis_opt(after_ms).single()?;
                for d in 0..8 {
                    let day = start.date_naive() + Duration::days(d);
                    if !self.weekdays.is_empty() && !self.weekdays.contains(&(day.weekday().num_days_from_sunday() as u8)) {
                        continue;
                    }
                    // A time skipped by a DST change runs an hour later.
                    let at = Local
                        .from_local_datetime(&day.and_time(time))
                        .earliest()
                        .or_else(|| Local.from_local_datetime(&(day.and_time(time) + Duration::hours(1))).earliest())?;
                    let ms = at.timestamp_millis();
                    if ms > after_ms {
                        return Some(ms);
                    }
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schedule_next_run() {
        let off = JobSchedule::default();
        assert_eq!(off.next_after(0), None);
        let every15 = JobSchedule { enabled: true, minutes: 15, ..Default::default() };
        let t = Local.with_ymd_and_hms(2026, 3, 10, 9, 7, 0).unwrap().timestamp_millis();
        assert_eq!(every15.next_after(t), Some(Local.with_ymd_and_hms(2026, 3, 10, 9, 15, 0).unwrap().timestamp_millis()));
        let daily = JobSchedule { enabled: true, mode: ScheduleMode::Daily, at: "08:30".into(), ..Default::default() };
        assert_eq!(daily.next_after(t), Some(Local.with_ymd_and_hms(2026, 3, 11, 8, 30, 0).unwrap().timestamp_millis()));
        // 2026-03-10 is a Tuesday; Mondays only → the 16th.
        let mondays = JobSchedule { weekdays: vec![1], ..daily.clone() };
        assert_eq!(mondays.next_after(t), Some(Local.with_ymd_and_hms(2026, 3, 16, 8, 30, 0).unwrap().timestamp_millis()));
        assert_eq!(JobSchedule { at: "25:00".into(), ..daily }.next_after(t), None);
    }
}
