//! dumBot: bots on the left, the selected bot's markdown in the middle, and its opencode runs
//! on the right. Bots wake on their own schedules while the app runs.

mod appbundle;
mod bots;
mod cli;
mod opencode;
mod schedule;
mod store;
mod update;

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
    IconButton, IconButtonVariant, Kit, Markdown, Spinner, TextArea, TextInput,
};

use bots::{Bot, Run};
use opencode::{Client, Entry, Snapshot};
use store::{ModelRef, Store};

type Job = Box<dyn FnOnce(&mut App, &mut Tx) + Send>;

/// How often bots' folders are re-read and running sessions checked, so schedules set by
/// `dumbot wake` and file changes show up.
const SCAN_SECS: f64 = 5.0;

enum Confirm {
    InstallApp,
    Update,
    Delete(String),
}

struct App {
    store: Store,
    jobs: (Sender<Job>, Receiver<Job>),
    waker: Option<Waker>,
    oc: Option<Client>,
    oc_error: Option<String>,
    oc_connecting: bool,
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
    schedule_edit: Option<(TextEdit, TextEdit)>,
    toast: Option<(String, bool)>,
    toast_until: Option<f64>,
    update: Option<String>,
    update_hidden: bool,
    updating: bool,
    next_update_check: f64,
}

fn init() -> App {
    App {
        store: Store::load(),
        jobs: channel(),
        waker: None,
        oc: None,
        oc_error: None,
        oc_connecting: false,
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
        schedule_edit: None,
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

    /// The opencode client once connected; starts connecting in the background otherwise.
    fn client(&mut self) -> Option<Client> {
        if self.oc.is_none() && !self.oc_connecting {
            self.oc_connecting = true;
            self.bg(Client::connect, |s, res, tx| {
                s.oc_connecting = false;
                match res {
                    Ok(c) => {
                        s.oc = Some(c);
                        s.oc_error = None;
                        s.next_chat_poll = 0.0;
                        s.next_scan = 0.0;
                        tx.wake_after(Duration::from_millis(1));
                    }
                    Err(e) => s.oc_error = Some(e.to_string()),
                }
            });
        }
        self.oc.clone()
    }

    fn select(&mut self, slug: &str) {
        self.selected = Some(slug.to_string());
        self.run = None;
        self.file = None;
        self.composer = TextEdit::new("");
        self.next_chat_poll = 0.0;
        self.reload_files();
    }

    fn reload_files(&mut self) {
        let Some(slug) = &self.selected else {
            self.files.clear();
            return;
        };
        let files = bots::files(slug);
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

    /// Re-reads every bot from disk.
    fn reload_bots(&mut self) {
        let bots = bots::list();
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
            let res = bots::update(&slug, |b| {
                schedule::fired(b, now);
            });
            match res {
                Ok(bot) => {
                    let reason = match &bot.note {
                        Some(n) => format!("scheduled wake-up ({n})"),
                        None => "scheduled wake-up".into(),
                    };
                    self.bots.insert(slug.clone(), bot);
                    self.start_run(slug, reason, None);
                }
                Err(e) => self.notify(format!("Couldn't wake {slug}: {e:#}"), true),
            }
        }
    }

    /// Starts a new run of a bot in a fresh session, with `message` from the user if any.
    fn start_run(&mut self, slug: String, reason: String, message: Option<String>) {
        let Some(bot) = self.bots.get(&slug).cloned() else {
            return;
        };
        let default = self.store.model.clone();
        let oc = self.client();
        self.starting.insert(slug.clone());
        let s2 = slug.clone();
        self.bg(
            move || -> anyhow::Result<String> {
                let oc = match oc {
                    Some(c) => c,
                    None => Client::connect()?,
                };
                bots::start_run(&oc, &s2, &bot, &reason, message.as_deref(), default)
            },
            move |s, res, tx| {
                s.starting.remove(&slug);
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
        self.session_call(move |oc, id| oc.prompt(id, &text));
    }

    fn session_call(
        &mut self,
        f: impl FnOnce(&Client, &str) -> anyhow::Result<()> + Send + 'static,
    ) {
        let (Some(oc), Some(id)) = (self.client(), self.session_id()) else {
            return;
        };
        self.bg(
            move || f(&oc, &id),
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

    fn load_models(&mut self) {
        let Some(oc) = self.oc.clone() else { return };
        self.models_loading = true;
        self.bg(
            move || oc.models(),
            |s, res, _| {
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
                    Err(e) => s.notify(format!("Couldn't list opencode models: {e}"), true),
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
        self.store.model = ModelRef::parse(key);
        self.save();
        // Bots on the default model switch their latest run too.
        let Some(m) = self.store.model.clone() else {
            return;
        };
        let sessions: Vec<String> = self
            .bots
            .values()
            .filter(|b| b.model.is_none())
            .filter_map(|b| b.last_session().map(str::to_string))
            .collect();
        self.switch_model(sessions, m);
    }

    /// Switches sessions to `m`, so their next turn (a message, or the rest of a working
    /// run) already uses it.
    fn switch_model(&mut self, sessions: Vec<String>, m: ModelRef) {
        let Some(oc) = self.client() else { return };
        if sessions.is_empty() {
            return;
        }
        self.bg(
            move || sessions.iter().try_for_each(|id| oc.set_model(id, &m)),
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
        match bots::update(&slug, |b| b.model = model) {
            Ok(b) => {
                let effective = b.model.clone().or_else(|| self.store.model.clone());
                if let (Some(id), Some(m)) = (b.last_session(), effective) {
                    self.switch_model(vec![id.to_string()], m);
                }
                self.bots.insert(slug, b);
            }
            Err(e) => self.notify(e.to_string(), true),
        }
    }

    fn create_bot(&mut self) {
        let Some((name, purpose)) = &self.new_bot else {
            return;
        };
        if purpose.text.trim().is_empty() {
            return self.notify("Describe what the bot is for.", true);
        }
        match bots::create(&name.text, &purpose.text) {
            Ok(slug) => {
                self.new_bot = None;
                self.reload_bots();
                self.select(&slug);
                self.start_run(
                    slug,
                    "first run. You were just created: read your purpose, work out how you'll \
                     do the job, rewrite AGENTS.md to match, do a first pass of the work if it \
                     makes sense, and schedule your next run."
                        .into(),
                    None,
                );
            }
            Err(e) => self.notify(format!("{e:#}"), true),
        }
    }

    fn save_schedule(&mut self) {
        let (Some(slug), Some((when, note))) = (self.selected.clone(), &self.schedule_edit) else {
            return;
        };
        let now = bots::now();
        let note = Some(note.text.trim().to_string()).filter(|n| !n.is_empty());
        let res = schedule::parse(&when.text, now)
            .and_then(|sched| bots::update(&slug, |b| schedule::apply(b, sched, note, now)));
        match res {
            Ok(b) => {
                self.bots.insert(slug, b);
                self.schedule_edit = None;
                self.next_scan = 0.0;
            }
            Err(e) => self.notify(format!("{e:#}"), true),
        }
    }

    fn delete_bot(&mut self, slug: &str) {
        self.confirm = None;
        if let Err(e) = bots::delete(slug) {
            self.notify(format!("{e:#}"), true);
        }
        self.reload_bots();
    }

    fn open_folder(&mut self) {
        if let Some(slug) = &self.selected
            && let Err(e) = std::process::Command::new("open")
                .arg(bots::dir(slug))
                .spawn()
        {
            self.notify(format!("Couldn't open the folder: {e}"), true);
        }
    }

    fn open_terminal(&mut self) {
        let (Some(slug), Some(id)) = (self.selected.clone(), self.session_id()) else {
            return;
        };
        let cmd = format!("cd '{}' && opencode -s {id}", bots::dir(&slug).display());
        let script = format!(
            "tell application \"Terminal\"\nactivate\ndo script \"{}\"\nend tell",
            cmd.replace('"', "\\\"")
        );
        if let Err(e) = std::process::Command::new("osascript")
            .args(["-e", &script])
            .spawn()
        {
            self.notify(format!("Couldn't open Terminal: {e}"), true);
        }
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
        let Some(prefix) = update::brew_prefix() else {
            return;
        };
        self.updating = true;
        self.bg(
            move || update::brew_upgrade(&prefix).and_then(|_| update::relaunch()),
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
    s.reload_bots();
    if let Some(slug) = s.bots.keys().next().cloned() {
        s.select(&slug);
    }
    s.client();
}

fn poll(s: &mut App, now: f64, tx: &mut Tx) -> bool {
    let mut changed = false;
    while let Ok(job) = s.jobs.1.try_recv() {
        job(s, tx);
        changed = true;
    }
    let live = rune::tooling::headless_run().is_none();
    if s.oc.is_some() && s.models.is_empty() && !s.models_loading {
        s.load_models();
    }
    // Every few seconds: re-read bots, see which runs are working, and wake the due ones.
    if live && now >= s.next_scan {
        s.next_scan = now + SCAN_SECS;
        tx.wake_after(Duration::from_secs_f64(SCAN_SECS));
        s.reload_bots();
        changed = true;
        if let Some(oc) = s.client()
            && !s.active_inflight
        {
            {
                s.active_inflight = true;
                s.bg(
                    move || oc.active_sessions(),
                    |s, res, _| {
                        s.active_inflight = false;
                        if let Ok(active) = res {
                            s.active = active;
                            s.wake_due();
                        }
                    },
                );
            }
        }
    }
    if live && now >= s.next_update_check {
        s.next_update_check = now + update::CHECK_EVERY_SECS;
        tx.wake_after(Duration::from_secs_f64(update::CHECK_EVERY_SECS));
        s.check_for_update();
    }
    if s.toast.is_some() {
        match s.toast_until {
            None => {
                let secs = if s.toast.as_ref().is_some_and(|t| t.1) {
                    8.0
                } else {
                    4.0
                };
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
    if let Some(id) = s.session_id()
        && !s.chat_inflight
        && now >= s.next_chat_poll
        && let Some(oc) = s.client()
    {
        s.chat_inflight = true;
        s.bg(
            {
                let id = id.clone();
                move || oc.snapshot(&id)
            },
            move |s, res, tx| {
                s.chat_inflight = false;
                let running = match res {
                    Ok(snap) => {
                        let running = snap.running;
                        if s.chats.get(&id) != Some(&snap) {
                            s.chats.insert(id, snap);
                            s.reload_files();
                        }
                        running
                    }
                    Err(_) => false,
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
    let split = SplitView::new(main_pane(s, &k))
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
                .shrink(),
        )
        .pin(Some(16.0), None, None, Some(16.0))
        .on_press(|s: &mut App| s.toast = None);
    k.elevate(card, &k.elevation.overlay)
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
        let running =
            s.is_running(bot) || s.selected.as_ref().is_some_and(|x| s.starting.contains(x));
        r = r
            .child(
                small("Run now", ButtonVariant::Contrast)
                    .disabled(running)
                    .on_press(|s: &mut App| s.run_now())
                    .build(k),
            )
            .child(
                small("Schedule…", ButtonVariant::Ghost)
                    .on_press(|s: &mut App| {
                        let mut when = TextEdit::new("");
                        when.focused = true;
                        let note =
                            TextEdit::new(s.bot().and_then(|b| b.note.clone()).unwrap_or_default());
                        s.schedule_edit = Some((when, note));
                    })
                    .build(k),
            )
            .child(
                small("Open folder", ButtonVariant::Ghost)
                    .on_press(|s: &mut App| s.open_folder())
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
            "Starting…".to_string()
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
    let current = known(&options, s.store.model.as_ref());
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
    sb = sb.footer(footer);
    sb.build(k)
}

fn main_pane(s: &App, k: &Kit) -> El {
    let (Some(slug), Some(bot)) = (s.selected.clone(), s.bot()) else {
        let msg = if s.bots.is_empty() {
            "Create a bot to get started (⌘N). Describe what it's for; it writes its own instructions and decides when to run."
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
            .color(k.c.text_base),
    );
    if let Some(n) = &bot.note {
        head = head.child(
            text(format!("Note for next run: {n}"))
                .font(k.ty.small)
                .color(k.c.text_muted)
                .wrap(),
        );
    }
    let model = bot.model.as_ref().map_or_else(
        || {
            format!(
                "default ({})",
                s.store
                    .model
                    .as_ref()
                    .map_or("opencode default".into(), |m| s.model_name(m))
            )
        },
        |m| s.model_name(m),
    );
    head = head.child(
        text(format!(
            "{} · model: {model} · {} runs",
            bots::dir(&slug).display(),
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
                .child(head.grow(1.0))
                .child(row().padding(12.0).child(delete)),
        )
        .child(
            row()
                .w_full()
                .grow(1.0)
                .min_h(0.0)
                .child(list)
                .child(viewer),
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
    if let Some(e) = &s.oc_error {
        transcript = transcript.child(
            text(format!("opencode unavailable: {e}"))
                .font(k.ty.small)
                .color(k.c.danger.fg)
                .wrap(),
        );
    }
    match snap {
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
        actions = actions
            .child(
                small("Open in terminal", ButtonVariant::Ghost)
                    .on_press(|s: &mut App| s.open_terminal())
                    .build(k),
            )
            .child(
                small("New run", ButtonVariant::Outline)
                    .disabled(running)
                    .on_press(|s: &mut App| s.run_now())
                    .build(k),
            );
    }
    if running {
        actions = actions.child(
            small("Stop", ButtonVariant::Danger)
                .on_press(|s: &mut App| s.session_call(|oc, id| oc.interrupt(id)))
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
        Entry::Error(e) => text(e.clone()).font(k.ty.small).color(k.c.danger.fg).wrap(),
    }
}

fn permission(k: &Kit, p: &opencode::Permission) -> El {
    let btn = |label: &'static str, decision: &'static str, v: ButtonVariant| {
        let id = p.id.clone();
        small(label, v)
            .on_press(move |s: &mut App| {
                let id = id.clone();
                s.session_call(move |oc, sid| oc.reply_permission(sid, &id, decision));
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

fn close_dialogs(s: &mut App) {
    s.confirm = None;
    s.new_bot = None;
    s.schedule_edit = None;
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
                "It starts with this purpose in its AGENTS.md, then rewrites its own instructions and picks its schedule on its first run.",
            ),
            vec![label(k, "Name"), name, label(k, "Purpose"), purpose],
            vec![cancel(k), create],
            k,
            layer,
        );
    }
    if let Some((when, note)) = &s.schedule_edit {
        let when = TextInput::new(when)
            .placeholder("in 2h · at 09:00 · every 6h · on wed,thu at 19:00 · off")
            .width(None)
            .on_edit(|s: &mut App| &mut s.schedule_edit.as_mut().expect("dialog open").0)
            .on_submit(|s: &mut App, _| s.save_schedule())
            .build(k);
        let note = TextInput::new(note)
            .placeholder("Optional note for the next run")
            .width(None)
            .on_edit(|s: &mut App| &mut s.schedule_edit.as_mut().expect("dialog open").1)
            .on_submit(|s: &mut App, _| s.save_schedule())
            .build(k);
        let save = Button::new("Save")
            .variant(ButtonVariant::Contrast)
            .on_press(|s: &mut App| s.save_schedule())
            .build(k);
        let current = s
            .bot()
            .map(|b| schedule::describe(b, bots::now()))
            .unwrap_or_default();
        modal(
            "schedule",
            "Schedule",
            Some(&format!(
                "{current}. Bots usually set this themselves with `dumbot wake`."
            )),
            vec![label(k, "When"), when, label(k, "Note"), note],
            vec![cancel(k), save],
            k,
            layer,
        );
    }
    match &s.confirm {
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
                Some("Its folder, AGENTS.md and memory are removed. Its opencode sessions stay."),
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
            let brew = update::brew_prefix().is_some();
            let notes = format!("https://github.com/{}/releases/tag/{tag}", update::REPO);
            let desc = if brew {
                format!(
                    "dumBot {tag} is available (you have v{}).",
                    update::current()
                )
            } else {
                format!(
                    "dumBot {tag} is available (you have v{}). This copy wasn't installed with Homebrew; download it from the release page.",
                    update::current()
                )
            };
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
            if brew {
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
    for r in ["on wed,thu at 19:00", "on sat at 09:00", "on sun at 11:30"] {
        schedule::apply(&mut ff, schedule::parse(r, now).unwrap(), None, now);
    }
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

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if appbundle::cli(&args) || cli::run(&args) {
        return;
    }
    let _ = rune::set_app_icon(include_bytes!("../assets/icon.png").to_vec());
    let spec = AppSpec {
        scenes: &[
            ("demo", demo),
            ("new-bot", |s: &mut App| {
                demo(s);
                s.new_bot = Some(new_bot_edits());
            }),
            ("empty", |_: &mut App| {}),
            ("weekly", |s: &mut App| {
                demo(s);
                s.selected = Some("fantasy-football".into());
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
        assert!(bots::preamble(&bots::load(&slug).unwrap(), "test").starts_with(bots::RUN_PREFIX));
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
