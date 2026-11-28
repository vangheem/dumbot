//! The server browser: on a dumBot server, bots use a Chrome of the server's own, with the
//! browse extension and relay (`docker/browser.Dockerfile`), in the `dumbot-browser`
//! container. Its home (`~/.local/share/dumbot/browser`: Chrome's profile and browse's keys)
//! is a folder on the server, so what you sign in to stays signed in across restarts and
//! rebuilds. You see and use it through its viewer (`view`), for sign-ins and handoffs.
//!
//! It runs on the host's network, listening on 127.0.0.1 only: the relay on `RELAY_PORT`
//! (bots' containers share the host's network) and the viewer on `VIEW_PORT`. Docker starts it
//! again after a reboot; the server checks on it every minute and recreates it when its image
//! changes (a new dumBot version, which also picks up the newest browse).

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::store;

pub const RELAY_PORT: u16 = 19988;
pub const VIEW_PORT: u16 = 7459;
const NAME: &str = "dumbot-browser";
const DOCKERFILE: &str = include_str!("docker/browser.Dockerfile");

/// Google builds Chrome for Linux on x86_64 only.
pub fn supported() -> bool {
    cfg!(target_os = "linux") && cfg!(target_arch = "x86_64")
}

fn home() -> PathBuf {
    store::data_dir().join("browser")
}

/// The relay's token, which bots' containers get (read-only) to use the relay.
pub fn token_file() -> PathBuf {
    home().join(".local/share/browse/relay-token")
}

fn relay_log() -> PathBuf {
    home().join(".local/state/browse/logs/relay.log")
}

fn tag() -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (DOCKERFILE, env!("CARGO_PKG_VERSION")).hash(&mut h);
    format!("{NAME}:{:016x}", h.finish())
}

fn docker(args: &[&str]) -> Result<String> {
    let out = Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .context("run docker")?;
    if !out.status.success() {
        return Err(anyhow!(
            "docker {}: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

static ENSURE: Mutex<()> = Mutex::new(());

/// Gets the browser running: builds its image when it's missing, and creates (or recreates,
/// for a new image) and starts its container.
pub fn ensure() -> Result<()> {
    if !supported() {
        return Err(anyhow!(
            "the server browser needs Linux on x86_64 (Chrome isn't built for this server)"
        ));
    }
    let _lock = ENSURE.lock().unwrap_or_else(|e| e.into_inner());
    let tag = tag();
    if docker(&["image", "inspect", "--format", "{{.Id}}", &tag]).is_err() {
        build(&tag)?;
    }
    let current = docker(&[
        "inspect",
        "--format",
        "{{.State.Status}}\t{{.Config.Image}}",
        NAME,
    ])
    .ok();
    match current.as_deref().and_then(|c| c.split_once('\t')) {
        Some((state, image)) if image == tag => {
            if state != "running" {
                docker(&["start", NAME])?;
            }
            return Ok(());
        }
        Some(_) => {
            // Stopped properly first: Chrome saves its cookies and session as it closes.
            let _ = docker(&["stop", "-t", "30", NAME]);
            docker(&["rm", "-f", NAME])?;
        }
        None => {}
    }
    std::fs::create_dir_all(home())?;
    let meta = std::fs::metadata(home())?;
    use std::os::unix::fs::MetadataExt;
    let mut args = vec![
        "run".to_string(),
        "-d".into(),
        "--name".into(),
        NAME.into(),
        "--label".into(),
        "dumbot.browser=1".into(),
        "--restart".into(),
        "unless-stopped".into(),
        // Time for Chrome to close properly when stopped (or the server shuts down).
        "--stop-timeout".into(),
        "30".into(),
        "--network".into(),
        "host".into(),
        "--user".into(),
        format!("{}:{}", meta.uid(), meta.gid()),
        "--shm-size".into(),
        "1g".into(),
        // Chrome's own sandbox needs namespaces, which Docker's default seccomp filter refuses;
        // this keeps Chrome sandboxed (instead of `--no-sandbox`).
        "--security-opt".into(),
        "seccomp=unconfined".into(),
        "--mount".into(),
        format!("type=bind,src={},dst=/data", home().display()),
        "-e".into(),
        format!("DUMBOT_VIEW_PORT={VIEW_PORT}"),
    ];
    if let Some(tz) = crate::context::current()
        .and_then(|c| c.time_zone)
        .or_else(crate::container::time_zone)
    {
        args.extend(["-e".into(), format!("TZ={tz}")]);
    }
    args.push(tag);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    docker(&args)?;
    Ok(())
}

fn build(tag: &str) -> Result<()> {
    let log = store::data_dir().join("containers/browser-build.log");
    // An empty build context: the Dockerfile copies nothing in.
    let context = store::data_dir().join("containers/.browser-build");
    std::fs::create_dir_all(&context)?;
    let mut child = Command::new("docker")
        .args([
            "build",
            "--label",
            "dumbot.browser=1",
            "-t",
            tag,
            "-f",
            "-",
            ".",
        ])
        .current_dir(&context)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log)?)
        .spawn()
        .context("run docker build")?;
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), DOCKERFILE.as_bytes())?;
    drop(child.stdin.take());
    if !child.wait()?.success() {
        let tail: Vec<String> = std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .rev()
            .take(20)
            .map(String::from)
            .collect();
        let tail: Vec<String> = tail.into_iter().rev().collect();
        return Err(anyhow!(
            "building the server browser failed (log: {}):\n{}",
            log.display(),
            tail.join("\n")
        ));
    }
    // Older browser images.
    if let Ok(old) = docker(&[
        "image",
        "ls",
        "--filter",
        "label=dumbot.browser=1",
        "--format",
        "{{.Repository}}:{{.Tag}}",
    ]) {
        for t in old.lines().filter(|t| *t != tag) {
            let _ = docker(&["image", "rm", t]);
        }
    }
    Ok(())
}

/// The browse version in the browser's image, which bots' images install too (an MCP server
/// and a relay of different versions may not understand each other).
pub fn version() -> Option<String> {
    static CACHE: Mutex<Option<(String, String)>> = Mutex::new(None);
    let tag = tag();
    let mut c = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((t, v)) = c.as_ref()
        && *t == tag
    {
        return Some(v.clone());
    }
    let v = docker(&["run", "--rm", "--entrypoint", "browse", &tag, "version"]).ok()?;
    let v = v
        .split_whitespace()
        .last()?
        .trim_start_matches('v')
        .to_string();
    *c = Some((tag, v.clone()));
    Some(v)
}

/// A bot waiting on you in the server browser (a sign-in, 2FA, a captcha).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Handoff {
    pub tab: u64,
    pub url: String,
    pub title: String,
    pub since: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Info {
    /// What's wrong, if it isn't ready.
    pub problem: Option<String>,
    pub handoffs: Vec<Handoff>,
}

/// How the server browser is doing, and who's waiting on you in it.
pub fn info() -> Info {
    let status = docker(&["exec", NAME, "browse", "status", "--json"])
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    let Some(st) = status else {
        return Info {
            problem: Some("The server browser isn't running yet.".into()),
            handoffs: Vec::new(),
        };
    };
    let problem = match (
        st["pairingState"].as_str(),
        st["extension"]["connected"] == true,
    ) {
        (_, true) => None,
        (Some("not-seen" | "unpaired"), _) => {
            Some("The server browser is still setting up browse.".into())
        }
        _ => Some("The server browser's browse extension isn't connected.".into()),
    };
    let handoffs = pending(&std::fs::read_to_string(relay_log()).unwrap_or_default())
        .into_iter()
        .map(|(tab, since)| {
            let t = st["tabs"]
                .as_array()
                .and_then(|a| a.iter().find(|x| x["tab"].as_u64() == Some(tab)));
            let field = |k: &str| {
                t.and_then(|t| t[k].as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            Handoff {
                tab,
                url: field("url"),
                title: field("title"),
                since,
            }
        })
        .collect();
    Info { problem, handoffs }
}

/// Handoffs started and not finished, from the relay's log: `(tab, when it started)`.
fn pending(log: &str) -> Vec<(u64, String)> {
    let mut open: Vec<(u64, String)> = Vec::new();
    // The log's end is enough: a handoff waits ten minutes at most by default.
    let lines: Vec<&str> = log.lines().collect();
    for line in &lines[lines.len().saturating_sub(5000)..] {
        let mut it = line.split_whitespace();
        let (Some(at), Some(event)) = (it.next(), it.next()) else {
            continue;
        };
        let Some(tab) = it
            .find_map(|w| w.strip_prefix("tabId="))
            .and_then(|t| t.parse::<u64>().ok())
        else {
            continue;
        };
        match event {
            "handoff.started" => open.push((tab, at.to_string())),
            "handoff.finished" => open.retain(|(t, _)| *t != tab),
            _ => {}
        }
    }
    open
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_pending_handoffs_from_the_relay_log() {
        let log = "2026-11-28T19:29:12Z handoff.started tabId=3\n\
                   2026-11-28T19:29:13Z tool.outcome tool=navigate tabId=4 ms=5\n\
                   2026-11-28T19:29:27Z handoff.finished tabId=3 outcome=continued\n\
                   2026-11-28T19:29:36Z handoff.started tabId=5\n";
        assert_eq!(pending(log), [(5, "2026-11-28T19:29:36Z".to_string())]);
    }
}
