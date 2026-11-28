//! Every run happens in its bot's Docker container, `dumbot-<slug>`, running `opencode serve`.
//!
//! - **Image** (`dumbot-runner:<hash>`): built on this computer from `docker/Dockerfile`, so
//!   it's for this computer's architecture. The hash covers the Dockerfile, dumBot's version
//!   (and its source, for development builds), and the opencode and browse versions installed
//!   on the host; when any of them changes, a new image is built.
//! - **Container**: created on a bot's first run and kept. Between runs it's paused (frozen,
//!   not stopped), so the next run starts at once. It's recreated when its image or settings
//!   change, which a label on it records.
//! - **Mounts**: a persistent home of its own (opencode's sessions live there), the host's
//!   opencode config (models, auth, agents, skills), the bot's folder and a file with its
//!   environment variables, at the same paths as on the host, plus the tools and folders the
//!   bot opted in to (`bots::Sandbox`).
//!
//! Inside, `dumbot` is a Linux build of this command line: from the release matching this
//! version, or built from this checkout for development builds.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};

use crate::bots::{self, Bot};
use crate::opencode::Client;
use crate::store;

/// Reports what a slow operation is doing now ("Building image"), for the app to show.
pub type Step<'a> = &'a (dyn Fn(&str) + Sync);

const IMAGE: &str = "dumbot-runner";
const DOCKERFILE: &str = include_str!("docker/Dockerfile");
/// opencode's port inside the container (bridge networking).
const PORT: u16 = 4096;
const READY_TIMEOUT: Duration = Duration::from_secs(90);

/// A tool a bot can opt in to, set up from the host's own.
pub struct Tool {
    pub id: &'static str,
    pub name: &'static str,
    pub about: &'static str,
}

pub const TOOLS: &[Tool] = &[
    Tool {
        id: "gh",
        name: "GitHub CLI",
        about: "gh, logged in with your host's `gh auth token`; git pushes to GitHub use it too.",
    },
    Tool {
        id: "browse",
        name: "Browse",
        about: "Your browser, through the browse relay running on this computer.",
    },
];

/// Something wrong with running bots, and what to do about it.
#[derive(Debug, Clone, PartialEq)]
pub struct Issue {
    /// `None` for everything.
    pub bot: Option<String>,
    pub problem: String,
    pub fix: String,
    /// Bots still run; something they opted in to may not work.
    pub warning: bool,
}

// ---- docker -------------------------------------------------------------------------------

fn docker(args: &[&str]) -> Result<String> {
    let out = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("run docker (is Docker installed?)")?;
    if !out.status.success() {
        return Err(anyhow!(
            "docker {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Docker's server version, or why it can't be used.
pub fn check_docker() -> Result<String> {
    docker(&["version", "--format", "{{.Server.Version}}"])
        .map_err(|e| anyhow!("Docker isn't available: {e:#}"))
}

fn name(slug: &str) -> String {
    format!("dumbot-{slug}")
}

/// Docker Desktop (and the like) runs containers in a VM: they reach the host through
/// `host.docker.internal` and publish ports to it. On Linux they share the host's network.
fn host_network() -> bool {
    !cfg!(target_os = "macos")
}

fn short_hash(parts: &[&str]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    parts.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn state_dir(slug: &str) -> PathBuf {
    store::data_dir().join("containers").join(slug)
}

fn opencode_config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("opencode")
}

fn command_output(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !s.is_empty()).then_some(s)
}

/// `2.2.7` from `opencode v2.2.7`.
fn version_of(cmd: &str, args: &[&str]) -> Option<String> {
    let v = command_output(cmd, args)?;
    let v = v.split_whitespace().last()?.trim_start_matches('v');
    Some(v.to_string())
}

// ---- image --------------------------------------------------------------------------------

/// Where `dumbot` for the image comes from.
enum CliSource {
    /// This checkout, by path.
    Local(PathBuf),
    /// The GitHub release of this version.
    Release,
}

fn cli_source() -> CliSource {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let local = dir.join("Cargo.toml").exists() && dir.join("src/main.rs").exists();
    match std::env::var("DUMBOT_CLI_SOURCE").as_deref() {
        Ok("release") => CliSource::Release,
        _ if local => CliSource::Local(dir),
        _ => CliSource::Release,
    }
}

/// The contents of everything that goes into building the command line from source.
fn source_fingerprint(dir: &Path) -> String {
    fn walk(at: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        let Ok(entries) = std::fs::read_dir(at) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if let Ok(b) = std::fs::read(&p) {
                out.push((p, b));
            }
        }
    }
    let mut files = Vec::new();
    walk(&dir.join("src"), &mut files);
    for f in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml"] {
        if let Ok(b) = std::fs::read(dir.join(f)) {
            files.push((dir.join(f), b));
        }
    }
    files.sort();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    files.hash(&mut h);
    format!("{:016x}", h.finish())
}

struct ImageSpec {
    tag: String,
    args: Vec<(String, String)>,
    context: PathBuf,
}

fn image_spec() -> ImageSpec {
    let (source, context, fingerprint) = match cli_source() {
        CliSource::Local(dir) => {
            let f = source_fingerprint(&dir);
            ("local", dir, f)
        }
        CliSource::Release => {
            let empty = store::data_dir().join("containers/.build");
            ("release", empty, String::new())
        }
    };
    let args = vec![
        ("CLI_SOURCE".to_string(), source.to_string()),
        (
            "DUMBOT_VERSION".into(),
            env!("CARGO_PKG_VERSION").to_string(),
        ),
        (
            "OPENCODE_VERSION".into(),
            version_of("opencode", &["--version"]).unwrap_or_default(),
        ),
        (
            "BROWSE_VERSION".into(),
            browse_bin()
                .and_then(|b| version_of(&b, &["version"]))
                .unwrap_or_default(),
        ),
    ];
    let mut parts: Vec<&str> = vec![DOCKERFILE, &fingerprint];
    for (k, v) in &args {
        parts.push(k);
        parts.push(v);
    }
    ImageSpec {
        tag: format!("{IMAGE}:{}", short_hash(&parts)),
        args,
        context,
    }
}

static BUILD: Mutex<()> = Mutex::new(());

fn build_log() -> PathBuf {
    store::data_dir().join("containers/build.log")
}

fn build_error() -> PathBuf {
    store::data_dir().join("containers/build-error.txt")
}

/// The image's tag, building it first when it's missing (only one build at a time).
pub fn ensure_image(step: Step) -> Result<String> {
    let spec = image_spec();
    let _lock = BUILD.lock().unwrap_or_else(|e| e.into_inner());
    if docker(&["image", "inspect", "--format", "{{.Id}}", &spec.tag]).is_ok() {
        return Ok(spec.tag);
    }
    step("Building the bot image (first run after an update takes a few minutes)");
    std::fs::create_dir_all(&spec.context)?;
    std::fs::create_dir_all(build_log().parent().unwrap())?;
    let mut args: Vec<String> = vec![
        "build".into(),
        "--progress=plain".into(),
        "--label".into(),
        "dumbot.runner=1".into(),
        "-t".into(),
        spec.tag.clone(),
        "-f".into(),
        "-".into(),
    ];
    for (k, v) in &spec.args {
        args.push("--build-arg".into());
        args.push(format!("{k}={v}"));
    }
    args.push(spec.context.display().to_string());
    let mut child = Command::new("docker")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("run docker build")?;
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(DOCKERFILE.as_bytes())?;
    let mut log = std::fs::File::create(build_log())?;
    let mut tail: Vec<String> = Vec::new();
    let mut last_step = Instant::now() - Duration::from_secs(1);
    for line in BufReader::new(child.stderr.take().expect("piped"))
        .lines()
        .map_while(Result::ok)
    {
        let _ = writeln!(log, "{line}");
        // `#7 [stage 3/9] RUN apt-get …`: the build's current step.
        if let Some(rest) = line.split_once(" [").map(|x| x.1)
            && last_step.elapsed() > Duration::from_millis(250)
        {
            last_step = Instant::now();
            let what: String = rest.chars().take(90).collect();
            step(&format!("Building the bot image: [{what}"));
        }
        tail.push(line);
        if tail.len() > 30 {
            tail.remove(0);
        }
    }
    let status = child.wait()?;
    if !status.success() {
        let msg = format!(
            "Building the bot image failed (full log: {}):\n{}",
            build_log().display(),
            tail.join("\n")
        );
        let _ = std::fs::write(build_error(), &msg);
        return Err(anyhow!(msg));
    }
    let _ = std::fs::remove_file(build_error());
    // Older images: anything labelled ours that isn't this one, unless a container uses it.
    if let Ok(old) = docker(&[
        "image",
        "ls",
        "--filter",
        "label=dumbot.runner=1",
        "--format",
        "{{.Repository}}:{{.Tag}}",
    ]) {
        for t in old.lines().filter(|t| *t != spec.tag) {
            let _ = docker(&["image", "rm", t]);
        }
    }
    Ok(spec.tag)
}

// ---- host tools ---------------------------------------------------------------------------

fn browse_bin() -> Option<String> {
    let local = home().join(".local/bin/browse");
    if local.exists() {
        return Some(local.display().to_string());
    }
    command_output("sh", &["-c", "command -v browse"])
}

/// The host's browse relay port, starting the relay when it isn't running.
fn browse_port() -> Result<u16> {
    let bin = browse_bin().ok_or_else(|| anyhow!("browse isn't installed on this computer"))?;
    let port = || -> Option<u16> {
        let v: Value = serde_json::from_str(&command_output(&bin, &["status", "--json"])?).ok()?;
        let endpoint = v["relay"]["endpoint"].as_str()?;
        endpoint
            .rsplit(':')
            .next()?
            .trim_end_matches('/')
            .parse()
            .ok()
    };
    if let Some(p) = port() {
        return Ok(p);
    }
    Command::new(&bin)
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start the browse relay")?;
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(250));
        if let Some(p) = port() {
            return Ok(p);
        }
    }
    Err(anyhow!(
        "the browse relay isn't running; run `browse status` on this computer"
    ))
}

fn gh_token() -> Result<String> {
    command_output("gh", &["auth", "token"])
        .ok_or_else(|| anyhow!("`gh auth token` failed on this computer; run `gh auth login`"))
}

/// The host's time zone (`America/Chicago`), so bots' local times match yours.
fn time_zone() -> Option<String> {
    if let Ok(tz) = std::env::var("TZ")
        && !tz.is_empty()
    {
        return Some(tz);
    }
    let link = std::fs::read_link("/etc/localtime").ok()?;
    let s = link.to_string_lossy().into_owned();
    s.split_once("zoneinfo/").map(|x| x.1.to_string())
}

// ---- MCP servers --------------------------------------------------------------------------

/// Removes `//` and `/* */` comments and trailing commas, so JSONC parses as JSON.
fn strip_jsonc(s: &str) -> String {
    let (mut out, b) = (String::with_capacity(s.len()), s.as_bytes());
    let (mut i, mut in_str) = (0, false);
    while i < b.len() {
        let c = b[i] as char;
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < b.len() {
                out.push(b[i + 1] as char);
                i += 1;
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
            out.push(c);
        } else if s[i..].starts_with("//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        } else if s[i..].starts_with("/*") {
            i = s[i + 2..].find("*/").map_or(b.len(), |e| i + 4 + e);
            continue;
        } else if c == ',' {
            let next = s[i + 1..].trim_start();
            if !(next.starts_with('}') || next.starts_with(']')) {
                out.push(c);
            }
        } else {
            out.push(c);
        }
        i += 1;
    }
    // Multi-byte characters were pushed byte by byte above; rebuild from bytes.
    let bytes: Vec<u8> = out.chars().map(|c| c as u32 as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The MCP servers in the host's opencode config, by name: under `mcp.servers`, and the
/// older form, directly under `mcp`.
pub fn host_mcp() -> BTreeMap<String, Value> {
    let dir = opencode_config_dir();
    let mut out = BTreeMap::new();
    for f in ["opencode.jsonc", "opencode.json", "config.json"] {
        let Ok(raw) = std::fs::read_to_string(dir.join(f)) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(&strip_jsonc(&raw)) else {
            continue;
        };
        for (k, v) in v["mcp"].as_object().into_iter().flatten() {
            if v.get("type").is_some() {
                out.insert(k.clone(), v.clone());
            }
        }
        for (k, v) in v["mcp"]["servers"].as_object().into_iter().flatten() {
            out.insert(k.clone(), v.clone());
        }
    }
    out
}

/// Why an MCP server may not work in a container, if it may not: one that runs a command
/// runs it inside the container, where only the image's tools exist.
pub fn mcp_caveat(server: &Value) -> Option<String> {
    let cmd = server["command"].as_array()?.first()?.as_str()?;
    Some(format!(
        "runs `{cmd}` inside the container, which has only the image's programs, not your computer's"
    ))
}

/// The `mcp` section of a bot's opencode config: the host's servers the bot didn't opt in to
/// are turned off, and browse runs from the image.
pub fn mcp_config(bot: &Bot) -> Value {
    let mut servers = serde_json::Map::new();
    for (name, v) in host_mcp() {
        if bot.sandbox.mcp.contains(&name) {
            continue;
        }
        // An entry must say what it is, even to turn it off.
        let mut off = json!({ "type": v["type"], "disabled": true });
        for key in ["url", "command"] {
            if !v[key].is_null() {
                off[key] = v[key].clone();
            }
        }
        servers.insert(name, off);
    }
    if bot.sandbox.has_tool("browse") {
        let mut env = json!({ "OPENCODE_BROWSER_AUTO_UPDATE": "off" });
        if let Ok(p) = browse_port() {
            env["OPENCODE_BROWSER_PORT"] = json!(p.to_string());
        }
        servers.insert(
            "browse".into(),
            json!({ "type": "local", "command": ["browse", "mcp"], "environment": env }),
        );
    }
    json!({ "servers": servers })
}

// ---- containers ---------------------------------------------------------------------------

/// A container's state: `running`, `paused`, `exited`, … (Docker's words).
pub fn states() -> Result<HashMap<String, String>> {
    let out = docker(&[
        "ps",
        "-a",
        "--filter",
        "label=dumbot.bot",
        "--format",
        "{{.Label \"dumbot.bot\"}}\t{{.State}}",
    ])?;
    Ok(out
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect())
}

fn password(slug: &str) -> Result<String> {
    let path = state_dir(slug).join("password");
    if let Ok(p) = std::fs::read_to_string(&path)
        && !p.trim().is_empty()
    {
        return Ok(p.trim().to_string());
    }
    let mut buf = [0u8; 24];
    use std::io::Read;
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    use base64::Engine;
    let p = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf);
    write_private(&path, p.as_bytes())?;
    Ok(p)
}

/// Writes in place (never a rename), so a file bind-mounted into a container stays the same
/// file and the container sees the change.
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)?;
    Ok(())
}

fn free_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port())
}

/// Writes what the container reads at every command, so it's current without recreating
/// it: the bot's environment variables (plus tool tokens), and git's settings.
fn write_runtime_files(slug: &str, bot: &Bot) -> Result<Vec<Issue>> {
    let mut issues = Vec::new();
    let mut vars = crate::env::load().for_bot(slug);
    if bot.sandbox.has_tool("gh") {
        match gh_token() {
            Ok(t) => {
                vars.insert("GH_TOKEN".into(), t);
            }
            Err(e) => issues.push(Issue {
                bot: Some(slug.into()),
                problem: format!("GitHub CLI isn't logged in: {e}"),
                fix: "Run `gh auth login` on this computer.".into(),
                warning: true,
            }),
        }
    }
    let env = crate::env::Env {
        global: vars,
        bots: BTreeMap::new(),
    };
    write_private(
        &state_dir(slug).join("env.json"),
        &serde_json::to_vec_pretty(&env)?,
    )?;
    let mut git = String::from("# Written by dumBot for this bot's container.\n[user]\n");
    for key in ["name", "email"] {
        if let Some(v) = command_output("git", &["config", "--global", &format!("user.{key}")]) {
            git.push_str(&format!("\t{key} = {v}\n"));
        }
    }
    if bot.sandbox.has_tool("gh") {
        git.push_str(
            "[credential \"https://github.com\"]\n\thelper = \n\thelper = !gh auth git-credential\n",
        );
    }
    let home = state_dir(slug).join("home");
    std::fs::create_dir_all(&home)?;
    std::fs::write(home.join(".gitconfig"), git)?;
    Ok(issues)
}

struct Spec {
    args: Vec<String>,
    hash: String,
}

fn container_spec(slug: &str, bot: &Bot, image: &str) -> Result<Spec> {
    let home = home();
    let st = state_dir(slug);
    let meta = std::fs::metadata(&home)?;
    let mut mounts: Vec<(PathBuf, PathBuf, bool)> = vec![
        (st.join("home"), home.clone(), false),
        (opencode_config_dir(), opencode_config_dir(), false),
        (bots::dir(slug), bots::dir(slug), false),
        (
            st.join("env.json"),
            store::data_dir().join("env.json"),
            true,
        ),
    ];
    std::fs::create_dir_all(opencode_config_dir())?;
    let mut env: Vec<(String, String)> = vec![
        ("HOME".into(), home.display().to_string()),
        (
            "USER".into(),
            std::env::var("USER").unwrap_or_else(|_| "dumbot".into()),
        ),
        ("DUMBOT_BOT".into(), slug.into()),
        ("SHELL".into(), "/bin/bash".into()),
        ("TERM".into(), "xterm-256color".into()),
    ];
    for v in ["XDG_CONFIG_HOME", "XDG_DATA_HOME"] {
        if let Ok(x) = std::env::var(v) {
            env.push((v.into(), x));
        }
    }
    if let Some(tz) = time_zone() {
        env.push(("TZ".into(), tz));
    }
    if bot.sandbox.has_tool("browse") {
        let token = home.join(".local/share/browse/relay-token");
        if token.exists() {
            mounts.push((token.clone(), token, true));
        }
        if !host_network()
            && let Ok(p) = browse_port()
        {
            env.push(("DUMBOT_BROWSE_FORWARD".into(), p.to_string()));
        }
    }
    for m in &bot.sandbox.mounts {
        let p = match m.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None => PathBuf::from(m),
        };
        mounts.push((p.clone(), p, false));
    }
    let name = name(slug);
    let mut args: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--init".into(),
        "--name".into(),
        name,
        "--label".into(),
        format!("dumbot.bot={slug}"),
        "--user".into(),
        format!("{}:{}", meta.uid(), meta.gid()),
        "-w".into(),
        bots::dir(slug).display().to_string(),
    ];
    if host_network() {
        // The port is part of the spec, so it's kept while the spec is.
        let port = existing_port(slug).map_or_else(free_port, Ok)?;
        args.extend([
            "--network".into(),
            "host".into(),
            "--label".into(),
            format!("dumbot.port={port}"),
        ]);
        env.push(("DUMBOT_HOSTNAME".into(), "127.0.0.1".into()));
        env.push(("DUMBOT_PORT".into(), port.to_string()));
    } else {
        args.extend([
            "-p".into(),
            format!("127.0.0.1::{PORT}"),
            "--add-host".into(),
            "host.docker.internal:host-gateway".into(),
        ]);
    }
    for (src, dst, ro) in &mounts {
        let mut m = format!("type=bind,src={},dst={}", src.display(), dst.display());
        if *ro {
            m.push_str(",readonly");
        }
        args.extend(["--mount".into(), m]);
    }
    for (k, v) in &env {
        args.extend(["-e".into(), format!("{k}={v}")]);
    }
    // Hashed before the password and image are added: neither changes without the hash.
    let mut parts: Vec<&str> = args.iter().map(String::as_str).collect();
    parts.push(image);
    let hash = short_hash(&parts);
    args.extend([
        "--label".into(),
        format!("dumbot.config={hash}"),
        "-e".into(),
        format!("OPENCODE_SERVER_PASSWORD={}", password(slug)?),
        image.into(),
    ]);
    Ok(Spec { args, hash })
}

fn existing_port(slug: &str) -> Option<u16> {
    docker(&[
        "inspect",
        "--format",
        "{{index .Config.Labels \"dumbot.port\"}}",
        &name(slug),
    ])
    .ok()?
    .parse()
    .ok()
}

static CLIENTS: Mutex<Option<HashMap<String, Client>>> = Mutex::new(None);

/// A client for a bot's container if it's running (or paused), without starting anything.
pub fn client(slug: &str) -> Result<Client> {
    if let Some(c) = CLIENTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(slug))
    {
        return Ok(c.clone());
    }
    let url = if host_network() {
        let port = existing_port(slug).ok_or_else(|| anyhow!("{slug} has no container"))?;
        format!("http://127.0.0.1:{port}")
    } else {
        let out = docker(&["port", &name(slug), &format!("{PORT}/tcp")])?;
        let addr = out
            .lines()
            .find(|l| l.starts_with("127.0.0.1:"))
            .ok_or_else(|| anyhow!("{slug}'s container publishes no port"))?;
        format!("http://{addr}")
    };
    let c = Client::new(&url, &password(slug)?);
    CLIENTS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(slug.into(), c.clone());
    Ok(c)
}

fn forget_client(slug: &str) {
    if let Some(m) = CLIENTS.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        m.remove(slug);
    }
}

fn error_file(slug: &str) -> PathBuf {
    state_dir(slug).join("last-error.txt")
}

/// Gets the bot's container running opencode, building, creating, starting or resuming
/// whatever it takes, and returns its client.
pub fn ensure(slug: &str, bot: &Bot, step: Step) -> Result<Client> {
    let res = ensure_inner(slug, bot, step);
    match &res {
        Ok(_) => {
            let _ = std::fs::remove_file(error_file(slug));
        }
        Err(e) => {
            let _ = write_private(&error_file(slug), format!("{e:#}").as_bytes());
        }
    }
    res
}

fn ensure_inner(slug: &str, bot: &Bot, step: Step) -> Result<Client> {
    step("Checking Docker");
    check_docker()?;
    step("Checking the bot image");
    let image = ensure_image(step)?;
    step("Preparing the container");
    write_runtime_files(slug, bot)?;
    let spec = container_spec(slug, bot, &image)?;
    let name = name(slug);
    let current = docker(&[
        "inspect",
        "--format",
        "{{.State.Status}}\t{{index .Config.Labels \"dumbot.config\"}}",
        &name,
    ])
    .ok();
    let state = match current.as_deref().and_then(|c| c.split_once('\t')) {
        Some((state, hash)) if hash == spec.hash => Some(state.to_string()),
        Some(_) => {
            step("Recreating the container (its settings or image changed)");
            docker(&["rm", "-f", &name])?;
            None
        }
        None => None,
    };
    forget_client(slug);
    match state.as_deref() {
        None => {
            step("Creating the container");
            let args: Vec<&str> = spec.args.iter().map(String::as_str).collect();
            docker(&args)?;
        }
        Some("paused") => {
            step("Resuming the container");
            docker(&["unpause", &name])?;
        }
        Some("running") => {}
        Some(_) => {
            step("Starting the container");
            docker(&["start", &name])?;
        }
    }
    step("Waiting for opencode in the container");
    let started = Instant::now();
    let c = loop {
        if let Ok(c) = client(slug)
            && c.healthy()
        {
            break c;
        }
        forget_client(slug);
        let status = docker(&["inspect", "--format", "{{.State.Status}}", &name])?;
        if status != "running" || started.elapsed() > READY_TIMEOUT {
            let logs = logs(slug, 30);
            return Err(anyhow!(
                "opencode didn't start in {name} (container {status}):\n{logs}"
            ));
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    step("Copying your opencode logins");
    sync_credentials(&c, bot)?;
    Ok(c)
}

/// Copies the host's opencode logins into the container: every model provider's, and those
/// of the MCP servers the bot opted in to. They live in opencode's database, which can't be
/// shared with a container safely. A login the container refreshed is replaced by the host's.
fn sync_credentials(c: &Client, bot: &Bot) -> Result<()> {
    let host = Client::connect()
        .context("reach the opencode service on this computer, for your logins")?
        .credentials()?;
    let theirs: HashMap<String, Value> = c
        .credentials()?
        .into_iter()
        .filter_map(|e| Some((e["id"].as_str()?.to_string(), e)))
        .collect();
    for e in host {
        let mcp = e["integrationID"]
            .as_str()
            .is_some_and(|i| i.starts_with("mcp_"));
        let label = e["label"].as_str().unwrap_or_default();
        if mcp && !bot.sandbox.mcp.iter().any(|m| m == label) {
            continue;
        }
        let id = e["id"].as_str().unwrap_or_default();
        match theirs.get(id) {
            Some(t) if t["value"] == e["value"] && t["active"] == e["active"] => {}
            Some(_) => c.put_credential(&e, true)?,
            None => c.put_credential(&e, false)?,
        }
    }
    Ok(())
}

/// The last `n` lines the container printed.
pub fn logs(slug: &str, n: usize) -> String {
    Command::new("docker")
        .args(["logs", "--tail", &n.to_string(), &name(slug)])
        .output()
        .map(|o| {
            format!(
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            )
            .trim()
            .to_string()
        })
        .unwrap_or_default()
}

/// Freezes an idle container; the next run resumes it at once.
pub fn pause(slug: &str) -> Result<()> {
    docker(&["pause", &name(slug)]).map(|_| ())
}

/// Unfreezes a paused container, so its sessions can be read.
pub fn resume(slug: &str) -> Result<()> {
    docker(&["unpause", &name(slug)]).map(|_| ())
}

/// Removes a bot's container and what dumBot keeps for it (on deleting the bot).
pub fn remove(slug: &str) {
    let _ = docker(&["rm", "-f", &name(slug)]);
    forget_client(slug);
    let _ = std::fs::remove_dir_all(state_dir(slug));
}

/// The command that opens a run in the opencode TUI, attached to the bot's container.
pub fn attach_command(slug: &str, session: &str) -> Result<String> {
    let c = client(slug)?;
    Ok(format!(
        "OPENCODE_SERVER_PASSWORD='{}' opencode --server {} -s {session}",
        password(slug)?,
        c.url()
    ))
}

// ---- issues -------------------------------------------------------------------------------

/// Everything that stops bots from running, or something they opted in to from working.
pub fn doctor(all: &BTreeMap<String, Bot>) -> Vec<Issue> {
    let mut out = Vec::new();
    let issue = |bot: Option<&str>, problem: String, fix: &str, warning: bool| Issue {
        bot: bot.map(str::to_string),
        problem,
        fix: fix.into(),
        warning,
    };
    if let Err(e) = check_docker() {
        out.push(issue(
            None,
            format!("{e:#}"),
            "Install Docker Desktop (or Docker Engine) and start it. Bots only run in containers.",
            false,
        ));
        return out;
    }
    if let Ok(msg) = std::fs::read_to_string(build_error()) {
        out.push(issue(
            None,
            msg,
            "Fix what the log says (often the network), then run a bot again to retry.",
            false,
        ));
    }
    let states = states().unwrap_or_default();
    let host = host_mcp();
    let needs_browse = all.values().any(|b| b.sandbox.has_tool("browse"));
    let browse = needs_browse.then(browse_port);
    for (slug, bot) in all {
        let s = Some(slug.as_str());
        if let Ok(e) = std::fs::read_to_string(error_file(slug)) {
            out.push(issue(
                s,
                format!("Its last start failed: {e}"),
                "Run it again; if it keeps failing, check Docker and the container's logs.",
                false,
            ));
        }
        if let Some(st) = states.get(slug)
            && (st == "exited" || st == "dead")
        {
            out.push(issue(
                s,
                format!("Its container stopped:\n{}", logs(slug, 10)),
                "It's started again on the next run.",
                true,
            ));
        }
        if bot.sandbox.has_tool("gh") && gh_token().is_err() {
            out.push(issue(
                s,
                "Uses the GitHub CLI, but it isn't logged in on this computer.".into(),
                "Run `gh auth login`.",
                true,
            ));
        }
        if bot.sandbox.has_tool("browse")
            && let Some(Err(e)) = &browse
        {
            out.push(issue(
                s,
                format!("Uses Browse, but {e:#}."),
                "Install browse (opencode.ai/product/browse) and check `browse status`.",
                true,
            ));
        }
        for m in &bot.sandbox.mcp {
            match host.get(m) {
                None => out.push(issue(
                    s,
                    format!("Uses the MCP server {m:?}, which isn't in your opencode config."),
                    "Remove it from the bot's container settings.",
                    true,
                )),
                Some(v) => {
                    if let Some(c) = mcp_caveat(v) {
                        out.push(issue(
                            s,
                            format!("MCP server {m:?} {c}."),
                            "Prefer a remote MCP server, or a tool the bot can opt in to.",
                            true,
                        ));
                    }
                }
            }
        }
        for m in &bot.sandbox.mounts {
            let p = m
                .strip_prefix("~/")
                .map_or_else(|| PathBuf::from(m), |r| home().join(r));
            if !p.exists() {
                out.push(issue(
                    s,
                    format!("Mounts {m}, which doesn't exist."),
                    "Create it or remove it from the bot's container settings.",
                    false,
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_jsonc() {
        let v: Value = serde_json::from_str(&strip_jsonc(
            "{\n // hi\n \"a\": \"x//y\", /* c */ \"b\": [1, 2,],\n \"é\": 1,\n}",
        ))
        .unwrap();
        assert_eq!(v["a"], "x//y");
        assert_eq!(v["b"], json!([1, 2]));
        assert_eq!(v["é"], 1);
    }
}
