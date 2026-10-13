//! One line per request, appended to the audit file. The daemon's own user owns it.

use std::io::Write;
use std::path::PathBuf;

pub struct Audit {
    path: PathBuf,
}

impl Audit {
    pub fn new(path: PathBuf) -> Audit {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        Audit { path }
    }

    pub fn log(&self, line: &str) {
        let ts = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let res = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| writeln!(f, "{ts} {line}"));
        if let Err(e) = res {
            tracing::warn!("audit write failed: {e}");
        }
    }
}
