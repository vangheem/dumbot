//! Commands bots (and people) run: `dumbot wake <bot> <when> [--note TEXT]` and
//! `dumbot list`. They change `bot.json` directly; the running app notices within seconds.

use anyhow::{Result, anyhow};

use crate::{bots, schedule};

/// Returns false for anything that isn't one of these commands, so the app runs.
pub fn run(args: &[String]) -> bool {
    let result = match args.get(1).map(String::as_str) {
        Some("wake") => wake(&args[2..]),
        Some("list") => list(),
        Some("help" | "--help" | "-h") => {
            println!("{USAGE}");
            Ok(())
        }
        _ => return false,
    };
    if let Err(e) = result {
        eprintln!("dumbot: {e:#}");
        std::process::exit(1);
    }
    true
}

const USAGE: &str = "dumBot: bots that rewrite themselves, run through opencode.

  dumbot                         open the app
  dumbot list                    list bots and when they wake
  dumbot wake <bot> <when> [--note TEXT]
        <when>: now | in 30m | in 1h30m | at 09:00 | at 2026-12-01 09:00 | every 6h | off
  dumbot install-app             add dumbot.app to ~/Applications (macOS)
  dumbot uninstall-app";

fn wake(args: &[String]) -> Result<()> {
    let mut words = Vec::new();
    let mut note = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--note" | "-n" => {
                note = Some(
                    it.next()
                        .ok_or_else(|| anyhow!("--note needs text"))?
                        .clone(),
                )
            }
            _ => words.push(a.as_str()),
        }
    }
    let (bot, when) = words
        .split_first()
        .ok_or_else(|| anyhow!("usage: dumbot wake <bot> <when> [--note TEXT]"))?;
    let slug = bots::slugify(bot);
    let now = bots::now();
    let sched = schedule::parse(&when.join(" "), now)?;
    let b = bots::update(&slug, |b| schedule::apply(b, sched, note, now))?;
    println!("{}: {}", b.name, schedule::describe(&b, now));
    Ok(())
}

fn list() -> Result<()> {
    let now = bots::now();
    for (slug, b) in bots::list() {
        println!("{slug}\t{}", schedule::describe(&b, now));
    }
    Ok(())
}
