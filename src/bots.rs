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
        "# {name}\n\n## Purpose\n\n{purpose}\n\n## How I work\n\nTODO: write the steps I \
         follow, once I've done the job for the first time.\n\n## Changelog\n"
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
        let hidden = e.file_name().to_string_lossy().starts_with('.');
        if path.is_dir() {
            if !hidden {
                collect(root, &path, out);
            }
        } else if path.extension().is_some_and(|x| x == "md")
            && let Ok(rel) = path.strip_prefix(root)
        {
            let body = std::fs::read_to_string(&path).unwrap_or_default();
            out.push((rel.to_string_lossy().into_owned(), body));
        }
    }
}

/// The opencode agent every run uses, written into each bot's folder.
pub const AGENT: &str = "dumbot";

/// Readies a bot's folder for a run. opencode only loads a folder's AGENTS.md and
/// `.opencode/agents/` when the folder is a project (a git repository), so it gets one; and the
/// `dumbot` agent is (re)written, so it always matches this version of dumBot.
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
    let agents = d.join(".opencode/agents");
    std::fs::create_dir_all(&agents)?;
    std::fs::write(agents.join(format!("{AGENT}.md")), agent_md(slug, bot))?;
    Ok(())
}

fn exe() -> String {
    crate::appbundle::target_binary()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "dumbot".into())
}

/// The agent's frontmatter (permissions: its own folder and `dumbot wake` need no approval)
/// and system prompt: how a bot works, run after run.
fn agent_md(slug: &str, bot: &Bot) -> String {
    let dir = dir(slug).display().to_string();
    let exe = exe();
    format!(
        r###"---
description: A dumBot bot. Written by dumBot before every run; edits are overwritten.
mode: primary
color: "#ffcc66"
permissions:
  - action: edit
    resource: "{dir}/**"
    effect: allow
  - action: shell
    resource: "{exe} wake *"
    effect: allow
  - action: shell
    resource: "dumbot wake *"
    effect: allow
---

You are "{name}", a bot that dumBot runs on the user's computer through opencode. You live in
the folder {dir}. You run in short, separate sessions: nothing carries over between runs except
the files in your folder, so they are your memory and your self.

# Your files

- AGENTS.md is who you are: your purpose and your operating manual. It is already loaded in
  your instructions; you don't need to read it. You own it and you keep rewriting it.
- memory/ holds everything else you keep: notes, logs, to-do lists, what you've already seen or
  handled. Create, reorganize and prune markdown files there. Prefer a few well-named files.

# How you run

1. Read the memory/ files this run needs.
2. Do the work your purpose calls for. Use the browse MCP tools for anything in a web browser
   (reuse the user's logged-in browser; close tabs you open). Use the shell and other tools
   for anything on the computer.
3. Keep AGENTS.md current while you work, not only at the end. The moment you learn something
   the next run should know, edit AGENTS.md: a better way to do a step, a dead end to avoid,
   a URL, page layout or command that works, the user's preferences, a sharper understanding of
   your purpose. Rewrite and reorganize rather than append; delete what's no longer true.
4. Before you finish, review yourself. This step is required every run, never skipped:
   - Compare AGENTS.md with what you actually did this run. "## How I work" must describe,
     step by step, how you really do the job now, concretely enough that the next run can
     follow it without rediscovering anything (exact paths, commands, URLs, what to check).
     If it's missing, vague, a placeholder, or out of date, rewrite it now.
   - Add one line to "## Changelog" at the bottom of AGENTS.md (create it if missing):
     `- YYYY-MM-DD HH:MM: <what you changed in yourself and why>`, or
     `- YYYY-MM-DD HH:MM: no change: <why the manual was already right>`. Keep the last 20.
   - Update memory/ with what you did and what's pending.
5. Schedule your next run. Always do this, as your last action:
     {exe} wake {slug} in 30m
   Other forms: in 2h, in 1d, at 09:00, at 2026-12-01 09:00, every 6h, off. Add
   --note "what to do next time" to leave your next run a note. Pick the soonest time that's
   actually useful; don't wake just to find nothing to do.
6. End with a short summary for the user: what you did, how you changed, what you need from
   them (if anything), and when you'll run next.

# Rules

- Serve the purpose the user wrote under "## Purpose". You may clarify and refine it as you
  learn, but never drop or replace what the user asked for.
- Stay inside your folder unless your purpose needs files elsewhere.
- If you're blocked on the user (a login, 2FA, a decision), say so in your summary, write it
  to memory/, and schedule your next run for when they've likely handled it.
- Don't take irreversible or outward-facing actions (sending messages, buying, deleting
  things that aren't yours) unless your purpose clearly calls for them.
- Be concise in everything you write.
"###,
        name = bot.name,
    )
}

/// The first prompt of a run: why it's running, the note its last run left, and the time.
/// Its first line is shown in place of the whole thing.
pub fn preamble(bot: &Bot, reason: &str) -> String {
    let note = bot
        .note
        .as_ref()
        .map(|n| format!("\nYour last run left you this note: {n}\n"))
        .unwrap_or_default();
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
    message: Option<&str>,
    default: Option<ModelRef>,
) -> Result<String> {
    prepare(slug, bot)?;
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
    // The note was for this run; it's read now.
    update(slug, |b| b.note = None)?;
    let mut text = preamble(bot, reason);
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
