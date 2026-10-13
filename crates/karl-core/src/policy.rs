//! The daemon config, `karl.toml`: sources to sync, profiles that may read them,
//! and the limits applied to every reply. Nothing in here ever reaches the model.

use anyhow::{bail, Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::model::{Item, News};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub daemon: DaemonCfg,
    #[serde(default)]
    pub source: BTreeMap<String, SourceCfg>,
    #[serde(default)]
    pub profile: BTreeMap<String, ProfileCfg>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonCfg {
    pub db: PathBuf,
    pub audit: PathBuf,
    /// TOML file with one table per secret name, mode 0600.
    pub secrets: PathBuf,
    #[serde(default = "d_retention")]
    pub retention_days: u32,
    /// Group that owns the profile sockets (0660). The daemon user must be a member.
    #[serde(default)]
    pub socket_group: Option<String>,
    #[serde(default = "d_admin_socket")]
    pub admin_socket: PathBuf,
}

fn d_retention() -> u32 {
    90
}
fn d_admin_socket() -> PathBuf {
    PathBuf::from("/run/karl/admin.sock")
}

/// One thing to sync. `kind` picks the implementation; the rest of the keys
/// belong to that implementation and are kept verbatim in `extra`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceCfg {
    pub kind: String,
    /// Name of the table in the secrets file.
    pub secret: String,
    #[serde(default = "d_every", with = "humantime")]
    pub every: Duration,
    /// OAuth scopes the stored token must not exceed. Checked at start.
    #[serde(default)]
    pub scopes: Vec<String>,
    /// What counts as news for this source: `changes` or `arrivals`. Default: the source decides.
    #[serde(default)]
    pub news: Option<News>,
    #[serde(flatten)]
    pub extra: toml::Table,
}

fn d_every() -> Duration {
    Duration::from_secs(300)
}

/// Write authorization of a profile on a source. Reads are always allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Mode {
    /// Writes are refused with exit 3.
    #[serde(
        rename = "not-authorized",
        alias = "no",
        alias = "ro",
        alias = "denied"
    )]
    NotAuthorized,
    /// Writes are queued; a human approves or denies them on the admin socket.
    #[serde(rename = "authorization-required", alias = "ask", alias = "confirm")]
    AuthorizationRequired,
    /// Writes execute at once.
    #[serde(rename = "authorized", alias = "yes", alias = "rw")]
    Authorized,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::NotAuthorized => "not-authorized",
            Mode::AuthorizationRequired => "authorization-required",
            Mode::Authorized => "authorized",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileCfg {
    pub socket: PathBuf,
    /// Unix users allowed to connect. Resolved to uids at start.
    pub uids: Vec<String>,
    pub sources: Vec<String>,
    #[serde(default)]
    pub mode: HashMap<String, Mode>,
    #[serde(default = "d_max_lines")]
    pub max_lines: usize,
    #[serde(default = "d_max_bytes")]
    pub max_bytes: usize,
    /// Replies per window, e.g. `60/h`, `10/m`.
    #[serde(default = "d_rate")]
    pub rate: String,
    /// `source:field=glob`. field ∈ label, from, title, thread, visibility.
    #[serde(default)]
    pub hide: Vec<String>,
    /// Built-in scrubbers: `otp`, `card`.
    #[serde(default)]
    pub redact: Vec<String>,
    /// Chars per rendered line before the body is cut.
    #[serde(default = "d_line_chars")]
    pub line_chars: usize,
}

fn d_max_lines() -> usize {
    40
}
fn d_max_bytes() -> usize {
    4096
}
fn d_rate() -> String {
    "60/h".into()
}
fn d_line_chars() -> usize {
    160
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let s =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&s)
    }

    pub fn parse(s: &str) -> Result<Config> {
        let c: Config = toml::from_str(s).context("parse karl.toml")?;
        c.validate()?;
        Ok(c)
    }

    fn validate(&self) -> Result<()> {
        let mut sockets = std::collections::HashSet::new();
        for (name, p) in &self.profile {
            for s in &p.sources {
                if !self.source.contains_key(s) {
                    bail!("profile {name}: unknown source {s}");
                }
            }
            for s in p.mode.keys() {
                if !p.sources.contains(s) {
                    bail!("profile {name}: mode for {s} which is not in its sources");
                }
            }
            if !sockets.insert(&p.socket) {
                bail!("profile {name}: socket {} used twice", p.socket.display());
            }
            if p.uids.is_empty() {
                bail!("profile {name}: uids is empty, nobody could connect");
            }
            parse_rate(&p.rate).with_context(|| format!("profile {name}: rate"))?;
            for h in &p.hide {
                HideRule::parse(h).with_context(|| format!("profile {name}: hide {h}"))?;
            }
            for r in &p.redact {
                if !["otp", "card"].contains(&r.as_str()) {
                    bail!("profile {name}: unknown redact {r} (otp, card)");
                }
            }
        }
        Ok(())
    }
}

impl ProfileCfg {
    pub fn mode_for(&self, source: &str) -> Mode {
        self.mode
            .get(source)
            .copied()
            .unwrap_or(Mode::NotAuthorized)
    }
}

/// A profile with its rules compiled. Built once at start.
#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    pub cfg: ProfileCfg,
    hide: Vec<HideRule>,
    redact: Vec<(Regex, &'static str)>,
    pub rate: RateSpec,
}

impl Profile {
    pub fn compile(name: &str, cfg: &ProfileCfg) -> Result<Profile> {
        let hide = cfg
            .hide
            .iter()
            .map(|h| HideRule::parse(h))
            .collect::<Result<Vec<_>>>()?;
        let redact = cfg
            .redact
            .iter()
            .map(|r| match r.as_str() {
                // no look-around in the regex crate: capture the neighbours and put them back
                "otp" => (
                    Regex::new(r"(^|[^\d.])\d{6,8}([^\d.]|$)").unwrap(),
                    "${1}[otp]${2}",
                ),
                "card" => (
                    Regex::new(r"(^|[^\d])(?:\d[ -]?){13,19}([^\d]|$)").unwrap(),
                    "${1}[card]${2}",
                ),
                _ => unreachable!("validated"),
            })
            .collect();
        Ok(Profile {
            name: name.to_string(),
            cfg: cfg.clone(),
            hide,
            redact,
            rate: parse_rate(&cfg.rate)?,
        })
    }

    pub fn hidden(&self, it: &Item) -> bool {
        self.hide.iter().any(|h| h.matches(it))
    }

    pub fn redact(&self, s: &str) -> String {
        let mut out = s.to_string();
        for (re, rep) in &self.redact {
            out = re.replace_all(&out, *rep).into_owned();
        }
        out
    }

    /// Sources of this profile, for store filters.
    pub fn sources(&self) -> &[String] {
        &self.cfg.sources
    }
}

/// `source:field=glob`. `source` may be `*`.
#[derive(Debug, Clone)]
pub struct HideRule {
    pub source: Option<String>,
    pub field: HideField,
    pub re: Regex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HideField {
    Label,
    From,
    Title,
    Thread,
}

impl HideRule {
    pub fn parse(s: &str) -> Result<HideRule> {
        let (src, rest) = s
            .split_once(':')
            .with_context(|| format!("hide rule {s}: want source:field=glob"))?;
        let (field, glob) = rest
            .split_once('=')
            .with_context(|| format!("hide rule {s}: want field=glob"))?;
        let field = match field {
            "label" | "visibility" => HideField::Label,
            "from" => HideField::From,
            "title" => HideField::Title,
            "thread" => HideField::Thread,
            other => bail!(
                "hide rule {s}: unknown field {other} (label, from, title, thread, visibility)"
            ),
        };
        let source = if src == "*" {
            None
        } else {
            Some(src.to_string())
        };
        Ok(HideRule {
            source,
            field,
            re: glob_to_regex(glob)?,
        })
    }

    pub fn matches(&self, it: &Item) -> bool {
        if let Some(s) = &self.source {
            if *s != it.source {
                return false;
            }
        }
        match self.field {
            HideField::Label => it.labels.iter().any(|l| self.re.is_match(l)),
            HideField::From => {
                self.re.is_match(&it.who)
                    || it
                        .who_addr
                        .as_deref()
                        .map(|a| self.re.is_match(a))
                        .unwrap_or(false)
            }
            HideField::Title => it
                .title
                .as_deref()
                .map(|t| self.re.is_match(t))
                .unwrap_or(false),
            HideField::Thread => {
                it.thread_name
                    .as_deref()
                    .map(|t| self.re.is_match(t))
                    .unwrap_or(false)
                    || it
                        .thread
                        .as_deref()
                        .map(|t| self.re.is_match(t))
                        .unwrap_or(false)
            }
        }
    }
}

/// `*` matches anything, everything else is literal, whole string, case-insensitive.
pub fn glob_to_regex(glob: &str) -> Result<Regex> {
    let mut re = String::from("(?i)^");
    for part in glob
        .split('*')
        .map(regex::escape)
        .collect::<Vec<_>>()
        .join(".*")
        .chars()
    {
        re.push(part);
    }
    re.push('$');
    Ok(Regex::new(&re)?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateSpec {
    pub count: usize,
    pub window: Duration,
}

pub fn parse_rate(s: &str) -> Result<RateSpec> {
    let (n, w) = s
        .split_once('/')
        .with_context(|| format!("rate {s}: want N/m, N/h or N/d"))?;
    let count: usize = n.trim().parse().with_context(|| format!("rate {s}"))?;
    let window = match w.trim() {
        "m" | "min" => Duration::from_secs(60),
        "h" | "hour" => Duration::from_secs(3600),
        "d" | "day" => Duration::from_secs(86400),
        other => bail!("rate {s}: unknown window {other}"),
    };
    if count == 0 {
        bail!("rate {s}: count must be > 0");
    }
    Ok(RateSpec { count, window })
}

/// Sliding-window limiter, one per profile, in memory.
#[derive(Debug)]
pub struct RateLimiter {
    spec: RateSpec,
    hits: VecDeque<Instant>,
}

impl RateLimiter {
    pub fn new(spec: RateSpec) -> RateLimiter {
        RateLimiter {
            spec,
            hits: VecDeque::new(),
        }
    }

    /// Ok if a call is allowed now (and records it), else how long until one is.
    pub fn check(&mut self, now: Instant) -> std::result::Result<(), Duration> {
        while let Some(t) = self.hits.front() {
            if now.duration_since(*t) >= self.spec.window {
                self.hits.pop_front();
            } else {
                break;
            }
        }
        if self.hits.len() >= self.spec.count {
            let oldest = *self.hits.front().unwrap();
            return Err(self.spec.window.saturating_sub(now.duration_since(oldest)));
        }
        self.hits.push_back(now);
        Ok(())
    }
}

/// `30s`, `5m`, `2h`, `3d`, `1w`.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let (n, unit) = s.split_at(split);
    let n: u64 = n.parse().ok()?;
    let mult = match unit {
        "s" => 1,
        "m" | "min" => 60,
        "h" => 3600,
        "d" => 86400,
        "w" => 7 * 86400,
        _ => return None,
    };
    Some(Duration::from_secs(n * mult))
}

mod humantime {
    use super::parse_duration;
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{}s", d.as_secs()))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let s = String::deserialize(d)?;
        parse_duration(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("bad duration {s} (30s, 5m, 2h, 3d)")))
    }
}

/// A secrets file: `[name]` tables of string keys. Checked for mode 0600.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Secrets(pub BTreeMap<String, BTreeMap<String, String>>);

impl Secrets {
    pub fn load(path: &Path) -> Result<Secrets> {
        use std::os::unix::fs::PermissionsExt;
        let meta =
            std::fs::metadata(path).with_context(|| format!("secrets {}", path.display()))?;
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            bail!("{}: mode {:o}, need 0600 (chmod 600)", path.display(), mode);
        }
        let s = std::fs::read_to_string(path)?;
        let t: BTreeMap<String, BTreeMap<String, String>> =
            toml::from_str(&s).context("parse secrets")?;
        Ok(Secrets(t))
    }

    pub fn get(&self, table: &str, key: &str) -> Result<&str> {
        self.0
            .get(table)
            .and_then(|t| t.get(key))
            .map(String::as_str)
            .with_context(|| format!("secrets: missing [{table}] {key}"))
    }
}

pub const EXAMPLE: &str = r#"[daemon]
db        = "/var/lib/karl/karl.db"
audit     = "/var/log/karl/audit.log"
secrets   = "/var/lib/karl/secrets.toml"
retention_days = 90
socket_group = "karl-cli"
admin_socket = "/run/karl/admin.sock"

[source.jira]
kind   = "jira"
site   = "https://example.atlassian.net"
secret = "atl-alex"
every  = "5m"
window_days = 30               # issues updated in the last N days
news   = "changes"             # a status change is news (default for jira); mail uses "arrivals"

[source.gm]
kind    = "gmail"
account = "me@example.com"
secret  = "google-alex"
every   = "3m"
scopes  = ["https://www.googleapis.com/auth/gmail.readonly"]
window_days = 14

[source.gcal]
kind     = "gcal"
secret   = "google-alex"
every    = "10m"
scopes   = ["https://www.googleapis.com/auth/calendar.readonly"]
calendar = "primary"
days_ahead = 14

[profile.alex]
socket    = "/run/karl/alex.sock"
uids      = ["hermes"]
sources   = ["gm", "gcal", "jira"]
# writes per source: not-authorized (default) | authorization-required | authorized
mode      = { jira = "authorization-required" }
max_lines = 40
max_bytes = 4096
rate      = "60/h"
hide      = ["gm:label=Bank", "gm:from=*@otp.*", "gcal:visibility=private"]
redact    = ["otp"]
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Kind;

    #[test]
    fn example_parses() {
        let c = Config::parse(EXAMPLE).unwrap();
        assert_eq!(c.profile["alex"].mode_for("gm"), Mode::NotAuthorized);
        assert_eq!(
            c.profile["alex"].mode_for("jira"),
            Mode::AuthorizationRequired
        );
        assert_eq!(c.source["jira"].news, Some(News::Changes));
        assert_eq!(c.source["gm"].news, None);
        let aliased = EXAMPLE.replace(
            "jira = \"authorization-required\"",
            "jira = \"rw\", gm = \"ask\"",
        );
        let c2 = Config::parse(&aliased).unwrap();
        assert_eq!(c2.profile["alex"].mode_for("jira"), Mode::Authorized);
        assert_eq!(
            c2.profile["alex"].mode_for("gm"),
            Mode::AuthorizationRequired
        );
        assert_eq!(c.source["jira"].every, Duration::from_secs(300));
        assert_eq!(
            c.source["jira"].extra["site"].as_str(),
            Some("https://example.atlassian.net")
        );
        let p = Profile::compile("alex", &c.profile["alex"]).unwrap();
        assert_eq!(
            p.rate,
            RateSpec {
                count: 60,
                window: Duration::from_secs(3600)
            }
        );
    }

    #[test]
    fn validation() {
        let bad = EXAMPLE.replace(
            "sources   = [\"gm\", \"gcal\", \"jira\"]",
            "sources   = [\"nope\"]",
        );
        assert!(Config::parse(&bad)
            .unwrap_err()
            .to_string()
            .contains("unknown source"));
        let bad = EXAMPLE.replace("uids      = [\"hermes\"]", "uids = []");
        assert!(Config::parse(&bad).is_err());
    }

    #[test]
    fn hide_and_redact() {
        let c = Config::parse(EXAMPLE).unwrap();
        let p = Profile::compile("alex", &c.profile["alex"]).unwrap();
        let mut it = Item::new("gm", Kind::Mail, "1", "Bank Co", 1);
        assert!(!p.hidden(&it));
        it.labels = vec!["Bank".into()];
        assert!(p.hidden(&it));
        it.labels.clear();
        it.who_addr = Some("noreply@otp.example".into());
        assert!(p.hidden(&it));
        it.source = "gcal".into();
        assert!(!p.hidden(&it), "rule is scoped to gm");
        assert_eq!(
            p.redact("code 123456 now, 2026-10-13, 3.14159265"),
            "code [otp] now, 2026-10-13, 3.14159265"
        );
    }

    #[test]
    fn rate_limiter() {
        let mut r = RateLimiter::new(RateSpec {
            count: 2,
            window: Duration::from_secs(10),
        });
        let t0 = Instant::now();
        assert!(r.check(t0).is_ok());
        assert!(r.check(t0).is_ok());
        let wait = r.check(t0 + Duration::from_secs(1)).unwrap_err();
        assert_eq!(wait, Duration::from_secs(9));
        assert!(r.check(t0 + Duration::from_secs(10)).is_ok());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("30m"), Some(Duration::from_secs(1800)));
        assert_eq!(parse_duration("x"), None);
        assert!(parse_rate("5/x").is_err());
    }
}
