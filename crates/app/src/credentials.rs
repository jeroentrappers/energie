//! `energie credentials`: hand a cloud device new credentials, or show how its sign-in is doing.
//!
//! `set` asks for the kind's fields (the password without echo on a terminal; one line per
//! field from stdin otherwise) and posts them to the running service's
//! `POST /api/cloud/{id}/credentials`, which only accepts loopback peers, keeps them in the state
//! dir (0600) and signs in at once. Nothing is written by this process itself, so it works with
//! the service's `DynamicUser` state dir. `status` prints `GET /api/cloud`.

use std::io::BufRead;

use serde_json::{Map, Value};

/// The service's address unless `--url` says otherwise (the default `listen`).
pub const DEFAULT_URL: &str = "http://127.0.0.1:8080";

/// `energie credentials …`
#[derive(Debug, PartialEq)]
pub enum CredCmd {
    Set { device: String, url: String },
    Status { url: String },
}

/// The arguments after `credentials`.
pub fn parse(args: &[String]) -> Result<CredCmd, String> {
    let (cmd, rest) = args.split_first().ok_or("credentials needs set <device> or status")?;
    let mut url = DEFAULT_URL.to_string();
    let mut device = None;
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--url" => url = it.next().ok_or("--url needs a value")?.clone(),
            s if !s.starts_with('-') && device.is_none() && cmd == "set" => device = Some(s.to_string()),
            _ => return Err(format!("unexpected credentials argument: {a}")),
        }
    }
    match cmd.as_str() {
        "set" => {
            device.map(|device| CredCmd::Set { device, url }).ok_or_else(|| "credentials set needs a device id".into())
        }
        "status" => Ok(CredCmd::Status { url }),
        other => Err(format!("unknown credentials command: {other}")),
    }
}

/// One field to ask for: key, prompt, secret, required.
type Ask = (&'static str, &'static str, bool, bool);

/// The fields a kind takes.
pub fn fields_for(kind: &str) -> Option<&'static [Ask]> {
    const KIA: &[Ask] = &[
        ("username", "Kia Connect username (e-mail)", false, true),
        ("password", "Password", true, true),
        ("pin", "PIN (optional)", true, false),
    ];
    const VW: &[Ask] = &[("email", "VW Group ID e-mail", false, true), ("password", "Password", true, true)];
    match kind {
        "kia_connect" => Some(KIA),
        "cupra_data_act" => Some(VW),
        _ => None,
    }
}

/// Where the answers come from.
pub trait Prompt {
    fn ask(&mut self, label: &str, secret: bool) -> std::io::Result<String>;
}

/// The terminal: the prompt on stderr, a secret read without echo.
pub struct Terminal;

impl Prompt for Terminal {
    fn ask(&mut self, label: &str, secret: bool) -> std::io::Result<String> {
        if secret {
            return rpassword::prompt_password(format!("{label}: "));
        }
        eprint!("{label}: ");
        Lines(std::io::stdin().lock()).ask(label, false)
    }
}

/// One answer per line (stdin that is not a terminal, tests).
pub struct Lines<R>(pub R);

impl<R: BufRead> Prompt for Lines<R> {
    fn ask(&mut self, _label: &str, _secret: bool) -> std::io::Result<String> {
        let mut line = String::new();
        self.0.read_line(&mut line)?;
        Ok(line.trim_end_matches(['\r', '\n']).to_string())
    }
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|e| format!("http client: {e}"))
}

async fn cloud(url: &str) -> Result<Map<String, Value>, String> {
    let resp = client()?
        .get(format!("{}/api/cloud", url.trim_end_matches('/')))
        .send()
        .await
        .map_err(|e| format!("cannot reach the service at {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{url}/api/cloud: HTTP {}", resp.status().as_u16()));
    }
    match resp.json::<Value>().await {
        Ok(Value::Object(m)) => Ok(m),
        _ => Err(format!("{url}/api/cloud: not a JSON object")),
    }
}

/// One line per state: `ev3 (kia_connect): ok, credentials from state, last login … ok`.
fn line(id: &str, kind: &str, a: &Value) -> String {
    let s = |k: &str| a[k].as_str().unwrap_or("–").to_string();
    let mut out = format!("{id} ({kind}): {}, credentials from {}", s("state"), s("credentials"));
    if a["last_login"].is_string() {
        out.push_str(&format!(", last login {}: {}", s("last_login"), s("last_result")));
    }
    if a["rejected_logins"].as_u64().unwrap_or(0) > 0 {
        out.push_str(&format!(", {} rejected in a row", a["rejected_logins"]));
    }
    if a["next_login"].is_string() {
        out.push_str(&format!(", next login {}", s("next_login")));
    }
    out
}

/// `credentials status`: one line per device.
pub async fn status(url: &str) -> Result<String, String> {
    let all = cloud(url).await?;
    if all.is_empty() {
        return Ok("no device signs in with credentials".into());
    }
    Ok(all.iter().map(|(id, v)| line(id, v["kind"].as_str().unwrap_or("?"), &v["auth"])).collect::<Vec<_>>().join("\n"))
}

/// `credentials set <device>`: ask, post, and the outcome as one line; `Ok(false)` when the
/// login did not succeed.
pub async fn set(url: &str, device: &str, prompt: &mut dyn Prompt) -> Result<(String, bool), String> {
    let all = cloud(url).await?;
    let kind = all.get(device).and_then(|v| v["kind"].as_str()).ok_or_else(|| {
        let known: Vec<&str> = all.keys().map(String::as_str).collect();
        format!("{device} does not sign in with credentials (those that do: {})", known.join(", "))
    })?;
    let asks = fields_for(kind).ok_or_else(|| format!("{device}: no credentials prompt for kind {kind}"))?;
    let mut fields = Map::new();
    for (key, label, secret, required) in asks {
        let v = prompt.ask(label, *secret).map_err(|e| format!("reading {key}: {e}"))?;
        if v.is_empty() && *required {
            return Err(format!("{key} is required"));
        }
        if !v.is_empty() {
            fields.insert((*key).into(), Value::String(v));
        }
    }
    let resp = client()?
        .post(format!("{}/api/cloud/{device}/credentials", url.trim_end_matches('/')))
        .json(&Value::Object(fields))
        .send()
        .await
        .map_err(|e| format!("cannot reach the service at {url}: {e}"))?;
    let code = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if code != 200 {
        return Err(format!("{device}: HTTP {code}: {text}"));
    }
    let v = serde_json::from_str::<Value>(&text)
        .ok()
        .filter(|v| v["auth"].is_object())
        .ok_or_else(|| format!("{device}: not a JSON answer"))?;
    Ok((line(device, kind, &v["auth"]), v["auth"]["state"] == "ok"))
}

/// Run `energie credentials …`; the exit code.
pub fn main(cmd: CredCmd) -> i32 {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    let tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let out = rt.block_on(async {
        match &cmd {
            CredCmd::Status { url } => status(url).await.map(|s| (s, true)),
            CredCmd::Set { device, url } if tty => set(url, device, &mut Terminal).await,
            CredCmd::Set { device, url } => set(url, device, &mut Lines(std::io::stdin().lock())).await,
        }
    });
    match out {
        Ok((text, ok)) => {
            println!("{text}");
            if ok { 0 } else { 1 }
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

#[cfg(test)]
mod tests;
