//! dumBot: bots on the left, the selected bot's markdown in the middle, and its opencode runs
//! on the right. Bots wake on their own schedules while the app runs.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use rune::{
    AppSpec, Chrome, Cx, El, KeyEvent, ScrollSpec, TextEdit, Tx, Waker, col, el, row, text,
};
use rune_kit::shell::{
    self, PaneState, Sidebar, SidebarRow, SidebarSection, SidebarToggle, SplitView, Toolbar,
};
use rune_kit::{
    Button, ButtonVariant, Combobox, ComboboxOption, ComboboxState, ControlSize, Dialog,
    IconButton, IconButtonVariant, Kit, Markdown, SegmentedControl, Spinner, TextArea, TextInput,
};

use crate::api::{self, Backend, Change, Request, SessionAction};
use crate::bots::{self, Bot, Run};
use crate::opencode::{Entry, Snapshot};
use crate::remote::{self, Server};
use crate::store::{ModelRef, Store};
use crate::{appbundle, container, env, mcp, opencode, schedule, update};

type Job = Box<dyn FnOnce(&mut App, &mut Tx) + Send>;

/// How often bots' folders are re-read and running sessions checked, so schedules set by
/// `dumbot wake` and file changes show up.
const SCAN_SECS: f64 = 5.0;

enum Confirm {
    InstallApp,
    Update,
    Delete(String),
    /// Move these local bots to the server just connected to.
    MoveBots(Vec<String>),
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Files,
    Schedule,
}

/// The "Add a wake-up" form.
struct SchedForm {
    /// 0 once, 1 weekly, 2 repeating.
    kind: usize,
    /// Once: days from today.
    day: usize,
    /// Once and weekly: `HH:MM`.
    time: String,
    /// Weekly: Monday first.
    days: [bool; 7],
    /// Repeating: every `every` `unit`s (0 minutes, 1 hours, 2 days).
    every: TextEdit,
    unit: usize,
    note: TextEdit,
}

impl Default for SchedForm {
    fn default() -> Self {
        SchedForm {
            kind: 0,
            day: 1,
            time: "09:00".into(),
            days: [false; 7],
            every: TextEdit::new("6"),
            unit: 1,
            note: TextEdit::new(""),
        }
    }
}

const UNITS: [(&str, i64); 3] = [("minutes", 60), ("hours", 3600), ("days", 86400)];
const DAY_NAMES: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// The environment variables dialog: global (`bot` none) or one bot's. Values are typed in
/// and never shown again.
struct EnvEdit {
    bot: Option<String>,
    env: env::Env,
    name: TextEdit,
    value: TextEdit,
}

impl EnvEdit {
    fn open(bot: Option<String>, env: env::Env) -> EnvEdit {
        let mut name = TextEdit::new("");
        name.focused = true;
        EnvEdit {
            bot,
            env,
            name,
            value: TextEdit::new(""),
        }
    }
}

/// The Server dialog: where bots run, and pairing with a server.
struct ConnectEdit {
    addr: TextEdit,
    /// The server being paired with, pinned to the certificate it showed.
    pinned: Option<Server>,
    /// The code to approve on the server, once it asked for one.
    code: Option<String>,
    busy: bool,
    error: Option<String>,
}

/// The bot-only MCP servers for all bots, as JSON, being edited.
struct McpEdit {
    text: TextEdit,
}

/// A bot start's lifecycle: each step and when it began, then when it ended and how.
#[derive(Clone, Default)]
struct Progress {
    steps: Vec<(String, f64)>,
    done: Option<(f64, Result<(), String>)>,
}

/// The container settings dialog: the bot's sandbox, being edited.
struct SandboxEdit {
    slug: String,
    sandbox: bots::Sandbox,
    mounts: TextEdit,
    /// The host's MCP servers, with why each may not work in a container.
    servers: Vec<(String, Option<String>)>,
    /// MCP servers only this bot gets, as JSON.
    own_mcp: TextEdit,
    /// Names of the bot-only servers every bot gets.
    global_mcp: Vec<String>,
}

/// How long an idle container stays unpaused, in seconds.
const PAUSE_AFTER_SECS: f64 = 60.0;
/// How often issues are checked, in seconds.
const DOCTOR_SECS: f64 = 60.0;
/// How often a server is sent this computer's opencode config and logins, in seconds.
const CONTEXT_PUSH_SECS: f64 = 600.0;

/// Servers as the JSON the MCP editors show.
fn servers_json(s: &mcp::Servers) -> String {
    if s.is_empty() {
        String::new()
    } else {
        serde_json::to_string_pretty(s).unwrap_or_default()
    }
}

struct App {
    store: Store,
    jobs: (Sender<Job>, Receiver<Job>),
    waker: Option<Waker>,
    /// Where bots live: this computer, or a dumBot server.
    backend: Backend,
    /// The default model for bots (the server's, when connected to one).
    default_model: Option<ModelRef>,
    /// Bots the server is starting runs of (scheduled ones), with the step each is on.
    server_starting: BTreeMap<String, String>,
    /// Why the server can't be reached, if it can't.
    server_error: Option<String>,
    connect: Option<ConnectEdit>,
    next_pair: f64,
    next_context_push: f64,
    mcp_edit: Option<McpEdit>,
    moving: bool,
    next_models: f64,
    /// Each bot's container state (`running`, `paused`, …), from the last scan.
    containers: HashMap<String, String>,
    /// When each bot last had a reason to keep its container unpaused (app clock).
    last_busy: HashMap<String, f64>,
    /// The steps of each bot's latest start, with when each began (app clock).
    progress: HashMap<String, Progress>,
    issues: Vec<container::Issue>,
    issues_inflight: bool,
    next_doctor: f64,
    show_issues: bool,
    sandbox_edit: Option<SandboxEdit>,
    /// Why the shown run can't be read.
    chat_error: Option<String>,
    /// The app clock at the last poll, for elapsed times in views.
    clock: f64,
    sidebar: PaneState,
    inspector: PaneState,
    bots: BTreeMap<String, Bot>,
    selected: Option<String>,
    /// The selected bot's markdown files and the one shown.
    files: Vec<(String, String)>,
    file: Option<String>,
    /// Sessions the agent is working in.
    active: HashSet<String>,
    active_inflight: bool,
    /// Bots whose run is being started.
    starting: HashSet<String>,
    next_scan: f64,
    /// The run shown in the agent panel; the bot's latest when unset.
    run: Option<String>,
    run_combo: ComboboxState,
    chats: HashMap<String, Snapshot>,
    chat_inflight: bool,
    next_chat_poll: f64,
    composer: TextEdit,
    models: Vec<ComboboxOption>,
    models_loading: bool,
    model_combo: ComboboxState,
    default_combo: ComboboxState,
    busy: bool,
    confirm: Option<Confirm>,
    new_bot: Option<(TextEdit, TextEdit)>,
    tab: Tab,
    sched: SchedForm,
    day_combo: ComboboxState,
    time_combo: ComboboxState,
    /// The instructions for all bots, being edited.
    instructions: Option<TextEdit>,
    env_edit: Option<EnvEdit>,
    toast: Option<(String, bool)>,
    toast_until: Option<f64>,
    update: Option<String>,
    update_hidden: bool,
    updating: bool,
    next_update_check: f64,
}

fn init() -> App {
    let store = Store::load();
    App {
        backend: store.remote.clone().map_or(Backend::Local, Backend::Remote),
        default_model: store.model.clone(),
        store,
        jobs: channel(),
        waker: None,
        server_starting: BTreeMap::new(),
        server_error: None,
        connect: None,
        next_pair: 0.0,
        next_context_push: 0.0,
        mcp_edit: None,
        moving: false,
        next_models: 0.0,
        containers: HashMap::new(),
        last_busy: HashMap::new(),
        progress: HashMap::new(),
        issues: Vec::new(),
        issues_inflight: false,
        next_doctor: 0.0,
        show_issues: false,
        sandbox_edit: None,
        chat_error: None,
        clock: 0.0,
        sidebar: PaneState::new(280.0).range(200.0, 480.0),
        inspector: PaneState::new(480.0).range(320.0, 900.0),
        bots: BTreeMap::new(),
        selected: None,
        files: Vec::new(),
        file: None,
        active: HashSet::new(),
        active_inflight: false,
        starting: HashSet::new(),
        next_scan: 0.0,
        run: None,
        run_combo: ComboboxState::default(),
        chats: HashMap::new(),
        chat_inflight: false,
        next_chat_poll: 0.0,
        composer: TextEdit::new(""),
        models: Vec::new(),
        models_loading: false,
        model_combo: ComboboxState::default(),
        default_combo: ComboboxState::default(),
        busy: false,
        confirm: None,
        new_bot: None,
        tab: Tab::Files,
        sched: SchedForm::default(),
        day_combo: ComboboxState::default(),
        time_combo: ComboboxState::default(),
        instructions: None,
        env_edit: None,
        toast: None,
        toast_until: None,
        update: None,
        update_hidden: false,
        updating: false,
        next_update_check: 0.0,
    }
}

impl App {
    /// Runs `work` on a thread and `done` with its result on the UI thread.
    fn bg<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
        done: impl FnOnce(&mut App, T, &mut Tx) + Send + 'static,
    ) {
        let (sender, waker) = (self.jobs.0.clone(), self.waker.clone());
        std::thread::spawn(move || {
            let out = work();
            let _ = sender.send(Box::new(move |s: &mut App, tx: &mut Tx| done(s, out, tx)));
            if let Some(w) = waker {
                w();
            }
        });
    }

    fn notify(&mut self, msg: impl Into<String>, error: bool) {
        self.toast = Some((msg.into(), error));
        self.toast_until = None;
    }

    fn save(&mut self) {
        if let Err(e) = self.store.save() {
            self.notify(format!("Couldn't save settings: {e}"), true);
        }
    }

    fn bot(&self) -> Option<&Bot> {
        self.bots.get(self.selected.as_ref()?)
    }

    /// The run shown in the agent panel.
    fn session_id(&self) -> Option<String> {
        let bot = self.bot()?;
        self.run
            .clone()
            .filter(|id| bot.runs.iter().any(|r| &r.session == id))
            .or_else(|| bot.last_session().map(str::to_string))
    }

    fn is_running(&self, bot: &Bot) -> bool {
        bot.last_session()
            .is_some_and(|id| self.active.contains(id))
    }

    fn wake(&self) {
        if let Some(w) = &self.waker {
            w();
        }
    }

    /// Whether a bot's run is being started, here or by the server.
    fn is_starting(&self, slug: &str) -> bool {
        self.starting.contains(slug) || self.server_starting.contains_key(slug)
    }

    /// Changes a bot where it lives, returning it as changed.
    fn update_bot(&mut self, slug: &str, change: Change) -> Option<Bot> {
        let req = Request::UpdateBot {
            slug: slug.into(),
            change,
        };
        match self.backend.call::<Bot>(req) {
            Ok(b) => {
                self.bots.insert(slug.into(), b.clone());
                Some(b)
            }
            Err(e) => {
                self.notify(format!("{e:#}"), true);
                None
            }
        }
    }

    /// A function a background start reports its steps through.
    fn stepper(&self, slug: &str) -> impl Fn(&str) + Send + Sync + 'static {
        let (sender, waker, slug) = (self.jobs.0.clone(), self.waker.clone(), slug.to_string());
        move |step: &str| {
            let (slug, step) = (slug.clone(), step.to_string());
            let _ = sender.send(Box::new(move |s: &mut App, tx: &mut Tx| {
                let at = tx.clock();
                s.progress.entry(slug).or_default().steps.push((step, at));
            }));
            if let Some(w) = &waker {
                w();
            }
        }
    }

    /// Pauses containers that have had nothing to do for a while: no working run, not
    /// starting, not the selected bot (whose runs are on screen).
    fn pause_idle(&mut self, now: f64) {
        let mut idle = Vec::new();
        for (slug, st) in &self.containers {
            let busy = self.selected.as_deref() == Some(slug.as_str())
                || self.starting.contains(slug)
                || self
                    .bots
                    .get(slug)
                    .is_some_and(|b| b.runs.iter().any(|r| self.active.contains(&r.session)));
            let last = self.last_busy.entry(slug.clone()).or_insert(now);
            if busy || st != "running" {
                *last = now;
            } else if now - *last > PAUSE_AFTER_SECS {
                idle.push(slug.clone());
            }
        }
        for slug in idle {
            self.containers.insert(slug.clone(), "paused".into());
            self.bg(move || container::pause(&slug), |_, _, _| {});
        }
    }

    fn select(&mut self, slug: &str) {
        self.chat_error = None;
        self.selected = Some(slug.to_string());
        self.run = None;
        self.file = None;
        self.composer = TextEdit::new("");
        self.next_chat_poll = 0.0;
        self.reload_files();
    }

    fn reload_files(&mut self) {
        let Some(slug) = self.selected.clone() else {
            self.files.clear();
            return;
        };
        let backend = self.backend.clone();
        self.bg(
            move || {
                let files =
                    backend.call::<Vec<(String, String)>>(Request::Files { slug: slug.clone() });
                (slug, files)
            },
            |s, (slug, res), _| {
                if let Ok(files) = res
                    && s.selected.as_ref() == Some(&slug)
                {
                    s.set_files(files);
                }
            },
        );
    }

    fn set_files(&mut self, files: Vec<(String, String)>) {
        if files != self.files {
            self.files = files;
        }
        if self
            .file
            .as_ref()
            .is_none_or(|f| !self.files.iter().any(|x| &x.0 == f))
        {
            self.file = self.files.first().map(|f| f.0.clone());
        }
    }

    /// Re-reads every bot: at once from disk, or soon from the server.
    fn reload_bots(&mut self) {
        if self.backend.is_remote() {
            self.next_scan = 0.0;
            return self.wake();
        }
        self.apply_bots(bots::list());
    }

    fn apply_bots(&mut self, bots: BTreeMap<String, Bot>) {
        if bots != self.bots {
            self.bots = bots;
        }
        if self
            .selected
            .as_ref()
            .is_some_and(|s| !self.bots.contains_key(s))
        {
            self.selected = None;
        }
        self.reload_files();
    }

    /// Wakes the bots that are due. A bot whose last run is still working waits for it.
    fn wake_due(&mut self) {
        let now = bots::now();
        let due: Vec<String> = self
            .bots
            .iter()
            .filter(|(slug, b)| b.due(now) && !self.starting.contains(*slug) && !self.is_running(b))
            .map(|(slug, _)| slug.clone())
            .collect();
        for slug in due {
            // Take the wake-up off the schedule first, so it can't fire twice.
            let mut notes = Vec::new();
            let res = bots::update(&slug, |b| notes = schedule::fired(b, now));
            match res {
                Ok(bot) => {
                    self.bots.insert(slug.clone(), bot);
                    self.start_run(slug, "scheduled wake-up".into(), notes, None);
                }
                Err(e) => self.notify(format!("Couldn't wake {slug}: {e:#}"), true),
            }
        }
    }

    /// Starts a new run of a bot in a fresh session, with `message` from the user if any.
    fn start_run(
        &mut self,
        slug: String,
        reason: String,
        notes: Vec<String>,
        message: Option<String>,
    ) {
        if !self.bots.contains_key(&slug) {
            return;
        }
        self.starting.insert(slug.clone());
        self.progress.insert(slug.clone(), Progress::default());
        let s2 = slug.clone();
        let step = self.stepper(&slug);
        let backend = self.backend.clone();
        self.bg(
            move || -> anyhow::Result<String> {
                let req = Request::StartRun {
                    slug: s2,
                    reason,
                    notes,
                    message,
                };
                backend.call_with(req, &step)
            },
            move |s, res, tx| {
                s.starting.remove(&slug);
                s.last_busy.insert(slug.clone(), tx.clock());
                s.next_doctor = 0.0;
                if let Some(p) = s.progress.get_mut(&slug) {
                    let how = res.as_ref().map(|_| ()).map_err(|e| format!("{e:#}"));
                    p.done = Some((tx.clock(), how));
                }
                match res {
                    Ok(id) => {
                        s.active.insert(id);
                        s.reload_bots();
                        if s.selected.as_ref() == Some(&slug) {
                            s.run = None;
                            s.next_chat_poll = 0.0;
                            tx.wake_after(Duration::from_millis(300));
                        }
                    }
                    Err(e) => s.notify(format!("Couldn't start {slug}: {e:#}"), true),
                }
            },
        );
    }

    fn run_now(&mut self) {
        if let Some(slug) = self.selected.clone() {
            let msg = self.composer.text.trim().to_string();
            self.composer = TextEdit::new("");
            self.start_run(
                slug,
                "started by the user".into(),
                Vec::new(),
                (!msg.is_empty()).then_some(msg),
            );
        }
    }

    /// Sends the composer to the shown run, or starts a run with it when there is none.
    fn send_composer(&mut self) {
        let text = self.composer.text.trim().to_string();
        if text.is_empty() || self.busy {
            return;
        }
        if self.session_id().is_none() {
            return self.run_now();
        }
        self.composer = TextEdit::new("");
        self.busy = true;
        self.session_call(SessionAction::Prompt { text });
    }

    fn session_call(&mut self, action: SessionAction) {
        let (Some(slug), Some(session)) = (self.selected.clone(), self.session_id()) else {
            return;
        };
        let backend = self.backend.clone();
        self.bg(
            move || {
                backend.call::<()>(Request::Session {
                    slug,
                    session,
                    action,
                })
            },
            |s, res, tx| {
                s.busy = false;
                s.next_chat_poll = 0.0;
                s.next_scan = 0.0;
                tx.wake_after(Duration::from_millis(300));
                if let Err(e) = res {
                    s.notify(e.to_string(), true);
                }
            },
        );
    }

    /// Models from a running container (they share the host's opencode config), or the
    /// host's opencode service. Tried again a little later while neither is up.
    fn load_models(&mut self) {
        self.models_loading = true;
        let backend = self.backend.clone();
        self.bg(
            move || backend.call::<Vec<(String, String, String)>>(Request::Models),
            |s, res, tx| {
                s.models_loading = false;
                match res {
                    Ok(list) => {
                        s.models = list
                            .into_iter()
                            .map(|(id, name, provider)| {
                                let mut o = ComboboxOption::new(id, name);
                                o.detail = provider;
                                o
                            })
                            .collect();
                    }
                    Err(_) => {
                        s.next_models = tx.clock() + 15.0;
                        tx.wake_after(Duration::from_secs(15));
                    }
                }
            },
        );
    }

    fn model_name(&self, m: &ModelRef) -> String {
        let key = m.key();
        self.models
            .iter()
            .find(|o| o.id == key)
            .map_or(key, |o| o.label.clone())
    }

    fn choose_default_model(&mut self, key: &str) {
        self.default_model = ModelRef::parse(key);
        if self.backend.is_remote() {
            let (backend, model) = (self.backend.clone(), self.default_model.clone());
            self.bg(
                move || backend.call::<()>(Request::SetDefaultModel { model }),
                |s, res, _| {
                    if let Err(e) = res {
                        s.notify(format!("Couldn't save the default model: {e:#}"), true);
                    }
                },
            );
        } else {
            self.store.model = self.default_model.clone();
            self.save();
        }
        // Bots on the default model switch their latest run too.
        let Some(m) = self.default_model.clone() else {
            return;
        };
        let sessions: Vec<(String, String)> = self
            .bots
            .iter()
            .filter(|(_, b)| b.model.is_none())
            .filter_map(|(slug, b)| Some((slug.clone(), b.last_session()?.to_string())))
            .collect();
        self.switch_model(sessions, m);
    }

    /// Switches sessions to `m`, so their next turn (a message, or the rest of a working
    /// run) already uses it.
    /// Only containers that are up: a paused or missing one's run isn't going anywhere, and
    /// its next run starts on the bot's model anyway.
    fn switch_model(&mut self, sessions: Vec<(String, String)>, m: ModelRef) {
        let sessions: Vec<_> = sessions
            .into_iter()
            .filter(|(slug, _)| self.containers.get(slug).is_some_and(|st| st == "running"))
            .collect();
        if sessions.is_empty() {
            return;
        }
        let backend = self.backend.clone();
        self.bg(
            move || {
                sessions.into_iter().try_for_each(|(slug, session)| {
                    backend.call::<()>(Request::Session {
                        slug,
                        session,
                        action: SessionAction::SetModel { model: m.clone() },
                    })
                })
            },
            |s, res, _| {
                s.next_chat_poll = 0.0;
                if let Err(e) = res {
                    s.notify(format!("Couldn't switch the run's model: {e}"), true);
                }
            },
        );
    }

    /// `""` goes back to the default model.
    fn choose_bot_model(&mut self, key: &str) {
        let Some(slug) = self.selected.clone() else {
            return;
        };
        let model = ModelRef::parse(key);
        if let Some(b) = self.update_bot(&slug, Change::Model(model)) {
            let effective = b.model.clone().or_else(|| self.default_model.clone());
            if let (Some(id), Some(m)) = (b.last_session(), effective) {
                self.switch_model(vec![(slug, id.to_string())], m);
            }
        }
    }

    fn create_bot(&mut self) {
        let Some((name, purpose)) = self
            .new_bot
            .as_ref()
            .map(|(n, p)| (n.text.clone(), p.text.clone()))
        else {
            return;
        };
        if purpose.trim().is_empty() {
            return self.notify("Describe what the bot is for.", true);
        }
        let req = Request::Create {
            name: name.clone(),
            purpose,
        };
        match self.backend.call::<String>(req) {
            Ok(slug) => {
                self.new_bot = None;
                self.reload_bots();
                // Known at once, so its run can start before the next scan.
                self.bots.entry(slug.clone()).or_insert_with(|| Bot {
                    name: name.trim().into(),
                    ..Bot::default()
                });
                self.select(&slug);
                self.start_run(
                    slug,
                    "first run. You were just created: read your purpose, work out how you'll \
                     do the job, rewrite AGENTS.md to match, do a first pass of the work if it \
                     makes sense, and schedule your next run."
                        .into(),
                    Vec::new(),
                    None,
                );
            }
            Err(e) => self.notify(format!("{e:#}"), true),
        }
    }

    fn add_wakeup(&mut self) {
        let Some(slug) = self.selected.clone() else {
            return;
        };
        let now = bots::now();
        let f = &self.sched;
        let sched = match f.kind {
            0 => match schedule::local_at(f.day as i64, &f.time, now) {
                Some(t) if t > now => schedule::Schedule::At(t),
                _ => return self.notify("That time has already passed.", true),
            },
            1 => {
                let days: Vec<u8> = (0..7u8).filter(|d| f.days[*d as usize]).collect();
                if days.is_empty() {
                    return self.notify("Pick at least one day.", true);
                }
                schedule::Schedule::Weekly(vec![bots::Rule {
                    days,
                    time: f.time.clone(),
                    note: None,
                }])
            }
            _ => match f.every.text.trim().parse::<i64>() {
                Ok(n) if n > 0 => schedule::Schedule::Every(n * UNITS[f.unit].1),
                _ => return self.notify("Enter how often, as a whole number.", true),
            },
        };
        let note = Some(f.note.text.trim().to_string()).filter(|n| !n.is_empty());
        let change = Change::Schedule {
            schedule: sched,
            note,
            now,
        };
        if self.update_bot(&slug, change).is_some() {
            self.sched.note = TextEdit::new("");
            self.next_scan = 0.0;
        }
    }

    fn remove_wakeup(&mut self, item: schedule::Item) {
        let Some(slug) = self.selected.clone() else {
            return;
        };
        self.update_bot(&slug, Change::RemoveWakeup(item));
    }

    fn save_instructions(&mut self) {
        let Some(t) = self.instructions.take() else {
            return;
        };
        match self.backend.call::<()>(Request::SaveInstructions {
            text: t.text.clone(),
        }) {
            Ok(()) => self.notify("Saved. Bots follow it from their next run.", false),
            Err(e) => self.notify(format!("Couldn't save: {e:#}"), true),
        }
    }

    fn open_instructions(&mut self) {
        match self.backend.call::<String>(Request::Instructions) {
            Ok(text) => {
                let mut t = TextEdit::new(text);
                t.focused = true;
                self.instructions = Some(t);
            }
            Err(e) => self.notify(format!("{e:#}"), true),
        }
    }

    /// `None` for the global variables.
    fn open_env(&mut self, bot: Option<String>) {
        match self.backend.call::<env::Env>(Request::Env) {
            Ok(e) => self.env_edit = Some(EnvEdit::open(bot, e)),
            Err(e) => self.notify(format!("{e:#}"), true),
        }
    }

    fn add_env(&mut self) {
        let Some(e) = &self.env_edit else { return };
        let name = e.name.text.trim().to_string();
        if !env::valid_name(&name) {
            return self.notify(
                "Names use letters, digits and underscores, and don't start with a digit.",
                true,
            );
        }
        if e.value.text.is_empty() {
            return self.notify("Enter a value.", true);
        }
        let value = Some(e.value.text.clone());
        self.set_env(name, value);
    }

    fn remove_env(&mut self, name: &str) {
        self.set_env(name.to_string(), None);
    }

    fn set_env(&mut self, name: String, value: Option<String>) {
        let Some(e) = &self.env_edit else { return };
        let req = Request::SetEnv {
            bot: e.bot.clone(),
            name,
            value,
        };
        match self.backend.call::<env::Env>(req) {
            Ok(fresh) => {
                if let Some(e) = &mut self.env_edit {
                    e.env = fresh;
                    e.name = TextEdit::new("");
                    e.name.focused = true;
                    e.value = TextEdit::new("");
                }
            }
            Err(err) => self.notify(format!("Couldn't save: {err:#}"), true),
        }
    }

    fn open_mcp(&mut self) {
        match self.backend.call::<mcp::BotMcp>(Request::BotMcp) {
            Ok(m) => {
                self.mcp_edit = Some(McpEdit {
                    text: TextEdit::new(servers_json(&m.global)),
                })
            }
            Err(e) => self.notify(format!("{e:#}"), true),
        }
    }

    fn save_mcp(&mut self) {
        let Some(e) = &self.mcp_edit else { return };
        let servers = match mcp::parse(&e.text.text) {
            Ok(s) => s,
            Err(err) => return self.notify(format!("{err:#}"), true),
        };
        match self
            .backend
            .call::<mcp::BotMcp>(Request::SaveBotMcp { bot: None, servers })
        {
            Ok(_) => {
                self.mcp_edit = None;
                self.notify("Saved. Bots get these from their next run.", false);
            }
            Err(err) => self.notify(format!("Couldn't save: {err:#}"), true),
        }
    }

    fn open_sandbox(&mut self) {
        let Some(slug) = self.selected.clone() else {
            return;
        };
        let sandbox = self
            .bots
            .get(&slug)
            .map(|b| b.sandbox.clone())
            .unwrap_or_default();
        let bot_mcp = match self.backend.call::<mcp::BotMcp>(Request::BotMcp) {
            Ok(m) => m,
            Err(e) => return self.notify(format!("{e:#}"), true),
        };
        let mut servers: Vec<(String, Option<String>)> = container::host_mcp()
            .into_iter()
            .filter(|(name, _)| name != "browse")
            .map(|(name, v)| (name, container::mcp_caveat(&v)))
            .collect();
        // Opted-in servers since removed from the config still show, to be turned off.
        for m in &sandbox.mcp {
            if !servers.iter().any(|(n, _)| n == m) {
                servers.push((m.clone(), Some("isn't in your opencode config".into())));
            }
        }
        let slug_for_mcp = slug.clone();
        self.sandbox_edit = Some(SandboxEdit {
            slug,
            mounts: TextEdit::new(sandbox.mounts.join("\n")),
            sandbox,
            servers,
            own_mcp: TextEdit::new(servers_json(
                bot_mcp
                    .bots
                    .get(&slug_for_mcp)
                    .unwrap_or(&mcp::Servers::new()),
            )),
            global_mcp: bot_mcp.global.keys().cloned().collect(),
        });
    }

    fn save_sandbox(&mut self) {
        let Some(own) = self.sandbox_edit.as_ref().map(|e| e.own_mcp.text.clone()) else {
            return;
        };
        let own = match mcp::parse(&own) {
            Ok(s) => s,
            Err(err) => return self.notify(format!("This bot's MCP servers: {err:#}"), true),
        };
        let Some(e) = self.sandbox_edit.take() else {
            return;
        };
        let req = Request::SaveBotMcp {
            bot: Some(e.slug.clone()),
            servers: own,
        };
        if let Err(err) = self.backend.call::<mcp::BotMcp>(req) {
            return self.notify(format!("Couldn't save: {err:#}"), true);
        }
        let mut sandbox = e.sandbox;
        sandbox.mounts = e
            .mounts
            .text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        if self.update_bot(&e.slug, Change::Sandbox(sandbox)).is_some() {
            self.next_doctor = 0.0;
            self.notify(
                "Saved. The bot's next run uses its new container settings.",
                false,
            );
        }
    }

    fn delete_bot(&mut self, slug: &str) {
        self.confirm = None;
        if let Err(e) = self
            .backend
            .call::<()>(Request::Delete { slug: slug.into() })
        {
            self.notify(format!("{e:#}"), true);
        }
        self.reload_bots();
    }

    /// Asks for files, or a folder, to upload to the selected bot.
    fn pick_upload(&mut self, tx: &mut Tx, folder: bool) {
        let Some(slug) = self.selected.clone() else {
            return;
        };
        let options = rune::FileDialogOptions {
            title: "Upload to the bot".into(),
            ..Default::default()
        };
        if folder {
            tx.open_folder(options, move |s: &mut App, res, _| match res {
                Ok(Some(p)) => s.upload(slug, vec![p]),
                Ok(None) => {}
                Err(e) => s.notify(format!("Couldn't choose a folder: {}", e.0), true),
            });
        } else {
            tx.open_files(options, move |s: &mut App, res, _| match res {
                Ok(Some(paths)) => s.upload(slug, paths),
                Ok(None) => {}
                Err(e) => s.notify(format!("Couldn't choose files: {}", e.0), true),
            });
        }
    }

    /// Copies files and folders into the bot's `files/`, where it lives.
    fn upload(&mut self, slug: String, paths: Vec<std::path::PathBuf>) {
        if paths.is_empty() {
            return;
        }
        self.busy = true;
        let backend = self.backend.clone();
        self.bg(
            move || {
                let files = api::collect_upload(&paths)?;
                let n = files.len();
                backend.call::<()>(Request::Upload { slug, files })?;
                Ok::<_, anyhow::Error>(n)
            },
            |s, res, _| {
                s.busy = false;
                match res {
                    Ok(n) => {
                        s.reload_files();
                        s.notify(
                            format!(
                                "Uploaded {n} file{} to the bot's files/.",
                                if n == 1 { "" } else { "s" }
                            ),
                            false,
                        );
                    }
                    Err(e) => s.notify(format!("Couldn't upload: {e:#}"), true),
                }
            },
        );
    }

    fn open_connect(&mut self) {
        let addr = match &self.backend {
            Backend::Remote(s) => s.addr.clone(),
            Backend::Local => String::new(),
        };
        let mut addr = TextEdit::new(addr);
        addr.focused = true;
        self.connect = Some(ConnectEdit {
            addr,
            pinned: None,
            code: None,
            busy: false,
            error: None,
        });
    }

    /// Asks the server in the dialog to pair, or whether it has approved this app yet.
    fn try_connect(&mut self) {
        let Some(c) = &mut self.connect else { return };
        if c.busy {
            return;
        }
        let addr = c.addr.text.trim();
        if addr.is_empty() {
            c.error = Some("Enter the server's address.".into());
            return;
        }
        let addr = remote::normalize_addr(addr);
        // A different address starts over.
        if c.pinned.as_ref().is_some_and(|p| p.addr != addr) {
            c.pinned = None;
            c.code = None;
        }
        let server = c.pinned.clone().unwrap_or(Server {
            addr,
            fingerprint: None,
        });
        c.busy = true;
        c.error = None;
        self.bg(
            move || server.pair(),
            |s, res, tx| {
                let Some(c) = &mut s.connect else { return };
                c.busy = false;
                match res {
                    Ok((pinned, Some(code))) => {
                        c.pinned = Some(pinned);
                        c.code = Some(code);
                        s.next_pair = tx.clock() + 2.0;
                        tx.wake_after(Duration::from_secs(2));
                    }
                    Ok((pinned, None)) => s.connected(pinned),
                    Err(e) => c.error = Some(format!("{e:#}")),
                }
            },
        );
    }

    /// Paired: bots are the server's from now on. Offers to move this computer's bots there.
    fn connected(&mut self, server: Server) {
        self.connect = None;
        self.store.remote = Some(server.clone());
        self.save();
        let addr = server.addr.clone();
        self.switch_backend(Backend::Remote(server));
        self.notify(format!("Connected to {addr}."), false);
        let local: Vec<String> = bots::list().into_keys().collect();
        if !local.is_empty() {
            self.confirm = Some(Confirm::MoveBots(local));
        }
    }

    fn disconnect(&mut self) {
        self.connect = None;
        self.store.remote = None;
        self.save();
        self.switch_backend(Backend::Local);
        self.notify("Bots run on this computer again.", false);
    }

    fn switch_backend(&mut self, backend: Backend) {
        self.default_model = match &backend {
            Backend::Local => self.store.model.clone(),
            Backend::Remote(_) => None,
        };
        crate::tunnel::carry(match &backend {
            Backend::Remote(s) => Some(s.clone()),
            Backend::Local => None,
        });
        self.backend = backend;
        self.bots.clear();
        self.selected = None;
        self.files.clear();
        self.file = None;
        self.run = None;
        self.containers.clear();
        self.active.clear();
        self.chats.clear();
        self.progress.clear();
        self.server_starting.clear();
        self.server_error = None;
        self.issues.clear();
        self.models.clear();
        self.next_models = 0.0;
        self.next_scan = 0.0;
        self.next_doctor = 0.0;
        self.next_context_push = 0.0;
        self.wake();
    }

    /// Sends the server what its bots take from this computer (see `context`).
    fn push_context(&mut self) {
        if !self.backend.is_remote() {
            return;
        }
        let backend = self.backend.clone();
        self.bg(
            move || {
                backend.call::<()>(Request::SyncContext {
                    context: crate::context::gather(),
                })
            },
            |s, res, _| {
                if let Err(e) = res {
                    s.notify(
                        format!("Couldn't send your opencode config to the server: {e:#}"),
                        true,
                    );
                }
            },
        );
    }

    /// Copies local bots to the server, removing each here once the server has it.
    fn move_bots(&mut self, slugs: Vec<String>) {
        self.confirm = None;
        self.moving = true;
        let backend = self.backend.clone();
        self.bg(
            move || {
                let mut moved = 0;
                let mut failed = Vec::new();
                for slug in slugs {
                    let res = api::export(&slug)
                        .and_then(|req| backend.call::<()>(req))
                        .and_then(|_| bots::delete(&slug));
                    match res {
                        Ok(()) => moved += 1,
                        Err(e) => failed.push(format!("{slug}: {e:#}")),
                    }
                }
                (moved, failed)
            },
            |s, (moved, failed), _| {
                s.moving = false;
                s.reload_bots();
                if failed.is_empty() {
                    s.notify(format!("Moved {moved} bot(s) to the server."), false);
                } else {
                    s.notify(
                        format!("Moved {moved}; couldn't move:\n{}", failed.join("\n")),
                        true,
                    );
                }
            },
        );
    }

    fn check_for_update(&mut self) {
        self.bg(update::latest_release, |s, res, _| {
            let Ok(tag) = res else { return };
            let newer = update::is_newer(&tag, update::current());
            let skipped = s.store.skipped_version.as_deref() == Some(tag.as_str());
            s.update = (newer && !skipped).then_some(tag);
        });
    }

    fn run_update(&mut self) {
        let Some(tag) = self.update.clone() else {
            return;
        };
        self.updating = true;
        self.bg(
            move || update::update_self(&tag).and_then(|_| update::relaunch()),
            |s, res, tx| {
                s.updating = false;
                match res {
                    Ok(()) => tx.quit(),
                    Err(e) => {
                        s.confirm = None;
                        s.notify(format!("Update failed: {e:#}"), true);
                    }
                }
            },
        );
    }

    fn install_app(&mut self) {
        self.confirm = None;
        self.bg(appbundle::install, |s, res, _| match res {
            Ok(app) => s.notify(
                format!(
                    "Added {}. Open dumBot from Spotlight or Launchpad.",
                    app.display()
                ),
                false,
            ),
            Err(e) => s.notify(format!("Couldn't add dumbot.app: {e:#}"), true),
        });
    }
}

fn setup(s: &mut App, waker: Waker) {
    s.waker = Some(waker);
    if rune::tooling::headless_run().is_some() {
        return;
    }
    if appbundle::supported()
        && !appbundle::launched_from_app()
        && !appbundle::installed()
        && !s.store.skip_app_prompt
    {
        s.confirm = Some(Confirm::InstallApp);
    }
    if let Backend::Remote(server) = &s.backend {
        crate::tunnel::carry(Some(server.clone()));
    }
    s.reload_bots();
    if let Some(slug) = s.bots.keys().next().cloned() {
        s.select(&slug);
    }
}

fn poll(s: &mut App, now: f64, tx: &mut Tx) -> bool {
    let mut changed = false;
    s.clock = now;
    while let Ok(job) = s.jobs.1.try_recv() {
        job(s, tx);
        changed = true;
    }
    let live = rune::tooling::headless_run().is_none();
    if live && s.models.is_empty() && !s.models_loading && now >= s.next_models {
        s.load_models();
    }
    if live && s.backend.is_remote() && now >= s.next_context_push {
        s.next_context_push = now + CONTEXT_PUSH_SECS;
        s.push_context();
    }
    if s.connect
        .as_ref()
        .is_some_and(|c| c.code.is_some() && !c.busy)
        && now >= s.next_pair
    {
        s.try_connect();
    }
    if live && now >= s.next_doctor && !s.issues_inflight {
        s.next_doctor = now + DOCTOR_SECS;
        s.issues_inflight = true;
        let backend = s.backend.clone();
        s.bg(
            move || backend.call::<Vec<container::Issue>>(Request::Doctor),
            |s, res, _| {
                s.issues_inflight = false;
                s.issues = res.unwrap_or_else(|e| {
                    vec![container::Issue {
                        bot: None,
                        problem: format!("Can't reach the dumBot server: {e:#}"),
                        fix: "Check that it's running (`systemctl --user status dumbot` there) \
                              and reachable, or choose Server… to run bots on this computer."
                            .into(),
                        warning: false,
                    }]
                });
            },
        );
    }
    // A start's steps show live elapsed times.
    if !s.starting.is_empty() {
        tx.wake_after(Duration::from_millis(200));
        changed = true;
    }
    // Every few seconds: re-read bots, see which runs are working, and wake the due ones.
    if live && now >= s.next_scan {
        s.next_scan = now + SCAN_SECS;
        tx.wake_after(Duration::from_secs_f64(SCAN_SECS));
        if !s.backend.is_remote() {
            s.reload_bots();
        }
        changed = true;
        if !s.active_inflight {
            s.active_inflight = true;
            let backend = s.backend.clone();
            let asked = backend.clone();
            s.bg(
                move || backend.call::<api::Status>(Request::Status),
                move |s, res, tx| {
                    s.active_inflight = false;
                    if s.backend != asked {
                        return;
                    }
                    let st = match res {
                        Ok(st) => st,
                        Err(e) => {
                            s.server_error = Some(format!("{e:#}"));
                            return;
                        }
                    };
                    s.server_error = None;
                    s.containers = st.states;
                    s.active = st.active;
                    if s.backend.is_remote() {
                        // The server wakes its bots and pauses its containers itself.
                        s.apply_bots(st.bots);
                        s.server_starting = st.starting;
                        s.default_model = st.default_model;
                        if s.selected.is_none()
                            && let Some(slug) = s.bots.keys().next().cloned()
                        {
                            s.select(&slug);
                        }
                    } else {
                        s.wake_due();
                        s.pause_idle(tx.clock());
                    }
                },
            );
        }
    }
    if live && now >= s.next_update_check {
        s.next_update_check = now + update::CHECK_EVERY_SECS;
        tx.wake_after(Duration::from_secs_f64(update::CHECK_EVERY_SECS));
        s.check_for_update();
    }
    if s.toast.is_some() {
        match s.toast_until {
            // Errors stay until dismissed, so they can be read and copied.
            None if s.toast.as_ref().is_some_and(|t| t.1) => {}
            None => {
                let secs = 4.0;
                s.toast_until = Some(now + secs);
                tx.wake_after(Duration::from_secs_f64(secs));
            }
            Some(t) if now >= t => {
                s.toast = None;
                s.toast_until = None;
                changed = true;
            }
            _ => {}
        }
    }
    // Follow the shown run: quickly while it works, slowly otherwise.
    if let (Some(slug), Some(id)) = (s.selected.clone(), s.session_id())
        && !s.chat_inflight
        && now >= s.next_chat_poll
        && !s.starting.contains(&slug)
    {
        s.chat_inflight = true;
        let id2 = id.clone();
        let backend = s.backend.clone();
        s.bg(
            move || backend.call::<Snapshot>(Request::Snapshot { slug, session: id2 }),
            move |s, res, tx| {
                s.chat_inflight = false;
                let running = match res {
                    Ok(snap) => {
                        s.chat_error = None;
                        let running = snap.running;
                        if s.chats.get(&id) != Some(&snap) {
                            s.chats.insert(id, snap);
                            s.reload_files();
                        }
                        running
                    }
                    Err(e) => {
                        s.chat_error = Some(format!("{e:#}"));
                        false
                    }
                };
                let wait = if running { 0.8 } else { 5.0 };
                s.next_chat_poll = tx.clock() + wait;
                tx.wake_after(Duration::from_secs_f64(wait));
            },
        );
    }
    changed
}

fn on_key(s: &mut App, ev: &KeyEvent, _tx: &mut Tx) -> bool {
    if shell::is_sidebar_toggle(ev) {
        s.sidebar.toggle();
        return true;
    }
    if ev.is_command('i') {
        s.inspector.toggle();
        return true;
    }
    if ev.is_command('n') {
        s.new_bot = Some(new_bot_edits());
        return true;
    }
    false
}

fn new_bot_edits() -> (TextEdit, TextEdit) {
    let mut name = TextEdit::new("");
    name.focused = true;
    (name, TextEdit::new(""))
}

// ---------------------------------------------------------------------------------------- view

fn view(s: &App, cx: &mut Cx<App>) -> El {
    let k = rune_theme_opencode::kit(cx.system_dark()).clone();
    let mut layer = Vec::new();
    let title = s.bot().map_or("dumBot".to_string(), |b| b.name.clone());
    let toolbar = Toolbar::new()
        .leading(
            text(title)
                .font(k.ty.ui_medium)
                .color(k.c.text_base)
                .truncate(),
        )
        .trailing(toolbar_actions(s, &k));
    let split = SplitView::new(main_pane(s, &k, &mut layer))
        .sidebar(
            &s.sidebar,
            |s: &mut App| &mut s.sidebar,
            sidebar(s, &k, &mut layer),
        )
        .inspector(
            &s.inspector,
            |s: &mut App| &mut s.inspector,
            agent_panel(s, &k, &mut layer),
        )
        .toolbar(toolbar)
        .titlebar_inset(cx.titlebar_inset())
        .frame_inset(8.0)
        .toolbar_in_panel()
        .sidebar_toggle(SidebarToggle::new().in_header())
        .reduced_motion(cx.is_reduced_motion())
        .build(&k);
    dialogs(s, &k, &mut layer);
    let mut root = col().w_full().h_full().child(split);
    if let Some((msg, err)) = &s.toast {
        root = root.child(toast(&k, msg, *err));
    }
    rune_kit::root(root, layer)
}

fn toast(k: &Kit, msg: &str, error: bool) -> El {
    let c = if error { &k.c.danger } else { &k.c.success };
    let card = row()
        .key("toast")
        .max_w(420.0)
        .px(14.0)
        .py(10.0)
        .gap(10.0)
        .items_center()
        .radius(8.0)
        .fill(k.c.layer_01)
        .border(1.0, c.border)
        .child(el().size(8.0, 8.0).radius(4.0).fill(c.icon))
        .child(
            text(msg.to_string())
                .font(k.ty.ui)
                .color(k.c.text_base)
                .wrap()
                .selectable(true)
                .shrink(),
        );
    let card = if error {
        let m = msg.to_string();
        card.max_w(560.0).child(copy_button(k, m)).child(
            small("Dismiss", ButtonVariant::GhostFaint)
                .on_press(|s: &mut App| s.toast = None)
                .build(k),
        )
    } else {
        card.on_press(|s: &mut App| s.toast = None)
    }
    .pin(Some(16.0), None, None, Some(16.0));
    k.elevate(card, &k.elevation.overlay)
}

/// Copies `text` to the clipboard.
fn copy_button(k: &Kit, text: String) -> El {
    small("Copy", ButtonVariant::Ghost)
        .on_press_tx(move |_: &mut App, tx| tx.copy(text.clone()))
        .build(k)
}

fn small(label: &str, v: ButtonVariant) -> Button {
    Button::new(label)
        .variant(v)
        .control_size(ControlSize::Small)
}

fn toolbar_actions(s: &App, k: &Kit) -> El {
    let mut r = row().gap(6.0).items_center();
    if s.busy || !s.starting.is_empty() {
        r = r.child(Spinner::new().build(k));
    }
    if let Some(bot) = s.bot() {
        let running = s.is_running(bot) || s.selected.as_ref().is_some_and(|x| s.is_starting(x));
        r = r
            .child(
                small("Run now", ButtonVariant::Contrast)
                    .disabled(running)
                    .on_press(|s: &mut App| s.run_now())
                    .build(k),
            )
            .child(
                small("Schedule…", ButtonVariant::Ghost)
                    .on_press(|s: &mut App| s.tab = Tab::Schedule)
                    .build(k),
            )
            .child(
                small("Environment…", ButtonVariant::Ghost)
                    .on_press(|s: &mut App| s.open_env(s.selected.clone()))
                    .build(k),
            )
            .child(
                small("Container…", ButtonVariant::Ghost)
                    .on_press(|s: &mut App| s.open_sandbox())
                    .build(k),
            )
            .child(
                small("Upload files…", ButtonVariant::Ghost)
                    .on_press_tx(|s: &mut App, tx| s.pick_upload(tx, false))
                    .build(k),
            )
            .child(
                small("Upload folder…", ButtonVariant::Ghost)
                    .on_press_tx(|s: &mut App, tx| s.pick_upload(tx, true))
                    .build(k),
            );
    }
    let open = s.inspector.is_open();
    r.child(
        IconButton::new(&shell::INSPECTOR)
            .variant(IconButtonVariant::GhostMuted)
            .expanded(open)
            .label(if open { "Hide panel" } else { "Show panel" })
            .shortcut("⌘I")
            .on_press(|s: &mut App| s.inspector.toggle())
            .build(k)
            .key("inspector-toggle"),
    )
}

/// The combobox's current id: the model's key, or "" (the default option) for none. A model
/// that isn't an option (models still loading, or since disabled) gives none, so the
/// placeholder shows instead of a blank field.
fn known(options: &[ComboboxOption], m: Option<&ModelRef>) -> Option<String> {
    let key = m.map_or(String::new(), ModelRef::key);
    options.iter().any(|o| o.id == key).then_some(key)
}

fn sidebar(s: &App, k: &Kit, layer: &mut Vec<El>) -> El {
    let now = bots::now();
    let mut rows = Vec::new();
    for (slug, bot) in &s.bots {
        let status = if s.starting.contains(slug) {
            match s.progress.get(slug).and_then(|p| p.steps.last()) {
                Some((step, at)) => format!("{step}… {}", secs(s.clock - at)),
                None => "Starting…".to_string(),
            }
        } else if let Some(step) = s.server_starting.get(slug) {
            format!("{step}…")
        } else if s.is_running(bot) {
            "Running…".to_string()
        } else {
            schedule::next_line(bot, now)
        };
        let open = slug.clone();
        rows.push(
            SidebarRow::new(bot.name.clone())
                .subtitle(status)
                .height(44.0)
                .selected(s.selected.as_ref() == Some(slug))
                .on_select(move |s: &mut App, _| s.select(&open))
                .build(k)
                .key(format!("bot/{slug}")),
        );
    }
    let mut sb = Sidebar::new()
        .label("Bots")
        .header(
            SidebarRow::new("New bot")
                .icon(&rune_kit::glyphs::SPARKLE)
                .height(32.0)
                .on_select(|s: &mut App, _| s.new_bot = Some(new_bot_edits()))
                .build(k)
                .key("new-bot"),
        )
        .section(
            SidebarSection::new("Bots")
                .key("bots")
                .count(s.bots.len())
                .rows(rows),
        );
    let mut footer = col().w_full();
    if !s.issues.is_empty() {
        let problems = s.issues.iter().filter(|i| !i.warning).count();
        let label = match (problems, s.issues.len() - problems) {
            (0, w) => format!("{w} warning{}", if w == 1 { "" } else { "s" }),
            (p, _) => format!("{p} problem{} running bots", if p == 1 { "" } else { "s" }),
        };
        footer = footer.child(
            SidebarRow::new(label)
                .icon(&rune_kit::glyphs::SPARKLE)
                .height(36.0)
                .on_select(|s: &mut App, _| s.show_issues = true)
                .build(k)
                .key("issues"),
        );
    }
    if let (Some(tag), false) = (&s.update, s.update_hidden) {
        footer = footer.child(
            SidebarRow::new(format!("Update available: {tag}"))
                .icon(&rune_kit::glyphs::SPARKLE)
                .height(36.0)
                .on_select(|s: &mut App, _| s.confirm = Some(Confirm::Update))
                .build(k)
                .key("update-available"),
        );
    }
    let mut options = vec![ComboboxOption::new("", "opencode default")];
    options.extend(s.models.iter().cloned());
    let current = known(&options, s.default_model.as_ref());
    let model = Combobox::new(
        "default-model",
        &s.default_combo,
        &options,
        |s: &mut App| &mut s.default_combo,
    )
    .label("Default model")
    .current(current.as_deref())
    .placeholder(if s.models.is_empty() {
        "Loading models…"
    } else {
        "opencode default"
    })
    .search_placeholder("Search models")
    .width(240.0)
    .max_height(360.0)
    .placement(rune_kit::Placement::TopStart)
    .disabled(s.models.is_empty())
    .on_choose(|s: &mut App, id: &str, _: &mut Tx| s.choose_default_model(id))
    .build(k, layer);
    footer = footer.child(
        col()
            .w_full()
            .px(12.0)
            .py(8.0)
            .gap(6.0)
            .child(
                text("Default model")
                    .font(k.ty.small_medium)
                    .color(k.c.text_muted),
            )
            .child(model),
    );
    footer = footer
        .child(
            SidebarRow::new("Instructions for all bots")
                .height(32.0)
                .on_select(|s: &mut App, _| s.open_instructions())
                .build(k)
                .key("instructions"),
        )
        .child(
            SidebarRow::new("Environment variables")
                .height(32.0)
                .on_select(|s: &mut App, _| s.open_env(None))
                .build(k)
                .key("environment"),
        )
        .child(
            SidebarRow::new("MCP servers for bots")
                .height(32.0)
                .on_select(|s: &mut App, _| s.open_mcp())
                .build(k)
                .key("bot-mcp"),
        )
        .child(
            SidebarRow::new(match (&s.backend, &s.server_error) {
                (Backend::Local, _) => "Bots run on this computer".to_string(),
                (Backend::Remote(r), None) => format!("Bots run on {}", r.addr),
                (Backend::Remote(r), Some(_)) => format!("Can't reach {}", r.addr),
            })
            .subtitle("Server…")
            .height(44.0)
            .on_select(|s: &mut App, _| s.open_connect())
            .build(k)
            .key("server"),
        );
    sb = sb.footer(footer);
    sb.build(k)
}

fn main_pane(s: &App, k: &Kit, layer: &mut Vec<El>) -> El {
    let (Some(slug), Some(bot)) = (s.selected.clone(), s.bot()) else {
        let msg = if s.bots.is_empty() {
            "Create a bot to get started (⌘N). Describe what it's for; it writes its own instructions and decides when to run. With opencode Browse (opencode.ai/product/browse) set up, bots can use your browser, logged in as you: most useful bots need it."
        } else {
            "Pick a bot."
        };
        return col().w_full().h_full().center().padding(24.0).child(
            text(msg)
                .font(k.ty.ui)
                .color(k.c.text_muted)
                .wrap()
                .max_w(460.0),
        );
    };
    let now = bots::now();
    let mut head = col().w_full().px(16.0).py(12.0).gap(4.0).child(
        text(schedule::describe(bot, now))
            .font(k.ty.ui_medium)
            .color(k.c.text_base)
            .wrap(),
    );
    let model = bot.model.as_ref().map_or_else(
        || {
            format!(
                "default ({})",
                s.default_model
                    .as_ref()
                    .map_or("opencode default".into(), |m| s.model_name(m))
            )
        },
        |m| s.model_name(m),
    );
    head = head.child(
        text(format!(
            "{} · model: {model} · {} runs",
            match &s.backend {
                Backend::Local => bots::dir(&slug).display().to_string(),
                Backend::Remote(r) => format!("on {}", r.addr),
            },
            bot.runs.len()
        ))
        .font(k.ty.small)
        .color(k.c.text_faint)
        .truncate(),
    );

    let mut list = col()
        .w(220.0)
        .h_full()
        .gap(2.0)
        .padding(8.0)
        .border_sides([0.0, 1.0, 0.0, 0.0], k.c.border_muted);
    for (path, _) in &s.files {
        let selected = s.file.as_ref() == Some(path);
        let p = path.clone();
        list = list.child(
            row()
                .w_full()
                .px(8.0)
                .py(6.0)
                .radius(6.0)
                .fill(if selected {
                    k.c.bg_accent
                } else {
                    k.c.layer_01.alpha(0.0)
                })
                .cursor(rune::Cursor::Pointer)
                .child(
                    text(path.clone())
                        .font(if selected {
                            k.ty.small_medium
                        } else {
                            k.ty.small
                        })
                        .color(k.c.text_base)
                        .truncate(),
                )
                .on_press(move |s: &mut App| s.file = Some(p.clone()))
                .key(format!("file/{path}")),
        );
    }
    let body = s
        .file
        .as_ref()
        .and_then(|f| s.files.iter().find(|x| &x.0 == f))
        .map_or(String::new(), |f| f.1.clone());
    let viewer = col()
        .grow(1.0)
        .h_full()
        .min_w(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(
            col()
                .w_full()
                .padding(20.0)
                .child(Markdown::new(body).selectable(true).build(k)),
        );
    let drop_slug = slug.clone();
    let delete = small("Delete bot", ButtonVariant::GhostFaint)
        .on_press(move |s: &mut App| s.confirm = Some(Confirm::Delete(slug.clone())))
        .build(k);
    col()
        .w_full()
        .h_full()
        .min_h(0.0)
        .child(
            row()
                .w_full()
                .items_start()
                .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_muted)
                .child(head.grow(1.0).min_w(0.0))
                .child(row().padding(12.0).child(delete)),
        )
        .child(
            row()
                .w_full()
                .px(16.0)
                .py(8.0)
                .border_sides([0.0, 0.0, 1.0, 0.0], k.c.border_muted)
                .child(
                    SegmentedControl::new(
                        &[
                            "Files".to_string(),
                            format!("Schedule ({})", schedule::items(bot).len()),
                        ],
                        Some((s.tab == Tab::Schedule) as usize),
                    )
                    .width(Some(260.0))
                    .on_pick(|s: &mut App, i, _: &mut Tx| {
                        s.tab = if i == 1 { Tab::Schedule } else { Tab::Files }
                    })
                    .build(k),
                ),
        )
        .on_drop_files(
            move |s: &mut App, paths: &[std::path::PathBuf], _: &mut Tx| {
                s.upload(drop_slug.clone(), paths.to_vec())
            },
        )
        .child(match s.tab {
            Tab::Files => row()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .child(list)
                .child(viewer),
            Tab::Schedule => schedule_pane(s, k, layer, bot),
        })
}

/// The bot's wake-ups, one item each, and the form to add one.
fn schedule_pane(s: &App, k: &Kit, layer: &mut Vec<El>, bot: &Bot) -> El {
    let now = bots::now();
    let heading = |t: &str| {
        text(t.to_string())
            .font(k.ty.ui_medium)
            .color(k.c.text_base)
    };
    let card = || {
        col()
            .w_full()
            .padding(12.0)
            .gap(6.0)
            .radius(8.0)
            .border(1.0, k.c.border_muted)
    };

    let mut list = col().w_full().gap(8.0);
    let items = schedule::items(bot);
    if items.is_empty() {
        list = list.child(
            text("No wake-ups. The bot runs only when you start it, until it schedules itself.")
                .font(k.ty.ui)
                .color(k.c.text_muted)
                .wrap(),
        );
    }
    for (i, item) in items.into_iter().enumerate() {
        let (kind, at, note) = schedule::item(bot, item, now);
        let mut info = col().grow(1.0).min_w(0.0).gap(2.0).child(
            row()
                .gap(8.0)
                .items_center()
                .child(text(kind).font(k.ty.ui_medium).color(k.c.text_base))
                .child(text(at).font(k.ty.small).color(k.c.text_faint)),
        );
        if let Some(n) = note {
            info = info.child(text(n).font(k.ty.small).color(k.c.text_muted).wrap());
        }
        list = list.child(
            row()
                .w_full()
                .px(12.0)
                .py(10.0)
                .gap(12.0)
                .items_center()
                .radius(8.0)
                .fill(k.c.layer_01)
                .child(info)
                .child(
                    small("Remove", ButtonVariant::GhostFaint)
                        .on_press(move |s: &mut App| s.remove_wakeup(item))
                        .build(k),
                )
                .key(format!("wake/{i}")),
        );
    }

    let f = &s.sched;
    let kind = SegmentedControl::new(&["Once", "Weekly", "Repeating"], Some(f.kind))
        .width(Some(300.0))
        .on_pick(|s: &mut App, i, _: &mut Tx| s.sched.kind = i)
        .build(k);
    let field = |label: &str, control: El| {
        row()
            .w_full()
            .gap(12.0)
            .items_center()
            .child(
                text(label.to_string())
                    .font(k.ty.small_medium)
                    .color(k.c.text_muted)
                    .w(70.0),
            )
            .child(control)
    };
    let times: Vec<ComboboxOption> = (0..96)
        .map(|q| {
            let t = format!("{:02}:{:02}", q / 4, q % 4 * 15);
            ComboboxOption::new(t.clone(), t)
        })
        .collect();
    let time = Combobox::new("wake-time", &s.time_combo, &times, |s: &mut App| {
        &mut s.time_combo
    })
    .label("Time")
    .current(Some(f.time.as_str()))
    .search_placeholder("e.g. 19:30")
    .width(140.0)
    .max_height(280.0)
    .on_choose(|s: &mut App, id: &str, _: &mut Tx| s.sched.time = id.to_string())
    .build(k, layer);
    let mut form = card().gap(12.0).child(heading("Add a wake-up")).child(kind);
    match f.kind {
        0 => {
            let days: Vec<ComboboxOption> = (0..14)
                .map(|d| ComboboxOption::new(d.to_string(), schedule::day_label(d, now)))
                .collect();
            let day = Combobox::new("wake-day", &s.day_combo, &days, |s: &mut App| {
                &mut s.day_combo
            })
            .label("Day")
            .current(Some(f.day.to_string().as_str()))
            .width(200.0)
            .max_height(280.0)
            .on_choose(|s: &mut App, id: &str, _: &mut Tx| s.sched.day = id.parse().unwrap_or(0))
            .build(k, layer);
            form = form.child(field("Day", day)).child(field("Time", time));
        }
        1 => {
            let mut toggles = row().gap(4.0);
            for (d, name) in DAY_NAMES.iter().enumerate() {
                toggles = toggles.child(
                    small(
                        name,
                        if f.days[d] {
                            ButtonVariant::Contrast
                        } else {
                            ButtonVariant::Outline
                        },
                    )
                    .on_press(move |s: &mut App| s.sched.days[d] ^= true)
                    .build(k),
                );
            }
            let preset = |label: &'static str, days: [bool; 7]| {
                small(label, ButtonVariant::Ghost)
                    .on_press(move |s: &mut App| s.sched.days = days)
                    .build(k)
            };
            let presets = row()
                .gap(4.0)
                .child(preset(
                    "Weekdays",
                    [true, true, true, true, true, false, false],
                ))
                .child(preset(
                    "Weekends",
                    [false, false, false, false, false, true, true],
                ))
                .child(preset("Every day", [true; 7]));
            form = form
                .child(field("Days", toggles))
                .child(field("", presets))
                .child(field("Time", time));
        }
        _ => {
            let n = TextInput::new(&f.every)
                .width(Some(80.0))
                .on_edit(|s: &mut App| &mut s.sched.every)
                .on_submit(|s: &mut App, _| s.add_wakeup())
                .build(k);
            let unit = SegmentedControl::new(&UNITS.map(|u| u.0), Some(f.unit))
                .width(Some(260.0))
                .on_pick(|s: &mut App, i, _: &mut Tx| s.sched.unit = i)
                .build(k);
            form = form.child(field(
                "Every",
                row().gap(8.0).items_center().child(n).child(unit),
            ));
        }
    }
    let note = TextInput::new(&f.note)
        .placeholder("Optional: what this run should do")
        .width(None)
        .on_edit(|s: &mut App| &mut s.sched.note)
        .on_submit(|s: &mut App, _| s.add_wakeup())
        .build(k);
    form = form.child(field("Note", note.grow(1.0))).child(
        row().w_full().child(row().grow(1.0)).child(
            Button::new("Add wake-up")
                .variant(ButtonVariant::Contrast)
                .on_press(|s: &mut App| s.add_wakeup())
                .build(k),
        ),
    );

    col()
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(
            col()
                .w_full()
                .max_w(720.0)
                .padding(20.0)
                .gap(16.0)
                .child(heading("Wake-ups"))
                .child(
                    text(format!(
                        "{}. Bots also schedule themselves with `dumbot wake` as they work.",
                        schedule::next_line(bot, now)
                    ))
                    .font(k.ty.small)
                    .color(k.c.text_faint)
                    .wrap(),
                )
                .child(list)
                .child(form),
        )
}

fn agent_panel(s: &App, k: &Kit, layer: &mut Vec<El>) -> El {
    let Some(bot) = s.bot() else {
        return col().w_full().h_full().padding(16.0).center().child(
            text("Pick a bot to see its runs.")
                .font(k.ty.ui)
                .color(k.c.text_muted)
                .wrap(),
        );
    };
    let id = s.session_id();
    let snap = id.as_ref().and_then(|id| s.chats.get(id));
    let running = snap.is_some_and(|c| c.running);
    let now = bots::now();

    // Which run: newest first.
    let runs: Vec<ComboboxOption> = bot
        .runs
        .iter()
        .rev()
        .map(|r| {
            let mut o = ComboboxOption::new(r.session.clone(), schedule::when(r.started, now));
            o.detail = r.reason.chars().take(60).collect();
            o
        })
        .collect();
    let picker = Combobox::new("run", &s.run_combo, &runs, |s: &mut App| &mut s.run_combo)
        .label("Run")
        .current(id.as_deref())
        .placeholder("No runs yet")
        .search_placeholder("Search runs")
        .width(260.0)
        .max_height(360.0)
        .disabled(runs.is_empty())
        .on_choose(|s: &mut App, id: &str, _: &mut Tx| {
            s.run = Some(id.to_string());
            s.next_chat_poll = 0.0;
        })
        .build(k, layer);
    let head = row()
        .w_full()
        .px(12.0)
        .py(8.0)
        .gap(8.0)
        .items_center()
        .child(text("Run").font(k.ty.small_medium).color(k.c.text_muted))
        .child(picker);

    let mut transcript = col().w_full().gap(10.0).padding(12.0);
    if let Some(u) = snap.map(|c| &c.usage) {
        transcript = transcript.child(
            text(usage_line(s, u))
                .font(k.ty.small)
                .color(k.c.text_faint)
                .wrap()
                .key("usage"),
        );
    }
    if let Some(e) = s.chat_error.as_ref().filter(|_| snap.is_none()) {
        transcript = transcript.child(
            text(format!("Can't read this run: {e}"))
                .font(k.ty.small)
                .color(k.c.danger.fg)
                .wrap()
                .selectable(true),
        );
    }
    if let Some(slug) = &s.selected
        && let Some(p) = s.progress.get(slug)
    {
        transcript = transcript.child(progress_view(s, k, p, s.starting.contains(slug)));
    }
    match snap {
        None if s.selected.as_ref().is_some_and(|x| s.starting.contains(x)) => {}
        None if id.is_none() => {
            transcript = transcript.child(
                text("This bot hasn't run yet. Run it now, or message it below to start a run with your message.")
                    .font(k.ty.ui)
                    .color(k.c.text_muted)
                    .wrap(),
            );
        }
        None => transcript = transcript.child(Spinner::new().build(k)),
        Some(snap) => {
            for (i, e) in snap.entries.iter().enumerate() {
                let e = match e {
                    Entry::User(t) if t.starts_with(bots::RUN_PREFIX) => {
                        let first = t.lines().next().unwrap_or_default();
                        transcript = transcript.child(
                            text(first.trim_start_matches(bots::RUN_PREFIX).to_string())
                                .font(k.ty.small_medium)
                                .color(k.c.text_faint)
                                .wrap()
                                .key(format!("r{i}")),
                        );
                        match t.split_once(bots::MESSAGE_MARK) {
                            Some((_, m)) => Entry::User(m.to_string()),
                            None => continue,
                        }
                    }
                    e => e.clone(),
                };
                transcript = transcript.child(entry(k, &e).key(format!("e{i}")));
            }
            for p in &snap.permissions {
                transcript = transcript.child(permission(k, p));
            }
            if running {
                transcript = transcript.child(
                    row()
                        .gap(8.0)
                        .items_center()
                        .child(Spinner::new().build(k))
                        .child(text("Working…").font(k.ty.small).color(k.c.text_faint)),
                );
            }
        }
    }

    let mut options = vec![ComboboxOption::new("", "Default model")];
    options.extend(s.models.iter().cloned());
    let current = known(&options, bot.model.as_ref());
    let model = Combobox::new("bot-model", &s.model_combo, &options, |s: &mut App| {
        &mut s.model_combo
    })
    .label("Model")
    .current(current.as_deref())
    .search_placeholder("Search models")
    .width(240.0)
    .max_height(320.0)
    .placement(rune_kit::Placement::TopStart)
    .disabled(s.models.is_empty())
    .on_choose(|s: &mut App, id: &str, _: &mut Tx| s.choose_bot_model(id))
    .build(k, layer);
    let model_row = row()
        .w_full()
        .gap(8.0)
        .items_center()
        .child(
            text("Bot model")
                .font(k.ty.small_medium)
                .color(k.c.text_muted),
        )
        .child(model);
    let mut actions = row().gap(6.0).items_center().child(row().grow(1.0));
    if id.is_some() {
        actions = actions.child(
            small("New run", ButtonVariant::Outline)
                .disabled(running)
                .on_press(|s: &mut App| s.run_now())
                .build(k),
        );
    }
    if running {
        actions = actions.child(
            small("Stop", ButtonVariant::Danger)
                .on_press(|s: &mut App| s.session_call(SessionAction::Interrupt))
                .build(k),
        );
    }
    actions = actions.child(
        small("Send", ButtonVariant::Contrast)
            .disabled(s.busy || s.composer.text.trim().is_empty())
            .on_press(|s: &mut App| s.send_composer())
            .build(k),
    );
    let composer = col()
        .w_full()
        .gap(8.0)
        .padding(12.0)
        .border_sides([1.0, 0.0, 0.0, 0.0], k.c.border_muted)
        .child(model_row)
        .child(
            TextArea::new(&s.composer)
                .placeholder("Message this run… (Enter to send, Shift-Enter for a newline)")
                .auto_grow(3, 12)
                .width(None)
                .on_edit(|s: &mut App| &mut s.composer)
                .on_submit(|s: &mut App, _| s.send_composer())
                .build(k),
        )
        .child(actions);
    col()
        .w_full()
        .h_full()
        .min_h(0.0)
        .child(head)
        .child(
            col()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .scroll(ScrollSpec { stick_bottom: true })
                .child(transcript),
        )
        .child(composer)
}

fn secs(t: f64) -> String {
    format!("{:.1}s", t.max(0.0))
}

/// A start's steps with how long each took: live while it's starting, then a one-line summary
/// (or the error) above the run.
fn progress_view(s: &App, k: &Kit, p: &Progress, live: bool) -> El {
    let end = p.done.as_ref().map_or(s.clock, |d| d.0);
    let took = |i: usize| p.steps.get(i + 1).map_or(end, |x| x.1) - p.steps[i].1;
    let total = p.steps.first().map_or(0.0, |f| end - f.1);
    if !live {
        let slowest = (0..p.steps.len())
            .max_by(|a, b| took(*a).total_cmp(&took(*b)))
            .map(|i| {
                format!(
                    " (slowest: {} {})",
                    p.steps[i].0.to_lowercase(),
                    secs(took(i))
                )
            })
            .unwrap_or_default();
        return match p.done.as_ref().map(|d| &d.1) {
            Some(Err(e)) => {
                let msg = format!("Start failed after {}: {e}", secs(total));
                col()
                    .w_full()
                    .gap(4.0)
                    .child(
                        text(msg.clone())
                            .font(k.ty.small)
                            .color(k.c.danger.fg)
                            .wrap()
                            .selectable(true),
                    )
                    .child(copy_button(k, msg))
                    .key("progress")
            }
            _ => text(format!("Started in {}{slowest}", secs(total)))
                .font(k.ty.small)
                .color(k.c.text_faint)
                .wrap()
                .key("progress"),
        };
    }
    let mut list = col().w_full().gap(4.0);
    for (i, (step, _)) in p.steps.iter().enumerate() {
        let current = i + 1 == p.steps.len();
        let mut r = row().w_full().gap(8.0).items_center();
        r = if current {
            r.child(Spinner::new().build(k))
        } else {
            r.child(text("✓").font(k.ty.small).color(k.c.text_faint))
        };
        list = list.child(
            r.child(
                text(step.clone())
                    .font(if current {
                        k.ty.small_medium
                    } else {
                        k.ty.small
                    })
                    .color(if current {
                        k.c.text_base
                    } else {
                        k.c.text_muted
                    })
                    .wrap()
                    .grow(1.0)
                    .min_w(0.0),
            )
            .child(text(secs(took(i))).font(k.ty.mono).color(k.c.text_faint))
            .key(format!("step{i}")),
        );
    }
    col()
        .w_full()
        .padding(10.0)
        .gap(6.0)
        .radius(8.0)
        .fill(k.c.layer_01)
        .child(
            text(format!("Starting the run · {}", secs(total)))
                .font(k.ty.small_medium)
                .color(k.c.text_muted),
        )
        .child(list)
        .key("progress")
}

/// `Claude Haiku · 15.7k in (15.7k cached) · 33 out · $0.002`.
fn usage_line(s: &App, u: &opencode::Usage) -> String {
    let n = |v: u64| match v {
        0..1_000 => v.to_string(),
        1_000..1_000_000 => format!("{:.1}k", v as f64 / 1e3),
        _ => format!("{:.2}M", v as f64 / 1e6),
    };
    let model = u
        .model
        .as_ref()
        .map_or("model not chosen yet".into(), |m| s.model_name(m));
    let input = u.input + u.cache_read + u.cache_write;
    let cached = u.cache_read + u.cache_write;
    let mut line = format!("{model} · {} in", n(input));
    if cached > 0 {
        line.push_str(&format!(" ({} cached)", n(cached)));
    }
    line.push_str(&format!(" · {} out", n(u.output + u.reasoning)));
    if u.cost > 0.0 {
        line.push_str(&format!(" · ${:.3}", u.cost));
    }
    line
}

fn entry(k: &Kit, e: &Entry) -> El {
    match e {
        Entry::User(t) => row().w_full().child(row().grow(1.0)).child(
            col()
                .max_w(380.0)
                .px(12.0)
                .py(8.0)
                .radius(10.0)
                .fill(k.c.bg_accent)
                .child(
                    text(t.clone())
                        .font(k.ty.body)
                        .color(k.c.text_base)
                        .wrap()
                        .selectable(true),
                ),
        ),
        Entry::Text(t) => col()
            .w_full()
            .child(Markdown::new(t.clone()).selectable(true).build(k)),
        Entry::Reasoning(t) => text(t.clone())
            .font(k.ty.small)
            .color(k.c.text_faint)
            .wrap()
            .max_lines(3),
        Entry::Tool {
            name,
            summary,
            status,
        } => {
            let ink = if status == "error" {
                k.c.danger.fg
            } else {
                k.c.text_muted
            };
            row()
                .w_full()
                .gap(6.0)
                .child(text(format!("▸ {name}")).font(k.ty.small_medium).color(ink))
                .child(
                    text(summary.clone())
                        .font(k.ty.mono)
                        .color(k.c.text_faint)
                        .truncate()
                        .grow(1.0),
                )
        }
        Entry::Shell { command, status } => text(format!("$ {command}  ({status})"))
            .font(k.ty.mono)
            .color(k.c.text_muted)
            .truncate(),
        Entry::Error(e) => text(e.clone())
            .font(k.ty.small)
            .color(k.c.danger.fg)
            .wrap()
            .selectable(true),
    }
}

fn permission(k: &Kit, p: &opencode::Permission) -> El {
    let btn = |label: &'static str, decision: &'static str, v: ButtonVariant| {
        let id = p.id.clone();
        small(label, v)
            .on_press(move |s: &mut App| {
                let id = id.clone();
                s.session_call(SessionAction::Permission {
                    request: id.clone(),
                    decision: decision.into(),
                });
            })
            .build(k)
    };
    let what = if p.message.is_empty() {
        format!("{}: {}", p.action, p.resources.join(", "))
    } else {
        p.message.clone()
    };
    col()
        .w_full()
        .padding(10.0)
        .gap(8.0)
        .radius(8.0)
        .fill(k.c.warning.bg)
        .child(
            text(format!("Permission requested — {what}"))
                .font(k.ty.ui)
                .color(k.c.warning.fg)
                .wrap(),
        )
        .child(
            row()
                .gap(6.0)
                .child(btn("Allow once", "once", ButtonVariant::Contrast))
                .child(btn("Always", "always", ButtonVariant::Outline))
                .child(btn("Reject", "reject", ButtonVariant::Ghost)),
        )
        .key(format!("perm/{}", p.id))
}

fn env_dialog(s: &App, e: &EnvEdit, k: &Kit, layer: &mut Vec<El>) {
    let mine = e
        .bot
        .as_ref()
        .map_or(&e.env.global, |b| e.env.bots.get(b).unwrap_or(&EMPTY_VARS));
    let mut rows = col().w_full().gap(4.0);
    for name in mine.keys() {
        let n = name.clone();
        rows = rows.child(
            row()
                .w_full()
                .gap(8.0)
                .items_center()
                .child(
                    text(name.clone())
                        .font(k.ty.mono)
                        .color(k.c.text_base)
                        .grow(1.0),
                )
                .child(text("set").font(k.ty.small).color(k.c.text_faint))
                .child(
                    small("Remove", ButtonVariant::GhostFaint)
                        .on_press(move |s: &mut App| s.remove_env(&n))
                        .build(k),
                )
                .key(format!("env/{name}")),
        );
    }
    if e.bot.is_some() {
        for name in e.env.global.keys().filter(|n| !mine.contains_key(*n)) {
            rows = rows.child(
                row()
                    .w_full()
                    .gap(8.0)
                    .child(
                        text(name.clone())
                            .font(k.ty.mono)
                            .color(k.c.text_faint)
                            .grow(1.0),
                    )
                    .child(text("global").font(k.ty.small).color(k.c.text_faint))
                    .key(format!("env/global/{name}")),
            );
        }
    }
    if mine.is_empty() && (e.bot.is_none() || e.env.global.is_empty()) {
        rows = rows.child(
            text("No variables yet.")
                .font(k.ty.small)
                .color(k.c.text_faint),
        );
    }
    let name = TextInput::new(&e.name)
        .placeholder("Name, e.g. API_TOKEN")
        .width(None)
        .on_edit(|s: &mut App| &mut s.env_edit.as_mut().expect("dialog open").name)
        .on_submit(|s: &mut App, _| s.add_env())
        .build(k);
    let value = TextInput::new(&e.value)
        .placeholder("Value")
        .secure()
        .width(None)
        .on_edit(|s: &mut App| &mut s.env_edit.as_mut().expect("dialog open").value)
        .on_submit(|s: &mut App, _| s.add_env())
        .build(k);
    let add = small("Add", ButtonVariant::Outline)
        .on_press(|s: &mut App| s.add_env())
        .build(k);
    let done = Button::new("Done")
        .variant(ButtonVariant::Contrast)
        .on_press(close_dialogs)
        .build(k);
    let (title, desc) = match &e.bot {
        None => (
            "Environment variables".to_string(),
            "Every bot's shell commands, scripts and subagents get these. Bots see only the names; values are hidden from them, even in command output. A bot's own variable of the same name wins.",
        ),
        Some(b) => (
            format!(
                "{} environment",
                s.bots.get(b).map_or(b.as_str(), |x| x.name.as_str())
            ),
            "This bot's shell commands, scripts and subagents get these as well as the global ones. Bots see only the names; values are hidden from them, even in command output.",
        ),
    };
    modal(
        "environment",
        &title,
        Some(desc),
        vec![
            rows,
            name,
            value,
            row().w_full().child(row().grow(1.0)).child(add),
        ],
        vec![done],
        k,
        layer,
    );
}

fn toggle_row(
    k: &Kit,
    title: &str,
    about: &str,
    caveat: Option<&str>,
    on: bool,
    flip: impl Fn(&mut App) + 'static,
) -> El {
    let mut info = col().grow(1.0).min_w(0.0).gap(2.0).child(
        text(title.to_string())
            .font(k.ty.ui_medium)
            .color(k.c.text_base),
    );
    if !about.is_empty() {
        info = info.child(
            text(about.to_string())
                .font(k.ty.small)
                .color(k.c.text_muted)
                .wrap(),
        );
    }
    if let Some(c) = caveat {
        info = info.child(
            text(format!("Warning: {c}."))
                .font(k.ty.small)
                .color(k.c.warning.fg)
                .wrap(),
        );
    }
    row().w_full().gap(12.0).items_center().child(info).child(
        small(
            if on { "On" } else { "Off" },
            if on {
                ButtonVariant::Contrast
            } else {
                ButtonVariant::Outline
            },
        )
        .on_press(flip)
        .build(k),
    )
}

fn sandbox_dialog(s: &App, e: &SandboxEdit, k: &Kit, layer: &mut Vec<El>) {
    let mut body = vec![label(k, "Tools")];
    for t in container::TOOLS {
        let on = e.sandbox.has_tool(t.id);
        let id = t.id;
        body.push(
            toggle_row(k, t.name, t.about, None, on, move |s: &mut App| {
                let tools = &mut s.sandbox_edit.as_mut().expect("dialog open").sandbox.tools;
                if let Some(i) = tools.iter().position(|x| x == id) {
                    tools.remove(i);
                } else {
                    tools.push(id.into());
                }
            })
            .key(format!("tool/{id}")),
        );
    }
    body.push(label(k, "MCP servers from your opencode config"));
    if e.servers.is_empty() {
        body.push(
            text("Your opencode config has no MCP servers.")
                .font(k.ty.small)
                .color(k.c.text_faint),
        );
    }
    for (name, caveat) in &e.servers {
        let on = e.sandbox.mcp.contains(name);
        let n = name.clone();
        body.push(
            toggle_row(k, name, "", caveat.as_deref(), on, move |s: &mut App| {
                let mcp = &mut s.sandbox_edit.as_mut().expect("dialog open").sandbox.mcp;
                if let Some(i) = mcp.iter().position(|x| x == &n) {
                    mcp.remove(i);
                } else {
                    mcp.push(n.clone());
                }
            })
            .key(format!("mcp/{name}")),
        );
    }
    body.push(label(
        k,
        "MCP servers only this bot gets (JSON, opencode's mcp format)",
    ));
    if !e.global_mcp.is_empty() {
        body.push(
            text(format!(
                "It also gets every bot's: {}.",
                e.global_mcp.join(", ")
            ))
            .font(k.ty.small)
            .color(k.c.text_faint)
            .wrap(),
        );
    }
    body.push(
        TextArea::new(&e.own_mcp)
            .placeholder("{ \"notes\": { \"type\": \"remote\", \"url\": \"https://…/mcp\" } }")
            .auto_grow(2, 10)
            .width(None)
            .on_edit(|s: &mut App| &mut s.sandbox_edit.as_mut().expect("dialog open").own_mcp)
            .build(k),
    );
    body.push(label(
        k,
        "More folders (read-write, same path; one per line)",
    ));
    body.push(
        TextArea::new(&e.mounts)
            .placeholder("~/projects/notes")
            .auto_grow(2, 8)
            .width(None)
            .on_edit(|s: &mut App| &mut s.sandbox_edit.as_mut().expect("dialog open").mounts)
            .build(k),
    );
    let save = Button::new("Save")
        .variant(ButtonVariant::Contrast)
        .on_press(|s: &mut App| s.save_sandbox())
        .build(k);
    let name = s
        .bots
        .get(&e.slug)
        .map_or(e.slug.as_str(), |b| b.name.as_str());
    modal(
        "sandbox",
        &format!("{name} container"),
        Some(
            "Every run happens in this bot's Docker container, with opencode, its folder and your opencode config. Turn on what else it needs. Changes take effect on its next run.",
        ),
        body,
        vec![cancel(k), save],
        k,
        layer,
    );
}

fn issues_dialog(s: &App, k: &Kit, layer: &mut Vec<El>) {
    let mut body = Vec::new();
    if s.issues.is_empty() {
        body.push(text("No issues.").font(k.ty.ui).color(k.c.text_muted));
    }
    for (i, issue) in s.issues.iter().enumerate() {
        let who = issue.bot.as_ref().map_or("All bots".to_string(), |b| {
            s.bots.get(b).map_or(b.clone(), |x| x.name.clone())
        });
        let tone = if issue.warning {
            k.c.warning.fg
        } else {
            k.c.danger.fg
        };
        body.push(
            col()
                .w_full()
                .gap(4.0)
                .padding(10.0)
                .radius(8.0)
                .fill(k.c.layer_01)
                .child(text(who).font(k.ty.small_medium).color(tone))
                .child(
                    text(issue.problem.clone())
                        .font(k.ty.small)
                        .color(k.c.text_base)
                        .wrap()
                        .selectable(true),
                )
                .child(
                    text(issue.fix.clone())
                        .font(k.ty.small)
                        .color(k.c.text_muted)
                        .wrap()
                        .selectable(true),
                )
                .child(copy_button(k, format!("{}\n{}", issue.problem, issue.fix)))
                .key(format!("issue/{i}")),
        );
    }
    let recheck = Button::new("Check again")
        .variant(ButtonVariant::Ghost)
        .on_press(|s: &mut App| s.next_doctor = 0.0)
        .build(k);
    let done = Button::new("Done")
        .variant(ButtonVariant::Contrast)
        .on_press(close_dialogs)
        .build(k);
    modal(
        "issues",
        "Running bots",
        Some(
            "What's keeping bots from running, or something they use from working. `dumbot doctor` shows the same.",
        ),
        vec![
            col()
                .w_full()
                .gap(8.0)
                .max_h(480.0)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .children(body),
        ],
        vec![recheck, done],
        k,
        layer,
    );
}

fn connect_dialog(s: &App, c: &ConnectEdit, k: &Kit, layer: &mut Vec<El>) {
    let mut body = vec![
        label(k, "Server address"),
        TextInput::new(&c.addr)
            .placeholder("192.168.1.20 or myserver:7457")
            .width(None)
            .on_edit(|s: &mut App| &mut s.connect.as_mut().expect("dialog open").addr)
            .on_submit(|s: &mut App, _| s.try_connect())
            .build(k),
    ];
    if let Some(code) = &c.code {
        let cmd = format!("dumbot server approve {code}");
        body.push(
            col()
                .w_full()
                .gap(6.0)
                .padding(10.0)
                .radius(8.0)
                .fill(k.c.layer_01)
                .child(
                    text("The server doesn't know this app yet. To pair them, run this on the server:")
                        .font(k.ty.ui)
                        .color(k.c.text_base)
                        .wrap(),
                )
                .child(
                    row()
                        .w_full()
                        .gap(8.0)
                        .items_center()
                        .child(
                            text(cmd.clone())
                                .font(k.ty.mono)
                                .color(k.c.text_base)
                                .selectable(true)
                                .grow(1.0),
                        )
                        .child(copy_button(k, cmd)),
                )
                .child(
                    text(format!(
                        "The code comes from both machines' keys, so it only matches if nothing is in between. Server key: {}…",
                        c.pinned
                            .as_ref()
                            .and_then(|p| p.fingerprint.as_deref())
                            .map_or("", |f| &f[..16])
                    ))
                    .font(k.ty.small)
                    .color(k.c.text_faint)
                    .wrap(),
                )
                .child(
                    row()
                        .gap(8.0)
                        .items_center()
                        .child(Spinner::new().build(k))
                        .child(
                            text("Waiting for approval…")
                                .font(k.ty.small)
                                .color(k.c.text_faint),
                        ),
                ),
        );
    }
    if let Some(e) = &c.error {
        body.push(
            text(e.clone())
                .font(k.ty.small)
                .color(k.c.danger.fg)
                .wrap()
                .selectable(true),
        );
    }
    let mut actions = Vec::new();
    if s.backend.is_remote() {
        actions.push(
            Button::new("Run bots on this computer")
                .variant(ButtonVariant::Ghost)
                .on_press(|s: &mut App| s.disconnect())
                .build(k),
        );
    }
    actions.push(cancel(k));
    actions.push(
        Button::new(if c.busy { "Connecting…" } else { "Connect" })
            .variant(ButtonVariant::Contrast)
            .disabled(c.busy || c.code.is_some())
            .on_press(|s: &mut App| s.try_connect())
            .build(k),
    );
    modal(
        "server",
        "Where bots run",
        Some(
            "Bots run on this computer, or on a dumBot server (`dumbot install-server` on that machine). Connected to a server, this app manages its bots, and the server runs them on schedule while the app is closed, with your opencode config and logins.",
        ),
        body,
        actions,
        k,
        layer,
    );
}

static EMPTY_VARS: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();

fn close_dialogs(s: &mut App) {
    s.confirm = None;
    s.new_bot = None;
    s.instructions = None;
    s.env_edit = None;
    s.show_issues = false;
    s.sandbox_edit = None;
    s.connect = None;
    s.mcp_edit = None;
}

fn modal(
    key: &str,
    title: &str,
    description: Option<&str>,
    body: Vec<El>,
    actions: Vec<El>,
    k: &Kit,
    layer: &mut Vec<El>,
) {
    let mut footer = row().w_full().gap(8.0).pt(4.0).child(row().grow(1.0));
    for a in actions {
        footer = footer.child(a);
    }
    let mut content = col().w_full().px(16.0).pb(16.0).gap(12.0);
    if let Some(desc) = description {
        content = content.child(
            text(desc.to_string())
                .font(k.ty.ui)
                .color(k.c.text_muted)
                .wrap()
                .mt(-6.0),
        );
    }
    Dialog::new(key, title)
        .open(true)
        .fit()
        .width(500.0)
        .child(content.children(body).child(footer))
        .on_close(close_dialogs)
        .build(k, layer);
}

fn cancel(k: &Kit) -> El {
    Button::new("Cancel")
        .variant(ButtonVariant::Ghost)
        .on_press(close_dialogs)
        .build(k)
}

fn label(k: &Kit, t: &str) -> El {
    text(t.to_string())
        .font(k.ty.small_medium)
        .color(k.c.text_muted)
}

fn dialogs(s: &App, k: &Kit, layer: &mut Vec<El>) {
    if let Some(t) = &s.instructions {
        let area = TextArea::new(t)
            .placeholder("What every bot should know: services you run and how to use them, which environment variables hold what, your time zone, house rules…")
            .auto_grow(10, 24)
            .width(None)
            .on_edit(|s: &mut App| s.instructions.as_mut().expect("dialog open"))
            .build(k);
        let save = Button::new("Save")
            .variant(ButtonVariant::Contrast)
            .on_press(|s: &mut App| s.save_instructions())
            .build(k);
        modal(
            "instructions",
            "Instructions for all bots",
            Some(
                "Part of every bot's system prompt, and its subagents'. Takes effect from each bot's next run.",
            ),
            vec![area],
            vec![cancel(k), save],
            k,
            layer,
        );
    }
    if let Some(e) = &s.env_edit {
        env_dialog(s, e, k, layer);
    }
    if let Some(e) = &s.sandbox_edit {
        sandbox_dialog(s, e, k, layer);
    }
    if s.show_issues {
        issues_dialog(s, k, layer);
    }
    if let Some(c) = &s.connect {
        connect_dialog(s, c, k, layer);
    }
    if let Some(e) = &s.mcp_edit {
        let area = TextArea::new(&e.text)
            .placeholder("{\n  \"gotify\": { \"type\": \"remote\", \"url\": \"https://…/mcp\", \"headers\": { \"Authorization\": \"Bearer …\" } },\n  \"tool\": { \"type\": \"local\", \"command\": [\"npx\", \"-y\", \"some-mcp\"] }\n}")
            .auto_grow(10, 24)
            .width(None)
            .on_edit(|s: &mut App| &mut s.mcp_edit.as_mut().expect("dialog open").text)
            .build(k);
        let save = Button::new("Save")
            .variant(ButtonVariant::Contrast)
            .on_press(|s: &mut App| s.save_mcp())
            .build(k);
        modal(
            "bot-mcp",
            "MCP servers for bots",
            Some(
                "Every bot gets these, as well as the MCP servers from your opencode config it's turned on (Container…), and its own. A JSON object of servers by name, in opencode's mcp format. Local servers run inside the bot's container.",
            ),
            vec![area],
            vec![cancel(k), save],
            k,
            layer,
        );
    }
    if let Some((name, purpose)) = &s.new_bot {
        let name = TextInput::new(name)
            .placeholder("Inbox triager")
            .width(None)
            .on_edit(|s: &mut App| &mut s.new_bot.as_mut().expect("dialog open").0)
            .build(k);
        let purpose = TextArea::new(purpose)
            .placeholder("What should it do? e.g. Every morning, check my Gmail in the browser, archive newsletters and write me a summary of what needs a reply.")
            .auto_grow(5, 14)
            .width(None)
            .on_edit(|s: &mut App| &mut s.new_bot.as_mut().expect("dialog open").1)
            .build(k);
        let create = Button::new("Create and run")
            .variant(ButtonVariant::Contrast)
            .on_press(|s: &mut App| s.create_bot())
            .build(k);
        modal(
            "new-bot",
            "New bot",
            Some(
                "It starts with this purpose in its AGENTS.md, then rewrites its own instructions and picks its schedule on its first run. Bots use your browser (logged in as you) through browse, so describe sites by name: \"my ESPN fantasy team\".",
            ),
            vec![label(k, "Name"), name, label(k, "Purpose"), purpose],
            vec![cancel(k), create],
            k,
            layer,
        );
    }
    match &s.confirm {
        Some(Confirm::MoveBots(slugs)) => {
            let names: Vec<String> = slugs.clone();
            let ok = Button::new(if s.moving {
                "Moving…"
            } else {
                "Move to the server"
            })
            .variant(ButtonVariant::Contrast)
            .disabled(s.moving)
            .on_press(move |s: &mut App| s.move_bots(names.clone()))
            .build(k);
            let keep = Button::new("Keep them here")
                .variant(ButtonVariant::Ghost)
                .on_press(close_dialogs)
                .build(k);
            modal(
                "move-bots",
                &format!(
                    "Move {} bot{} to the server?",
                    slugs.len(),
                    if slugs.len() == 1 { "" } else { "s" }
                ),
                Some(&format!(
                    "This computer has {}. Each one's folder (AGENTS.md, memory, scripts, skills), schedule, settings and own environment variables are copied to the server, then removed here. Run history stays behind. Bots left here don't run while the app is connected to a server.",
                    slugs.join(", ")
                )),
                vec![],
                vec![keep, ok],
                k,
                layer,
            );
        }
        Some(Confirm::Delete(slug)) => {
            let slug = slug.clone();
            let name = s.bots.get(&slug).map_or(slug.clone(), |b| b.name.clone());
            let ok = Button::new("Delete")
                .variant(ButtonVariant::Danger)
                .on_press(move |s: &mut App| s.delete_bot(&slug))
                .build(k);
            modal(
                "delete",
                &format!("Delete {name}?"),
                Some("Its folder, AGENTS.md, memory, container and run history are removed."),
                vec![],
                vec![cancel(k), ok],
                k,
                layer,
            );
        }
        Some(Confirm::InstallApp) => {
            let skip = Button::new("Don't ask again")
                .variant(ButtonVariant::Ghost)
                .on_press(|s: &mut App| {
                    s.store.skip_app_prompt = true;
                    s.save();
                    s.confirm = None;
                })
                .build(k);
            let ok = Button::new("Add to Applications")
                .variant(ButtonVariant::Contrast)
                .on_press(|s: &mut App| s.install_app())
                .build(k);
            modal(
                "install-app",
                "Add dumBot to Applications?",
                Some(
                    "Creates ~/Applications/dumbot.app so you can open dumBot from Spotlight, Launchpad or the Dock.",
                ),
                vec![],
                vec![skip, cancel(k), ok],
                k,
                layer,
            );
        }
        Some(Confirm::Update) => {
            let Some(tag) = s.update.clone() else { return };
            let notes = format!("https://github.com/{}/releases/tag/{tag}", update::REPO);
            let desc = format!(
                "dumBot {tag} is available (you have v{}).",
                update::current()
            );
            let skip_tag = tag.clone();
            let mut actions = vec![
                Button::new("Skip this version")
                    .variant(ButtonVariant::Ghost)
                    .on_press(move |s: &mut App| {
                        s.store.skipped_version = Some(skip_tag.clone());
                        s.save();
                        s.update = None;
                        s.confirm = None;
                    })
                    .build(k),
                Button::new("Later")
                    .variant(ButtonVariant::Ghost)
                    .on_press(|s: &mut App| {
                        s.update_hidden = true;
                        s.confirm = None;
                    })
                    .build(k),
                Button::new("Release notes")
                    .variant(ButtonVariant::Ghost)
                    .on_press_tx(move |_: &mut App, tx| {
                        let _ = tx.open_url(&notes);
                    })
                    .build(k),
            ];
            {
                actions.push(
                    Button::new(if s.updating {
                        "Updating…"
                    } else {
                        "Update and restart"
                    })
                    .variant(ButtonVariant::Contrast)
                    .disabled(s.updating)
                    .on_press(|s: &mut App| s.run_update())
                    .build(k),
                );
            }
            modal(
                "update",
                "Update available",
                Some(&desc),
                vec![],
                actions,
                k,
                layer,
            );
        }
        None => {}
    }
}

/// Headless-only sample data (`--scene demo`).
fn demo(s: &mut App) {
    let now = bots::now();
    let bot = |name: &str, next: Option<i64>, every: Option<i64>| Bot {
        name: name.into(),
        next_run: next,
        every_next: every.map(|e| now + e),
        every,
        ..Bot::default()
    };
    let mut inbox = bot("Inbox triager", Some(now + 7200), Some(7200));
    inbox.note = Some("Check whether Dana replied about the lease.".into());
    inbox.runs = vec![Run {
        session: "ses_demo".into(),
        started: now - 600,
        reason: "scheduled wake-up".into(),
    }];
    s.bots.insert("inbox-triager".into(), inbox);
    let mut ff = bot("Fantasy football", None, None);
    for (r, note) in [
        ("on wed,thu at 19:00", Some("Pre-lock lineup check")),
        ("on sat at 09:00", None),
        ("on sun at 11:30", Some("Final lineup before kickoff")),
    ] {
        let note = note.map(str::to_string);
        schedule::apply(&mut ff, schedule::parse(r, now).unwrap(), note, now);
    }
    ff.next_run = Some(now + 3 * 3600);
    ff.note = Some("Verify the waiver claims went through".into());
    s.bots.insert("fantasy-football".into(), ff);
    s.bots
        .insert("weekly-review".into(), bot("Weekly review", None, None));
    s.selected = Some("inbox-triager".into());
    s.files = vec![
        (
            "AGENTS.md".into(),
            "# Inbox triager\n\n## Purpose\n\nKeep my Gmail inbox under control.\n\n## How I work\n\n- Open Gmail with the browse tools.\n- Archive newsletters; label receipts.\n- Summarize what needs a reply in `memory/today.md`.\n- Wake every 2h during the day.".into(),
        ),
        ("memory/seen.md".into(), "# Seen\n\n- 2026-11-24: 41 threads".into()),
        ("memory/today.md".into(), "# Needs a reply\n\n1. Dana: lease renewal".into()),
    ];
    s.file = Some("AGENTS.md".into());
    s.models = ["Claude Opus", "Claude Haiku", "GPT-6", "Gemini 4"]
        .iter()
        .map(|n| {
            let mut o = ComboboxOption::new(format!("demo/{n}"), *n);
            o.detail = "demo".into();
            o
        })
        .collect();
    s.chats.insert(
        "ses_demo".into(),
        Snapshot {
            entries: vec![
                Entry::User(format!("{}scheduled wake-up\n\n…", bots::RUN_PREFIX)),
                Entry::Tool { name: "browse_navigate".into(), summary: "https://mail.google.com".into(), status: "completed".into() },
                Entry::Text("Archived 12 newsletters. One new thread needs a reply; added it to `memory/today.md`.".into()),
                Entry::Shell { command: "dumbot wake inbox-triager in 2h".into(), status: "completed".into() },
            ],
            running: false,
            permissions: Vec::new(),
            usage: opencode::Usage {
                model: Some(ModelRef::parse("demo/Claude Haiku").unwrap()),
                input: 1_204,
                output: 2_310,
                cache_read: 48_200,
                cost: 0.041,
                ..Default::default()
            },
        },
    );
}

pub fn main() {
    let _ = rune::set_app_icon(include_bytes!("../assets/icon.png").to_vec());
    let spec = AppSpec {
        scenes: &[
            ("demo", demo),
            ("new-bot", |s: &mut App| {
                demo(s);
                s.new_bot = Some(new_bot_edits());
            }),
            ("empty", |_: &mut App| {}),
            ("starting", |s: &mut App| {
                demo(s);
                s.clock = 10.0;
                s.starting.insert("inbox-triager".into());
                s.progress.insert(
                    "inbox-triager".into(),
                    Progress {
                        steps: vec![
                            ("Preparing the bot's folder".into(), 1.0),
                            ("Checking Docker".into(), 1.1),
                            (
                                "Building the bot image: [stage-3 5/8] RUN curl …".into(),
                                1.4,
                            ),
                        ],
                        done: None,
                    },
                );
                s.issues = vec![container::Issue {
                    bot: Some("inbox-triager".into()),
                    problem: "Uses the GitHub CLI, but it isn't logged in on this computer.".into(),
                    fix: "Run `gh auth login`.".into(),
                    warning: true,
                }];
            }),
            ("container", |s: &mut App| {
                demo(s);
                s.sandbox_edit = Some(SandboxEdit {
                    slug: "inbox-triager".into(),
                    sandbox: bots::Sandbox {
                        tools: vec!["gh".into()],
                        mcp: vec!["sentry".into()],
                        mounts: vec![],
                    },
                    mounts: TextEdit::new("~/projects/notes"),
                    servers: vec![
                        ("sentry".into(), None),
                        (
                            "railway".into(),
                            Some("runs `railway` inside the container, which has only the image's programs, not your computer's".into()),
                        ),
                    ],
                    own_mcp: TextEdit::new("{\n  \"notes\": { \"type\": \"remote\", \"url\": \"https://notes.example/mcp\" }\n}"),
                    global_mcp: vec!["gotify".into()],
                });
            }),
            ("environment", |s: &mut App| {
                demo(s);
                let mut e = EnvEdit::open(Some("inbox-triager".into()), env::Env::default());
                e.env.global.insert("API_TOKEN".into(), "x".into());
                e.env.global.insert("SERVICE_URL".into(), "x".into());
                e.env
                    .scope(Some("inbox-triager"))
                    .insert("GMAIL_LABEL".into(), "x".into());
                e.env
                    .scope(Some("inbox-triager"))
                    .insert("SERVICE_URL".into(), "y".into());
                e.value = TextEdit::new("x".repeat(400));
                s.env_edit = Some(e);
            }),
            ("server", |s: &mut App| {
                demo(s);
                s.connect = Some(ConnectEdit {
                    addr: TextEdit::new("192.168.86.36"),
                    pinned: Some(Server {
                        addr: "192.168.86.36:7457".into(),
                        fingerprint: Some(
                            "415049c3b42b285f90c47104df3f812c5c81365ac588ff090c6b840e3a3ef8db"
                                .into(),
                        ),
                    }),
                    code: Some("JM9X-4G3W".into()),
                    busy: false,
                    error: None,
                });
            }),
            ("move-bots", |s: &mut App| {
                demo(s);
                s.backend = Backend::Remote(Server {
                    addr: "192.168.86.36:7457".into(),
                    fingerprint: None,
                });
                s.confirm = Some(Confirm::MoveBots(vec![
                    "inbox-triager".into(),
                    "fantasy-football".into(),
                ]));
            }),
            ("bot-mcp", |s: &mut App| {
                demo(s);
                s.mcp_edit = Some(McpEdit {
                    text: TextEdit::new(
                        "{\n  \"gotify\": {\n    \"type\": \"remote\",\n    \"url\": \"https://gotify.example/mcp\"\n  }\n}",
                    ),
                });
            }),
            ("instructions", |s: &mut App| {
                demo(s);
                s.instructions = Some(TextEdit::new("I'm in US Eastern time."));
            }),
            ("weekly", |s: &mut App| {
                demo(s);
                s.selected = Some("fantasy-football".into());
                s.tab = Tab::Schedule;
                s.sched.kind = 1;
                s.sched.days = [false, false, false, false, false, true, true];
                s.sched.time = "10:00".into();
            }),
            ("schedule-once", |s: &mut App| {
                demo(s);
                s.tab = Tab::Schedule;
                s.time_combo.step(rune_kit::ComboboxEvent::Toggle {
                    current: Some("09:00"),
                });
            }),
            ("default-model", |s: &mut App| {
                demo(s);
                s.default_combo
                    .step(rune_kit::ComboboxEvent::Toggle { current: None });
            }),
        ],
        setup: Some(setup),
        poll: Some(poll),
        on_key: Some(on_key),
        fonts: rune_theme_opencode::FONTS,
        families: rune_theme_opencode::FAMILIES,
        min_size: Some((900.0, 560.0)),
        ..AppSpec::new("dumBot", (1440.0, 900.0), init, view)
    };
    let chrome = Chrome {
        full_size_content: true,
        traffic_lights: Some((16.0, 25.0)),
        drag_regions: true,
    };
    rune::App::new(spec).chrome(chrome).modifier_keys().run();
}

#[cfg(test)]
mod live_tests {
    use super::*;
    use crate::opencode::Client;

    /// Hits the local opencode service: `cargo test -- --ignored`. Creates a bot in a temp
    /// data dir and opens a session in its folder, as a run does, without prompting a model.
    #[test]
    #[ignore]
    fn starts_a_session_in_a_bots_folder() {
        let tmp = std::env::temp_dir().join(format!("dumbot-it-{}", std::process::id()));
        // SAFETY: the only test that touches the environment.
        unsafe { std::env::set_var("XDG_DATA_HOME", &tmp) };
        let slug = bots::create("Live test", "Do nothing.").unwrap();
        assert!(bots::list().contains_key(&slug));
        assert!(bots::preamble("test", &[]).starts_with(bots::RUN_PREFIX));
        bots::prepare(&slug, &bots::load(&slug).unwrap()).unwrap();
        assert!(bots::dir(&slug).join(".git").exists());
        assert_eq!(bots::files(&slug)[0].0, "AGENTS.md");
        let oc = Client::connect().unwrap();
        let id = oc
            .create_session("dumbot live test", &bots::dir(&slug), bots::AGENT, None)
            .unwrap();
        bots::add_run(
            &slug,
            Run {
                session: id.clone(),
                started: bots::now(),
                reason: "test".into(),
            },
        )
        .unwrap();
        assert_eq!(bots::load(&slug).unwrap().last_session(), Some(id.as_str()));
        oc.active_sessions().unwrap();
        oc.snapshot(&id).unwrap();
        std::fs::remove_dir_all(tmp).unwrap();
    }
}
