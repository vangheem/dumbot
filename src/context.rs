//! What a server's bots take from the app's computer, as they would from the host when
//! running locally: its opencode config (models, agents, skills and MCP servers), opencode
//! logins and version, GitHub CLI token, git identity and time zone. The app sends it when it
//! connects and every few minutes; the server keeps the latest under
//! `~/.local/share/dumbot/client/` (owner-only) and uses it for every bot, also while the app
//! is closed.

use std::path::{Path, PathBuf};

use anyhow::Result;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{container, store};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HostContext {
    #[serde(default)]
    pub opencode_version: Option<String>,
    #[serde(default)]
    pub time_zone: Option<String>,
    #[serde(default)]
    pub git_name: Option<String>,
    #[serde(default)]
    pub git_email: Option<String>,
    #[serde(default)]
    pub gh_token: Option<String>,
    /// opencode's stored logins (`/api/credential`), secrets included.
    #[serde(default)]
    pub credentials: Vec<Value>,
    /// The browse version and relay token, when browse is set up (see `tunnel`).
    #[serde(default)]
    pub browse_version: Option<String>,
    #[serde(default)]
    pub browse_token: Option<String>,
    #[serde(default)]
    pub browse_port: Option<u16>,
    /// The opencode config folder's files: relative path and base64 contents.
    #[serde(default)]
    pub config: Vec<(String, String)>,
}

/// Files bigger than this, or past this total, aren't sent (they aren't config).
const MAX_FILE: u64 = 1024 * 1024;
const MAX_TOTAL: u64 = 16 * 1024 * 1024;

fn dir() -> PathBuf {
    store::data_dir().join("client")
}

/// Where a server keeps the app's opencode config, mounted into bots' containers.
pub fn config_dir() -> PathBuf {
    dir().join("opencode")
}

/// The app's browse relay token, on a server.
pub fn relay_token() -> PathBuf {
    dir().join("browse-relay-token")
}

/// The app's context on a server; `None` everywhere else.
pub fn current() -> Option<HostContext> {
    let raw = std::fs::read(dir().join("context.json")).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// This computer's context, to send to a server.
pub fn gather() -> HostContext {
    let git = |k: &str| container::command_output("git", &["config", "--global", k]);
    let mut config = Vec::new();
    let mut total = 0;
    let root = container::opencode_config_dir();
    collect(&root, &root, &mut config, &mut total);
    HostContext {
        opencode_version: container::version_of("opencode", &["--version"]),
        time_zone: container::time_zone(),
        git_name: git("user.name"),
        git_email: git("user.email"),
        gh_token: container::command_output("gh", &["auth", "token"]),
        credentials: crate::opencode::Client::connect()
            .and_then(|c| c.credentials())
            .unwrap_or_default(),
        browse_version: container::browse_version(),
        browse_token: std::fs::read_to_string(container::browse_token_file()).ok(),
        browse_port: container::browse_port().ok(),
        config,
    }
}

fn collect(root: &Path, at: &Path, out: &mut Vec<(String, String)>, total: &mut u64) {
    let Ok(entries) = std::fs::read_dir(at) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(meta) = std::fs::metadata(e.path()) else {
            continue;
        };
        if meta.is_dir() {
            if !matches!(name.as_str(), "node_modules" | ".git" | "bun.lock") {
                collect(root, &e.path(), out, total);
            }
        } else if meta.len() <= MAX_FILE
            && *total + meta.len() <= MAX_TOTAL
            && let Ok(body) = std::fs::read(e.path())
            && let Ok(rel) = e.path().strip_prefix(root)
        {
            *total += meta.len();
            out.push((
                rel.to_string_lossy().into_owned(),
                base64::engine::general_purpose::STANDARD.encode(body),
            ));
        }
    }
}

/// Stores an app's context on the server, replacing the last one.
pub fn save(ctx: &HostContext) -> Result<()> {
    let cfg = config_dir();
    let _ = std::fs::remove_dir_all(&cfg);
    std::fs::create_dir_all(&cfg)?;
    for (rel, body) in &ctx.config {
        let p = cfg.join(rel);
        // Only paths inside the folder.
        if Path::new(rel)
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            continue;
        }
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::write(p, base64::engine::general_purpose::STANDARD.decode(body)?)?;
    }
    match &ctx.browse_token {
        Some(t) => crate::remote::write_private(&relay_token(), t.as_bytes())?,
        None => {
            let _ = std::fs::remove_file(relay_token());
        }
    }
    let mut kept = ctx.clone();
    kept.config.clear();
    crate::remote::write_private(&dir().join("context.json"), &serde_json::to_vec(&kept)?)
}
