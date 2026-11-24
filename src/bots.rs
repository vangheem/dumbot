//! Bots on disk. Each is a folder, `~/.local/share/dumbot/bots/<slug>/`, holding:
//!
//! - `AGENTS.md`: the bot's identity and instructions. Runs use the folder as their working
//!   directory, so opencode loads it into every session, and the bot rewrites it as it changes.
//! - `memory/`: any other markdown state the bot keeps.
//! - `.opencode/`: the bot's own skills and subagents, plus dumBot's (see `MANAGED`).
//! - `bot.json`: dumBot's record of the bot's model, schedule and runs. Bots change their
//!   schedule through `dumbot wake`, never by editing it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::opencode::Client;
use crate::store::{self, ModelRef};

/// How many past runs a bot remembers.
const MAX_RUNS: usize = 50;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Bot {
    pub name: String,
    /// Overrides the app's default model.
    #[serde(default)]
    pub model: Option<ModelRef>,
    /// A one-off wake-up, or the next tick of `every`, in Unix seconds.
    #[serde(default)]
    pub next_run: Option<i64>,
    /// After each scheduled wake-up, wake again this many seconds later.
    #[serde(default)]
    pub every: Option<i64>,
    /// Weekly wake-ups (`on wed,thu at 19:00`).
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// The last scheduled wake-up (or when the rules last changed): rules fire for times after it.
    #[serde(default)]
    pub last_wake: Option<i64>,
    /// The note for the one-off (or interval) wake-up: what that run should do.
    #[serde(default)]
    pub note: Option<String>,
    /// Oldest first.
    #[serde(default)]
    pub runs: Vec<Run>,
}

/// A weekly wake-up: these days at this local time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    /// 0 is Monday.
    pub days: Vec<u8>,
    /// `HH:MM`, local time.
    pub time: String,
    /// What runs woken by this rule should do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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
        crate::schedule::next(self, now).is_some_and(|t| t <= now)
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
///
/// Holds a lock on the bot meanwhile: a bot may run several `dumbot wake`s at once.
pub fn update(slug: &str, f: impl FnOnce(&mut Bot)) -> Result<Bot> {
    let lock = std::fs::File::create(dir(slug).join("bot.json.lock"))
        .with_context(|| format!("no bot {slug:?}"))?;
    lock.lock()?;
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
    include_str!("prompts/starting-agents.md")
        .replace("{name}", name)
        .replace("{purpose}", purpose)
}

pub fn delete(slug: &str) -> Result<()> {
    let d = dir(slug);
    std::fs::remove_dir_all(&d).with_context(|| format!("remove {}", d.display()))
}

/// The bot's markdown files as `(relative path, contents)`: AGENTS.md first, then the rest
/// by path, including the skills and subagents it wrote under `.opencode/` (but not dumBot's).
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
        let name = e.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if name != ".git" {
                collect(root, &path, out);
            }
        } else if path.extension().is_some_and(|x| x == "md")
            && let Ok(rel) = path.strip_prefix(root)
        {
            let rel = rel.to_string_lossy().into_owned();
            if !managed(&rel) {
                let body = std::fs::read_to_string(&path).unwrap_or_default();
                out.push((rel, body));
            }
        }
    }
}

/// The opencode agent every run uses, written into each bot's folder.
pub const AGENT: &str = "dumbot";

/// What dumBot writes into every bot's folder before each run, by path: the bot's agent (its
/// operating loop and permissions), a worker subagent for parallel tasks, and the skill for
/// evolving itself. They're dumBot's, overwritten every run, so they match this version.
const MANAGED: &[(&str, &str)] = &[
    (
        ".opencode/agents/dumbot.md",
        include_str!("prompts/agent.md"),
    ),
    (
        ".opencode/agents/dumbot-worker.md",
        include_str!("prompts/worker.md"),
    ),
    (
        ".opencode/skills/dumbot-evolve/SKILL.md",
        include_str!("prompts/evolve.md"),
    ),
];

fn managed(rel: &str) -> bool {
    MANAGED.iter().any(|(p, _)| *p == rel)
}

/// Readies a bot's folder for a run. opencode only loads a folder's AGENTS.md and
/// `.opencode/` agents and skills when the folder is a project (a git repository), so it
/// gets one; and dumBot's files are (re)written.
pub fn prepare(slug: &str, bot: &Bot) -> Result<()> {
    let d = dir(slug);
    if !d.join(".git").exists() {
        let out = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&d)
            .output()
            .context("run git init in the bot's folder")?;
        if !out.status.success() {
            return Err(anyhow!(
                "git init failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
    }
    std::fs::create_dir_all(d.join("memory/work"))?;
    let (dir, exe) = (d.display().to_string(), exe());
    for (rel, template) in MANAGED {
        let path = d.join(rel);
        std::fs::create_dir_all(path.parent().unwrap())?;
        let body = template
            .replace("{name}", &bot.name)
            .replace("{dir}", &dir)
            .replace("{exe}", &exe)
            .replace("{slug}", slug);
        std::fs::write(path, body)?;
    }
    Ok(())
}

fn exe() -> String {
    crate::appbundle::target_binary()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "dumbot".into())
}

/// The first prompt of a run: why it's running, the notes left for the wake-ups that woke
/// it, and the time. Its first line is shown in place of the whole thing.
pub fn preamble(reason: &str, notes: &[String]) -> String {
    let note = if notes.is_empty() {
        String::new()
    } else {
        let list: Vec<String> = notes.iter().map(|n| format!("- {n}")).collect();
        format!("\nNotes you left for this run:\n{}\n", list.join("\n"))
    };
    let now = chrono::Local::now().format("%A %Y-%m-%d %H:%M %Z");
    format!("{RUN_PREFIX}{reason}\n{note}\nIt is now {now}. Begin your run.")
}

/// Starts a run of a bot in a fresh session of the `dumbot` agent, with `message` from the
/// user if any, and returns the session id. The bot's own model wins over `default`.
pub fn start_run(
    oc: &Client,
    slug: &str,
    bot: &Bot,
    reason: &str,
    notes: &[String],
    message: Option<&str>,
    default: Option<ModelRef>,
) -> Result<String> {
    prepare(slug, bot)?;
    oc.await_agent(&dir(slug), AGENT)?;
    let model = bot.model.clone().or(default);
    let title = format!("{}: {reason}", bot.name);
    let id = oc.create_session(&title, &dir(slug), AGENT, model.as_ref())?;
    add_run(
        slug,
        Run {
            session: id.clone(),
            started: now(),
            reason: reason.into(),
        },
    )?;
    let mut text = preamble(reason, notes);
    if let Some(m) = message {
        text = format!("{text}\n\n{MESSAGE_MARK}{m}");
    }
    oc.prompt(&id, &text)?;
    Ok(id)
}

/// Separates a user's message from the preamble in a run's first prompt.
pub const MESSAGE_MARK: &str = "Message from the user:\n";

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
