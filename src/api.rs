//! Everything the app does to bots, as requests: run in-process when bots run on this
//! computer, or sent to a dumBot server (see `remote` and `server`). Either way `handle` does
//! the work, on the machine the bots live on.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::bots::{self, Bot, Sandbox};
use crate::context::HostContext;
use crate::opencode::{Client, Snapshot};
use crate::remote::{Reply, Server};
use crate::schedule::{self, Item, Schedule};
use crate::store::{self, ModelRef, Store};
use crate::{container, env, mcp};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // short-lived, and mostly sent as JSON
pub enum Request {
    /// Pairing, or checking it.
    Hello {
        #[serde(default)]
        name: String,
    },
    Status,
    Files {
        slug: String,
    },
    Create {
        name: String,
        purpose: String,
    },
    Delete {
        slug: String,
    },
    UpdateBot {
        slug: String,
        change: Change,
    },
    Instructions,
    SaveInstructions {
        text: String,
    },
    /// Variables' names (values are never sent back).
    Env,
    /// `value: None` removes the variable.
    SetEnv {
        bot: Option<String>,
        name: String,
        value: Option<String>,
    },
    BotMcp,
    /// The MCP servers a bot installed for itself.
    InstalledMcp {
        slug: String,
    },
    SaveBotMcp {
        bot: Option<String>,
        servers: mcp::Servers,
    },
    SetDefaultModel {
        model: Option<ModelRef>,
    },
    StartRun {
        slug: String,
        reason: String,
        notes: Vec<String>,
        message: Option<String>,
    },
    Snapshot {
        slug: String,
        session: String,
    },
    Session {
        slug: String,
        session: String,
        action: SessionAction,
    },
    Models,
    Doctor,
    SyncContext {
        context: HostContext,
    },
    /// Files for a bot to use, into its `files/` (relative path, base64).
    Upload {
        slug: String,
        files: Vec<(String, String)>,
    },
    /// A bot moved here from another computer: its files (relative path, base64), record and
    /// variables.
    Import {
        slug: String,
        bot: Bot,
        files: Vec<(String, String)>,
        env: BTreeMap<String, String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Change {
    Model(Option<ModelRef>),
    Schedule {
        schedule: Schedule,
        note: Option<String>,
        now: i64,
    },
    RemoveWakeup(Item),
    Sandbox(Sandbox),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionAction {
    Prompt { text: String },
    Interrupt,
    Permission { request: String, decision: String },
    SetModel { model: ModelRef },
}

/// The bots and what their containers are doing.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    pub bots: BTreeMap<String, Bot>,
    /// Each bot's container state (`running`, `paused`, …).
    pub states: HashMap<String, String>,
    /// Sessions being worked in.
    pub active: HashSet<String>,
    /// Bots whose run is being started here, with the step it's on.
    pub starting: BTreeMap<String, String>,
    pub default_model: Option<ModelRef>,
    /// The server browser, on a server.
    #[serde(default)]
    pub browser: Option<crate::browser::Info>,
}

pub type Step<'a> = &'a (dyn Fn(&str) + Sync);

/// Where the app's bots live.
#[derive(Clone, Debug, PartialEq)]
pub enum Backend {
    Local,
    Remote(Server),
}

impl Backend {
    pub fn is_remote(&self) -> bool {
        matches!(self, Backend::Remote(_))
    }

    pub fn call<T: DeserializeOwned>(&self, req: Request) -> Result<T> {
        self.call_with(req, &|_| {})
    }

    /// `step` hears progress (starting a run).
    pub fn call_with<T: DeserializeOwned>(&self, req: Request, step: Step) -> Result<T> {
        let v = match self {
            Backend::Local => handle(req, step)?,
            Backend::Remote(server) => {
                // Starting a run may build the image first, which takes minutes.
                let timeout = match req {
                    Request::StartRun { .. } => Duration::from_secs(30 * 60),
                    _ => Duration::from_secs(60),
                };
                match server.call(&serde_json::to_value(&req)?, timeout, step)? {
                    Reply::Ok(v) => v,
                    Reply::Unpaired { .. } => {
                        return Err(anyhow!(
                            "the server no longer knows this app; connect to it again to pair"
                        ));
                    }
                }
            }
        };
        Ok(serde_json::from_value(v)?)
    }
}

// ---- runs in progress ---------------------------------------------------------------------

/// Bots whose run is starting on this machine, with the current step.
static STARTING: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());
/// When the app last looked at a bot's runs, so its container isn't paused meanwhile.
static TOUCHED: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);

pub fn starting() -> BTreeMap<String, String> {
    STARTING.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

fn touch(slug: &str) {
    TOUCHED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(slug.into(), Instant::now());
}

/// Whether the app looked at the bot within `within`.
pub fn touched(slug: &str, within: Duration) -> bool {
    TOUCHED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(slug))
        .is_some_and(|t| t.elapsed() < within)
}

/// Starts a run of a bot (see `bots::start_run`), noting it's starting meanwhile.
pub fn start_run(
    slug: &str,
    reason: &str,
    notes: &[String],
    message: Option<&str>,
    step: Step,
) -> Result<String> {
    {
        let mut s = STARTING.lock().unwrap_or_else(|e| e.into_inner());
        if s.contains_key(slug) {
            return Err(anyhow!("{slug} is already starting"));
        }
        s.insert(slug.into(), "Starting".into());
    }
    let res = (|| {
        let bot = bots::load(slug)?;
        let note = |st: &str| {
            if let Some(v) = STARTING
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_mut(slug)
            {
                *v = st.to_string();
            }
            step(st);
        };
        bots::start_run(
            slug,
            &bot,
            reason,
            notes,
            message,
            Store::load().model,
            &note,
        )
    })();
    STARTING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(slug);
    touch(slug);
    res
}

/// Which sessions are working, in the running containers.
pub fn active(states: &HashMap<String, String>) -> HashSet<String> {
    let mut active = HashSet::new();
    for (slug, _) in states.iter().filter(|(_, st)| *st == "running") {
        if let Ok(a) = container::client(slug).and_then(|c| c.active_sessions()) {
            active.extend(a);
        }
    }
    active
}

/// The opencode a bot's runs are in: its container (resumed if paused), or for runs from
/// before bots ran in containers, this computer's opencode service.
fn bot_client(slug: &str) -> Result<Client> {
    touch(slug);
    match container::state(slug).as_deref() {
        None => Client::connect(),
        Some("paused") => {
            container::resume(slug)?;
            container::client(slug)
        }
        Some(_) => container::client(slug),
    }
}

// ---- the work -----------------------------------------------------------------------------

pub fn handle(req: Request, step: Step) -> Result<Value> {
    let ok = |v: Value| Ok(v);
    match req {
        Request::Hello { .. } => ok(json!({ "version": env!("CARGO_PKG_VERSION") })),
        Request::Status => {
            let states = container::states().unwrap_or_default();
            let active = active(&states);
            to(Status {
                bots: bots::list(),
                states,
                active,
                starting: starting(),
                default_model: Store::load().model,
                browser: (crate::context::current().is_some() && crate::browser::supported())
                    .then(crate::browser::info),
            })
        }
        Request::Files { slug } => to(bots::files(&slug)),
        Request::Create { name, purpose } => to(bots::create(&name, &purpose)?),
        Request::Delete { slug } => to(bots::delete(&slug)?),
        Request::UpdateBot { slug, change } => to(bots::update(&slug, |b| match change {
            Change::Model(m) => b.model = m,
            Change::Schedule {
                schedule,
                note,
                now,
            } => schedule::apply(b, schedule, note, now),
            Change::RemoveWakeup(item) => schedule::remove(b, item),
            Change::Sandbox(s) => b.sandbox = s,
        })?),
        Request::Instructions => to(store::instructions()),
        Request::SaveInstructions { text } => to(store::save_instructions(&text)?),
        Request::Env => {
            let mut e = env::load();
            for v in e
                .global
                .values_mut()
                .chain(e.bots.values_mut().flatten().map(|x| x.1))
            {
                v.clear();
            }
            to(e)
        }
        Request::SetEnv { bot, name, value } => {
            if !env::valid_name(&name) {
                return Err(anyhow!(
                    "names use letters, digits and underscores, and don't start with a digit"
                ));
            }
            let mut e = env::load();
            match value {
                Some(v) => {
                    e.scope(bot.as_deref()).insert(name, v);
                }
                None => {
                    e.scope(bot.as_deref()).remove(&name);
                    if let Some(b) = &bot
                        && e.bots.get(b).is_some_and(|v| v.is_empty())
                    {
                        e.bots.remove(b);
                    }
                }
            }
            e.save()?;
            handle(Request::Env, step)
        }
        Request::BotMcp => to(mcp::load()),
        Request::InstalledMcp { slug } => to(mcp::installed(&slug)),
        Request::SaveBotMcp { bot, servers } => {
            let mut m = mcp::load();
            m.set(bot.as_deref(), servers);
            m.save()?;
            to(m)
        }
        Request::SetDefaultModel { model } => {
            let mut s = Store::load();
            s.model = model;
            to(s.save()?)
        }
        Request::StartRun {
            slug,
            reason,
            notes,
            message,
        } => to(start_run(&slug, &reason, &notes, message.as_deref(), step)?),
        Request::Snapshot { slug, session } => {
            let snap: Result<Snapshot> = bot_client(&slug).and_then(|c| c.snapshot(&session));
            to(snap.or_else(|e| {
                // A run from before bots ran in containers, whose bot has one now.
                Client::connect()
                    .and_then(|h| h.snapshot(&session))
                    .map_err(|_| anyhow!("can't read this run: {e:#}"))
            })?)
        }
        Request::Session {
            slug,
            session,
            action,
        } => {
            let c = bot_client(&slug)?;
            match action {
                SessionAction::Prompt { text } => c.prompt(&session, &text)?,
                SessionAction::Interrupt => c.interrupt(&session)?,
                SessionAction::Permission { request, decision } => {
                    c.reply_permission(&session, &request, &decision)?
                }
                SessionAction::SetModel { model } => c.set_model(&session, &model)?,
            }
            ok(Value::Null)
        }
        Request::Models => {
            let states = container::states().unwrap_or_default();
            for (slug, _) in states.iter().filter(|(_, st)| *st == "running") {
                if let Ok(list) = container::client(slug).and_then(|c| c.models()) {
                    return to(list);
                }
            }
            to(Client::connect()
                .context("no bot container is running yet")?
                .models()?)
        }
        Request::Doctor => to(container::doctor(&bots::list())),
        Request::SyncContext { context } => {
            crate::context::save(&context)?;
            ok(Value::Null)
        }
        Request::Upload { slug, files } => {
            let d = bots::dir(&slug);
            if !d.join("bot.json").exists() {
                return Err(anyhow!("no bot {slug:?}"));
            }
            write_files(&d.join(bots::UPLOADS), &files)?;
            ok(Value::Null)
        }
        Request::Import {
            slug,
            mut bot,
            files,
            env: vars,
        } => {
            if slug != bots::slugify(&slug) || slug.is_empty() {
                return Err(anyhow!("{slug:?} isn't a bot's name"));
            }
            let d = bots::dir(&slug);
            if d.exists() {
                return Err(anyhow!("the server already has a bot called {slug}"));
            }
            write_files(&d, &files)?;
            // Its runs are in the old computer's containers.
            bot.runs.clear();
            std::fs::create_dir_all(&d)?;
            bots::save(&slug, &bot)?;
            if !vars.is_empty() {
                let mut e = env::load();
                e.bots.insert(slug.clone(), vars);
                e.save()?;
            }
            ok(Value::Null)
        }
    }
}

/// Writes `(relative path, base64)` files under `root`, refusing paths that leave it.
fn write_files(root: &Path, files: &[(String, String)]) -> Result<()> {
    for (rel, body) in files {
        if Path::new(rel)
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(anyhow!("{rel:?} isn't inside the bot's folder"));
        }
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(p, base64::engine::general_purpose::STANDARD.decode(body)?)?;
    }
    Ok(())
}

/// Most an upload carries (the server reads requests of up to 256 MB, base64 included).
const MAX_UPLOAD: u64 = 128 * 1024 * 1024;

/// Files and folders the user picked, as `(path, base64)`: a file by its name, a folder's
/// files under the folder's name. Skips `.git`, `node_modules` and `.DS_Store`.
pub fn collect_upload(paths: &[std::path::PathBuf]) -> Result<Vec<(String, String)>> {
    fn walk(
        base: &Path,
        at: &Path,
        out: &mut Vec<(String, String)>,
        total: &mut u64,
    ) -> Result<()> {
        let meta = std::fs::metadata(at)?;
        if meta.is_dir() {
            let mut entries: Vec<_> = std::fs::read_dir(at)?.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for e in entries {
                let name = e.file_name();
                if !matches!(name.to_str(), Some(".git" | "node_modules" | ".DS_Store")) {
                    walk(base, &e.path(), out, total)?;
                }
            }
        } else if meta.is_file() {
            *total += meta.len();
            if *total > MAX_UPLOAD {
                return Err(anyhow!("uploads are limited to 128 MB at a time"));
            }
            let rel = at.strip_prefix(base)?.to_string_lossy().into_owned();
            let body = std::fs::read(at).with_context(|| format!("read {}", at.display()))?;
            out.push((rel, base64::engine::general_purpose::STANDARD.encode(body)));
        }
        Ok(())
    }
    let (mut out, mut total) = (Vec::new(), 0);
    for p in paths {
        let base = p.parent().unwrap_or(Path::new("/"));
        walk(base, p, &mut out, &mut total)?;
    }
    if out.is_empty() {
        return Err(anyhow!("there are no files there"));
    }
    Ok(out)
}

fn to(v: impl Serialize) -> Result<Value> {
    Ok(serde_json::to_value(v)?)
}

/// A local bot, ready to send to a server: its folder's files (not its git history) and
/// its own variables.
pub fn export(slug: &str) -> Result<Request> {
    fn walk(root: &Path, at: &Path, out: &mut Vec<(String, String)>) -> Result<()> {
        for e in std::fs::read_dir(at)?.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if p.is_dir() {
                if name != ".git" {
                    walk(root, &p, out)?;
                }
            } else if p.is_file() && !name.starts_with("bot.json") {
                let rel = p.strip_prefix(root)?.to_string_lossy().into_owned();
                let body = std::fs::read(&p)?;
                out.push((rel, base64::engine::general_purpose::STANDARD.encode(body)));
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(&bots::dir(slug), &bots::dir(slug), &mut files)?;
    Ok(Request::Import {
        slug: slug.into(),
        bot: bots::load(slug)?,
        files,
        env: env::load().bots.remove(slug).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip() {
        let r = Request::UpdateBot {
            slug: "x".into(),
            change: Change::Schedule {
                schedule: Schedule::Every(60),
                note: None,
                now: 1,
            },
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["op"], "update_bot");
        let back: Request = serde_json::from_value(v).unwrap();
        assert!(matches!(back, Request::UpdateBot { .. }));
    }

    /// Against a paired server, with a data dir (`DUMBOT_DATA_DIR`) paired to it
    /// (`dumbot connect`): `cargo test remote_bot -- --ignored --nocapture`. A bot on the
    /// server reads a page with the server browser, on its own.
    #[test]
    #[ignore]
    fn remote_bot_runs_on_the_server() {
        let server = Store::load()
            .remote
            .expect("pair with `dumbot connect` first");
        let b = Backend::Remote(server);
        b.call::<()>(Request::SyncContext {
            context: crate::context::gather(),
        })
        .unwrap();
        let slug: String = b
            .call(Request::Create {
                name: "Remote test".into(),
                purpose: "Use the browse tools to open https://example.com and reply with the \
                          page title. Then stop; don't schedule anything."
                    .into(),
            })
            .unwrap();
        let id: String = b
            .call_with(
                Request::StartRun {
                    slug: slug.clone(),
                    reason: "integration test".into(),
                    notes: vec![],
                    message: None,
                },
                &|s| eprintln!("step: {s}"),
            )
            .unwrap();
        let st: Status = b.call(Request::Status).unwrap();
        assert!(st.bots.contains_key(&slug));
        assert_eq!(st.browser.unwrap().problem, None);
        let started = Instant::now();
        let snap = loop {
            std::thread::sleep(Duration::from_secs(5));
            let snap: Snapshot = b
                .call(Request::Snapshot {
                    slug: slug.clone(),
                    session: id.clone(),
                })
                .unwrap();
            if !snap.running || started.elapsed() > Duration::from_secs(300) {
                break snap;
            }
        };
        for e in &snap.entries {
            eprintln!("{}", format!("{e:?}").chars().take(200).collect::<String>());
        }
        b.call::<()>(Request::Delete { slug }).unwrap();
        use crate::opencode::Entry;
        // Read through the browser (browse tools run inside `execute`), not fetched.
        assert!(
            !snap
                .entries
                .iter()
                .any(|e| matches!(e, Entry::Tool { name, .. } if name == "webfetch"))
        );
        assert!(
            snap.entries
                .iter()
                .any(|e| matches!(e, Entry::Text(t) if t.contains("Example Domain")))
        );
    }

    /// Like `remote_bot_runs_on_the_server`: a bot reads an uploaded file and installs an MCP
    /// server for itself, whose tools its next run uses.
    #[test]
    #[ignore]
    fn remote_bot_uploads_and_installs_mcp() {
        let server = Store::load()
            .remote
            .expect("pair with `dumbot connect` first");
        let b = Backend::Remote(server);
        b.call::<()>(Request::SyncContext {
            context: crate::context::gather(),
        })
        .unwrap();
        let slug: String = b
            .call(Request::Create {
                name: "Mcp test".into(),
                purpose: "Test bot. Do exactly what each run's message says, then stop; never \
                          schedule anything."
                    .into(),
            })
            .unwrap();
        let dir = std::env::temp_dir().join(format!("dumbot-upload-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        std::fs::write(dir.join("docs/secret.txt"), "The word is PELICAN-42.\n").unwrap();
        let files = collect_upload(&[dir.join("docs")]).unwrap();
        b.call::<()>(Request::Upload {
            slug: slug.clone(),
            files,
        })
        .unwrap();
        let listed: Vec<(String, String)> = b.call(Request::Files { slug: slug.clone() }).unwrap();
        assert!(listed.iter().any(|f| f.1.contains("docs/secret.txt")));
        let run = |message: &str| -> Snapshot {
            let id: String = b
                .call(Request::StartRun {
                    slug: slug.clone(),
                    reason: "integration test".into(),
                    notes: vec![],
                    message: Some(message.into()),
                })
                .unwrap();
            let started = Instant::now();
            loop {
                std::thread::sleep(Duration::from_secs(5));
                let snap: Snapshot = b
                    .call(Request::Snapshot {
                        slug: slug.clone(),
                        session: id.clone(),
                    })
                    .unwrap();
                if !snap.running || started.elapsed() > Duration::from_secs(600) {
                    for e in &snap.entries {
                        eprintln!("{}", format!("{e:?}").chars().take(200).collect::<String>());
                    }
                    break snap;
                }
            }
        };
        run(
            "Install the MCP server named `everything` for yourself: it runs with \
             `npx -y @modelcontextprotocol/server-everything`. Then stop.",
        );
        let installed: Vec<(String, String)> =
            b.call(Request::Files { slug: slug.clone() }).unwrap();
        assert!(
            installed
                .iter()
                .any(|f| f.0 == "mcp.json" && f.1.contains("server-everything"))
        );
        let snap = run(
            "Read the word in files/docs/secret.txt, then call the `echo` tool of \
                        your `everything` MCP server with it, and reply with exactly what the \
                        tool returned. Then stop.",
        );
        b.call::<()>(Request::Delete { slug }).unwrap();
        use crate::opencode::Entry;
        // "Echo: …" is what the server's echo tool returns (MCP tools run inside `execute`).
        assert!(
            snap.entries
                .iter()
                .any(|e| matches!(e, Entry::Text(t) if t.contains("Echo: PELICAN-42")))
        );
    }
}
