//! Everything the request handlers share.

use crate::audit::Audit;
use crate::users::uid_of;
use anyhow::{bail, Context, Result};
use karl_core::model::News;
use karl_core::policy::{Config, Profile, RateLimiter, Secrets};
use karl_core::Store;
use karl_sources::{SharedStore, Source, SyncReport};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, Semaphore};

pub struct ProfileRt {
    pub policy: Profile,
    pub uids: Vec<u32>,
    pub limiter: Mutex<RateLimiter>,
    /// Concurrent requests per profile.
    pub sem: Semaphore,
}

#[derive(Debug, Default, Clone)]
pub struct SyncStatus {
    pub last_ok: Option<i64>,
    pub last_err: Option<String>,
    pub last: SyncReport,
}

pub struct App {
    pub cfg: Config,
    pub store: SharedStore,
    pub sources: BTreeMap<String, Arc<dyn Source>>,
    pub profiles: Vec<ProfileRt>,
    pub status: Mutex<BTreeMap<String, SyncStatus>>,
    pub audit: Audit,
    /// Poke a source's loop to sync now (`karld admin sync`).
    pub kick: BTreeMap<String, Arc<Notify>>,
}

impl App {
    pub fn build(cfg: Config) -> Result<Arc<App>> {
        let secrets = Secrets::load(&cfg.daemon.secrets)?;
        if let Some(dir) = cfg.daemon.db.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let store = Arc::new(Mutex::new(Store::open(&cfg.daemon.db)?));
        let mut sources: BTreeMap<String, Arc<dyn Source>> = BTreeMap::new();
        let mut kick = BTreeMap::new();
        for (id, scfg) in &cfg.source {
            sources.insert(
                id.clone(),
                Arc::from(karl_sources::build(
                    id,
                    scfg,
                    &secrets,
                    &cfg.daemon.secrets,
                )?),
            );
            kick.insert(id.clone(), Arc::new(Notify::new()));
        }
        let mut profiles = Vec::new();
        for (name, pcfg) in &cfg.profile {
            let policy = Profile::compile(name, pcfg)?;
            let mut uids = Vec::new();
            for u in &pcfg.uids {
                uids.push(uid_of(u).with_context(|| format!("profile {name}: unknown user {u}"))?);
            }
            if uids.contains(&0) {
                bail!("profile {name}: root in uids makes no sense");
            }
            profiles.push(ProfileRt {
                limiter: Mutex::new(RateLimiter::new(policy.rate)),
                sem: Semaphore::new(4),
                policy,
                uids,
            });
        }
        let audit = Audit::new(cfg.daemon.audit.clone());
        Ok(Arc::new(App {
            cfg,
            store,
            sources,
            profiles,
            status: Mutex::new(BTreeMap::new()),
            audit,
            kick,
        }))
    }

    /// Implementation name of a source id (`gmail`, `jira`).
    pub fn source_kind(&self, id: &str) -> &str {
        self.cfg
            .source
            .get(id)
            .map(|s| s.kind.as_str())
            .unwrap_or("")
    }

    /// Command group a source feeds (`mail`, `jira`…), as the source declares it.
    pub fn group_of(&self, id: &str) -> &'static str {
        self.sources.get(id).map(|s| s.group()).unwrap_or("")
    }

    /// What "news" means for a source: the policy override, else the source's default.
    pub fn news_of(&self, id: &str) -> News {
        self.cfg
            .source
            .get(id)
            .and_then(|c| c.news)
            .unwrap_or_else(|| {
                self.sources
                    .get(id)
                    .map(|s| s.news())
                    .unwrap_or(News::Changes)
            })
    }

    pub fn record_sync(&self, id: &str, result: &Result<SyncReport>) {
        let mut st = self.status.lock().unwrap();
        let e = st.entry(id.to_string()).or_default();
        match result {
            Ok(r) => {
                e.last_ok = Some(chrono::Utc::now().timestamp());
                e.last_err = None;
                e.last = *r;
            }
            Err(err) => e.last_err = Some(format!("{err:#}")),
        }
    }

    /// `gm 2m · jira 1m · gcal ERR` for the given source ids.
    pub fn status_line(&self, ids: &[String]) -> String {
        let now = chrono::Utc::now().timestamp();
        let st = self.status.lock().unwrap();
        ids.iter()
            .map(|id| match st.get(id) {
                Some(s) if s.last_err.is_some() => format!("{id} ERR"),
                Some(s) if s.last_ok.is_some() => format!("{id} {}", age(now - s.last_ok.unwrap())),
                _ => format!("{id} …"),
            })
            .collect::<Vec<_>>()
            .join(" · ")
    }

    pub fn cleanup_sockets(&self) {
        for p in &self.profiles {
            let _ = std::fs::remove_file(&p.policy.cfg.socket);
        }
        let _ = std::fs::remove_file(&self.cfg.daemon.admin_socket);
    }
}

pub fn age(secs: i64) -> String {
    match secs {
        s if s < 60 => "<1m".into(),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}
