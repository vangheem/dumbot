//! App-wide settings in `~/.local/share/dumbot/state.json`. Bots keep their own state in
//! their folders (see `bots`).

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Store {
    /// The default model for bots without their own; opencode's default when unset.
    #[serde(default)]
    pub model: Option<ModelRef>,
    /// The user chose "Don't ask again" for adding dumbot.app to ~/Applications.
    #[serde(default)]
    pub skip_app_prompt: bool,
    /// A release the user chose to skip ("v0.4.0").
    #[serde(default)]
    pub skipped_version: Option<String>,
    /// The dumBot server this app manages bots on; bots run on this computer when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<crate::remote::Server>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelRef {
    pub provider: String,
    pub id: String,
}

impl ModelRef {
    /// `provider/id`, the combobox option id.
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }

    pub fn parse(key: &str) -> Option<ModelRef> {
        let (provider, id) = key.split_once('/')?;
        Some(ModelRef {
            provider: provider.into(),
            id: id.into(),
        })
    }
}

/// `~/.local/share/dumbot`, or `$DUMBOT_DATA_DIR` (to run a second instance, or tests).
pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("DUMBOT_DATA_DIR") {
        return PathBuf::from(d);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/share"))
        .join("dumbot")
}

fn instructions_path() -> PathBuf {
    data_dir().join("instructions.md")
}

/// The user's instructions for every bot, added to every run's system prompt.
pub fn instructions() -> String {
    std::fs::read_to_string(instructions_path()).unwrap_or_default()
}

pub fn save_instructions(text: &str) -> Result<()> {
    std::fs::create_dir_all(data_dir()).context("create data dir")?;
    std::fs::write(instructions_path(), text)?;
    Ok(())
}

impl Store {
    fn path() -> PathBuf {
        data_dir().join("state.json")
    }

    pub fn load() -> Store {
        std::fs::read(Self::path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        std::fs::create_dir_all(path.parent().unwrap()).context("create data dir")?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}
