//! What the app's TLS (see `remote`) runs over: a TCP connection to the server, or a WebSocket,
//! so the server can sit behind an HTTPS proxy such as a Cloudflare tunnel
//! (`wss://bots.example.com`). The proxy carries the WebSocket's bytes; inside them is the same
//! mutual TLS between the app's and the server's own keys, so it can't read or change them.
//!
//! The server takes both on one port: a TLS hello starts with 0x16, anything else is an HTTP
//! request to upgrade to a WebSocket.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use tungstenite::{Message, WebSocket};

pub enum Sock {
    Tcp(TcpStream),
    Ws(Box<Ws>),
}

pub struct Ws {
    ws: WebSocket<Inner>,
    /// The underlying connection, to set timeouts on.
    tcp: TcpStream,
    buf: Vec<u8>,
    pos: usize,
}

enum Inner {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Read for Inner {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Inner::Plain(s) => s.read(b),
            Inner::Tls(s) => s.read(b),
        }
    }
}

impl Write for Inner {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        match self {
            Inner::Plain(s) => s.write(b),
            Inner::Tls(s) => s.write(b),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Inner::Plain(s) => s.flush(),
            Inner::Tls(s) => s.flush(),
        }
    }
}

fn ws_err(e: tungstenite::Error) -> std::io::Error {
    match e {
        tungstenite::Error::Io(e) => e,
        tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed => {
            std::io::Error::new(ErrorKind::ConnectionAborted, "the WebSocket closed")
        }
        e => std::io::Error::other(e.to_string()),
    }
}

impl Read for Ws {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        while self.pos >= self.buf.len() {
            match self.ws.read() {
                Ok(Message::Binary(b)) => {
                    self.buf = b;
                    self.pos = 0;
                }
                Ok(Message::Close(_)) => return Ok(0),
                Ok(_) => {} // pings are answered by tungstenite
                Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                    return Ok(0);
                }
                Err(e) => return Err(ws_err(e)),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

impl Write for Ws {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.ws.send(Message::Binary(b.to_vec())).map_err(ws_err)?;
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.ws.flush().map_err(ws_err)
    }
}

impl Read for Sock {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Sock::Tcp(s) => s.read(b),
            Sock::Ws(s) => s.read(b),
        }
    }
}

impl Write for Sock {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        match self {
            Sock::Tcp(s) => s.write(b),
            Sock::Ws(s) => s.write(b),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Sock::Tcp(s) => s.flush(),
            Sock::Ws(s) => s.flush(),
        }
    }
}

impl Sock {
    fn tcp(&self) -> &TcpStream {
        match self {
            Sock::Tcp(s) => s,
            Sock::Ws(w) => &w.tcp,
        }
    }

    pub fn set_read_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        self.tcp().set_read_timeout(d)
    }

    pub fn set_write_timeout(&self, d: Option<Duration>) -> std::io::Result<()> {
        self.tcp().set_write_timeout(d)
    }

    pub fn peer(&self) -> String {
        self.tcp()
            .peer_addr()
            .map(|a| a.to_string())
            .unwrap_or_default()
    }

    /// A server's side of a new connection: TLS straight away, or a WebSocket to upgrade.
    pub fn accept(tcp: TcpStream) -> Result<Sock> {
        let mut first = [0u8; 1];
        tcp.set_read_timeout(Some(Duration::from_secs(30)))?;
        if tcp.peek(&mut first)? == 0 {
            return Err(anyhow!("the connection closed"));
        }
        if first[0] == 0x16 {
            return Ok(Sock::Tcp(tcp));
        }
        let handle = tcp.try_clone()?;
        let ws = tungstenite::accept(Inner::Plain(tcp))
            .map_err(|e| anyhow!("WebSocket handshake: {e}"))?;
        Ok(Sock::Ws(Box::new(Ws {
            ws,
            tcp: handle,
            buf: Vec::new(),
            pos: 0,
        })))
    }

    /// Connects to `addr`: `host:port`, or a `ws://` / `wss://` URL (through a proxy).
    pub fn connect(addr: &str) -> Result<Sock> {
        let Some((scheme, rest)) = addr.split_once("://") else {
            return Ok(Sock::Tcp(tcp(addr)?));
        };
        let host_port = rest.split('/').next().unwrap_or(rest);
        let (host, port) = match host_port.rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() => (h.to_string(), p.parse::<u16>()?),
            _ => (
                host_port.to_string(),
                if scheme == "wss" { 443 } else { 80 },
            ),
        };
        let tcp = tcp(&format!("{host}:{port}"))?;
        let handle = tcp.try_clone()?;
        let inner = match scheme {
            "wss" => {
                let mut roots = rustls::RootCertStore::empty();
                roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
                let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
                    rustls::crypto::ring::default_provider(),
                ))
                .with_safe_default_protocol_versions()?
                .with_root_certificates(roots)
                .with_no_client_auth();
                let name = rustls::pki_types::ServerName::try_from(host.clone())?;
                let conn = rustls::ClientConnection::new(Arc::new(cfg), name)?;
                Inner::Tls(Box::new(rustls::StreamOwned::new(conn, tcp)))
            }
            "ws" => Inner::Plain(tcp),
            s => return Err(anyhow!("unknown address scheme {s}://")),
        };
        let (ws, _) = tungstenite::client::client(addr, inner)
            .map_err(|e| anyhow!("WebSocket to {addr}: {e}"))?;
        Ok(Sock::Ws(Box::new(Ws {
            ws,
            tcp: handle,
            buf: Vec::new(),
            pos: 0,
        })))
    }
}

fn tcp(addr: &str) -> Result<TcpStream> {
    let a = addr
        .to_socket_addrs()
        .with_context(|| format!("resolve {addr}"))?
        .next()
        .ok_or_else(|| anyhow!("{addr} has no address"))?;
    TcpStream::connect_timeout(&a, Duration::from_secs(10))
        .with_context(|| format!("connect to the dumBot server at {addr}"))
}
