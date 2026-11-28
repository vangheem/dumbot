//! The connection between the app and a dumBot server: mutual TLS, each side with its own
//! self-signed certificate (made on first use). Neither trusts a certificate authority; they
//! trust each other's certificate, by fingerprint, once the user has paired them:
//!
//! 1. The app connects. The server doesn't know its certificate, so it records it as pending
//!    and answers `unpaired`.
//! 2. The app shows a code made from both certificates' fingerprints (as each side saw them)
//!    and asks the user to run `dumbot server approve <code>` on the server.
//! 3. The server makes the same code for each pending certificate; the one that matches is
//!    approved. Someone in the middle would have swapped a certificate on one side, so the
//!    codes wouldn't match.
//! 4. From then on the app only talks to that server certificate, and the server only to
//!    approved ones. All traffic is TLS.
//!
//! Requests are one JSON line per connection. The server answers with `{"step": …}` lines
//! (progress) and then `{"ok": …}` or `{"err": …}`, or `{"unpaired": true}`.

use crate::transport::Sock;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::store;

pub const DEFAULT_PORT: u16 = 7457;

/// A certificate and its key (both DER), kept owner-only.
#[derive(Clone, Serialize, Deserialize)]
pub struct Identity {
    #[serde(with = "b64")]
    pub cert: Vec<u8>,
    #[serde(with = "b64")]
    key: Vec<u8>,
}

mod b64 {
    use base64::Engine;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD
            .decode(s)
            .map_err(serde::de::Error::custom)
    }
}

impl Identity {
    /// This machine's identity for `role` (`client` or `server`), made on first use.
    pub fn load(role: &str) -> Result<Identity> {
        let path = store::data_dir().join(format!("{role}-identity.json"));
        if let Ok(raw) = std::fs::read(&path) {
            return serde_json::from_slice(&raw).context("read the identity");
        }
        let key = rcgen::KeyPair::generate()?;
        let params = rcgen::CertificateParams::new(vec!["dumbot".to_string()])?;
        let cert = params.self_signed(&key)?;
        let id = Identity {
            cert: cert.der().to_vec(),
            key: key.serialize_der(),
        };
        write_private(&path, &serde_json::to_vec_pretty(&id)?)?;
        Ok(id)
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.cert)
    }

    fn chain(&self) -> (Vec<CertificateDer<'static>>, PrivateKeyDer<'static>) {
        (
            vec![CertificateDer::from(self.cert.clone())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key.clone())),
        )
    }
}

pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(data)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// SHA-256 of a certificate, as hex.
pub fn fingerprint(cert: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, cert)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The code the user carries from the app to the server: both fingerprints, hashed, as
/// `ABCD-EFGH` (40 bits; no 0/O/1/I).
pub fn pairing_code(server_fp: &str, client_fp: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let d = ring::digest::digest(
        &ring::digest::SHA256,
        format!("dumbot-pair\0{server_fp}\0{client_fp}").as_bytes(),
    );
    let mut bits = d.as_ref()[..5]
        .iter()
        .fold(0u64, |acc, b| (acc << 8) | *b as u64);
    let mut out = String::new();
    for i in 0..8 {
        if i == 4 {
            out.push('-');
        }
        out.push(ALPHABET[(bits & 31) as usize] as char);
        bits >>= 5;
    }
    out
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn algorithms() -> WebPkiSupportedAlgorithms {
    provider().signature_verification_algorithms
}

// ---- server side --------------------------------------------------------------------------

/// Takes any client certificate (after checking the client holds its key); whether it's
/// approved is checked once the handshake is done.
#[derive(Debug)]
struct AnyClient;

impl ClientCertVerifier for AnyClient {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &algorithms())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &algorithms())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        algorithms().supported_schemes()
    }
}

pub fn server_config(id: &Identity) -> Result<Arc<rustls::ServerConfig>> {
    let (chain, key) = id.chain();
    let cfg = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(Arc::new(AnyClient))
        .with_single_cert(chain, key)?;
    Ok(Arc::new(cfg))
}

/// A paired app, approved on the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Paired {
    pub fingerprint: String,
    pub name: String,
    pub since: i64,
}

/// What the server knows about apps: the approved ones, and those waiting for approval.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Clients {
    #[serde(default)]
    pub approved: Vec<Paired>,
    #[serde(default)]
    pub pending: Vec<Paired>,
}

/// Pending requests expire after an hour; at most this many are kept.
const MAX_PENDING: usize = 20;

fn clients_path() -> PathBuf {
    store::data_dir().join("server-clients.json")
}

impl Clients {
    pub fn load() -> Clients {
        std::fs::read(clients_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        write_private(&clients_path(), &serde_json::to_vec_pretty(self)?)
    }

    pub fn is_approved(&self, fp: &str) -> bool {
        self.approved.iter().any(|c| c.fingerprint == fp)
    }

    /// Records an unknown app's request to pair.
    pub fn request(fp: &str, name: &str) -> Result<()> {
        let mut c = Clients::load();
        let now = chrono::Utc::now().timestamp();
        c.pending
            .retain(|p| now - p.since < 3600 && p.fingerprint != fp);
        c.pending.push(Paired {
            fingerprint: fp.into(),
            name: name.chars().take(80).collect(),
            since: now,
        });
        let extra = c.pending.len().saturating_sub(MAX_PENDING);
        c.pending.drain(..extra);
        c.save()
    }

    /// Approves the pending app whose code is `code`.
    pub fn approve(code: &str, server_fp: &str) -> Result<Paired> {
        let want = code.trim().to_uppercase().replace(' ', "");
        let mut c = Clients::load();
        let i = c
            .pending
            .iter()
            .position(|p| pairing_code(server_fp, &p.fingerprint) == want)
            .ok_or_else(|| {
                anyhow!(
                    "no app is waiting with code {want}; connect from the app again (codes are \
                     kept for an hour)"
                )
            })?;
        let mut p = c.pending.remove(i);
        p.since = chrono::Utc::now().timestamp();
        c.approved.retain(|a| a.fingerprint != p.fingerprint);
        c.approved.push(p.clone());
        c.save()?;
        Ok(p)
    }
}

/// Reads one request line from a connection, for the server.
pub fn read_request(r: &mut impl BufRead) -> Result<Value> {
    let mut line = String::new();
    // Bot imports carry their files; anything much bigger isn't a request.
    let n = std::io::Read::take(r, 256 * 1024 * 1024)
        .read_line(&mut line)
        .context("read the request")?;
    if n == 0 {
        return Err(anyhow!("the connection closed"));
    }
    Ok(serde_json::from_str(&line)?)
}

pub fn write_line(w: &mut impl Write, v: &Value) -> Result<()> {
    let mut s = serde_json::to_string(v)?;
    s.push('\n');
    w.write_all(s.as_bytes())?;
    w.flush()?;
    Ok(())
}

// ---- client side --------------------------------------------------------------------------

/// Takes the server certificate it's pinned to, or (before pairing) any, remembering which.
#[derive(Debug)]
struct PinnedServer {
    expected: Option<String>,
    seen: Mutex<Option<String>>,
}

impl ServerCertVerifier for PinnedServer {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp = fingerprint(end_entity);
        if let Some(e) = &self.expected
            && *e != fp
        {
            return Err(rustls::Error::General(format!(
                "the server's key changed (expected {}…, got {}…); if you reinstalled the \
                 server, disconnect and pair again",
                &e[..12],
                &fp[..12]
            )));
        }
        *self.seen.lock().unwrap() = Some(fp);
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &algorithms())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &algorithms())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        algorithms().supported_schemes()
    }
}

/// `host` or `host:port`, with the default port; a `wss://` URL (a proxy) as it is.
pub fn normalize_addr(addr: &str) -> String {
    let a = addr.trim().trim_end_matches('/');
    if a.contains("://") {
        return a.to_string();
    }
    if a.rsplit_once(':')
        .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
    {
        a.to_string()
    } else {
        format!("{a}:{DEFAULT_PORT}")
    }
}

/// A server, by address, and the fingerprint the app is pinned to (none before pairing).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Server {
    pub addr: String,
    #[serde(default)]
    pub fingerprint: Option<String>,
}

/// What a call to a server came to.
pub enum Reply {
    Ok(Value),
    /// The server doesn't know this app yet: the user approves `code` on the server.
    Unpaired {
        code: String,
    },
}

impl Server {
    /// Sends `request`, calling `step` for each progress line, and returns the result.
    pub fn call(
        &self,
        request: &Value,
        timeout: Duration,
        step: &(dyn Fn(&str) + Sync),
    ) -> Result<Reply> {
        Ok(self.exchange(request, timeout, step)?.0)
    }

    /// Connects, sends `request` and returns the connection, to read the answer from, and
    /// the fingerprint of the server's certificate.
    fn connect(
        &self,
        request: &Value,
        timeout: Duration,
    ) -> Result<(
        BufReader<rustls::StreamOwned<rustls::ClientConnection, Sock>>,
        String,
    )> {
        let id = Identity::load("client")?;
        let verifier = Arc::new(PinnedServer {
            expected: self.fingerprint.clone(),
            seen: Mutex::new(None),
        });
        let (chain, key) = id.chain();
        let cfg = rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(verifier.clone())
            .with_client_auth_cert(chain, key)?;
        let tcp = Sock::connect(&self.addr)?;
        tcp.set_read_timeout(Some(timeout))?;
        tcp.set_write_timeout(Some(Duration::from_secs(60)))?;
        let conn = rustls::ClientConnection::new(Arc::new(cfg), ServerName::try_from("dumbot")?)?;
        let mut tls = rustls::StreamOwned::new(conn, tcp);
        write_line(&mut tls, request).context("send to the server")?;
        let fp = verifier
            .seen
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("the server sent no certificate"))?;
        Ok((BufReader::new(tls), fp))
    }

    /// A connection that sent `request`, for requests that keep it open (see `tunnel`).
    pub fn open(
        &self,
        request: &Value,
        timeout: Duration,
    ) -> Result<BufReader<rustls::StreamOwned<rustls::ClientConnection, Sock>>> {
        Ok(self.connect(request, timeout)?.0)
    }

    /// `call`, and the fingerprint of the server's certificate.
    fn exchange(
        &self,
        request: &Value,
        timeout: Duration,
        step: &(dyn Fn(&str) + Sync),
    ) -> Result<(Reply, String)> {
        let (mut r, server_fp) = self.connect(request, timeout)?;
        let mut line = String::new();
        loop {
            line.clear();
            if r.read_line(&mut line).context("read from the server")? == 0 {
                return Err(anyhow!("the server closed the connection"));
            }
            let v: Value = serde_json::from_str(&line)?;
            if let Some(s) = v["step"].as_str() {
                step(s);
                continue;
            }
            // Keeps a slow request's connection (and any proxy on the way) from going idle.
            if v["ping"] == true {
                continue;
            }
            if v["unpaired"] == true {
                let code = pairing_code(&server_fp, &Identity::load("client")?.fingerprint());
                return Ok((Reply::Unpaired { code }, server_fp));
            }
            if let Some(e) = v["err"].as_str() {
                return Err(anyhow!("{e}"));
            }
            return Ok((Reply::Ok(v["ok"].clone()), server_fp));
        }
    }

    /// Asks to pair, or checks whether the server has approved this app. Either way the
    /// returned server is pinned to the certificate it saw; keep using that one, so the
    /// server can't change between showing the code and its approval.
    pub fn pair(&self) -> Result<(Server, Option<String>)> {
        let name = crate::container::command_output("hostname", &[]).unwrap_or_default();
        let (reply, fp) = self.exchange(
            &json!({ "op": "hello", "name": name }),
            Duration::from_secs(10),
            &|_| {},
        )?;
        let pinned = Server {
            addr: self.addr.clone(),
            fingerprint: Some(fp),
        };
        Ok(match reply {
            Reply::Ok(_) => (pinned, None),
            Reply::Unpaired { code, .. } => (pinned, Some(code)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_bind_both_fingerprints() {
        let a = pairing_code("s1", "c1");
        assert_eq!(a.len(), 9);
        assert_eq!(a, pairing_code("s1", "c1"));
        assert_ne!(a, pairing_code("s2", "c1"));
        assert_ne!(a, pairing_code("s1", "c2"));
        assert_eq!(normalize_addr("box"), "box:7457");
        assert_eq!(normalize_addr("box:9"), "box:9");
        assert_eq!(
            normalize_addr("wss://bots.example.com/"),
            "wss://bots.example.com"
        );
    }
}
