//! When bots wake: parsing `in 2h`, `at 09:00`, `every 6h`, `off`, and describing schedules.

use anyhow::{Result, anyhow};
use chrono::{Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};

use crate::bots::Bot;

#[derive(Debug, Clone, PartialEq)]
pub enum Schedule {
    /// Once, at this Unix time.
    At(i64),
    /// Every this many seconds, first one from now.
    Every(i64),
    Off,
}

/// `now`, `in 30m`, `in 1h30m`, `at 09:00`, `at 2026-12-01 09:00`, `every 6h`, `off`.
pub fn parse(input: &str, now: i64) -> Result<Schedule> {
    let s = input.trim().to_lowercase();
    let (word, rest) = s.split_once(char::is_whitespace).unwrap_or((&s, ""));
    let rest = rest.trim();
    match word {
        "now" if rest.is_empty() => Ok(Schedule::At(now)),
        "off" | "never" if rest.is_empty() => Ok(Schedule::Off),
        "in" => Ok(Schedule::At(now + duration(rest)?)),
        "every" => Ok(Schedule::Every(duration(rest)?)),
        "at" => Ok(Schedule::At(at(rest, now)?)),
        _ => Err(anyhow!(
            "can't read {input:?}: use now, in 30m, at 09:00, at 2026-12-01 09:00, every 6h or off"
        )),
    }
}

/// `90s`, `30m`, `2h`, `1d`, `1w`, or combined like `1h30m`. Seconds.
pub fn duration(s: &str) -> Result<i64> {
    let bad = || anyhow!("can't read the duration {s:?}: use e.g. 30m, 2h, 1d or 1h30m");
    let mut total = 0i64;
    let mut num = String::new();
    for c in s.chars().filter(|c| !c.is_whitespace()) {
        if c.is_ascii_digit() {
            num.push(c);
            continue;
        }
        let n: i64 = num.parse().map_err(|_| bad())?;
        num.clear();
        total += n * match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86400,
            'w' => 7 * 86400,
            _ => return Err(bad()),
        };
    }
    if !num.is_empty() || total <= 0 {
        return Err(bad());
    }
    Ok(total)
}

/// A local time today (or tomorrow, once it's passed) or a local date and time.
fn at(s: &str, now: i64) -> Result<i64> {
    let bad = || anyhow!("can't read the time {s:?}: use e.g. 09:00 or 2026-12-01 09:00");
    let s = s.replace('t', " ");
    let local = |dt: NaiveDateTime| {
        Local
            .from_local_datetime(&dt)
            .earliest()
            .map(|d| d.timestamp())
            .ok_or_else(bad)
    };
    if let Ok(dt) = NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M") {
        return local(dt);
    }
    if let Ok(d) = NaiveDate::parse_from_str(&s, "%Y-%m-%d") {
        return local(d.and_hms_opt(9, 0, 0).unwrap());
    }
    let t = NaiveTime::parse_from_str(&s, "%H:%M").map_err(|_| bad())?;
    let today = Local
        .timestamp_opt(now, 0)
        .single()
        .ok_or_else(bad)?
        .date_naive();
    let when = local(today.and_time(t))?;
    if when > now {
        Ok(when)
    } else {
        local(today.succ_opt().ok_or_else(bad)?.and_time(t))
    }
}

pub fn apply(bot: &mut Bot, schedule: Schedule, note: Option<String>, now: i64) {
    match schedule {
        Schedule::At(t) => {
            bot.next_run = Some(t);
            bot.every = None;
        }
        Schedule::Every(secs) => {
            bot.next_run = Some(now + secs);
            bot.every = Some(secs);
        }
        Schedule::Off => {
            bot.next_run = None;
            bot.every = None;
        }
    }
    if note.is_some() {
        bot.note = note;
    }
}

/// `Today 14:30`, `Tomorrow 09:00`, `Mon Dec 1 09:00`.
pub fn when(t: i64, now: i64) -> String {
    let (Some(at), Some(today)) = (
        Local.timestamp_opt(t, 0).single(),
        Local.timestamp_opt(now, 0).single(),
    ) else {
        return String::new();
    };
    let days = (at.date_naive() - today.date_naive()).num_days();
    let time = at.format("%H:%M");
    match days {
        0 => format!("Today {time}"),
        1 => format!("Tomorrow {time}"),
        -1 => format!("Yesterday {time}"),
        _ => at.format("%a %b %-d %H:%M").to_string(),
    }
}

/// `2h`, `1d 6h`, `45m`.
pub fn span(secs: i64) -> String {
    let (d, h, m) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    let parts: Vec<String> = [(d, "d"), (h, "h"), (m, "m")]
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, u)| format!("{n}{u}"))
        .collect();
    if parts.is_empty() {
        format!("{secs}s")
    } else {
        parts.join(" ")
    }
}

/// A bot's schedule in a few words: `Next: Today 14:30 · every 2h`.
pub fn describe(bot: &Bot, now: i64) -> String {
    match (bot.next_run, bot.every) {
        (None, _) => "Not scheduled".into(),
        (Some(t), _) if t <= now => "Due now".into(),
        (Some(t), None) => format!("Next: {}", when(t, now)),
        (Some(t), Some(e)) => format!("Next: {} · every {}", when(t, now), span(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_schedules() {
        let now = 1_800_000_000;
        assert_eq!(parse("in 1h30m", now).unwrap(), Schedule::At(now + 5400));
        assert_eq!(parse("every 6h", now).unwrap(), Schedule::Every(6 * 3600));
        assert_eq!(parse("OFF", now).unwrap(), Schedule::Off);
        assert_eq!(parse("now", now).unwrap(), Schedule::At(now));
        let Schedule::At(t) = parse("at 09:00", now).unwrap() else {
            panic!()
        };
        assert!(t > now && t <= now + 86400);
        assert!(parse("at 2026-12-01 09:00", now).is_ok());
        assert!(parse("in 5 minutes", now).is_err());
        assert!(parse("in 0m", now).is_err());
        assert!(parse("tomorrow", now).is_err());
    }
}
