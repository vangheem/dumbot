//! Noticing newer releases and installing them: Homebrew installs through Homebrew, others
//! by replacing this binary with the release's (checked against its SHA-256).

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, anyhow};

use crate::appbundle;

pub const REPO: &str = "vangheem/dumbot";

/// How often a running dumbot looks for a new release.
pub const CHECK_EVERY_SECS: f64 = 6.0 * 3600.0;

pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v
        .trim()
        .trim_start_matches('v')
        .split('.')
        .map(|p| p.parse::<u64>().ok());
    Some((it.next()??, it.next()??, it.next()??))
}

/// Whether `latest` (a tag like `v0.4.0`) is newer than `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    matches!((parse(latest), parse(current)), (Some(l), Some(c)) if l > c)
}

/// The latest release's tag, from GitHub's API (anonymous; a check every few hours is far
/// below its rate limit).
pub fn latest_release() -> Result<String> {
    let v: serde_json::Value = ureq::get(&format!(
        "https://api.github.com/repos/{REPO}/releases/latest"
    ))
    .set("User-Agent", "dumbot")
    .set("Accept", "application/vnd.github+json")
    .timeout(std::time::Duration::from_secs(20))
    .call()
    .context("check for a new release")?
    .into_json()?;
    let tag = v["tag_name"].as_str().unwrap_or_default().to_string();
    if parse(&tag).is_none() {
        return Err(anyhow!("unexpected release tag {tag:?}"));
    }
    Ok(tag)
}

/// The release asset with a binary like this one: the app for macOS (universal) and Linux
/// x86_64, else the command line (which has `dumbot server`).
fn asset() -> Result<String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        a => return Err(anyhow!("no releases for {a}")),
    };
    Ok(match std::env::consts::OS {
        "macos" => "dumbot-macos-universal.tar.gz".into(),
        "linux" if cfg!(feature = "gui") && arch == "x86_64" => "dumbot-linux-x86_64.tar.gz".into(),
        "linux" => format!("dumbot-cli-linux-{arch}.tar.gz"),
        os => return Err(anyhow!("no releases for {os}")),
    })
}

fn download(url: &str) -> Result<Vec<u8>> {
    let r = ureq::get(url)
        .set("User-Agent", "dumbot")
        .timeout(std::time::Duration::from_secs(300))
        .call()
        .with_context(|| format!("download {url}"))?;
    let mut body = Vec::new();
    std::io::Read::read_to_end(&mut r.into_reader(), &mut body)?;
    Ok(body)
}

/// Replaces the binary at `exe` with release `tag`'s.
pub fn install_release(tag: &str, exe: &std::path::Path) -> Result<()> {
    let asset = asset()?;
    let base = format!("https://github.com/{REPO}/releases/download/{tag}/{asset}");
    let tarball = download(&base)?;
    let want = String::from_utf8(download(&format!("{base}.sha256"))?)?;
    let want = want
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string();
    let got: String = ring::digest::digest(&ring::digest::SHA256, &tarball)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if got != want {
        return Err(anyhow!(
            "{asset} doesn't match its SHA-256; not installing it"
        ));
    }
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("{} has no folder", exe.display()))?
        .join(format!(".dumbot-update-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let res = (|| {
        let mut tar = Command::new("tar")
            .args(["-xzf", "-", "-C"])
            .arg(&dir)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .context("run tar")?;
        std::io::Write::write_all(tar.stdin.as_mut().unwrap(), &tarball)?;
        drop(tar.stdin.take());
        if !tar.wait()?.success() {
            return Err(anyhow!("couldn't unpack {asset}"));
        }
        let new = dir.join("dumbot");
        let out = Command::new(&new)
            .arg("help")
            .output()
            .context("run the new dumbot")?;
        if !out.status.success() {
            return Err(anyhow!("the new dumbot doesn't run"));
        }
        // Renamed over the old one: the running program keeps its copy until it exits.
        std::fs::rename(&new, exe).with_context(|| format!("replace {}", exe.display()))
    })();
    let _ = std::fs::remove_dir_all(&dir);
    res
}

/// Installs release `tag` over this dumbot: through Homebrew when it came from there.
pub fn update_self(tag: &str) -> Result<()> {
    if let Some(prefix) = brew_prefix() {
        return brew_upgrade(&prefix);
    }
    let exe = std::env::current_exe()?.canonicalize()?;
    install_release(tag, &exe)
}

/// Homebrew's prefix when this dumbot was installed by Homebrew (`/opt/homebrew`).
pub fn brew_prefix() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let s = exe.to_str()?;
    let (prefix, _) = s.split_once("/Cellar/dumbot/")?;
    Some(PathBuf::from(prefix))
}

/// `brew update` (so the tap's new formula is seen), then upgrade dumbot.
pub fn brew_upgrade(prefix: &std::path::Path) -> Result<()> {
    let brew = prefix.join("bin/brew");
    let run = |args: &[&str]| -> Result<()> {
        let out = Command::new(&brew)
            .args(args)
            .env("HOMEBREW_NO_ENV_HINTS", "1")
            .env("HOMEBREW_NO_AUTO_UPDATE", "1")
            .output()
            .with_context(|| format!("run {}", brew.display()))?;
        if out.status.success() {
            Ok(())
        } else {
            let err = String::from_utf8_lossy(&out.stderr);
            Err(anyhow!(
                "brew {} failed: {}",
                args.join(" "),
                err.trim().lines().last().unwrap_or("")
            ))
        }
    };
    run(&["update", "--quiet"])?;
    run(&["upgrade", "vangheem/tap/dumbot"])
}

/// Starts the freshly installed dumbot: through the app when this one was opened as the app,
/// else the binary at Homebrew's version-independent path.
pub fn relaunch() -> Result<()> {
    if appbundle::launched_from_app() && appbundle::installed() {
        Command::new("open")
            .arg("-n")
            .arg(appbundle::app_path())
            .spawn()?;
    } else {
        Command::new(appbundle::target_binary()?).spawn()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_numerically() {
        assert!(is_newer("v0.10.0", "0.9.3"));
        assert!(is_newer("v1.0.0", "0.3.0"));
        assert!(!is_newer("v0.3.0", "0.3.0"));
        assert!(!is_newer("v0.2.9", "0.3.0"));
        assert!(!is_newer("latest", "0.3.0"));
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;

    /// Downloads the latest release and installs it over a copy of this binary.
    #[test]
    #[ignore]
    fn installs_the_latest_release() {
        let tag = latest_release().unwrap();
        let dir = std::env::temp_dir().join(format!("dumbot-up-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("dumbot");
        std::fs::write(&exe, "old").unwrap();
        install_release(&tag, &exe).unwrap();
        let out = Command::new(&exe).arg("help").output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("dumbot"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Hits GitHub and runs Homebrew: `cargo test -- --ignored`. Upgrading an up-to-date
    /// dumbot is a no-op, so this proves the commands work without changing anything.
    #[test]
    #[ignore]
    fn reads_the_latest_release_and_brew_upgrade_runs() {
        let tag = latest_release().unwrap();
        assert!(parse(&tag).is_some());
        let prefix = PathBuf::from("/opt/homebrew");
        if prefix.join("Cellar/dumbot").exists() {
            brew_upgrade(&prefix).unwrap();
        }
    }
}
