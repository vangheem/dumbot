//! MCP servers only bots get, apart from the ones in the opencode config: global ones (every
//! bot) and each bot's own, set by the user and kept in `~/.local/share/dumbot/mcp.json`
//! (owner-only, as they may hold tokens); and the ones a bot installs for itself with
//! `dumbot mcp`, kept in its folder's `mcp.json`. Each is in opencode's `mcp` format:
//! `{ "type": "remote", "url": "…", "headers": {…} }` or
//! `{ "type": "local", "command": ["…"], "environment": {…} }`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::store;

pub type Servers = BTreeMap<String, Value>;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BotMcp {
    #[serde(default)]
    pub global: Servers,
    /// By bot slug; these win over global ones of the same name.
    #[serde(default)]
    pub bots: BTreeMap<String, Servers>,
}

fn path() -> PathBuf {
    store::data_dir().join("mcp.json")
}

pub fn load() -> BotMcp {
    std::fs::read(path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

impl BotMcp {
    pub fn save(&self) -> Result<()> {
        crate::remote::write_private(&path(), &serde_json::to_vec_pretty(self)?)
    }

    /// A bot's servers: the global ones, then its own.
    pub fn for_bot(&self, slug: &str) -> Servers {
        let mut s = self.global.clone();
        s.extend(self.bots.get(slug).cloned().unwrap_or_default());
        s
    }

    /// `None` is global. An empty set removes a bot's entry.
    pub fn set(&mut self, bot: Option<&str>, servers: Servers) {
        match bot {
            None => self.global = servers,
            Some(slug) if servers.is_empty() => {
                self.bots.remove(slug);
            }
            Some(slug) => {
                self.bots.insert(slug.to_string(), servers);
            }
        }
    }
}

/// The servers a bot installed for itself.
pub fn installed(slug: &str) -> Servers {
    std::fs::read(installed_path(slug))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn installed_path(slug: &str) -> PathBuf {
    crate::bots::dir(slug).join("mcp.json")
}

fn save_installed(slug: &str, servers: &Servers) -> Result<()> {
    std::fs::write(installed_path(slug), serde_json::to_vec_pretty(servers)?)?;
    Ok(())
}

/// Replaces `{env:NAME}` in every string of `v` with the variable's value (left as is when
/// there's no such variable).
pub fn resolve_env(v: &mut Value, vars: &BTreeMap<String, String>) {
    match v {
        Value::String(s) if s.contains("{env:") => {
            for (k, val) in vars {
                *s = s.replace(&format!("{{env:{k}}}"), val);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| resolve_env(x, vars)),
        Value::Object(o) => o.values_mut().for_each(|x| resolve_env(x, vars)),
        _ => {}
    }
}

const USAGE: &str = "usage:
  dumbot mcp <bot> add <name> --url URL [--header 'Name: value']…
  dumbot mcp <bot> add <name> [--env NAME=value]… -- <command> [args…]
  dumbot mcp <bot> remove <name>
  dumbot mcp <bot> list
Values may use {env:NAME} for the bot's environment variables.";

/// `dumbot mcp …`: a bot installs, removes and lists its own MCP servers. They're on from
/// its next run.
pub fn cli(args: &[String]) -> Result<()> {
    let (Some(bot), Some(cmd)) = (args.first(), args.get(1)) else {
        return Err(anyhow!("{USAGE}"));
    };
    let slug = crate::bots::slugify(bot);
    crate::bots::load(&slug)?;
    let mut servers = installed(&slug);
    match (cmd.as_str(), args.get(2)) {
        ("list", _) => {
            for (name, v) in &servers {
                println!("{name}\t{v}");
            }
            if servers.is_empty() {
                println!("No MCP servers installed.");
            }
            return Ok(());
        }
        ("remove", Some(name)) => {
            if servers.remove(name).is_none() {
                return Err(anyhow!("{name:?} isn't installed"));
            }
            println!("Removed {name}; it's off from your next run.");
        }
        ("add", Some(name)) => {
            let server = parse_add(&args[3..])?;
            parse(&serde_json::to_string(&BTreeMap::from([(
                name.clone(),
                server.clone(),
            )]))?)?;
            servers.insert(name.clone(), server);
            println!("Installed {name}; its tools are there from your next run.");
        }
        _ => return Err(anyhow!("{USAGE}")),
    }
    save_installed(&slug, &servers)
}

fn parse_add(args: &[String]) -> Result<Value> {
    let mut it = args.iter();
    let (mut url, mut headers, mut env) = (None, serde_json::Map::new(), serde_json::Map::new());
    let mut command: Vec<String> = Vec::new();
    while let Some(a) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| anyhow!("{a} needs a value"))
        };
        match a.as_str() {
            "--url" => url = Some(value()?),
            "--header" | "-H" => {
                let h = value()?;
                let (k, v) = h
                    .split_once(':')
                    .ok_or_else(|| anyhow!("a header is 'Name: value'"))?;
                headers.insert(k.trim().into(), v.trim().into());
            }
            "--env" | "-e" => {
                let e = value()?;
                let (k, v) = e
                    .split_once('=')
                    .ok_or_else(|| anyhow!("--env is NAME=value"))?;
                env.insert(k.into(), v.into());
            }
            "--" => {
                command = it.by_ref().cloned().collect();
            }
            other => return Err(anyhow!("unknown option {other}\n{USAGE}")),
        }
    }
    match (url, command.is_empty()) {
        (Some(url), true) => {
            let mut v = serde_json::json!({ "type": "remote", "url": url });
            if !headers.is_empty() {
                v["headers"] = Value::Object(headers);
            }
            Ok(v)
        }
        (None, false) => {
            let mut v = serde_json::json!({ "type": "local", "command": command });
            if !env.is_empty() {
                v["environment"] = Value::Object(env);
            }
            Ok(v)
        }
        _ => Err(anyhow!("give either --url or -- <command>\n{USAGE}")),
    }
}

/// A server as the app's form shows it: a URL and headers, or a command and environment
/// variables; headers and variables one per line (`Name: value`, `NAME=value`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Form {
    pub name: String,
    pub remote: bool,
    /// The URL, or the command line.
    pub target: String,
    pub extra: String,
}

impl Form {
    pub fn of(name: &str, v: &Value) -> Form {
        let remote = v["type"] != "local";
        let pairs = |key: &str, sep: &str| {
            v[key]
                .as_object()
                .map(|o| {
                    o.iter()
                        .map(|(k, x)| format!("{k}{sep}{}", x.as_str().unwrap_or_default()))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default()
        };
        Form {
            name: name.into(),
            remote,
            target: if remote {
                v["url"].as_str().unwrap_or_default().into()
            } else {
                join_command(
                    &v["command"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(String::from))
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default(),
                )
            },
            extra: if remote {
                pairs("headers", ": ")
            } else {
                pairs("environment", "=")
            },
        }
    }

    /// The server, keeping whatever else `base` (the server being edited) had.
    pub fn server(&self, base: Option<&Value>) -> Result<(String, Value)> {
        let name = self.name.trim();
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
        {
            return Err(anyhow!("name it with letters, digits, - and _"));
        }
        let mut v = base.cloned().unwrap_or_else(|| serde_json::json!({}));
        let o = v.as_object_mut().ok_or_else(|| anyhow!("not a server"))?;
        for k in ["url", "headers", "command", "environment"] {
            o.remove(k);
        }
        let (sep, key) = if self.remote {
            (':', "headers")
        } else {
            ('=', "environment")
        };
        let mut extra = serde_json::Map::new();
        for line in self.extra.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let (k, x) = line.split_once(sep).ok_or_else(|| {
                anyhow!(if self.remote {
                    "write headers as Name: value"
                } else {
                    "write variables as NAME=value"
                })
            })?;
            extra.insert(k.trim().into(), x.trim().into());
        }
        if self.remote {
            o.insert("type".into(), "remote".into());
            o.insert("url".into(), self.target.trim().into());
        } else {
            let cmd = split_command(&self.target);
            if cmd.is_empty() {
                return Err(anyhow!("enter the command that runs the server"));
            }
            o.insert("type".into(), "local".into());
            o.insert("command".into(), cmd.into());
        }
        if !extra.is_empty() {
            o.insert(key.into(), Value::Object(extra));
        }
        let one = Servers::from([(name.to_string(), v)]);
        parse(&serde_json::to_string(&one)?)?;
        let (name, v) = one.into_iter().next().unwrap();
        Ok((name, v))
    }
}

/// Splits a command line on spaces, keeping quoted parts ('…' or "…") together.
pub fn split_command(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut quote, mut any) = (Vec::new(), String::new(), None, false);
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

pub fn join_command(parts: &[String]) -> String {
    parts
        .iter()
        .map(|p| {
            if p.is_empty() || p.contains(char::is_whitespace) {
                format!("\"{p}\"")
            } else {
                p.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// What a server is, in a line: its URL, or its command.
pub fn summary(v: &Value) -> String {
    match v["url"].as_str() {
        Some(u) => u.to_string(),
        None => join_command(
            &v["command"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        ),
    }
}

/// Parses the JSON object the app edits, checking each server says what it is.
pub fn parse(text: &str) -> Result<Servers> {
    if text.trim().is_empty() {
        return Ok(Servers::new());
    }
    let servers: Servers = serde_json::from_str(text)
        .map_err(|e| anyhow!("not a JSON object of servers by name: {e}"))?;
    for (name, v) in &servers {
        match v["type"].as_str() {
            Some("remote") if v["url"].is_string() => {}
            Some("local") if v["command"].is_array() => {}
            _ => {
                return Err(anyhow!(
                    "{name:?} needs \"type\": \"remote\" with a \"url\", or \"type\": \"local\" \
                     with a \"command\" list"
                ));
            }
        }
    }
    Ok(servers)
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Form {
        fn tap_extra(mut self) -> Form {
            self.extra = self.extra.trim().to_string();
            self
        }
    }

    #[test]
    fn bots_own_servers_win() {
        let mut m = BotMcp::default();
        m.set(
            None,
            parse(r#"{"a": {"type": "remote", "url": "g"}, "b": {"type": "remote", "url": "g"}}"#)
                .unwrap(),
        );
        m.set(
            Some("x"),
            parse(r#"{"b": {"type": "local", "command": ["b"]}}"#).unwrap(),
        );
        let s = m.for_bot("x");
        assert_eq!(s["a"]["url"], "g");
        assert_eq!(s["b"]["type"], "local");
        assert!(parse(r#"{"c": {"type": "remote"}}"#).is_err());
        m.set(Some("x"), Servers::new());
        assert!(m.bots.is_empty());
        assert_eq!(
            split_command(r#"npx -y "my pkg" --flag='a b'"#),
            ["npx", "-y", "my pkg", "--flag=a b"]
        );
        let base = serde_json::json!({ "type": "local", "command": ["x"], "timeout": 5 });
        let f = Form {
            name: "tool".into(),
            remote: false,
            target: r#"npx -y "my pkg""#.into(),
            extra: "TOKEN={env:TOKEN}\n".into(),
        };
        let (n, v) = f.server(Some(&base)).unwrap();
        assert_eq!(n, "tool");
        assert_eq!(v["timeout"], 5);
        assert_eq!(v["environment"]["TOKEN"], "{env:TOKEN}");
        assert_eq!(Form::of(&n, &v), f.clone().tap_extra());
        let r = Form {
            name: "r".into(),
            remote: true,
            target: "https://x/mcp".into(),
            extra: "Authorization: Bearer t".into(),
        };
        let (_, v) = r.server(None).unwrap();
        assert_eq!(v["headers"]["Authorization"], "Bearer t");
        assert!(
            Form {
                name: "bad name".into(),
                ..r
            }
            .server(None)
            .is_err()
        );
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let mut v = parse_add(&s(&[
            "--url",
            "https://x/mcp",
            "-H",
            "Authorization: Bearer {env:TOK}",
        ]))
        .unwrap();
        resolve_env(&mut v, &BTreeMap::from([("TOK".into(), "abc".into())]));
        assert_eq!(v["headers"]["Authorization"], "Bearer abc");
        let v = parse_add(&s(&["-e", "A=1", "--", "npx", "-y", "pkg"])).unwrap();
        assert_eq!(v["command"][2], "pkg");
        assert_eq!(v["environment"]["A"], "1");
    }
}
