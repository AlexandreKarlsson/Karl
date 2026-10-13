//! Microsoft identity platform, "public client" (no client secret): the
//! device-code flow once (`karld auth-ms`), refresh tokens after.
//!
//! Microsoft rotates the refresh token on every use and the old one dies with
//! its own 90-day clock, so the daemon keeps the newest in memory and writes it
//! back to the secrets file. A config management tool that re-templates the
//! file puts the vault's copy back: fine for 90 days, then `karld auth-ms` again.

use crate::http::{client, post_form_json};
use anyhow::{bail, Context, Result};
use karl_core::policy::Secrets;
use reqwest::header::HeaderMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

fn login(tenant: &str, leaf: &str) -> String {
    format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/{leaf}")
}

pub struct MsAuth {
    client: reqwest::Client,
    client_id: String,
    tenant: String,
    scopes: Vec<String>,
    /// (secrets file, table): where a rotated refresh token is written back.
    persist: Option<(PathBuf, String)>,
    refresh_token: Mutex<String>,
    cached: Mutex<Option<(String, Instant)>>,
}

impl MsAuth {
    /// `secrets[name]` holds `client_id`, `tenant` (optional, default `organizations`)
    /// and `refresh_token`. `scopes` are the delegated permissions to ask for on refresh.
    pub fn from_secrets(
        secrets: &Secrets,
        secrets_path: &std::path::Path,
        name: &str,
        scopes: &[String],
    ) -> Result<MsAuth> {
        Ok(MsAuth {
            client: client()?,
            client_id: secrets.get(name, "client_id")?.to_string(),
            tenant: secrets
                .get(name, "tenant")
                .unwrap_or("organizations")
                .to_string(),
            scopes: scopes.to_vec(),
            persist: Some((secrets_path.to_path_buf(), name.to_string())),
            refresh_token: Mutex::new(
                secrets
                    .get(name, "refresh_token")
                    .with_context(|| {
                        format!("[{name}] has no refresh_token: run `karld auth-ms {name}`")
                    })?
                    .to_string(),
            ),
            cached: Mutex::new(None),
        })
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub async fn access_token(&self) -> Result<String> {
        if let Some((t, exp)) = self.cached.lock().unwrap().clone() {
            if Instant::now() + Duration::from_secs(60) < exp {
                return Ok(t);
            }
        }
        let rt = self.refresh_token.lock().unwrap().clone();
        let scope = self.scopes.join(" ");
        let v = post_form_json(
            &self.client,
            &login(&self.tenant, "token"),
            &[
                ("client_id", self.client_id.as_str()),
                ("refresh_token", rt.as_str()),
                ("grant_type", "refresh_token"),
                ("scope", scope.as_str()),
            ],
        )
        .await
        .context("microsoft: refresh token")?;
        let tok = v["access_token"]
            .as_str()
            .context("microsoft: no access_token in reply")?
            .to_string();
        let ttl = v["expires_in"].as_u64().unwrap_or(3600);
        *self.cached.lock().unwrap() =
            Some((tok.clone(), Instant::now() + Duration::from_secs(ttl)));
        if let Some(new_rt) = v["refresh_token"].as_str() {
            if new_rt != rt {
                *self.refresh_token.lock().unwrap() = new_rt.to_string();
                if let Some((path, table)) = &self.persist {
                    if let Err(e) = Secrets::set_key(path, table, "refresh_token", new_rt) {
                        tracing::warn!(
                            "microsoft: could not persist the rotated refresh token: {e:#}"
                        );
                    }
                }
            }
        }
        Ok(tok)
    }

    pub async fn headers(&self) -> Result<HeaderMap> {
        crate::http::bearer(&self.access_token().await?)
    }
}

/// The one-time device-code flow: prints a URL and a code, polls until the
/// user has signed in, returns the refresh token. Works over ssh: nothing
/// listens, the browser can be anywhere.
pub async fn device_code_flow(client_id: &str, tenant: &str, scopes: &[String]) -> Result<String> {
    let c = client()?;
    let scope = scopes.join(" ");
    let v = post_form_json(
        &c,
        &login(tenant, "devicecode"),
        &[("client_id", client_id), ("scope", scope.as_str())],
    )
    .await
    .context("devicecode")?;
    let device_code = v["device_code"]
        .as_str()
        .context("no device_code in reply")?
        .to_string();
    let interval = v["interval"].as_u64().unwrap_or(5).max(1);
    let expires = v["expires_in"].as_u64().unwrap_or(900);
    println!(
        "{}\n",
        v["message"]
            .as_str()
            .unwrap_or("open the verification url and enter the code")
    );
    let deadline = Instant::now() + Duration::from_secs(expires);
    loop {
        if Instant::now() > deadline {
            bail!("device code expired before sign-in");
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
        // the poll answers 400 while pending: read the body whatever the status
        let form = [
            ("client_id", client_id),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", device_code.as_str()),
        ]
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");
        let r = c
            .post(login(tenant, "token"))
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(form)
            .send()
            .await
            .context("token poll")?;
        let text = r.text().await.unwrap_or_default();
        let v: serde_json::Value = serde_json::from_str(&text).context("token poll: not json")?;
        if let Some(rt) = v["refresh_token"].as_str() {
            return Ok(rt.to_string());
        }
        match v["error"].as_str() {
            Some("authorization_pending") | Some("slow_down") => continue,
            Some(e) => bail!("{e}: {}", v["error_description"].as_str().unwrap_or("")),
            None => bail!("token poll: no refresh_token and no error"),
        }
    }
}
