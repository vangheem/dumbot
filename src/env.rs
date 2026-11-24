//! Environment variables for bots' shell commands, global and per bot. They're kept in
//! `~/.local/share/dumbot/env.json` (readable only by the user), outside the bots' folders.
//!
//! Each bot's folder has an opencode config whose `shell` is a small wrapper
//! (`~/.local/share/dumbot/shell/<slug>`) that runs `dumbot shell <slug>`: it adds the variables
//! and runs the user's shell. opencode uses it for every shell command in the folder, the run's
//! and its subagents' alike. Bots only ever see the names: the wrapper replaces any value in
//! commands' output with `[$NAME]`, so the model never reads one.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::store;

/// Shorter values aren't hidden in output: they'd match ordinary text.
const MIN_HIDDEN_LEN: usize = 4;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Env {
    #[serde(default)]
    pub global: BTreeMap<String, String>,
    /// By bot slug; these win over global ones of the same name.
    #[serde(default)]
    pub bots: BTreeMap<String, BTreeMap<String, String>>,
}

fn path() -> PathBuf {
    store::data_dir().join("env.json")
}

pub fn load() -> Env {
    std::fs::read(path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

impl Env {
    pub fn save(&self) -> Result<()> {
        let path = path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        let tmp = path.with_extension("json.tmp");
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    /// A bot's variables: the global ones, then its own.
    pub fn for_bot(&self, slug: &str) -> BTreeMap<String, String> {
        let mut vars = self.global.clone();
        vars.extend(self.bots.get(slug).cloned().unwrap_or_default());
        vars
    }

    /// `None` is global.
    pub fn scope(&mut self, bot: Option<&str>) -> &mut BTreeMap<String, String> {
        match bot {
            None => &mut self.global,
            Some(slug) => self.bots.entry(slug.to_string()).or_default(),
        }
    }
}

/// `GOTIFY_TOKEN`: a letter or underscore, then letters, digits and underscores.
pub fn valid_name(name: &str) -> bool {
    let mut c = name.chars();
    c.next()
        .is_some_and(|f| f.is_ascii_alphabetic() || f == '_')
        && c.all(|x| x.is_ascii_alphanumeric() || x == '_')
}

pub fn wrapper_path(slug: &str) -> PathBuf {
    store::data_dir().join("shell").join(slug)
}

/// Writes the bot's shell wrapper, returning its path.
pub fn write_wrapper(slug: &str, exe: &str) -> Result<PathBuf> {
    let path = wrapper_path(slug);
    std::fs::create_dir_all(path.parent().unwrap())?;
    let exe = exe.replace('\'', r"'\''");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\n# Written by dumBot: adds {slug}'s environment variables to its shell commands.\nexec '{exe}' shell {slug} \"$@\"\n"
        ),
    )?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

/// The shell to hand commands to: the user's, unless that's a dumBot wrapper.
fn real_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty() && !PathBuf::from(s).starts_with(store::data_dir()))
        .unwrap_or_else(|| "/bin/zsh".into())
}

/// `dumbot shell <slug> <shell args…>`: runs the user's shell with the bot's variables, hiding
/// their values in its output. Exits with the shell's status.
pub fn run_shell(args: &[String]) -> Result<i32> {
    let slug = args
        .first()
        .ok_or_else(|| anyhow!("usage: dumbot shell <bot> [shell args]"))?;
    let vars = load().for_bot(slug);
    let shell = real_shell();
    let mut child = Command::new(&shell)
        .args(&args[1..])
        .envs(&vars)
        .env("SHELL", &shell)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("run {shell}"))?;
    let mut hide: Vec<(Vec<u8>, Vec<u8>)> = vars
        .iter()
        .filter(|(_, v)| v.len() >= MIN_HIDDEN_LEN)
        .map(|(k, v)| (v.as_bytes().to_vec(), format!("[${k}]").into_bytes()))
        .collect();
    // Longest first, so a value containing another is hidden whole.
    hide.sort_by_key(|(v, _)| std::cmp::Reverse(v.len()));
    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let h2 = hide.clone();
    let t = std::thread::spawn(move || copy_hidden(err, std::io::stderr(), &h2));
    copy_hidden(out, std::io::stdout(), &hide);
    let _ = t.join();
    let status = child.wait()?;
    Ok(status.code().unwrap_or_else(|| {
        use std::os::unix::process::ExitStatusExt;
        128 + status.signal().unwrap_or(1)
    }))
}

/// Copies line by line, replacing each secret value with its `[$NAME]`.
fn copy_hidden(from: impl Read, mut to: impl Write, hide: &[(Vec<u8>, Vec<u8>)]) {
    let mut from = BufReader::new(from);
    let mut line = Vec::new();
    while from.read_until(b'\n', &mut line).is_ok_and(|n| n > 0) {
        let _ = to.write_all(&hidden(&line, hide));
        let _ = to.flush();
        line.clear();
    }
}

fn hidden(line: &[u8], hide: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut out = line.to_vec();
    for (value, label) in hide {
        let mut i = 0;
        while let Some(at) = out[i..]
            .windows(value.len())
            .position(|w| w == value.as_slice())
        {
            let at = i + at;
            out.splice(at..at + value.len(), label.iter().copied());
            i = at + label.len();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_values_in_output() {
        let hide = vec![
            (b"tok-abc123".to_vec(), b"[$GOTIFY_TOKEN]".to_vec()),
            (b"hunter2".to_vec(), b"[$PW]".to_vec()),
        ];
        assert_eq!(
            hidden(b"curl -H 'X: tok-abc123' tok-abc123 hunter2!\n", &hide),
            b"curl -H 'X: [$GOTIFY_TOKEN]' [$GOTIFY_TOKEN] [$PW]!\n"
        );
        assert!(valid_name("GOTIFY_TOKEN") && valid_name("_x1"));
        assert!(!valid_name("1X") && !valid_name("A-B") && !valid_name(""));
    }
}
