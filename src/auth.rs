//! "Sign in with Hugging Face" for a native CLI — no client secret, no embedded browser.
//!
//! - Browser flow: OAuth 2.0 authorization code + PKCE (S256), callback on a loopback URL
//!   `http://127.0.0.1:<random port>/callback` (RFC 8252). HF accepts any port for loopback
//!   redirect URIs registered without one.
//! - Device flow (RFC 8628) for SSH/headless machines.
//!
//! Token precedence for hippo: `HF_TOKEN` env → hippo's own token (`~/.hippo/hf-token.json`)
//! → the `hf auth login` token (`~/.cache/huggingface/token`).

use crate::util::{atomic_write, random_hex};
use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const HF: &str = "https://huggingface.co";
/// Public OAuth app (no secret). Override with HIPPO_HF_CLIENT_ID.
const DEFAULT_CLIENT_ID: Option<&str> = None;
/// Least privilege: create buckets and access only those this app created.
/// Override with HIPPO_HF_SCOPES (e.g. "openid profile manage-repos").
const DEFAULT_SCOPES: &str = "openid profile contribute-repos";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub scope: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    scope: Option<String>,
}

impl From<TokenResponse> for StoredToken {
    fn from(t: TokenResponse) -> Self {
        Self {
            access_token: t.access_token,
            refresh_token: t.refresh_token,
            expires_at: t.expires_in.map(|s| crate::util::now() + chrono::Duration::seconds(s)),
            scope: t.scope,
        }
    }
}

pub fn client_id() -> Option<String> {
    std::env::var("HIPPO_HF_CLIENT_ID").ok().or(DEFAULT_CLIENT_ID.map(str::to_string))
}

fn scopes() -> String {
    std::env::var("HIPPO_HF_SCOPES").unwrap_or_else(|_| DEFAULT_SCOPES.to_string())
}

fn token_path() -> PathBuf {
    crate::config::hippo_home().join("hf-token.json")
}

pub fn load_token() -> Option<StoredToken> {
    let t: StoredToken = serde_json::from_str(&std::fs::read_to_string(token_path()).ok()?).ok()?;
    match t.expires_at {
        Some(e) if e <= crate::util::now() => None,
        _ => Some(t),
    }
}

fn save_token(t: &StoredToken) -> Result<()> {
    let p = token_path();
    atomic_write(&p, serde_json::to_string_pretty(t)?.as_bytes())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

pub fn logout() -> Result<bool> {
    let p = token_path();
    if p.exists() {
        std::fs::remove_file(p)?;
        return Ok(true);
    }
    Ok(false)
}

/// HF client using the best available token (see module docs).
pub fn hf_client() -> Result<hf_hub::HFClient> {
    if std::env::var_os("HF_TOKEN").is_none()
        && let Some(t) = load_token() {
            return hf_hub::HFClient::builder()
                .token(t.access_token)
                .build()
                .context("cannot create Hugging Face client");
        }
    hf_hub::HFClient::new().context("cannot create Hugging Face client")
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Whether a browser can plausibly be opened on this machine.
pub fn can_open_browser() -> bool {
    if std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some() {
        return false;
    }
    if cfg!(target_os = "linux") {
        return std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
    }
    true
}

fn open_browser(url: &str) -> bool {
    let cmd = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).status()
    } else if cfg!(windows) {
        std::process::Command::new("cmd").args(["/C", "start", "", url]).status()
    } else {
        std::process::Command::new("xdg-open").arg(url).status()
    };
    cmd.map(|s| s.success()).unwrap_or(false)
}

/// Browser login with PKCE + loopback callback. `say` prints user-facing lines.
pub async fn login_browser(say: impl Fn(&str)) -> Result<StoredToken> {
    let client_id = client_id().context("no OAuth client id configured (set HIPPO_HF_CLIENT_ID)")?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let verifier = b64(&rand::random::<[u8; 32]>());
    let challenge = b64(&Sha256::digest(verifier.as_bytes()));
    let state = random_hex(16);
    let mut url = reqwest::Url::parse(&format!("{HF}/oauth/authorize"))?;
    url.query_pairs_mut()
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect)
        .append_pair("response_type", "code")
        .append_pair("scope", &scopes())
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");

    if open_browser(url.as_str()) {
        say("Opened your browser to sign in with Hugging Face…");
    }
    say(&format!("If it didn't open, visit:\n  {url}"));

    let code = tokio::time::timeout(Duration::from_secs(300), wait_for_code(&listener, &state))
        .await
        .context("timed out waiting for the browser sign-in (5 min)")??;

    let resp = reqwest::Client::new()
        .post(format!("{HF}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("client_id", client_id.as_str()),
            ("code_verifier", verifier.as_str()),
        ])
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("Hugging Face rejected the sign-in: {} {}", resp.status(), resp.text().await.unwrap_or_default());
    }
    let token: StoredToken = resp.json::<TokenResponse>().await?.into();
    save_token(&token)?;
    Ok(token)
}

/// Serve the loopback callback until a request with the right `state` arrives.
/// Only GET /callback is answered; the `state` check rejects forged requests from other pages.
async fn wait_for_code(listener: &tokio::net::TcpListener, state: &str) -> Result<String> {
    loop {
        let (mut sock, _) = listener.accept().await?;
        let mut buf = vec![0u8; 8192];
        let n = tokio::time::timeout(Duration::from_secs(5), sock.read(&mut buf)).await.unwrap_or(Ok(0))?;
        let req = String::from_utf8_lossy(&buf[..n]);
        let target = req.lines().next().and_then(|l| l.strip_prefix("GET ")).and_then(|l| l.split(' ').next());
        let Some(target) = target.filter(|t| t.starts_with("/callback?")) else {
            let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
            continue;
        };
        let url = reqwest::Url::parse(&format!("http://127.0.0.1{target}"))?;
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        let (ok, body) = if q.get("state").map(String::as_str) != Some(state) {
            (None, "Invalid sign-in request (state mismatch). You can close this tab.")
        } else if let Some(err) = q.get("error") {
            let _ = respond(&mut sock, "Sign-in was cancelled. You can close this tab.").await;
            bail!("sign-in failed: {err}");
        } else if let Some(code) = q.get("code") {
            (Some(code.clone()), "🦛 Signed in to hippo. You can close this tab and return to your terminal.")
        } else {
            (None, "Missing authorization code. You can close this tab.")
        };
        let _ = respond(&mut sock, body).await;
        if let Some(code) = ok {
            return Ok(code);
        }
    }
}

async fn respond(sock: &mut tokio::net::TcpStream, msg: &str) -> std::io::Result<()> {
    let html = format!(
        "<!doctype html><meta charset=utf-8><title>hippo</title>\
         <body style=\"font:16px system-ui;display:grid;place-items:center;height:90vh\"><p>{msg}</p>"
    );
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Security-Policy: default-src 'none'; style-src 'unsafe-inline'\r\nCache-Control: no-store\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    );
    sock.write_all(resp.as_bytes()).await
}

#[derive(Deserialize)]
struct DeviceResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: u64,
    interval: Option<u64>,
}

/// Device-code login for SSH / headless machines.
pub async fn login_device(say: impl Fn(&str)) -> Result<StoredToken> {
    let client_id = client_id().context("no OAuth client id configured (set HIPPO_HF_CLIENT_ID)")?;
    let http = reqwest::Client::new();
    let resp = http
        .post(format!("{HF}/oauth/device"))
        .form(&[("client_id", client_id.as_str()), ("scope", scopes().as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("device sign-in failed: {} {}", resp.status(), resp.text().await.unwrap_or_default());
    }
    let d: DeviceResponse = resp.json().await?;
    say(&format!(
        "On any device, open {}\nand enter the code: {}",
        d.verification_uri_complete.as_deref().unwrap_or(&d.verification_uri),
        d.user_code
    ));
    let mut interval = d.interval.unwrap_or(5);
    let deadline = std::time::Instant::now() + Duration::from_secs(d.expires_in);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let r = http
            .post(format!("{HF}/oauth/token"))
            .form(&[
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", d.device_code.as_str()),
                ("client_id", client_id.as_str()),
            ])
            .send()
            .await?;
        if r.status().is_success() {
            let token: StoredToken = r.json::<TokenResponse>().await?.into();
            save_token(&token)?;
            return Ok(token);
        }
        let body: serde_json::Value = r.json().await.unwrap_or_default();
        match body.get("error").and_then(|e| e.as_str()) {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            Some(e) => bail!("device sign-in failed: {e}"),
            None => bail!("device sign-in failed: unexpected response"),
        }
    }
    bail!("device code expired — run `hippo login --device` again")
}
