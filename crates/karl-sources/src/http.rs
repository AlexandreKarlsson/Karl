//! One HTTP client for every source: timeouts, user agent, and a GET that
//! turns non-2xx into an error whose text never contains a credential.

use anyhow::{bail, Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION};
use std::time::Duration;

pub fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .connect_timeout(Duration::from_secs(10))
        .user_agent(concat!("karl/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

pub fn bearer(token: &str) -> Result<HeaderMap> {
    let mut h = HeaderMap::new();
    let mut v =
        HeaderValue::from_str(&format!("Bearer {token}")).context("token has invalid chars")?;
    v.set_sensitive(true);
    h.insert(AUTHORIZATION, v);
    h.insert(ACCEPT, HeaderValue::from_static("application/json"));
    Ok(h)
}

pub fn basic(user: &str, token: &str) -> Result<HeaderMap> {
    use base64::Engine;
    let enc = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{token}"));
    let mut h = HeaderMap::new();
    let mut v =
        HeaderValue::from_str(&format!("Basic {enc}")).context("credential has invalid chars")?;
    v.set_sensitive(true);
    h.insert(AUTHORIZATION, v);
    h.insert(ACCEPT, HeaderValue::from_static("application/json"));
    Ok(h)
}

/// GET `url` with `headers`, return the JSON body. Error text: status + first 300 chars of body.
pub async fn get_json(
    c: &reqwest::Client,
    url: &str,
    headers: HeaderMap,
) -> Result<serde_json::Value> {
    let resp = c
        .get(url)
        .headers(headers)
        .send()
        .await
        .with_context(|| format!("GET {}", redact_url(url)))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let snippet: String = text.chars().take(300).collect();
        bail!("GET {} → {status}: {snippet}", redact_url(url));
    }
    serde_json::from_str(&text).with_context(|| format!("GET {}: not json", redact_url(url)))
}

/// POST a JSON body. Used only by write paths that the daemon has authorized.
pub async fn post_json(
    c: &reqwest::Client,
    url: &str,
    headers: HeaderMap,
    body: &serde_json::Value,
) -> Result<serde_json::Value> {
    let resp = c
        .post(url)
        .headers(headers)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(serde_json::to_vec(body)?)
        .send()
        .await
        .with_context(|| format!("POST {}", redact_url(url)))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let snippet: String = text.chars().take(300).collect();
        bail!("POST {} → {status}: {snippet}", redact_url(url));
    }
    if text.trim().is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_str(&text).with_context(|| format!("POST {}: not json", redact_url(url)))
}

/// Any method with an optional text body; returns (status, body text). Non-2xx
/// is an error, except 207 (WebDAV multistatus) which is a success here.
pub async fn request_text(
    c: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    headers: HeaderMap,
    body: Option<String>,
) -> Result<(reqwest::StatusCode, String)> {
    let m = method.to_string();
    let mut req = c.request(method, url).headers(headers);
    if let Some(b) = body {
        req = req.body(b);
    }
    let resp = req
        .send()
        .await
        .with_context(|| format!("{m} {}", redact_url(url)))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() && status.as_u16() != 207 {
        let snippet: String = text.chars().take(300).collect();
        bail!("{m} {} → {status}: {snippet}", redact_url(url));
    }
    Ok((status, text))
}

/// POST a form, return JSON (OAuth token endpoint).
pub async fn post_form_json(
    c: &reqwest::Client,
    url: &str,
    form: &[(&str, &str)],
) -> Result<serde_json::Value> {
    let body = form
        .iter()
        .map(|(k, v)| format!("{}={}", urlencoding::encode(k), urlencoding::encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    let resp = c
        .post(url)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let snippet: String = text.chars().take(300).collect();
        bail!("POST {url} → {status}: {snippet}");
    }
    serde_json::from_str(&text).with_context(|| format!("POST {url}: not json"))
}

/// Keep host and path, drop the query (it may carry tokens or private search text).
pub fn redact_url(u: &str) -> String {
    u.split('?').next().unwrap_or(u).to_string()
}
