//! OAuth 2 for a "desktop" client: refresh tokens → short-lived access tokens,
//! a scope check at start, and the one-time loopback flow (`karld auth`).

use crate::http::{client, get_json, post_form_json};
use anyhow::{bail, Context, Result};
use karl_core::policy::Secrets;
use reqwest::header::HeaderMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const INFO_URL: &str = "https://oauth2.googleapis.com/tokeninfo";

pub struct GoogleAuth {
    client: reqwest::Client,
    client_id: String,
    client_secret: String,
    refresh_token: String,
    cached: Mutex<Option<(String, Instant)>>,
}

impl GoogleAuth {
    pub fn from_secrets(secrets: &Secrets, name: &str) -> Result<GoogleAuth> {
        Ok(GoogleAuth {
            client: client()?,
            client_id: secrets.get(name, "client_id")?.to_string(),
            client_secret: secrets.get(name, "client_secret")?.to_string(),
            refresh_token: secrets
                .get(name, "refresh_token")
                .with_context(|| format!("[{name}] has no refresh_token: run `karld auth {name}`"))?
                .to_string(),
            cached: Mutex::new(None),
        })
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    /// A valid access token, refreshed when within a minute of expiry.
    pub async fn access_token(&self) -> Result<String> {
        if let Some((t, exp)) = self.cached.lock().unwrap().clone() {
            if Instant::now() + Duration::from_secs(60) < exp {
                return Ok(t);
            }
        }
        let v = post_form_json(
            &self.client,
            TOKEN_URL,
            &[
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("refresh_token", self.refresh_token.as_str()),
                ("grant_type", "refresh_token"),
            ],
        )
        .await
        .context("google: refresh token")?;
        let tok = v["access_token"]
            .as_str()
            .context("google: no access_token in reply")?
            .to_string();
        let ttl = v["expires_in"].as_u64().unwrap_or(3600);
        *self.cached.lock().unwrap() =
            Some((tok.clone(), Instant::now() + Duration::from_secs(ttl)));
        Ok(tok)
    }

    pub async fn headers(&self) -> Result<HeaderMap> {
        crate::http::bearer(&self.access_token().await?)
    }

    /// Scopes the token actually carries. Used to refuse a token broader than the policy says.
    pub async fn granted_scopes(&self) -> Result<Vec<String>> {
        let tok = self.access_token().await?;
        let v = get_json(
            &self.client,
            &format!("{INFO_URL}?access_token={tok}"),
            HeaderMap::new(),
        )
        .await
        .context("google: tokeninfo")?;
        Ok(v["scope"]
            .as_str()
            .unwrap_or("")
            .split_whitespace()
            .map(String::from)
            .collect())
    }

    /// Fail if the token has a scope the config did not list. An empty `allowed` skips the check.
    pub async fn check_scopes(&self, allowed: &[String]) -> Result<()> {
        if allowed.is_empty() {
            return Ok(());
        }
        let granted = self.granted_scopes().await?;
        let extra: Vec<&String> = granted.iter().filter(|g| !allowed.contains(g)).collect();
        if !extra.is_empty() {
            bail!("google token carries scopes not in the policy: {extra:?}. Re-run `karld auth` with the right scopes.");
        }
        Ok(())
    }
}

/// The one-time loopback flow. Prints a URL, waits for the browser on
/// `127.0.0.1:port`, exchanges the code, returns the refresh token.
/// Over ssh: `ssh -L port:127.0.0.1:port host`, then open the URL on the laptop.
pub async fn loopback_flow(
    client_id: &str,
    client_secret: &str,
    scopes: &[String],
    port: u16,
) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let port = listener.local_addr()?.port();
    let redirect = format!("http://127.0.0.1:{port}");
    let url = format!(
        "{AUTH_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent",
        urlencoding::encode(client_id),
        urlencoding::encode(&redirect),
        urlencoding::encode(&scopes.join(" "))
    );
    println!("open this URL in a browser that can reach 127.0.0.1:{port}:\n\n{url}\n");
    let (mut sock, _) = listener.accept().await?;
    let mut buf = vec![0u8; 8192];
    let n = sock.read(&mut buf).await?;
    let req = String::from_utf8_lossy(&buf[..n]);
    let line = req.lines().next().unwrap_or("");
    let path = line.split_whitespace().nth(1).unwrap_or("");
    let code = path
        .split('?')
        .nth(1)
        .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("code=")))
        .map(|c| {
            urlencoding::decode(c)
                .map(|s| s.into_owned())
                .unwrap_or_default()
        });
    let body = if code.is_some() {
        "karl: authorized, you can close this tab."
    } else {
        "karl: no code in callback."
    };
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(resp.as_bytes()).await?;
    let code = code.context("no code in the callback")?;
    let v = post_form_json(
        &client()?,
        TOKEN_URL,
        &[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("code", code.as_str()),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect.as_str()),
        ],
    )
    .await
    .context("exchange code")?;
    v["refresh_token"]
        .as_str()
        .map(String::from)
        .context("no refresh_token in reply (revoke the app in your Google account and retry)")
}

/// RFC3339 → unix seconds.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp())
}
