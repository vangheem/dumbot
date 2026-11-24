//! Bots on disk. Each is a folder, `~/.local/share/dumbot/bots/<slug>/`, holding:
//!
//! - `AGENTS.md`: the bot's identity and instructions. Runs use the folder as their working
//!   directory, so opencode loads it into every session, and the bot rewrites it as it changes.
//! - `memory/`: any other markdown state the bot keeps.
//! - `bot.json`: dumBot's record of the bot's model, schedule and runs. Bots change their
//!   schedule through `dumbot wake`, never by editing it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::store::{self, ModelRef};

/// How many past runs a bot remembers.
const MAX_RUNS: usize = 50;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Bot {
    pub name: String,
    /// Overrides the app's default model.
    #[serde(default)]
    pub model: Option<ModelRef>,
    /// When it next wakes, in Unix seconds.
    #[serde(default)]
    pub next_run: Option<i64>,
    /// After each scheduled wake-up, wake again this many seconds later.
    #[serde(default)]
    pub every: Option<i64>,
    /// What the bot asked its next run to do.
    #[serde(default)]
    pub note: Option<String>,
    /// Oldest first.
    #[serde(default)]
    pub runs: Vec<Run>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    /// The opencode session.
    pub session: String,
    pub started: i64,
    pub reason: String,
}

impl Bot {
    pub fn due(&self, now: i64) -> bool {
        self.next_run.is_some_and(|t| t <= now)
    }

    pub fn last_session(&self) -> Option<&str> {
        self.runs.last().map(|r| r.session.as_str())
    }
}

pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn bots_dir() -> PathBuf {
    store::data_dir().join("bots")
}

pub fn dir(slug: &str) -> PathBuf {
    bots_dir().join(slug)
}

fn config_path(slug: &str) -> PathBuf {
    dir(slug).join("bot.json")
}

/// Every bot, by slug.
pub fn list() -> BTreeMap<String, Bot> {
    let Ok(entries) = std::fs::read_dir(bots_dir()) else {
        return BTreeMap::new();
    };
    entries
        .flatten()
        .filter_map(|e| {
            let slug = e.file_name().to_str()?.to_string();
            Some((slug.clone(), load(&slug).ok()?))
        })
        .collect()
}

pub fn load(slug: &str) -> Result<Bot> {
    let raw = std::fs::read(config_path(slug)).with_context(|| format!("no bot {slug:?}"))?;
    serde_json::from_slice(&raw).with_context(|| format!("read {slug}/bot.json"))
}

pub fn save(slug: &str, bot: &Bot) -> Result<()> {
    let path = config_path(slug);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(bot)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// Re-reads the bot, changes it and writes it back, so changes made meanwhile by
/// `dumbot wake` (from a bot's run) aren't lost.
pub fn update(slug: &str, f: impl FnOnce(&mut Bot)) -> Result<Bot> {
    let mut bot = load(slug)?;
    f(&mut bot);
    save(slug, &bot)?;
    Ok(bot)
}

pub fn add_run(slug: &str, run: Run) -> Result<Bot> {
    update(slug, |b| {
        b.runs.push(run);
        let extra = b.runs.len().saturating_sub(MAX_RUNS);
        b.runs.drain(..extra);
    })
}

pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Makes a bot's folder with a starting AGENTS.md, returning its slug.
pub fn create(name: &str, purpose: &str) -> Result<String> {
    let name = name.trim();
    let slug = slugify(name);
    if slug.is_empty() {
        return Err(anyhow!("give the bot a name"));
    }
    let d = dir(&slug);
    if d.exists() {
        return Err(anyhow!("there's already a bot called {slug}"));
    }
    std::fs::create_dir_all(d.join("memory")).context("create the bot's folder")?;
    std::fs::write(
        d.join("AGENTS.md"),
        starting_agents_md(name, purpose.trim()),
    )?;
    save(
        &slug,
        &Bot {
            name: name.into(),
            ..Bot::default()
        },
    )?;
    Ok(slug)
}

fn starting_agents_md(name: &str, purpose: &str) -> String {
    format!(
        "# {name}\n\n## Purpose\n\n{purpose}\n\n## How I work\n\nNot worked out yet. Rewrite \
         this section as I learn how to do the job well.\n"
    )
}

pub fn delete(slug: &str) -> Result<()> {
    let d = dir(slug);
    std::fs::remove_dir_all(&d).with_context(|| format!("remove {}", d.display()))
}

/// The bot's markdown files as `(relative path, contents)`: AGENTS.md first, then the rest
/// by path.
pub fn files(slug: &str) -> Vec<(String, String)> {
    let root = dir(slug);
    let mut out = Vec::new();
    collect(&root, &root, &mut out);
    out.sort_by(|a, b| (a.0 != "AGENTS.md", &a.0).cmp(&(b.0 != "AGENTS.md", &b.0)));
    out
}

fn collect(root: &Path, at: &Path, out: &mut Vec<(String, String)>) {
    let Ok(entries) = std::fs::read_dir(at) else {
        return;
    };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            collect(root, &path, out);
        } else if path.extension().is_some_and(|x| x == "md")
            && let Ok(rel) = path.strip_prefix(root)
        {
            let body = std::fs::read_to_string(&path).unwrap_or_default();
            out.push((rel.to_string_lossy().into_owned(), body));
        }
    }
}

/// The first prompt of every run: who the bot is, where its state lives, and how it
/// schedules its next run. Its first line is shown in place of the whole thing.
pub fn preamble(slug: &str, bot: &Bot, reason: &str) -> String {
    let exe = crate::appbundle::target_binary()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "dumbot".into());
    let dir = dir(slug);
    let now = chrono::Local::now().format("%A %Y-%m-%d %H:%M %Z");
    let note = bot
        .note
        .as_ref()
        .map(|n| format!("\nYour last run left this note for you: {n}\n"))
        .unwrap_or_default();
    format!(
        "{RUN_PREFIX}{reason}

You are \"{name}\", a bot that dumBot runs on the user's computer through opencode. Nothing \
carries over between runs except the files in your folder, {dir}:

- AGENTS.md: who you are, your purpose and how you work. It is loaded into every run. \
Rewrite it whenever you learn something about doing your job better; keep it concise, current \
and true. You may evolve, but keep serving the purpose the user gave you.
- memory/: everything else you keep (notes, logs, to-do lists, things already seen or done). \
Create, edit and prune markdown files there. Prefer a few well-organized files.

Write down what your next run needs to know before you finish.

Use the browse MCP tools for anything in a web browser.

When you're done, decide when you should run next and schedule it:
  {exe} wake {slug} in 30m
Other forms: in 2h, in 1d, at 09:00, at 2026-12-01 09:00, every 6h, off. Add \
--note \"what to do next time\" to leave your next run a note. Without a schedule you won't \
run again until the user starts you.
{note}
It is now {now}.",
        name = bot.name,
        dir = dir.display(),
    )
}

pub const RUN_PREFIX: &str = "dumBot run: ";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slugify("  Inbox Zero!  bot "), "inbox-zero-bot");
        assert_eq!(slugify("!!"), "");
    }
}
