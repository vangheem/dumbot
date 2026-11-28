//! `dumbot server`: bots run on this machine, for apps elsewhere to manage over a paired TLS
//! connection (see `remote`). It also does what the app does locally: wakes bots when they're
//! due and pauses idle containers. `dumbot install-server` keeps it running as a service.

use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};

use crate::api::{self, Request};
use crate::remote::{self, Clients, Identity};
use crate::transport::Sock;
use crate::{bots, container, schedule};

const SCAN: Duration = Duration::from_secs(5);
const PAUSE_AFTER: Duration = Duration::from_secs(60);

pub const USAGE: &str = "\
  dumbot server [--listen ADDR]  run bots on this machine for apps elsewhere (default 0.0.0.0:7457)
  dumbot server approve <code>   pair the app showing <code>
  dumbot server clients          list paired apps and pending requests
  dumbot server revoke <fp>      unpair an app (by fingerprint prefix)
  dumbot server tunnel <hostname>
                                 reach this server from anywhere through a Cloudflare tunnel
                                 (needs cloudflared, logged in: `cloudflared tunnel login`)
  dumbot install-server [--listen ADDR]
                                 run `dumbot server` as a service, at boot (systemd, launchd)
  dumbot uninstall-server
The service installs new releases itself (DUMBOT_NO_AUTO_UPDATE=1 turns that off).";

/// `dumbot server …`.
pub fn cli(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("approve") => {
            let code = args
                .get(1)
                .ok_or_else(|| anyhow!("usage: dumbot server approve <code>"))?;
            let id = Identity::load("server")?;
            let p = Clients::approve(code, &id.fingerprint())?;
            println!(
                "Paired with {} ({}…). It can manage this server's bots now.",
                if p.name.is_empty() {
                    "the app"
                } else {
                    &p.name
                },
                &p.fingerprint[..16]
            );
            Ok(())
        }
        Some("clients") => {
            let c = Clients::load();
            let id = Identity::load("server")?;
            println!("This server's fingerprint: {}", id.fingerprint());
            for p in &c.approved {
                println!("paired   {}  {}", &p.fingerprint[..16], p.name);
            }
            for p in &c.pending {
                println!("pending  {}  {}", &p.fingerprint[..16], p.name);
            }
            Ok(())
        }
        Some("tunnel") => tunnel(&args[1..]),
        Some("revoke") => {
            let prefix = args
                .get(1)
                .ok_or_else(|| anyhow!("usage: dumbot server revoke <fingerprint prefix>"))?;
            let mut c = Clients::load();
            let before = c.approved.len();
            c.approved
                .retain(|p| !p.fingerprint.starts_with(prefix.as_str()));
            if c.approved.len() == before {
                return Err(anyhow!("no paired app's fingerprint starts with {prefix}"));
            }
            c.save()?;
            println!("Unpaired.");
            Ok(())
        }
        _ => serve(&listen_arg(args)?),
    }
}

fn listen_arg(args: &[String]) -> Result<String> {
    match args.iter().position(|a| a == "--listen") {
        Some(i) => Ok(args
            .get(i + 1)
            .ok_or_else(|| anyhow!("--listen needs an address, like 0.0.0.0:7457"))?
            .clone()),
        None => Ok(format!("0.0.0.0:{}", remote::DEFAULT_PORT)),
    }
}

fn serve(listen: &str) -> Result<()> {
    let id = Identity::load("server")?;
    let cfg = remote::server_config(&id)?;
    let listener = TcpListener::bind(listen).with_context(|| format!("listen on {listen}"))?;
    eprintln!(
        "dumbot server {} listening on {listen}\nfingerprint {}",
        env!("CARGO_PKG_VERSION"),
        id.fingerprint()
    );
    std::thread::spawn(scheduler);
    // Only as the installed service, which is started again when it exits.
    if std::env::var("DUMBOT_SERVICE").as_deref() == Ok("1")
        && std::env::var_os("DUMBOT_NO_AUTO_UPDATE").is_none()
    {
        std::thread::spawn(updater);
    }
    if crate::browser::supported() {
        std::thread::spawn(|| {
            if let Err(e) = crate::browser::ensure() {
                eprintln!("server browser: {e:#}");
            }
        });
    }
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let cfg = cfg.clone();
        std::thread::spawn(move || {
            let peer = stream
                .peer_addr()
                .map(|a| a.to_string())
                .unwrap_or_default();
            if let Err(e) = connection(stream, cfg) {
                eprintln!("{peer}: {e:#}");
            }
        });
    }
    Ok(())
}

fn connection(tcp: TcpStream, cfg: Arc<rustls::ServerConfig>) -> Result<()> {
    // Straight from the app, or through a proxy as a WebSocket (see `transport`).
    let tcp = Sock::accept(tcp)?;
    let peer = tcp.peer();
    tcp.set_read_timeout(Some(Duration::from_secs(60)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(60)))?;
    let mut tls = rustls::StreamOwned::new(rustls::ServerConnection::new(cfg)?, tcp);
    while tls.conn.is_handshaking() {
        tls.conn
            .complete_io(&mut tls.sock)
            .context("TLS handshake")?;
    }
    let fp = tls
        .conn
        .peer_certificates()
        .and_then(|c| c.first())
        .map(|c| remote::fingerprint(c))
        .ok_or_else(|| anyhow!("no client certificate"))?;
    let (req, leftover) = {
        let mut r = BufReader::new(&mut tls);
        let req: Value = remote::read_request(&mut r)?;
        (req, r.buffer().to_vec())
    };
    if !Clients::load().is_approved(&fp) {
        if req["op"] == "hello" {
            Clients::request(&fp, req["name"].as_str().unwrap_or_default())?;
            eprintln!(
                "{peer}: an app asks to pair; run `dumbot server approve <code>` with the code it shows"
            );
        }
        remote::write_line(&mut tls, &json!({ "unpaired": true }))?;
        return finish(&mut tls);
    }
    if req["op"] == "browser_view" {
        return crate::view::serve(tls, leftover);
    }
    let res = serde_json::from_value::<Request>(req)
        .map_err(|e| anyhow!("unknown request (is the app a different version?): {e}"));
    let out = Mutex::new(&mut tls);
    let step = |s: &str| {
        let _ = remote::write_line(&mut **out.lock().unwrap(), &json!({ "step": s }));
    };
    // A slow request (building an image) pings now and then, so the connection, and any
    // proxy on the way, doesn't count it as idle.
    let done = std::sync::atomic::AtomicBool::new(false);
    let res = std::thread::scope(|sc| {
        sc.spawn(|| {
            let mut waited = Duration::ZERO;
            while !done.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
                waited += Duration::from_millis(250);
                if waited >= Duration::from_secs(30) {
                    waited = Duration::ZERO;
                    let _ =
                        remote::write_line(&mut **out.lock().unwrap(), &json!({ "ping": true }));
                }
            }
        });
        let res = res.and_then(|r| api::handle(r, &step));
        done.store(true, std::sync::atomic::Ordering::Relaxed);
        res
    });
    let tls = out.into_inner().unwrap();
    match res {
        Ok(v) => remote::write_line(tls, &json!({ "ok": v }))?,
        Err(e) => remote::write_line(tls, &json!({ "err": format!("{e:#}") }))?,
    }
    finish(tls)
}

fn finish(tls: &mut rustls::StreamOwned<rustls::ServerConnection, Sock>) -> Result<()> {
    tls.conn.send_close_notify();
    tls.flush()?;
    Ok(())
}

// ---- the scheduler ------------------------------------------------------------------------

fn scheduler() {
    let mut last_busy = HashMap::new();
    loop {
        if let Err(e) = tick(&mut last_busy) {
            eprintln!("scheduler: {e:#}");
        }
        std::thread::sleep(SCAN);
    }
}

/// Wakes the bots that are due (a bot whose last run is still working waits for it), and
/// pauses containers with nothing to do: no working run, not starting, and not looked at in
/// the app lately.
fn tick(last_busy: &mut HashMap<String, Instant>) -> Result<()> {
    // The server browser: running, and on the current image. Docker restarts it after a
    // reboot; this catches the rest (an update, someone stopping it).
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    if crate::browser::supported() {
        let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) {
            *last = Some(Instant::now());
            std::thread::spawn(|| {
                if let Err(e) = crate::browser::ensure() {
                    eprintln!("server browser: {e:#}");
                }
            });
        }
    }
    let states = container::states()?;
    let active = api::active(&states);
    let starting = api::starting();
    let now = bots::now();
    for (slug, b) in bots::list() {
        let working = b.runs.iter().any(|r| active.contains(&r.session));
        let latest_working = b.last_session().is_some_and(|id| active.contains(id));
        if b.due(now) && !starting.contains_key(&slug) && !latest_working {
            let mut notes = Vec::new();
            match bots::update(&slug, |b| notes = schedule::fired(b, now)) {
                Ok(_) => {
                    let slug = slug.clone();
                    std::thread::spawn(move || {
                        match api::start_run(&slug, "scheduled wake-up", &notes, None, &|_| {}) {
                            Ok(id) => eprintln!("{slug}: started run {id}"),
                            Err(e) => eprintln!("{slug}: couldn't start: {e:#}"),
                        }
                    });
                }
                Err(e) => eprintln!("{slug}: couldn't wake: {e:#}"),
            }
            continue;
        }
        if states.get(&slug).map(String::as_str) != Some("running") {
            last_busy.remove(&slug);
            continue;
        }
        let busy = working || starting.contains_key(&slug) || api::touched(&slug, PAUSE_AFTER);
        let last = last_busy.entry(slug.clone()).or_insert_with(Instant::now);
        if busy {
            *last = Instant::now();
        } else if last.elapsed() > PAUSE_AFTER {
            let _ = container::pause(&slug);
            last_busy.remove(&slug);
        }
    }
    Ok(())
}

// ---- a Cloudflare tunnel -----------------------------------------------------------------

const TUNNEL: &str = "dumbot";
const TUNNEL_UNIT: &str = "dumbot-tunnel.service";

fn cloudflared() -> Result<String> {
    let local = home().join(".local/bin/cloudflared");
    if local.exists() {
        return Ok(local.display().to_string());
    }
    container::command_output("sh", &["-c", "command -v cloudflared"]).ok_or_else(|| {
        anyhow!(
            "cloudflared isn't installed: \
             https://developers.cloudflare.com/cloudflare-one/connections/connect-networks/downloads/"
        )
    })
}

/// `dumbot server tunnel <hostname>`: a Cloudflare tunnel of its own (`dumbot`) from
/// `hostname` to this server, run as a service. The app connects to `wss://<hostname>`;
/// Cloudflare carries the WebSocket, and inside it is the same pinned TLS as on the network.
fn tunnel(args: &[String]) -> Result<()> {
    let hostname = args
        .first()
        .ok_or_else(|| anyhow!("usage: dumbot server tunnel <hostname>, like bots.example.com"))?
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("wss://")
        .trim_end_matches('/')
        .to_string();
    if cfg!(target_os = "macos") {
        return Err(anyhow!(
            "`dumbot server tunnel` sets up a systemd service; on macOS, run cloudflared yourself (see the README)"
        ));
    }
    let cf = cloudflared()?;
    let dir = home().join(".cloudflared");
    if !dir.join("cert.pem").exists() {
        return Err(anyhow!(
            "cloudflared isn't logged in to your Cloudflare account; run `{cf} tunnel login` first"
        ));
    }
    // `~/.cloudflared/config.yml` may name another tunnel, which cloudflared would use in place
    // of the one named here: tunnel commands get an empty config instead.
    let output = |args: &[&str]| -> Result<String> {
        let mut all = vec!["tunnel", "--config", "/dev/null"];
        all.extend(&args[1..]);
        let out = std::process::Command::new(&cf).args(&all).output()?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        if out.status.success() {
            Ok(text)
        } else {
            Err(anyhow!("cloudflared {}: {}", args.join(" "), text.trim()))
        }
    };
    let find = || -> Result<Option<String>> {
        let list: Value = serde_json::from_str(
            &std::process::Command::new(&cf)
                .args([
                    "tunnel",
                    "--config",
                    "/dev/null",
                    "list",
                    "--output",
                    "json",
                    "--name",
                    TUNNEL,
                ])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())?,
        )
        .unwrap_or(Value::Null);
        Ok(list
            .as_array()
            .and_then(|a| a.first())
            .and_then(|t| t["id"].as_str())
            .map(String::from))
    };
    let id = match find()? {
        Some(id) => id,
        None => {
            println!("Creating the Cloudflare tunnel {TUNNEL:?}…");
            output(&["tunnel", "create", TUNNEL])?;
            find()?.ok_or_else(|| anyhow!("cloudflared created no tunnel"))?
        }
    };
    println!("Pointing {hostname} at it…");
    // By id, replacing whatever the name pointed at before.
    output(&["tunnel", "route", "dns", "--overwrite-dns", &id, &hostname])?;
    let port = listen_port();
    let config = dir.join("dumbot.yml");
    std::fs::write(
        &config,
        format!(
            "# Written by `dumbot server tunnel`.\n\
             tunnel: {id}\n\
             credentials-file: {}\n\
             ingress:\n  \
               - hostname: {hostname}\n    \
                 service: http://127.0.0.1:{port}\n  \
               - service: http_status:404\n",
            dir.join(format!("{id}.json")).display()
        ),
    )?;
    let unit = home().join(".config/systemd/user").join(TUNNEL_UNIT);
    std::fs::create_dir_all(unit.parent().unwrap())?;
    std::fs::write(
        &unit,
        format!(
            "[Unit]\n\
             Description=Cloudflare tunnel to the dumBot server ({hostname})\n\
             After=network-online.target {UNIT}\n\
             Wants=network-online.target\n\n\
             [Service]\n\
             ExecStart={cf} --no-autoupdate tunnel --config {} run {TUNNEL}\n\
             Restart=always\n\
             RestartSec=5\n\n\
             [Install]\n\
             WantedBy=default.target\n",
            config.display()
        ),
    )?;
    run("systemctl", &["--user", "daemon-reload"])?;
    run("systemctl", &["--user", "enable", TUNNEL_UNIT])?;
    run("systemctl", &["--user", "restart", TUNNEL_UNIT])?;
    println!(
        "Done: wss://{hostname} reaches this server, now and after reboots.\n\
         In the app, choose Server… and change the address to wss://{hostname}; it stays \
         paired (the server's key is the same)."
    );
    Ok(())
}

/// The port the installed service listens on.
fn listen_port() -> u16 {
    std::fs::read_to_string(home().join(".config/systemd/user").join(UNIT))
        .ok()
        .and_then(|u| {
            u.split_whitespace()
                .skip_while(|w| *w != "--listen")
                .nth(1)
                .and_then(|a| a.rsplit(':').next()?.parse().ok())
        })
        .unwrap_or(remote::DEFAULT_PORT)
}

// ---- updates ------------------------------------------------------------------------------

/// Installs new releases as they come out: once no run is starting, replaces this binary and
/// exits, and the service manager starts the new one. Bots' runs live in their containers, so
/// they carry on meanwhile.
fn updater() {
    std::thread::sleep(Duration::from_secs(60));
    loop {
        match crate::update::latest_release() {
            Ok(tag) if crate::update::is_newer(&tag, crate::update::current()) => {
                while !api::starting().is_empty() {
                    std::thread::sleep(SCAN);
                }
                match crate::update::update_self(&tag) {
                    Ok(()) => {
                        eprintln!("updated to {tag}; restarting");
                        std::process::exit(0);
                    }
                    Err(e) => eprintln!("couldn't update to {tag}: {e:#}"),
                }
            }
            Ok(_) => {}
            Err(e) => eprintln!("checking for updates: {e:#}"),
        }
        std::thread::sleep(Duration::from_secs_f64(crate::update::CHECK_EVERY_SECS));
    }
}

// ---- the service --------------------------------------------------------------------------

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

const UNIT: &str = "dumbot.service";
const LABEL: &str = "com.github.vangheem.dumbot.server";

fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let st = std::process::Command::new(cmd)
        .args(args)
        .status()
        .with_context(|| format!("run {cmd}"))?;
    if !st.success() {
        return Err(anyhow!("`{cmd} {}` failed", args.join(" ")));
    }
    Ok(())
}

/// Copies this program to `~/.local/bin/dumbot` (unless it's there already), so the service
/// doesn't depend on where it was run from.
fn install_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let dest = home().join(".local/bin/dumbot");
    if dest.canonicalize().ok().as_ref() == Some(&exe) {
        return Ok(dest);
    }
    std::fs::create_dir_all(dest.parent().unwrap())?;
    let tmp = dest.with_extension("new");
    std::fs::copy(&exe, &tmp).context("copy dumbot to ~/.local/bin")?;
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// `dumbot install-server [--listen ADDR]`.
pub fn install(args: &[String]) -> Result<()> {
    let listen = listen_arg(args)?;
    container::check_docker().context("the server runs bots in Docker")?;
    let bin = install_binary()?;
    // Made now, so the fingerprint can be shown.
    let id = Identity::load("server")?;
    if cfg!(target_os = "macos") {
        install_launchd(&bin, &listen)?;
    } else {
        install_systemd(&bin, &listen)?;
    }
    println!(
        "dumBot server is running on {listen} and starts at boot.\n\
         Fingerprint: {}\n\
         In the app, choose Server… and connect to this machine; then run\n\
         `dumbot server approve <code>` here with the code it shows.",
        id.fingerprint()
    );
    if container::command_output("sh", &["-c", "command -v ufw"]).is_some() {
        let port = listen.rsplit(':').next().unwrap_or_default();
        println!(
            "If a firewall is on, let the app in: `sudo ufw allow {port}/tcp` (only from your \
             network: `sudo ufw allow from 192.168.0.0/16 to any port {port} proto tcp`)."
        );
    }
    Ok(())
}

fn install_systemd(bin: &std::path::Path, listen: &str) -> Result<()> {
    let unit = home().join(".config/systemd/user").join(UNIT);
    std::fs::create_dir_all(unit.parent().unwrap())?;
    let path = std::env::var("PATH").unwrap_or_default();
    std::fs::write(
        &unit,
        format!(
            "[Unit]\n\
             Description=dumBot server: runs bots for the dumBot app\n\
             After=network-online.target docker.service\n\
             Wants=network-online.target\n\n\
             [Service]\n\
             ExecStart={} server --listen {listen}\n\
             Environment=PATH={}/.local/bin:{path}\n\
             Environment=DUMBOT_SERVICE=1\n\
             Restart=always\n\
             RestartSec=5\n\n\
             [Install]\n\
             WantedBy=default.target\n",
            bin.display(),
            home().display()
        ),
    )?;
    // Without lingering, user services stop at logout and don't start at boot.
    let user = std::env::var("USER").unwrap_or_default();
    if run("loginctl", &["enable-linger", &user]).is_err() {
        eprintln!(
            "warning: couldn't turn on lingering, so the server only runs while you're logged \
             in; run `sudo loginctl enable-linger {user}`"
        );
    }
    run("systemctl", &["--user", "daemon-reload"])?;
    run("systemctl", &["--user", "enable", UNIT])?;
    run("systemctl", &["--user", "restart", UNIT])?;
    Ok(())
}

fn launch_agent() -> PathBuf {
    home().join(format!("Library/LaunchAgents/{LABEL}.plist"))
}

fn install_launchd(bin: &std::path::Path, listen: &str) -> Result<()> {
    let plist = launch_agent();
    let log = crate::store::data_dir().join("server.log");
    std::fs::create_dir_all(plist.parent().unwrap())?;
    std::fs::create_dir_all(log.parent().unwrap())?;
    let path = std::env::var("PATH").unwrap_or_default();
    std::fs::write(
        &plist,
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array><string>{}</string><string>server</string><string>--listen</string><string>{listen}</string></array>
  <key>EnvironmentVariables</key><dict><key>PATH</key><string>{path}</string><key>DUMBOT_SERVICE</key><string>1</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
            bin.display(),
            log.display()
        ),
    )?;
    let domain = format!("gui/{}", uid());
    let _ = run("launchctl", &["bootout", &format!("{domain}/{LABEL}")]);
    run(
        "launchctl",
        &["bootstrap", &domain, &plist.display().to_string()],
    )
}

fn uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(home()).map(|m| m.uid()).unwrap_or(501)
}

pub fn uninstall() -> Result<()> {
    if cfg!(target_os = "macos") {
        let _ = run("launchctl", &["bootout", &format!("gui/{}/{LABEL}", uid())]);
        let _ = std::fs::remove_file(launch_agent());
    } else {
        let _ = run("systemctl", &["--user", "disable", "--now", UNIT]);
        let _ = std::fs::remove_file(home().join(".config/systemd/user").join(UNIT));
        let _ = run("systemctl", &["--user", "daemon-reload"]);
    }
    println!("The dumBot server service is removed (bots and pairings are kept).");
    Ok(())
}
