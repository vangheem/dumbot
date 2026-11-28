//! Seeing the server browser from the app: its viewer (noVNC, on the server's loopback) is
//! carried over the paired TLS connection. The app listens on a port of its own computer's
//! loopback and opens a connection to the server (`{"op": "browser_view"}`) for each one made
//! to it, which the server joins to the viewer. You open it in your own browser.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::ops::{Deref, DerefMut};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use serde_json::json;

use crate::remote::Server;
use crate::transport::Sock;

/// Copies bytes both ways between a TLS stream and a plain one until either closes. A TLS
/// stream can't be split between threads, so one thread takes turns, each read waiting
/// briefly.
pub fn pipe<C, D>(mut tls: rustls::StreamOwned<C, Sock>, mut plain: TcpStream) -> Result<()>
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

/// The server's side: joins an app's connection to the browser's viewer. `early` is what the
/// app sent past its request line, read along with it.
pub fn serve<C, D>(tls: rustls::StreamOwned<C, Sock>, early: Vec<u8>) -> Result<()>
where
    C: DerefMut + Deref<Target = rustls::ConnectionCommon<D>>,
    D: rustls::SideData,
{
    let mut viewer = TcpStream::connect(("127.0.0.1", crate::browser::VIEW_PORT))?;
    viewer.write_all(&early)?;
    pipe(tls, viewer)
}

static FORWARDS: Mutex<Option<HashMap<String, u16>>> = Mutex::new(None);

/// The app's side: the local port that reaches `server`'s browser viewer (listening from the
/// first call on).
pub fn local_port(server: &Server) -> Result<u16> {
    let mut f = FORWARDS.lock().unwrap_or_else(|e| e.into_inner());
    let map = f.get_or_insert_with(HashMap::new);
    if let Some(p) = map.get(&server.addr) {
        return Ok(*p);
    }
    let l = TcpListener::bind("127.0.0.1:0")?;
    let port = l.local_addr()?.port();
    let s = server.clone();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let s = s.clone();
            std::thread::spawn(move || {
                let res = s
                    .open(&json!({ "op": "browser_view" }), Duration::from_secs(60))
                    .and_then(|r| pipe(r.into_inner(), c));
                if let Err(e) = res {
                    eprintln!("server browser view: {e:#}");
                }
            });
        }
    });
    map.insert(server.addr.clone(), port);
    Ok(port)
}

/// The viewer's page, scaled to the window and connecting at once.
pub fn url(port: u16) -> String {
    format!(
        "http://127.0.0.1:{port}/vnc.html?autoconnect=1&resize=scale&reconnect=1&path=websockify"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Backend, Request, Status};

    /// Against a paired server (see `api`'s live tests): the viewer comes through the app's
    /// local port, and a handoff in the server browser shows in the server's status.
    #[test]
    #[ignore]
    fn server_browser_view_and_handoffs() {
        let server = crate::store::Store::load().remote.unwrap();
        let port = local_port(&server).unwrap();
        let page = ureq::get(&format!("http://127.0.0.1:{port}/vnc.html"))
            .call()
            .unwrap()
            .into_string()
            .unwrap();
        assert!(page.contains("noVNC"));

        let host = std::env::var("DUMBOT_TEST_SSH").unwrap_or("nathan@192.168.86.36".into());
        let sh = move |cmd: &str| {
            std::process::Command::new("ssh")
                .args([host.as_str(), cmd])
                .output()
                .unwrap()
        };
        let tab = String::from_utf8(
            sh("docker exec dumbot-browser browse navigate https://example.com | jq -r .tab")
                .stdout,
        )
        .unwrap();
        let tab = tab.trim().to_string();
        let msg =
            format!(r#"{{"message":"Test: click Continue.","tab":{tab},"timeoutSeconds":20}}"#);
        let sh2 = sh.clone();
        let waiter = std::thread::spawn(move || {
            sh2(&format!(
                "docker exec dumbot-browser browse handoff --json '{msg}'"
            ))
        });
        std::thread::sleep(Duration::from_secs(5));
        let b = Backend::Remote(server);
        let st: Status = b.call(Request::Status).unwrap();
        let h = st.browser.unwrap().handoffs;
        assert!(
            h.iter()
                .any(|h| h.tab.to_string() == tab && h.url.contains("example.com")),
            "{h:?}"
        );
        waiter.join().unwrap();
        let st: Status = b.call(Request::Status).unwrap();
        assert!(st.browser.unwrap().handoffs.is_empty());
    }
}
