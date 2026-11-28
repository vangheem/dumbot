//! Browse through the app: bots on a server use the browser on the app's computer.
//!
//! Browse's MCP server (in a bot's container) talks to the browse relay, which runs beside
//! the browser. On a server, the server listens on the same loopback port as the app's relay
//! (the relay checks requests are for its own port); each connection to it is carried over
//! TLS to the app, which connects it to its relay:
//!
//! - While connected, the app holds a control connection (`{"op": "browse_tunnel"}`) open.
//! - For each connection to the relay port, the server sends `{"open": id}` on it, and the
//!   app opens a new connection (`{"op": "browse_data", "id": …}`), connects it to its relay
//!   and copies bytes both ways.
//!
//! So bots on a server use your browser (and you take over for logins) while the app is open.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::json;

use crate::remote::{self, Server};

/// The app's relay port, which bots on the server connect to on the server's loopback.
pub fn port() -> Option<u16> {
    crate::context::current()?.browse_port
}

// ---- server side --------------------------------------------------------------------------

type Control = Box<dyn Write + Send>;

static CONTROL: Mutex<Option<Control>> = Mutex::new(None);
static PENDING: std::sync::LazyLock<Mutex<HashMap<u64, TcpStream>>> =
    std::sync::LazyLock::new(Default::default);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn pending() -> std::sync::MutexGuard<'static, HashMap<u64, TcpStream>> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether an app is connected to carry browse.
pub fn connected() -> bool {
    CONTROL.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

/// Listens on the app's relay port (once per port), accepting bots' connections and asking
/// the app to carry each.
pub fn listen() -> Result<()> {
    static LISTENING: Mutex<Option<u16>> = Mutex::new(None);
    let Some(port) = port() else { return Ok(()) };
    let mut listening = LISTENING.lock().unwrap_or_else(|e| e.into_inner());
    if *listening == Some(port) {
        return Ok(());
    }
    let l = TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| anyhow!("listen on 127.0.0.1:{port} for browse: {e}"))?;
    *listening = Some(port);
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let mut control = CONTROL.lock().unwrap_or_else(|e| e.into_inner());
            let Some(w) = control.as_mut() else {
                continue; // no app: the connection closes
            };
            // Waiting before the app is asked, so its connection finds it.
            pending().insert(id, s);
            if remote::write_line(w, &json!({ "open": id })).is_err() {
                *control = None;
                pending().remove(&id);
            }
        }
    });
    Ok(())
}

/// Holds an app's control connection until it drops, pinging it so a dead one is noticed.
pub fn serve_control<C, D>(tls: rustls::StreamOwned<C, TcpStream>) -> Result<()>
where
    C: DerefMut + Deref<Target = rustls::ConnectionCommon<D>> + Send + 'static,
    D: rustls::SideData,
{
    let shared = Arc::new(Mutex::new(tls));
    struct W<T>(Arc<Mutex<T>>);
    impl<T: Write> Write for W<T> {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().write(b)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.0.lock().unwrap().flush()
        }
    }
    *CONTROL.lock().unwrap_or_else(|e| e.into_inner()) = Some(Box::new(W(shared.clone())));
    eprintln!("browse: the app is carrying browse");
    loop {
        std::thread::sleep(Duration::from_secs(20));
        let mut control = CONTROL.lock().unwrap_or_else(|e| e.into_inner());
        let Some(w) = control.as_mut() else { break };
        if remote::write_line(w, &json!({ "ping": true })).is_err() {
            *control = None;
            eprintln!("browse: the app went away");
            break;
        }
    }
    Ok(())
}

/// Joins an app's data connection to the bot's connection waiting for it.
/// `early` is what the app sent past its request line, before this got to it.
pub fn serve_data<C, D>(
    tls: rustls::StreamOwned<C, TcpStream>,
    id: u64,
    early: Vec<u8>,
) -> Result<()>
where
    C: DerefMut + Deref<Target = rustls::ConnectionCommon<D>> + Send + 'static,
    D: rustls::SideData,
{
    let mut s = pending()
        .remove(&id)
        .ok_or_else(|| anyhow!("no browse connection {id}"))?;
    s.write_all(&early)?;
    pipe(tls, s)
}

// ---- both sides ---------------------------------------------------------------------------

/// Copies bytes both ways between a TLS stream and a plain one until either closes. A TLS
/// stream can't be split between threads, so one thread takes turns, each read waiting
/// briefly.
fn pipe<C, D>(mut tls: rustls::StreamOwned<C, TcpStream>, mut plain: TcpStream) -> Result<()>
where
    C: DerefMut + Deref<Target = rustls::ConnectionCommon<D>>,
    D: rustls::SideData,
{
    let wait = Some(Duration::from_millis(10));
    tls.sock.set_read_timeout(wait)?;
    plain.set_read_timeout(wait)?;
    let waiting =
        |e: &std::io::Error| matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut);
    let mut buf = [0u8; 16 * 1024];
    loop {
        match plain.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                tls.write_all(&buf[..n])?;
                tls.flush()?;
            }
            Err(e) if waiting(&e) => {}
            Err(_) => break,
        }
        match tls.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => plain.write_all(&buf[..n])?,
            Err(e) if waiting(&e) => {}
            Err(_) => break,
        }
    }
    tls.conn.send_close_notify();
    let _ = tls.flush();
    let _ = plain.shutdown(std::net::Shutdown::Both);
    Ok(())
}

// ---- app side -----------------------------------------------------------------------------

/// The server the app carries browse for; `None` stops it.
static CARRY: Mutex<Option<Server>> = Mutex::new(None);

/// Carries browse for `server` from now on (starting the background work once).
pub fn carry(server: Option<Server>) {
    static STARTED: std::sync::Once = std::sync::Once::new();
    *CARRY.lock().unwrap_or_else(|e| e.into_inner()) = server;
    STARTED.call_once(|| {
        std::thread::spawn(|| {
            loop {
                let current = CARRY.lock().unwrap_or_else(|e| e.into_inner()).clone();
                if let Some(s) = current
                    && let Err(e) = control(&s)
                {
                    eprintln!("browse tunnel: {e:#}");
                }
                std::thread::sleep(Duration::from_secs(3));
            }
        });
    });
}

fn still(server: &Server) -> bool {
    CARRY.lock().unwrap_or_else(|e| e.into_inner()).as_ref() == Some(server)
}

/// One control connection: opens a data connection for each `open` until it drops.
fn control(server: &Server) -> Result<()> {
    // Without browse here there's nothing to carry.
    crate::container::browse_port()?;
    let mut r = server.open(&json!({ "op": "browse_tunnel" }), Duration::from_secs(60))?;
    let mut line = String::new();
    while still(server) {
        line.clear();
        if std::io::BufRead::read_line(&mut r, &mut line)? == 0 {
            break;
        }
        let v: serde_json::Value = serde_json::from_str(&line)?;
        if let Some(id) = v["open"].as_u64() {
            let s = server.clone();
            std::thread::spawn(move || {
                let res = (|| {
                    let relay =
                        TcpStream::connect(("127.0.0.1", crate::container::browse_port()?))?;
                    let tls = s
                        .open(
                            &json!({ "op": "browse_data", "id": id }),
                            Duration::from_secs(60),
                        )?
                        .into_inner();
                    pipe(tls, relay)
                })();
                if let Err(e) = res {
                    eprintln!("browse tunnel: {e:#}");
                }
            });
        }
    }
    Ok(())
}
