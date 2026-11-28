//! Commands bots (and people) run: `dumbot wake <bot> <when> [--note TEXT]` and
//! `dumbot list`. They change `bot.json` directly; the running app notices within seconds.

use anyhow::{Context, Result, anyhow};

use crate::{bots, container, schedule, store};

/// Returns false for anything that isn't one of these commands, so the app runs.
pub fn run(args: &[String]) -> bool {
    let result = match args.get(1).map(String::as_str) {
        Some("wake") => wake(&args[2..]),
        Some("list") => list(),
        Some("run") => run_bot(&args[2..]),
        Some("doctor") => doctor(),
        Some("server") => crate::server::cli(&args[2..]),
        Some("mcp") => crate::mcp::cli(&args[2..]),
        Some("install-server") => crate::server::install(&args[2..]),
        Some("uninstall-server") => crate::server::uninstall(),
        Some("connect") => connect(&args[2..]),
        Some("disconnect") => disconnect(),
        // The shell wrapper bots' commands run through (see `env`).
        Some("shell") => match crate::env::run_shell(&args[2..]) {
            Ok(code) => std::process::exit(code),
            Err(e) => Err(e),
        },
        Some("help" | "--help" | "-h") => {
            println!(
                "{USAGE}\n\nRunning bots on another machine:\n{}",
                crate::server::USAGE
            );
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
  dumbot run <bot> [message]     start a run now (in its container) and print its session id
  dumbot doctor                  check Docker, the bot image and each bot's container
  dumbot wake <bot> <when> [--note TEXT]
        <when>: now | in 30m | in 1h30m | at 09:00 | at 2026-12-01 09:00 | every 6h
                | on wed,thu at 19:00 | on weekends at 10:00,18:00 | off
        Weekly rules add up; in/at add a one-off wake-up; off clears everything.
  dumbot mcp <bot> add|remove|list   MCP servers a bot installs for itself (`dumbot mcp x` for help)
  dumbot connect <host[:port]>   manage the bots on a dumBot server (pairs on first use)
  dumbot disconnect              manage this computer's bots again
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

fn run_bot(args: &[String]) -> Result<()> {
    let bot = args
        .first()
        .ok_or_else(|| anyhow!("usage: dumbot run <bot> [message]"))?;
    let slug = bots::slugify(bot);
    let b = bots::load(&slug)?;
    let message = (args.len() > 1).then(|| args[1..].join(" "));
    let id = bots::start_run(
        &slug,
        &b,
        "started from the command line",
        &[],
        message.as_deref(),
        store::Store::load().model,
        &|s| eprintln!("{s}…"),
    )?;
    println!("{id}");
    Ok(())
}

fn doctor() -> Result<()> {
    if let Ok(v) = container::check_docker() {
        println!("Docker {v}");
    }
    let issues = container::doctor(&bots::list());
    if issues.is_empty() {
        println!("No issues.");
    }
    for i in &issues {
        let who = i.bot.as_deref().unwrap_or("all bots");
        let kind = if i.warning { "warning" } else { "problem" };
        println!(
            "{kind} ({who}): {}
    fix: {}",
            i.problem, i.fix
        );
    }
    if issues.iter().any(|i| !i.warning) {
        std::process::exit(1);
    }
    Ok(())
}

fn list() -> Result<()> {
    let now = bots::now();
    for (slug, b) in bots::list() {
        println!("{slug}\t{}", schedule::next_line(&b, now));
        for line in schedule::details(&b, now) {
            println!("    {line}");
        }
    }
    Ok(())
}

/// Pairs with a server (waiting for `dumbot server approve` there) and makes the app use it.
fn connect(args: &[String]) -> Result<()> {
    let addr = crate::remote::normalize_addr(
        args.first()
            .ok_or_else(|| anyhow!("usage: dumbot connect <host[:port]>"))?,
    );
    // A new address for the server this app is paired with keeps its key (see the app's).
    let mut server = crate::remote::Server {
        addr: addr.clone(),
        fingerprint: store::Store::load().remote.and_then(|r| r.fingerprint),
    };
    let mut shown = false;
    loop {
        let (pinned, code) = server.pair()?;
        server = pinned;
        let Some(code) = code else { break };
        if !shown {
            shown = true;
            println!(
                "On {}, run:\n\n    dumbot server approve {code}\n\nServer fingerprint: {}\nWaiting…",
                addr.rsplit_once(':').map_or(addr.as_str(), |x| x.0),
                server.fingerprint.as_deref().unwrap_or_default()
            );
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    let backend = crate::api::Backend::Remote(server.clone());
    backend
        .call::<()>(crate::api::Request::SyncContext {
            context: crate::context::gather(),
        })
        .context("send your opencode config to the server")?;
    let mut st = store::Store::load();
    st.remote = Some(server);
    st.save()?;
    println!("Connected to {addr}. The app manages the bots there now.");
    let local = bots::list();
    if !local.is_empty() {
        println!(
            "This computer has {} bot(s); the app offers to move them to the server.",
            local.len()
        );
    }
    Ok(())
}

fn disconnect() -> Result<()> {
    let mut st = store::Store::load();
    st.remote = None;
    st.save()?;
    println!("The app manages this computer's bots again.");
    Ok(())
}
