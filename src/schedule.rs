//! When bots wake. A bot can have, all at once: a one-off wake-up (`in 2h`, `at 09:00`), an
//! interval (`every 6h`) and weekly rules (`on wed,thu at 19:00`). It wakes at the earliest.

use anyhow::{Result, anyhow};
use chrono::{Datelike, Duration, Local, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};

use crate::bots::{Bot, Rule};

const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

#[derive(Debug, Clone, PartialEq)]
pub enum Schedule {
    /// Once, at this Unix time.
    At(i64),
    /// Every this many seconds, first one from now.
    Every(i64),
    /// Weekly rules to add.
    Weekly(Vec<Rule>),
    Off,
}

/// `now`, `in 30m`, `in 1h30m`, `at 09:00`, `at 2026-12-01 09:00`, `every 6h`,
/// `on wed,thu at 19:00`, `on weekends at 10:00,18:00`, `off`.
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
        "on" => Ok(Schedule::Weekly(weekly(rest)?)),
        _ => Err(anyhow!(
            "can't read {input:?}: use now, in 30m, at 09:00, at 2026-12-01 09:00, every 6h, \
             on wed,thu at 19:00, or off"
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

/// `wed,thu at 19:00`, `sat sun at 10:00,18:00`, `weekdays at 08:30`, `daily at 07:00`: one
/// rule per time.
fn weekly(s: &str) -> Result<Vec<Rule>> {
    let bad = || anyhow!("can't read {s:?}: use e.g. on wed,thu at 19:00 or on weekends at 10:00");
    let (days, times) = s.split_once(" at ").ok_or_else(bad)?;
    let mut picked = Vec::new();
    for word in days.split([',', ' ']).filter(|w| !w.is_empty()) {
        let add: Vec<u8> = match word {
            "daily" | "everyday" => (0..7).collect(),
            "weekdays" => (0..5).collect(),
            "weekends" => vec![5, 6],
            w => {
                let i = DAYS
                    .iter()
                    .position(|d| w.len() >= 3 && d.starts_with(&w[..3]))
                    .ok_or_else(bad)?;
                vec![i as u8]
            }
        };
        picked.extend(add);
    }
    picked.sort();
    picked.dedup();
    if picked.is_empty() {
        return Err(bad());
    }
    times
        .split([',', ' '])
        .filter(|t| !t.is_empty())
        .map(|t| {
            let t = NaiveTime::parse_from_str(t, "%H:%M").map_err(|_| bad())?;
            Ok(Rule {
                days: picked.clone(),
                time: t.format("%H:%M").to_string(),
                note: None,
            })
        })
        .collect()
}

/// `note` goes with what's scheduled: the rules being added, or the one-off or interval.
/// Adding a rule that exists (same days and time) replaces its note.
pub fn apply(bot: &mut Bot, schedule: Schedule, note: Option<String>, now: i64) {
    match schedule {
        Schedule::At(t) => {
            bot.next_run = Some(t);
            bot.note = note;
        }
        Schedule::Every(secs) => {
            bot.next_run = Some(now + secs);
            bot.every = Some(secs);
            bot.note = note;
        }
        Schedule::Weekly(rules) => {
            for mut r in rules {
                r.note = note.clone();
                match bot
                    .rules
                    .iter_mut()
                    .find(|x| x.days == r.days && x.time == r.time)
                {
                    Some(x) => x.note = r.note,
                    None => bot.rules.push(r),
                }
            }
            // Only times from now on: a new rule doesn't fire for earlier ones.
            bot.last_wake = Some(now);
        }
        Schedule::Off => {
            bot.next_run = None;
            bot.every = None;
            bot.rules.clear();
            bot.note = None;
        }
    }
}

/// When the bot next wakes: the earliest of its one-off or interval time and its rules' next
/// times after its last wake-up. A rule time missed while the app was closed is in the past,
/// so it's due now (once, however many were missed).
pub fn next(bot: &Bot, now: i64) -> Option<i64> {
    let after = bot.last_wake.unwrap_or(now);
    bot.rules
        .iter()
        .filter_map(|r| rule_next(r, after))
        .chain(bot.next_run)
        .min()
}

/// A rule's first time after `after`.
fn rule_next(r: &Rule, after: i64) -> Option<i64> {
    let time = NaiveTime::parse_from_str(&r.time, "%H:%M").ok()?;
    let start = Local.timestamp_opt(after, 0).single()?.date_naive();
    (0..=7)
        .map(|d| start + Duration::days(d))
        .filter(|day| {
            r.days
                .contains(&(day.weekday().num_days_from_monday() as u8))
        })
        .filter_map(|day| Local.from_local_datetime(&day.and_time(time)).earliest())
        .map(|t| t.timestamp())
        .find(|t| *t > after)
}

/// Records a scheduled wake-up at `now`, returning the notes of what fired: rules move on to
/// their next times, and a one-off (or interval tick) that was due is used up, the interval
/// scheduling its next tick.
pub fn fired(bot: &mut Bot, now: i64) -> Vec<String> {
    let after = bot.last_wake.unwrap_or(now);
    let mut notes: Vec<String> = bot
        .rules
        .iter()
        .filter(|r| rule_next(r, after).is_some_and(|t| t <= now))
        .filter_map(|r| Some(format!("{}: {}", rule(r), r.note.as_ref()?)))
        .collect();
    bot.last_wake = Some(now);
    if bot.next_run.is_some_and(|t| t <= now) {
        bot.next_run = bot.every.map(|e| now + e);
        let note = if bot.every.is_some() {
            bot.note.clone()
        } else {
            bot.note.take()
        };
        notes.extend(note);
    }
    notes
}

/// Each wake-up and its note, one per line, for `dumbot list`.
pub fn details(bot: &Bot, now: i64) -> Vec<String> {
    let note = |n: &Option<String>| n.as_ref().map_or(String::new(), |n| format!(": {n}"));
    let mut out = Vec::new();
    match (bot.next_run, bot.every) {
        (Some(t), None) => out.push(format!("once {}{}", when(t, now), note(&bot.note))),
        (Some(t), Some(e)) => out.push(format!(
            "every {}, next {}{}",
            span(e),
            when(t, now),
            note(&bot.note)
        )),
        _ => {}
    }
    for r in &bot.rules {
        out.push(format!("{}{}", rule(r), note(&r.note)));
    }
    out
}

/// `Wed, Thu 19:00`, `Weekdays 08:30`, `Daily 07:00`.
pub fn rule(r: &Rule) -> String {
    let days = match r.days.as_slice() {
        [0, 1, 2, 3, 4, 5, 6] => "Daily".to_string(),
        [0, 1, 2, 3, 4] => "Weekdays".to_string(),
        [5, 6] => "Weekends".to_string(),
        d => d
            .iter()
            .filter_map(|i| DAYS.get(*i as usize))
            .map(|n| {
                let mut c = n.chars();
                c.next()
                    .map_or(String::new(), |f| f.to_uppercase().chain(c).collect())
            })
            .collect::<Vec<_>>()
            .join(", "),
    };
    format!("{days} {}", r.time)
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

/// When the bot wakes next: `Next: Today 14:30`, `Due now`, `Not scheduled`.
pub fn next_line(bot: &Bot, now: i64) -> String {
    match next(bot, now) {
        None => "Not scheduled".into(),
        Some(t) if t <= now => "Due now".into(),
        Some(t) => format!("Next: {}", when(t, now)),
    }
}

/// A bot's whole schedule: `Next: Today 14:30 · every 2h · Wed, Thu 19:00`.
pub fn describe(bot: &Bot, now: i64) -> String {
    let mut out = next_line(bot, now);
    match (bot.next_run, bot.every) {
        (Some(t), None) if Some(t) != next(bot, now) => {
            out.push_str(&format!(" · once {}", when(t, now)))
        }
        (_, Some(e)) => out.push_str(&format!(" · every {}", span(e))),
        _ => {}
    }
    for r in &bot.rules {
        out.push_str(&format!(" · {}", rule(r)));
    }
    out
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
        let Schedule::Weekly(rules) = parse("on wed, Thursday at 19:00,21:30", now).unwrap() else {
            panic!()
        };
        assert_eq!(rules.len(), 2);
        assert_eq!(rule(&rules[0]), "Wed, Thu 19:00");
        assert!(parse("on someday at 19:00", now).is_err());
        assert!(parse("on wed at 7pm", now).is_err());
    }

    /// Wed/Thu evenings plus weekend mornings: wakes at each in turn, and a time missed while
    /// the app was closed is due once.
    #[test]
    fn weekly_rules_wake_in_turn() {
        let local = |s: &str| {
            Local
                .from_local_datetime(&NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap())
                .unwrap()
                .timestamp()
        };
        // 2026-11-23 is a Monday.
        let mut bot = Bot::default();
        let now = local("2026-11-23 12:00");
        for s in ["on wed,thu at 19:00", "on sat,sun at 10:00"] {
            apply(&mut bot, parse(s, now).unwrap(), None, now);
        }
        assert_eq!(next(&bot, now), Some(local("2026-11-25 19:00")));
        let wed = local("2026-11-25 19:00");
        assert!(bot.due(wed));
        assert!(fired(&mut bot, wed).is_empty());
        assert_eq!(next(&bot, wed), Some(local("2026-11-26 19:00")));
        fired(&mut bot, local("2026-11-26 19:00"));
        assert_eq!(next(&bot, wed), Some(local("2026-11-28 10:00")));
        // Closed all weekend: Saturday's and Sunday's are missed, and it's due once on Monday.
        let monday = local("2026-11-30 09:00");
        assert!(bot.due(monday));
        fired(&mut bot, monday);
        assert_eq!(next(&bot, monday), Some(local("2026-12-02 19:00")));
        // A one-off sooner than the rules wins, and is used up when it fires.
        apply(
            &mut bot,
            parse("in 2h", monday).unwrap(),
            Some("extra look".into()),
            monday,
        );
        assert_eq!(next(&bot, monday), Some(monday + 7200));
        assert_eq!(fired(&mut bot, monday + 7200), ["extra look"]);
        assert_eq!((bot.next_run, bot.note.as_ref()), (None, None));
        // Each rule keeps its own note, and a wake-up gets the notes of what fired.
        let note = |n: &str| Some(n.to_string());
        apply(
            &mut bot,
            parse("on wed at 19:00", monday).unwrap(),
            note("lineup"),
            monday,
        );
        apply(
            &mut bot,
            parse("on sat at 10:00", monday).unwrap(),
            note("waivers"),
            monday,
        );
        assert_eq!(
            fired(&mut bot, local("2026-12-02 19:00")),
            ["Wed 19:00: lineup"]
        );
        apply(&mut bot, Schedule::Off, None, monday);
        assert_eq!(next(&bot, monday), None);
    }
}
