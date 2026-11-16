//! When a job runs: every N minutes, at times of day (on chosen weekdays),
//! on days of the month, or by a cron expression, in a chosen time zone;
//! what happens to runs missed while nothing was running; and other jobs
//! whose success starts it.
//!
//! Wall-clock times (daily, monthly, cron) and daylight-saving changes: a
//! time skipped when clocks go forward runs once at the first valid time
//! after it (02:30 → 03:00); a time repeated when clocks go back runs once,
//! at its first occurrence. Intervals count elapsed time: the runs are
//! always exactly the interval apart, aligned to the zone's standard-time
//! clock (every 15 min → :00, :15, …). An interval that doesn't divide the
//! daylight-saving shift (90 min, a day) keeps its gaps, so its clock time
//! moves by the shift while daylight saving is on.

use chrono::{DateTime, Datelike, Duration, FixedOffset, Local, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, ts_rs::TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleMode {
    /// Every `minutes`.
    #[default]
    Interval,
    /// At the times of day (`times`, or `at`) on `weekdays` (0 = Sunday; empty = every day).
    Daily,
    /// At the times of day on `month_days` (1–31, -1 = the last day).
    Monthly,
    /// By the 5-field cron expression `cron`.
    Cron,
}

/// Scheduled runs missed while neither the app nor the background runner
/// was running (the computer was off or asleep).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, ts_rs::TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum MissedRuns {
    /// One run for all of them, as soon as possible.
    #[default]
    Once,
    /// None: only a run due in the last [`SKIP_GRACE_MS`] still runs.
    Skip,
    /// One run per missed time, one after the other (the last [`MISSED_KEPT`]).
    Each,
}

/// With [`MissedRuns::Skip`], a run this late still runs (a short sleep,
/// the app starting a minute after the time).
pub const SKIP_GRACE_MS: i64 = 10 * 60_000;
/// With [`MissedRuns::Each`], at most this many missed runs are made up.
pub const MISSED_KEPT: usize = 24;
/// Missed times counted at most in one window of [`JobSchedule::due`].
const MISSED_SCAN: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ts_rs::TS)]
#[ts(export)]
pub struct JobSchedule {
    /// The time schedule is on (`after_jobs` start runs either way).
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub mode: ScheduleMode,
    #[serde(default = "default_minutes")]
    pub minutes: u32,
    /// Time of day ("HH:MM") when `times` is empty.
    #[serde(default = "default_at")]
    pub at: String,
    /// Times of day ("HH:MM") for daily and monthly schedules; empty = `at`.
    #[serde(default)]
    #[ts(as = "Option<Vec<String>>", optional)]
    pub times: Vec<String>,
    #[serde(default)]
    pub weekdays: Vec<u8>,
    /// Days of the month (1–31; -1 = last day). A day a month doesn't have
    /// (31 in April) is skipped that month. Empty = the 1st.
    #[serde(default)]
    #[ts(as = "Option<Vec<i8>>", optional)]
    pub month_days: Vec<i8>,
    /// "minute hour day-of-month month day-of-week", e.g. "30 6 * * 1-5".
    #[serde(default)]
    #[ts(as = "Option<String>", optional)]
    pub cron: String,
    /// IANA name ("Europe/Paris"), "UTC", a fixed offset ("+05:30"), or
    /// empty for the computer's time zone.
    #[serde(default)]
    #[ts(as = "Option<String>", optional)]
    pub time_zone: String,
    #[serde(default)]
    #[ts(as = "Option<MissedRuns>", optional)]
    pub missed: MissedRuns,
    /// Ids of jobs whose successful run starts this one.
    #[serde(default)]
    #[ts(as = "Option<Vec<String>>", optional)]
    pub after_jobs: Vec<String>,
}

fn default_minutes() -> u32 {
    60
}
fn default_at() -> String {
    "08:00".into()
}

impl Default for JobSchedule {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: ScheduleMode::Interval,
            minutes: default_minutes(),
            at: default_at(),
            times: Vec::new(),
            weekdays: Vec::new(),
            month_days: Vec::new(),
            cron: String::new(),
            time_zone: String::new(),
            missed: MissedRuns::Once,
            after_jobs: Vec::new(),
        }
    }
}

/// What the scheduler does with a job now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    /// Nothing is due.
    No,
    /// Start a run and count the next one from `mark` (epoch ms).
    Run { mark: i64 },
    /// Missed runs are dropped ([`MissedRuns::Skip`]): count from `mark`, don't run.
    Skip { mark: i64 },
}

/// Most minutes between runs of an interval schedule (a year); the job
/// panel refuses the same.
pub const MAX_MINUTES: u32 = 525_600;

/// Why `expr` can't be a job's cron schedule (the check a save makes),
/// `None` when it can: the job panel asks this rather than repeat the rules.
pub fn cron_problem(expr: &str) -> Option<String> {
    JobSchedule { enabled: true, mode: ScheduleMode::Cron, cron: expr.into(), ..Default::default() }.problem()
}

/// Why `tz` can't be a job's time zone, `None` when it can (empty: the computer's).
pub fn time_zone_problem(tz: &str) -> Option<String> {
    Zone::parse(tz).err()
}

/// The IANA time zone names a job's time zone may be (exactly as spelled).
pub fn time_zone_names() -> Vec<&'static str> {
    chrono_tz::TZ_VARIANTS.iter().map(|t| t.name()).collect()
}

/// A schedule's time zone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Zone {
    Local,
    Named(chrono_tz::Tz),
    Fixed(FixedOffset),
}

impl Zone {
    /// "" / "local" → the computer's; "UTC"; "+05:30", "-08:00", "UTC+2"; an IANA name.
    pub fn parse(s: &str) -> Result<Zone, String> {
        let t = s.trim();
        if t.is_empty() || t.eq_ignore_ascii_case("local") {
            return Ok(Zone::Local);
        }
        if t.eq_ignore_ascii_case("utc") || t.eq_ignore_ascii_case("gmt") {
            return Ok(Zone::Named(chrono_tz::Tz::UTC));
        }
        // "UTC+2", "gmt-3": the prefix in any case.
        let prefixed = t.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("utc") || p.eq_ignore_ascii_case("gmt"));
        let off = if prefixed && t.len() > 3 { &t[3..] } else { t };
        if let Some(sign) = off.chars().next().filter(|c| *c == '+' || *c == '-') {
            let (h, m) = off[1..].split_once(':').unwrap_or((&off[1..], "0"));
            // Digits only: "+-5:00" or "+5:-30" is not an offset.
            let num = |x: &str| (!x.is_empty() && x.len() <= 2 && x.bytes().all(|b| b.is_ascii_digit())).then(|| x.parse::<i32>().ok()).flatten();
            let (h, m) = (num(h), num(m));
            if let (Some(h), Some(m)) = (h, m)
                && h <= 14
                && m < 60
            {
                let secs = (h * 3600 + m * 60) * if sign == '-' { -1 } else { 1 };
                return FixedOffset::east_opt(secs).map(Zone::Fixed).ok_or_else(|| format!("time zone \"{t}\" is not a valid offset"));
            }
            return Err(format!("time zone \"{t}\" is not a valid offset (like +05:30)"));
        }
        t.parse::<chrono_tz::Tz>()
            .map(Zone::Named)
            .map_err(|_| format!("unknown time zone \"{t}\" (an IANA name like Europe/Paris, UTC, or an offset like +05:30)"))
    }
}

/// A parsed 5-field cron expression (bit sets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u32,
    /// Bits 1–31.
    days: u32,
    /// Bits 1–12.
    months: u16,
    /// Bits 0–6 (0 = Sunday).
    weekdays: u8,
    /// Day of month / day of week start with `*` (also `*/2`): as in cron,
    /// a day must then match both fields' bits; with neither starred, a day
    /// matching either runs.
    any_day: bool,
    any_weekday: bool,
}

const MONTH_NAMES: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const DAY_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn cron_field(f: &str, lo: u32, hi: u32, names: &[&str], what: &str) -> Result<u64, String> {
    let value = |v: &str| -> Result<u32, String> {
        let l = v.to_ascii_lowercase();
        let n = names.iter().position(|n| *n == l).map(|i| i as u32 + if names.len() == 12 { 1 } else { 0 });
        let n = match n {
            Some(n) => n,
            None => v.parse::<u32>().map_err(|_| format!("cron {what}: \"{v}\" is not a number"))?,
        };
        if n < lo || n > hi {
            return Err(format!("cron {what}: {n} is out of range ({lo}–{hi})"));
        }
        Ok(n)
    };
    let mut bits = 0u64;
    for part in f.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().ok().filter(|s| *s > 0).ok_or_else(|| format!("cron {what}: bad step \"{s}\""))?),
            None => (part, 1),
        };
        let (a, b, named) = if range == "*" {
            (lo, hi, false)
        } else if let Some((a, b)) = range.split_once('-') {
            let named = [a, b].iter().any(|x| x.bytes().any(|c| c.is_ascii_alphabetic()));
            (value(a)?, value(b)?, named)
        } else {
            let a = value(range)?;
            // "5/15" is "5-max/15".
            (a, if part.contains('/') { hi } else { a }, false)
        };
        // Days of the week by name may go round the week: "fri-sun",
        // "sat-mon" (Sunday is 0). Numbers stay in order ("5-1" is refused).
        let seq: Vec<u32> = if a <= b {
            (a..=b).collect()
        } else if named && names.len() == 7 {
            (a..=6).chain(0..=b).collect()
        } else {
            return Err(format!("cron {what}: range {a}-{b} is backwards"));
        };
        // A huge step ("1-59/4294967295") ends the range, never wraps.
        for i in seq.iter().step_by(usize::try_from(step).unwrap_or(usize::MAX)) {
            bits |= 1 << i;
        }
    }
    Ok(bits)
}

impl Cron {
    pub fn parse(expr: &str) -> Result<Cron, String> {
        let e = expr.trim();
        let e = match e.to_ascii_lowercase().as_str() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            _ => e,
        };
        let f: Vec<&str> = e.split_whitespace().collect();
        if f.len() != 5 {
            return Err(format!("a cron expression has 5 fields (minute hour day month weekday), \"{expr}\" has {}", f.len()));
        }
        let dow = cron_field(f[4], 0, 7, &DAY_NAMES, "day of week")?;
        Ok(Cron {
            minutes: cron_field(f[0], 0, 59, &[], "minute")?,
            hours: cron_field(f[1], 0, 23, &[], "hour")? as u32,
            days: cron_field(f[2], 1, 31, &[], "day of month")? as u32,
            months: cron_field(f[3], 1, 12, &MONTH_NAMES, "month")? as u16,
            // 7 is Sunday too.
            weekdays: ((dow | (dow >> 7)) & 0x7f) as u8,
            // As in cron, a field starting with "*" (also "*/2") makes the day
            // rule "both" instead of "either".
            any_day: f[2].starts_with('*'),
            any_weekday: f[4].starts_with('*'),
        })
    }

    fn day_matches(&self, d: NaiveDate) -> bool {
        if self.months & (1 << d.month()) == 0 {
            return false;
        }
        let dom = self.days & (1 << d.day()) != 0;
        let dow = self.weekdays & (1 << d.weekday().num_days_from_sunday()) != 0;
        if self.any_day || self.any_weekday { dom && dow } else { dom || dow }
    }

    /// Whether some day ever matches: with the "both" rule a day of the
    /// month must exist in a chosen month (`30 2` never does), and over the
    /// years every date falls on every weekday.
    fn can_run(&self) -> bool {
        if !(self.any_day || self.any_weekday) {
            return true;
        }
        (1..=12u32).filter(|m| self.months & (1 << m) != 0).any(|m| {
            let max = match m {
                2 => 29,
                4 | 6 | 9 | 11 => 30,
                _ => 31,
            };
            (1..=max).any(|d| self.days & (1 << d) != 0)
        })
    }

    /// The longest wait between two days it runs on, in days, over the
    /// 400-year cycle of the calendar (`0 0 29 2 */7`, Feb 29 on a Sunday,
    /// waits up to 40 years). Only the "both" rule with a day of the month
    /// can wait more than a few weeks.
    fn longest_wait(&self) -> i64 {
        const ALL_DAYS: u32 = 0xffff_fffe;
        if !(self.any_day || self.any_weekday) || self.days & ALL_DAYS == ALL_DAYS {
            return 0;
        }
        let start = NaiveDate::from_ymd_opt(2000, 1, 1).unwrap_or_default();
        let cycle = 146_097; // days in 400 years
        let (mut first, mut last, mut most) = (None::<i64>, None::<i64>, 0i64);
        for k in 0..cycle {
            if self.day_matches(start + Duration::days(k)) {
                if let Some(l) = last {
                    most = most.max(k - l);
                }
                first.get_or_insert(k);
                last = Some(k);
            }
        }
        match (first, last) {
            // Round the cycle: from the last day back to the first.
            (Some(f), Some(l)) => most.max(f + cycle - l),
            _ => i64::MAX,
        }
    }

    fn times(&self) -> Vec<NaiveTime> {
        let mut out = Vec::new();
        for h in (0..24).filter(|h| self.hours & (1 << h) != 0) {
            for m in (0..60).filter(|m| self.minutes & (1 << m) != 0) {
                out.extend(NaiveTime::from_hms_opt(h, m, 0));
            }
        }
        out
    }
}

fn last_day(d: NaiveDate) -> u32 {
    let (y, m) = if d.month() == 12 { (d.year() + 1, 1) } else { (d.year(), d.month() + 1) };
    NaiveDate::from_ymd_opt(y, m, 1).and_then(|n| n.pred_opt()).map_or(28, |n| n.day())
}

/// Days ahead [`JobSchedule::next_after`] looks for a cron day (Feb 29
/// comes every 4, sometimes 8 years); a cron that can wait longer is
/// refused when saved.
const CRON_SCAN_DAYS: i64 = 366 * 8 + 2;

/// The instant a wall-clock time has in `z`: the first one when it is
/// repeated (clocks go back), the first valid time after it when it is
/// skipped (clocks go forward).
fn resolve<Z: TimeZone>(z: &Z, t: NaiveDateTime) -> Option<DateTime<Z>> {
    match z.from_local_datetime(&t) {
        LocalResult::Single(d) => Some(d),
        LocalResult::Ambiguous(a, b) => Some(if a <= b { a } else { b }),
        LocalResult::None => (1..=24 * 60).find_map(|m| z.from_local_datetime(&(t + Duration::minutes(m))).earliest()),
    }
}

/// The zone's standard-time offset in `year` (ms east of UTC): the smaller
/// of its offsets on January 1 and July 1, daylight saving being the larger
/// one in either hemisphere.
fn standard_offset_ms<Z: TimeZone>(z: &Z, year: i32) -> i64 {
    let off = |m: u32| {
        NaiveDate::from_ymd_opt(year, m, 1)
            .and_then(|d| d.and_hms_opt(12, 0, 0))
            .and_then(|t| z.from_local_datetime(&t).earliest())
            .map(|d| d.naive_local().and_utc().timestamp_millis() - d.timestamp_millis())
    };
    match (off(1), off(7)) {
        (Some(a), Some(b)) => a.min(b),
        (a, b) => a.or(b).unwrap_or(0),
    }
}

impl JobSchedule {
    /// Problems that stop the schedule from running (an unknown time zone,
    /// a bad cron expression or time), `None` when it is fine or off.
    pub fn problem(&self) -> Option<String> {
        if !self.enabled {
            return None;
        }
        if let Err(e) = Zone::parse(&self.time_zone) {
            return Some(e);
        }
        match self.mode {
            ScheduleMode::Interval if self.minutes == 0 => Some("every 0 minutes: choose at least 1 minute".into()),
            ScheduleMode::Interval if self.minutes > MAX_MINUTES => Some(format!("every {} minutes is more than a year ({MAX_MINUTES} minutes)", self.minutes)),
            ScheduleMode::Interval => None,
            ScheduleMode::Daily | ScheduleMode::Monthly => {
                if self.mode == ScheduleMode::Daily
                    && let Some(d) = self.weekdays.iter().find(|d| **d > 6)
                {
                    return Some(format!("{d} is not a day of the week (0 = Sunday … 6 = Saturday)"));
                }
                if let Some(bad) = self.time_texts().iter().find(|t| NaiveTime::parse_from_str(t.trim(), "%H:%M").is_err()) {
                    return Some(format!("\"{bad}\" is not a time of day (HH:MM)"));
                }
                if self.mode == ScheduleMode::Monthly
                    && let Some(d) = self.month_days.iter().find(|d| !(**d == -1 || (1..=31).contains(*d)))
                {
                    return Some(format!("{d} is not a day of the month (1–31, or the last day)"));
                }
                None
            }
            ScheduleMode::Cron => match Cron::parse(&self.cron) {
                Err(e) => Some(e),
                Ok(c) if !c.can_run() => Some(format!("cron \"{}\" never runs: no chosen month has that day", self.cron.trim())),
                Ok(c) if c.longest_wait() > CRON_SCAN_DAYS - 2 => {
                    Some(format!("cron \"{}\" runs too rarely: it can wait {} years between runs (at most 8)", self.cron.trim(), c.longest_wait() / 365))
                }
                Ok(_) => None,
            },
        }
    }

    fn time_texts(&self) -> Vec<&str> {
        if self.times.is_empty() { vec![self.at.as_str()] } else { self.times.iter().map(String::as_str).collect() }
    }

    /// The times of day, sorted, `None` if one is not HH:MM.
    fn times_of_day(&self) -> Option<Vec<NaiveTime>> {
        let mut t = self.time_texts().iter().map(|s| NaiveTime::parse_from_str(s.trim(), "%H:%M").ok()).collect::<Option<Vec<_>>>()?;
        t.sort();
        t.dedup();
        Some(t)
    }

    /// First run time strictly after `after_ms` (epoch ms), `None` when off
    /// or invalid.
    pub fn next_after(&self, after_ms: i64) -> Option<i64> {
        if !self.enabled {
            return None;
        }
        match Zone::parse(&self.time_zone).ok()? {
            Zone::Local => self.next_in(&Local, after_ms),
            Zone::Named(z) => self.next_in(&z, after_ms),
            Zone::Fixed(z) => self.next_in(&z, after_ms),
        }
    }

    fn next_in<Z: TimeZone>(&self, z: &Z, after_ms: i64) -> Option<i64> {
        let start = z.timestamp_millis_opt(after_ms).single()?;
        let days = |n: i64, matches: &dyn Fn(NaiveDate) -> bool, times: &[NaiveTime]| -> Option<i64> {
            // A skipped time may resolve into the next day: start a day early.
            // A day's times resolve in order, so the first one after `after_ms`
            // is found by halving (a 1-minute cron resolves ~11 times a day,
            // not every minute since the day before).
            let at = |day: NaiveDate, t: &NaiveTime| resolve(z, day.and_time(*t)).map(|d| d.timestamp_millis());
            for d in -1..n {
                let day = start.date_naive() + Duration::days(d);
                if !matches(day) {
                    continue;
                }
                let from = times.partition_point(|t| at(day, t).is_none_or(|ms| ms <= after_ms));
                if let Some(ms) = times[from..].iter().find_map(|t| at(day, t).filter(|ms| *ms > after_ms)) {
                    return Some(ms);
                }
            }
            None
        };
        match self.mode {
            ScheduleMode::Interval if self.minutes > 60 && 1440 % self.minutes == 0 => {
                // Hours dividing a day (every 2 h, 6 h, 1 day): the wall clock's
                // slots from midnight (00:00, 06:00, …) in summer and winter
                // alike, resolved like daily times when clocks change (the gap
                // around a change is an hour longer or shorter).
                let times: Vec<NaiveTime> =
                    (0..1440).step_by(self.minutes.max(1) as usize).filter_map(|m| NaiveTime::from_hms_opt(m / 60, m % 60, 0)).collect();
                days(3, &|_| true, &times)
            }
            ScheduleMode::Interval => {
                let step = i64::from(self.minutes.max(1)) * 60_000;
                // An hour or less (every 15 min → :00, :15, …, the same marks in
                // summer), or not dividing a day (90 min, 2 days): aligned to the
                // zone's standard-time clock, one offset all year, so the runs
                // stay exactly `step` apart when clocks change.
                let offset = standard_offset_ms(z, start.year());
                let local = after_ms + offset;
                Some((local.div_euclid(step) + 1) * step - offset)
            }
            ScheduleMode::Daily => {
                let times = self.times_of_day()?;
                let wd = |d: NaiveDate| self.weekdays.is_empty() || self.weekdays.contains(&(d.weekday().num_days_from_sunday() as u8));
                days(9, &wd, &times)
            }
            ScheduleMode::Monthly => {
                let times = self.times_of_day()?;
                let picked: Vec<i8> = if self.month_days.is_empty() { vec![1] } else { self.month_days.clone() };
                let md = |d: NaiveDate| picked.iter().any(|p| (*p == -1 && d.day() == last_day(d)) || (*p > 0 && *p as u32 == d.day()));
                days(400, &md, &times)
            }
            ScheduleMode::Cron => {
                let c = Cron::parse(&self.cron).ok()?;
                let times = c.times();
                days(CRON_SCAN_DAYS, &|d| c.day_matches(d), &times)
            }
        }
    }

    /// What to do now for a job whose next run is counted from `base`,
    /// following [`Self::missed`].
    pub fn due(&self, base: i64, now: i64) -> Due {
        let Some(first) = self.next_after(base).filter(|t| *t <= now) else { return Due::No };
        match self.missed {
            MissedRuns::Once => Due::Run { mark: now },
            // A time in the last SKIP_GRACE_MS (after `base`): one look, not a walk
            // through every missed time.
            MissedRuns::Skip => match self.next_after(base.max(now - SKIP_GRACE_MS - 1)) {
                Some(t) if t <= now => Due::Run { mark: now },
                _ => Due::Skip { mark: now },
            },
            MissedRuns::Each => {
                // The last MISSED_KEPT missed times, counted in a window that
                // grows back from now until it holds them (or reaches `base`), so
                // a 1-minute schedule missed for months looks at a few minutes.
                let mut span = 60 * 60_000i64;
                loop {
                    let from = base.max(now - span);
                    let mut kept = std::collections::VecDeque::new();
                    let mut t = from;
                    let mut steps = 0;
                    while let Some(n) = self.next_after(t).filter(|n| *n <= now) {
                        kept.push_back(n);
                        if kept.len() > MISSED_KEPT {
                            kept.pop_front();
                        }
                        t = n;
                        steps += 1;
                        if steps >= MISSED_SCAN {
                            // More than can be counted: one run, like Once.
                            return Due::Run { mark: now };
                        }
                    }
                    if kept.len() >= MISSED_KEPT || from == base {
                        // (`first` is due, so `kept` isn't empty when the window reaches `base`.)
                        return Due::Run { mark: kept.front().copied().unwrap_or(first) };
                    }
                    span = span.saturating_mul(4);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Tz;

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        chrono::Utc.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp_millis()
    }
    fn at(z: Tz, y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        z.with_ymd_and_hms(y, mo, d, h, mi, 0).earliest().unwrap().timestamp_millis()
    }
    fn sched(mode: ScheduleMode) -> JobSchedule {
        JobSchedule { enabled: true, mode, time_zone: "UTC".into(), ..Default::default() }
    }

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

    #[test]
    fn old_schedules_load_unchanged() {
        let s: JobSchedule = serde_json::from_str(r#"{"enabled":true,"mode":"daily","minutes":60,"at":"07:00","weekdays":[1]}"#).unwrap();
        assert_eq!(s, JobSchedule { enabled: true, mode: ScheduleMode::Daily, at: "07:00".into(), weekdays: vec![1], ..Default::default() });
        assert_eq!((s.missed, s.time_zone.as_str(), s.after_jobs.len()), (MissedRuns::Once, "", 0));
    }

    #[test]
    fn several_times_a_day_and_weekdays() {
        let s = JobSchedule { times: vec!["18:00".into(), "06:30".into(), "12:00".into()], weekdays: vec![1, 2, 3, 4, 5], ..sched(ScheduleMode::Daily) };
        // Friday 2026-03-13 13:00 → 18:00; then Monday 06:30 (weekend skipped).
        assert_eq!(s.next_after(utc(2026, 3, 13, 13, 0)), Some(utc(2026, 3, 13, 18, 0)));
        assert_eq!(s.next_after(utc(2026, 3, 13, 18, 0)), Some(utc(2026, 3, 16, 6, 30)));
        assert_eq!(s.next_after(utc(2026, 3, 16, 6, 30)), Some(utc(2026, 3, 16, 12, 0)));
        // Saturday and Sunday only.
        let we = JobSchedule { weekdays: vec![6, 0], ..s.clone() };
        assert_eq!(we.next_after(utc(2026, 3, 16, 0, 0)), Some(utc(2026, 3, 21, 6, 30)));
        assert_eq!(we.next_after(utc(2026, 3, 22, 18, 0)), Some(utc(2026, 3, 28, 6, 30)));
        let bad = JobSchedule { times: vec!["06:30".into(), "7pm".into()], ..s };
        assert!(bad.problem().unwrap().contains("7pm"));
        assert_eq!(bad.next_after(0), None);
    }

    #[test]
    fn monthly_days_and_the_last_day() {
        let s = JobSchedule { month_days: vec![31], at: "05:00".into(), ..sched(ScheduleMode::Monthly) };
        // The 31st: April has none → May 31.
        assert_eq!(s.next_after(utc(2026, 3, 31, 6, 0)), Some(utc(2026, 5, 31, 5, 0)));
        let last = JobSchedule { month_days: vec![-1], ..s.clone() };
        assert_eq!(last.next_after(utc(2026, 1, 31, 6, 0)), Some(utc(2026, 2, 28, 5, 0)));
        assert_eq!(last.next_after(utc(2028, 2, 1, 0, 0)), Some(utc(2028, 2, 29, 5, 0)), "leap year");
        assert_eq!(last.next_after(utc(2026, 4, 1, 0, 0)), Some(utc(2026, 4, 30, 5, 0)));
        let d29 = JobSchedule { month_days: vec![29], ..s.clone() };
        assert_eq!(d29.next_after(utc(2027, 1, 30, 0, 0)), Some(utc(2027, 3, 29, 5, 0)), "no Feb 29 in 2027");
        let d30_1 = JobSchedule { month_days: vec![1, 15], times: vec!["09:00".into(), "21:00".into()], ..s.clone() };
        assert_eq!(d30_1.next_after(utc(2026, 3, 15, 10, 0)), Some(utc(2026, 3, 15, 21, 0)));
        assert_eq!(d30_1.next_after(utc(2026, 3, 15, 21, 0)), Some(utc(2026, 4, 1, 9, 0)));
        // Default: the 1st.
        assert_eq!(JobSchedule { month_days: vec![], ..s.clone() }.next_after(utc(2026, 3, 2, 0, 0)), Some(utc(2026, 4, 1, 5, 0)));
        assert!(JobSchedule { month_days: vec![32], ..s }.problem().is_some());
    }

    #[test]
    fn cron_expressions() {
        let c = |e: &str| JobSchedule { cron: e.into(), ..sched(ScheduleMode::Cron) };
        // Weekdays at 06:30: Friday → Monday.
        assert_eq!(c("30 6 * * 1-5").next_after(utc(2026, 3, 13, 7, 0)), Some(utc(2026, 3, 16, 6, 30)));
        assert_eq!(c("30 6 * * MON-FRI").next_after(utc(2026, 3, 13, 7, 0)), Some(utc(2026, 3, 16, 6, 30)));
        assert_eq!(c("*/15 * * * *").next_after(utc(2026, 3, 13, 7, 1)), Some(utc(2026, 3, 13, 7, 15)));
        assert_eq!(c("0 9-17/4 * * *").next_after(utc(2026, 3, 13, 9, 0)), Some(utc(2026, 3, 13, 13, 0)));
        assert_eq!(c("0 0 1,15 * *").next_after(utc(2026, 3, 2, 0, 0)), Some(utc(2026, 3, 15, 0, 0)));
        // Day of month and day of week both set: either matches (cron's rule).
        assert_eq!(c("0 0 13 * 5").next_after(utc(2026, 3, 1, 0, 0)), Some(utc(2026, 3, 6, 0, 0)));
        // Sunday as 7; month names.
        assert_eq!(c("0 12 * * 7").next_after(utc(2026, 3, 13, 0, 0)), Some(utc(2026, 3, 15, 12, 0)));
        assert_eq!(c("0 0 1 jan *").next_after(utc(2026, 3, 13, 0, 0)), Some(utc(2027, 1, 1, 0, 0)));
        assert_eq!(c("@daily").next_after(utc(2026, 3, 13, 0, 0)), Some(utc(2026, 3, 14, 0, 0)));
        assert_eq!(c("0 0 29 2 *").next_after(utc(2026, 3, 1, 0, 0)), Some(utc(2028, 2, 29, 0, 0)));
        for bad in ["", "* * * *", "60 * * * *", "* 24 * * *", "* * 0 * *", "* * * 13 *", "* * * * 8", "5-1 * * * *", "*/0 * * * *", "x * * * *"] {
            assert!(c(bad).problem().is_some(), "{bad:?} is refused");
            assert_eq!(c(bad).next_after(0), None, "{bad:?}");
        }
        assert_eq!(c("0 0 31 2 *").next_after(0), None, "never");
        // A cron that never runs (no chosen month has the day) is refused on save.
        for never in ["0 0 30 2 *", "0 0 31 4,6 *", "0 0 30 2 */2"] {
            assert!(c(never).problem().unwrap().contains("never runs"), "{never}");
        }
        // Both day fields specific: either matches, so February's Mondays run.
        for ok in ["0 0 29 2 *", "0 0 31 * *", "0 0 30 2 1", "0 0 31 4,5 *"] {
            assert_eq!(c(ok).problem(), None, "{ok}");
        }
        // A huge step ends the range instead of overflowing.
        assert_eq!(c("1-59/4294967295 * * * *").next_after(utc(2026, 3, 13, 7, 1)), Some(utc(2026, 3, 13, 8, 1)));
    }

    /// Cron's day rule: a day field starting with `*` (also `*/n`) keeps its
    /// bits and both fields must match; with neither starred, either does.
    #[test]
    fn cron_day_rule() {
        let c = |e: &str| JobSchedule { cron: e.into(), ..sched(ScheduleMode::Cron) };
        let runs = |e: &str, from: i64, n: usize| {
            let mut t = from;
            (0..n)
                .map(|_| {
                    t = c(e).next_after(t).unwrap();
                    t
                })
                .collect::<Vec<_>>()
        };
        // Every other day of the month: the 1st, 3rd, 5th (not every day).
        assert_eq!(runs("0 0 */2 * *", utc(2026, 3, 1, 0, 0) - 1, 3), vec![utc(2026, 3, 1, 0, 0), utc(2026, 3, 3, 0, 0), utc(2026, 3, 5, 0, 0)]);
        // Sun, Tue, Thu, Sat (2026-03-01 is a Sunday).
        assert_eq!(
            runs("0 0 * * */2", utc(2026, 3, 1, 0, 0) - 1, 4),
            vec![utc(2026, 3, 1, 0, 0), utc(2026, 3, 3, 0, 0), utc(2026, 3, 5, 0, 0), utc(2026, 3, 7, 0, 0)]
        );
        // The 1st, only when it is a Sun, Wed or Sat: 2026-03-01 (Sun), then 2026-04-01 (Wed).
        assert_eq!(runs("0 0 1 * */3", utc(2026, 2, 2, 0, 0), 2), vec![utc(2026, 3, 1, 0, 0), utc(2026, 4, 1, 0, 0)]);
        // Mondays that are the 1st, 11th, 21st or 31st: 2026-05-11, then 2026-06-01.
        assert_eq!(runs("0 0 */10 * 1", utc(2026, 3, 1, 0, 0), 2), vec![utc(2026, 5, 11, 0, 0), utc(2026, 6, 1, 0, 0)]);
        // Both specific: either (the 13th, and every Friday).
        assert_eq!(runs("0 0 13 * 5", utc(2026, 3, 5, 0, 0), 3), vec![utc(2026, 3, 6, 0, 0), utc(2026, 3, 13, 0, 0), utc(2026, 3, 20, 0, 0)]);
    }

    #[test]
    fn time_zones() {
        let s = JobSchedule { at: "09:00".into(), ..sched(ScheduleMode::Daily) };
        let paris = JobSchedule { time_zone: "Europe/Paris".into(), ..s.clone() };
        // 09:00 in Paris (UTC+1 in winter) is 08:00 UTC.
        assert_eq!(paris.next_after(utc(2026, 1, 10, 0, 0)), Some(utc(2026, 1, 10, 8, 0)));
        assert_eq!(paris.next_after(utc(2026, 7, 10, 0, 0)), Some(utc(2026, 7, 10, 7, 0)), "summer time");
        let fixed = JobSchedule { time_zone: "+05:30".into(), ..s.clone() };
        assert_eq!(fixed.next_after(utc(2026, 1, 10, 0, 0)), Some(utc(2026, 1, 10, 3, 30)));
        assert_eq!(JobSchedule { time_zone: "UTC-8".into(), ..s.clone() }.next_after(utc(2026, 1, 10, 0, 0)), Some(utc(2026, 1, 10, 17, 0)));
        // An interval aligns to the zone's clock: every 2 h at even hours in +05:30.
        let iv = JobSchedule { mode: ScheduleMode::Interval, minutes: 120, ..fixed.clone() };
        assert_eq!(iv.next_after(utc(2026, 1, 10, 0, 0)), Some(utc(2026, 1, 10, 0, 30)));
        for bad in ["Mars/Base", "+25:00", "+5:99", "UTC+x", "+-5:00", "+5:-30", "+", "+5:", "+123"] {
            let b = JobSchedule { time_zone: bad.into(), ..s.clone() };
            assert!(b.problem().is_some(), "{bad}");
            assert_eq!(b.next_after(0), None);
        }
        assert_eq!(Zone::parse("utc"), Ok(Zone::Named(Tz::UTC)));
        assert_eq!(Zone::parse(""), Ok(Zone::Local));
    }

    #[test]
    fn daylight_saving_changes() {
        let ny = Tz::America__New_York;
        let s = JobSchedule { at: "02:30".into(), time_zone: "America/New_York".into(), ..sched(ScheduleMode::Daily) };
        // 2026-03-08: 02:00 → 03:00, 02:30 doesn't exist → runs once at 03:00.
        assert_eq!(s.next_after(at(ny, 2026, 3, 7, 12, 0)), Some(at(ny, 2026, 3, 8, 3, 0)));
        assert_eq!(s.next_after(at(ny, 2026, 3, 8, 3, 0)), Some(at(ny, 2026, 3, 9, 2, 30)));
        // 2026-11-01: 01:30 happens twice → runs once, the first time.
        let back = JobSchedule { at: "01:30".into(), ..s.clone() };
        let first = at(ny, 2026, 11, 1, 1, 30);
        assert_eq!(back.next_after(at(ny, 2026, 10, 31, 12, 0)), Some(first));
        assert_eq!(back.next_after(first), Some(at(ny, 2026, 11, 2, 1, 30)), "not again an hour later");
        // Cron every 15 min across the repeated hour: the second 01:xx is skipped.
        let c = JobSchedule { mode: ScheduleMode::Cron, cron: "*/15 * * * *".into(), ..s.clone() };
        let first_145 = at(ny, 2026, 11, 1, 1, 45);
        let t = c.next_after(first_145).unwrap();
        assert_eq!(t, at(ny, 2026, 11, 1, 2, 0));
        assert_eq!(t - first_145, 75 * 60_000);
        // …and across the skipped hour: 02:00–02:45 run once, at 03:00.
        let before = at(ny, 2026, 3, 8, 1, 45);
        let gap = c.next_after(before).unwrap();
        assert_eq!(gap, at(ny, 2026, 3, 8, 3, 0));
        assert_eq!(c.next_after(gap), Some(at(ny, 2026, 3, 8, 3, 15)));
        // An interval counts elapsed time across the change.
        let hourly = JobSchedule { mode: ScheduleMode::Interval, minutes: 60, ..s };
        let a = hourly.next_after(at(ny, 2026, 11, 1, 0, 30)).unwrap();
        let b = hourly.next_after(a).unwrap();
        let c2 = hourly.next_after(b).unwrap();
        assert_eq!((b - a, c2 - b), (3_600_000, 3_600_000));
    }

    #[test]
    fn missed_runs() {
        let hourly = JobSchedule { minutes: 60, ..sched(ScheduleMode::Interval) };
        let base = utc(2026, 3, 1, 0, 0);
        let now = utc(2026, 3, 1, 5, 20);
        assert_eq!(hourly.due(base, utc(2026, 3, 1, 0, 59)), Due::No);
        assert_eq!(hourly.due(base, now), Due::Run { mark: now });
        // Skip: the last time (05:00) is 20 min ago → nothing runs; 5 min ago → runs.
        let skip = JobSchedule { missed: MissedRuns::Skip, ..hourly.clone() };
        assert_eq!(skip.due(base, now), Due::Skip { mark: now });
        assert_eq!(skip.due(base, utc(2026, 3, 1, 5, 5)), Due::Run { mark: utc(2026, 3, 1, 5, 5) });
        // Each: 01:00 … 05:00, one at a time from the oldest.
        let each = JobSchedule { missed: MissedRuns::Each, ..hourly.clone() };
        let mut mark = base;
        let mut runs = Vec::new();
        while let Due::Run { mark: m } = each.due(mark, now) {
            runs.push(m);
            mark = m;
        }
        assert_eq!(runs, (1..=5).map(|h| utc(2026, 3, 1, h, 0)).collect::<Vec<_>>());
        // At most MISSED_KEPT: three days missed → the last 24.
        let late = utc(2026, 3, 4, 0, 30);
        let Due::Run { mark: m } = each.due(base, late) else { panic!() };
        assert_eq!(m, utc(2026, 3, 3, 1, 0));
    }

    /// A 1-minute cron missed for 90 days is decided quickly (the scheduler
    /// runs this on every tick), and the same as a full count would.
    #[test]
    fn long_missed_scans_are_quick() {
        let base = utc(2026, 1, 1, 0, 0);
        let now = utc(2026, 4, 1, 0, 0) + 30_000;
        for zone in ["UTC", "Europe/Paris", ""] {
            let c = JobSchedule { cron: "* * * * *".into(), time_zone: zone.into(), ..sched(ScheduleMode::Cron) };
            let t = std::time::Instant::now();
            let skip = JobSchedule { missed: MissedRuns::Skip, ..c.clone() }.due(base, now);
            let each = JobSchedule { missed: MissedRuns::Each, ..c.clone() }.due(base, now);
            assert!(t.elapsed() < std::time::Duration::from_millis(200), "{zone:?}: {:?}", t.elapsed());
            // The last minute (00:00) was 30 s ago: Skip runs; Each counts from 24 minutes back.
            assert_eq!(skip, Due::Run { mark: now }, "{zone:?}");
            assert_eq!(each, Due::Run { mark: utc(2026, 4, 1, 0, 0) - 23 * 60_000 }, "{zone:?}");
        }
        // A daily time missed for a year: Skip doesn't run (too late), Each keeps 24 days.
        let daily = JobSchedule { at: "06:00".into(), ..sched(ScheduleMode::Daily) };
        let now = utc(2027, 1, 1, 12, 0);
        assert_eq!(JobSchedule { missed: MissedRuns::Skip, ..daily.clone() }.due(base, now), Due::Skip { mark: now });
        assert_eq!(JobSchedule { missed: MissedRuns::Each, ..daily }.due(base, now), Due::Run { mark: utc(2026, 12, 9, 6, 0) });
    }

    /// Gaps between consecutive runs of `s` from `from`, `n` of them.
    fn gaps(s: &JobSchedule, from: i64, n: usize) -> Vec<i64> {
        let mut t = s.next_after(from).unwrap();
        let mut out = Vec::new();
        for _ in 0..n {
            let next = s.next_after(t).unwrap();
            out.push((next - t) / 60_000);
            t = next;
        }
        out
    }

    #[test]
    fn intervals_keep_their_gaps_when_clocks_change() {
        let ny = Tz::America__New_York;
        for minutes in [100u32, 2880, 45, 60] {
            let s = JobSchedule { minutes, time_zone: "America/New_York".into(), ..sched(ScheduleMode::Interval) };
            // Clocks go back (2026-11-01) and forward (2026-03-08).
            for from in [at(ny, 2026, 10, 30, 0, 0), at(ny, 2026, 3, 6, 0, 0)] {
                let g = gaps(&s, from, 4 * 1440 / minutes as usize);
                assert!(g.iter().all(|x| *x == i64::from(minutes)), "{minutes} min from {from}: {g:?}");
            }
        }
        // Lord Howe moves its clocks by 30 minutes.
        let lh = JobSchedule { minutes: 60, time_zone: "Australia/Lord_Howe".into(), ..sched(ScheduleMode::Interval) };
        let g = gaps(&lh, at(Tz::Australia__Lord_Howe, 2026, 4, 4, 0, 0), 72);
        assert!(g.iter().all(|x| *x == 60), "{g:?}");
        let g = gaps(&lh, at(Tz::Australia__Lord_Howe, 2026, 10, 3, 0, 0), 72);
        assert!(g.iter().all(|x| *x == 60), "{g:?}");
        // Still on the standard clock's quarter hours.
        let q = JobSchedule { minutes: 15, time_zone: "America/New_York".into(), ..sched(ScheduleMode::Interval) };
        assert_eq!(q.next_after(at(ny, 2026, 7, 1, 9, 7)), Some(at(ny, 2026, 7, 1, 9, 15)));
        assert_eq!(q.next_after(at(ny, 2026, 1, 1, 9, 7)), Some(at(ny, 2026, 1, 1, 9, 15)));
    }

    /// Hours that divide a day keep their wall-clock times all year: every
    /// day at midnight, every 6 h at 00/06/12/18, in summer too; around a
    /// clock change one gap is an hour longer or shorter, never two runs at once.
    #[test]
    fn day_dividing_intervals_keep_their_wall_clock_times() {
        let ny = Tz::America__New_York;
        let every = |minutes| JobSchedule { minutes, time_zone: "America/New_York".into(), ..sched(ScheduleMode::Interval) };
        for (m, d) in [(1, 15), (7, 15)] {
            assert_eq!(every(1440).next_after(at(ny, 2026, m, d, 9, 0)), Some(at(ny, 2026, m, d + 1, 0, 0)), "month {m}");
            assert_eq!(every(360).next_after(at(ny, 2026, m, d, 9, 0)), Some(at(ny, 2026, m, d, 12, 0)), "month {m}");
            assert_eq!(every(120).next_after(at(ny, 2026, m, d, 9, 0)), Some(at(ny, 2026, m, d, 10, 0)), "month {m}");
        }
        for minutes in [90u32, 120, 360, 1440] {
            for from in [at(ny, 2026, 10, 30, 0, 0), at(ny, 2026, 3, 6, 0, 0)] {
                let g = gaps(&every(minutes), from, 4 * 1440 / minutes as usize);
                let m = i64::from(minutes);
                assert!(g.iter().all(|x| (*x - m).abs() <= 60) && g.iter().filter(|x| **x != m).count() <= 1, "{minutes} min from {from}: {g:?}");
            }
        }
    }

    #[test]
    fn a_base_in_the_future_is_now() {
        let job = crate::Job {
            id: "j".into(),
            name: "j".into(),
            nodes: vec![],
            edges: vec![],
            schedule: JobSchedule { minutes: 60, ..sched(ScheduleMode::Interval) },
            last_scheduled_at: Some(utc(2030, 1, 1, 0, 0)),
            params: Default::default(),
            param_set: None,
            limits: Default::default(),
            created_at: 0,
            updated_at: utc(2026, 1, 1, 0, 0),
        };
        let now = utc(2026, 6, 1, 10, 30);
        // Marked while the clock was ahead (2030): counted from now, not 2030.
        assert_eq!(job.schedule_base_at(now), now);
        assert_eq!(job.schedule.next_after(job.schedule_base_at(now)), Some(utc(2026, 6, 1, 11, 0)));
        // A base in the past is kept.
        let past = crate::Job { last_scheduled_at: Some(utc(2026, 6, 1, 9, 0)), ..job };
        assert_eq!(past.schedule_base_at(now), utc(2026, 6, 1, 9, 0));
    }

    #[test]
    fn crons_that_run_too_rarely_are_refused() {
        let c = |e: &str| JobSchedule { cron: e.into(), ..sched(ScheduleMode::Cron) };
        // Feb 29 on a Sunday: up to 40 years apart.
        for e in ["0 0 29 2 */7", "0 0 29 2 sun"] {
            let p = c(e).problem();
            if e.ends_with("*/7") {
                assert!(p.as_deref().is_some_and(|p| p.contains("too rarely")), "{e}: {p:?}");
            } else {
                // Neither field starred: Feb 29 or any Sunday, often.
                assert_eq!(p, None, "{e}");
            }
        }
        assert!(c("0 0 25 12 */7").problem().unwrap().contains("too rarely"));
        assert!(c("0 0 1 2 */7").problem().unwrap().contains("too rarely"));
        // Rare but within the scan: Feb 29 every 4 years, Friday 13th.
        for e in ["0 0 29 2 *", "0 12 13 * */7", "0 0 13 * fri", "0 0 31 * *"] {
            assert_eq!(c(e).problem(), None, "{e}");
        }
        let fri13 = JobSchedule { cron: "0 12 13 * 5".into(), ..sched(ScheduleMode::Cron) };
        assert!(fri13.next_after(utc(2026, 1, 1, 0, 0)).is_some());
    }

    #[test]
    fn weekdays_names_and_offsets() {
        // A weekday outside 0–6 (cron's 7) is refused, not "never runs".
        let d = JobSchedule { weekdays: vec![7], ..sched(ScheduleMode::Daily) };
        assert!(d.problem().unwrap().contains("day of the week"));
        assert_eq!(JobSchedule { weekdays: vec![0, 6], ..d }.problem(), None);
        // UTC+2 / gmt-3 in any case.
        for z in ["utc+2", "UTC+2", "Utc+02:00", "gmt-3", "GMT-03:30"] {
            assert!(matches!(Zone::parse(z), Ok(Zone::Fixed(_))), "{z}");
        }
        assert!(Zone::parse("utc+x").is_err());
        // Day names going round the week.
        let fri_sun = Cron::parse("0 9 * * fri-sun").unwrap();
        assert_eq!(fri_sun.weekdays, 0b110_0001);
        assert_eq!(Cron::parse("0 9 * * sat-mon").unwrap().weekdays, 0b100_0011);
        assert_eq!(Cron::parse("0 9 * * fri-tue/2").unwrap().weekdays, (1 << 5) | (1 << 0) | (1 << 2));
        // Numbers stay in order.
        assert!(Cron::parse("0 9 * * 5-1").unwrap_err().contains("backwards"));
        assert!(Cron::parse("0 9 * jun-mar *").unwrap_err().contains("backwards"));
    }

    #[test]
    fn a_minute_cron_resolves_few_times() {
        // Speed: each next run costs a handful of resolves, not a day's worth.
        let c = JobSchedule { cron: "* * * * *".into(), time_zone: "Europe/Paris".into(), ..sched(ScheduleMode::Cron) };
        let mut t = utc(2026, 3, 28, 0, 0);
        let started = std::time::Instant::now();
        for _ in 0..3000 {
            t = c.next_after(t).unwrap();
        }
        assert_eq!(t, utc(2026, 3, 28, 0, 0) + 3000 * 60_000);
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
    }

    /// What the job panel asks at the field is what a save checks.
    #[test]
    fn field_checks_match_the_save() {
        assert_eq!(cron_problem("30 6 * * 1-5"), None);
        assert_eq!(cron_problem("+5 * * * *"), None, "a sign the number parser takes");
        for bad in ["61 * * * *", "0 6 * *", "0 0 30 2 *", "0 0 29 2 */7", ""] {
            assert!(cron_problem(bad).is_some(), "{bad}");
        }
        for ok in ["", "UTC", "utc+2", "+05:30", "Europe/Paris", "America/Coyhaique"] {
            assert_eq!(time_zone_problem(ok), None, "{ok}");
        }
        for bad in ["europe/paris", "+05:30:00", "Europe/Pari", "+-5:00"] {
            assert!(time_zone_problem(bad).is_some(), "{bad}");
        }
        let names = time_zone_names();
        assert!(names.contains(&"Europe/Paris") && names.contains(&"UTC"));
        let every = |minutes| JobSchedule { enabled: true, minutes, ..Default::default() }.problem();
        assert_eq!(every(MAX_MINUTES), None);
        assert!(every(MAX_MINUTES + 1).is_some_and(|p| p.contains("more than a year")));
        assert!(every(0).is_some_and(|p| p.contains("0 minutes")), "0 would run every minute");
        assert_eq!(every(1), None);
        assert_eq!(JobSchedule { enabled: false, minutes: 0, ..Default::default() }.problem(), None, "off");
    }
}
