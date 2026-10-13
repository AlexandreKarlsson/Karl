//! `karld`: the daemon. Holds the credentials, syncs the sources into the
//! cache, serves one unix socket per profile, enforces the policy, writes the audit log.
//!
//! ```text
//! karld [--config /etc/karl/karl.toml] run        serve (default)
//! karld [--config …] check                        validate config, secrets, credentials, scopes
//! karld [--config …] sync [source]                one sync pass, then exit
//! karld [--config …] auth <secret> [--port N] [--scopes a,b]   Google loopback OAuth, writes refresh_token
//! karld [--config …] auth-ms <secret> [--scopes a,b]           Microsoft device-code OAuth, writes refresh_token
//! karld example-config                            print a karl.toml to start from
//! ```

mod admin;
mod audit;
mod dispatch;
mod server;
mod state;
mod sync;
mod users;

use anyhow::{bail, Context, Result};
use karl_core::policy::{Config, EXAMPLE};
use state::App;
use std::path::{Path, PathBuf};

const DEFAULT_CONFIG: &str = "/etc/karl/karl.toml";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut config =
        PathBuf::from(std::env::var("KARL_CONFIG").unwrap_or_else(|_| DEFAULT_CONFIG.into()));
    if args.len() >= 2 && args[0] == "--config" {
        config = PathBuf::from(&args[1]);
        args.drain(..2);
    }
    let cmd = args.first().map(String::as_str).unwrap_or("run");
    match cmd {
        "run" => run(&config).await,
        "check" => check(&config).await,
        "sync" => sync_once(&config, args.get(1).map(String::as_str)).await,
        "auth" => auth(&config, &args[1..]).await,
        "auth-ms" => auth_ms(&config, &args[1..]).await,
        "example-config" => {
            print!("{EXAMPLE}");
            Ok(())
        }
        "--version" | "-V" => {
            println!("karld {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => bail!("unknown command {other} (run, check, sync, auth, auth-ms, example-config)"),
    }
}

async fn run(config: &Path) -> Result<()> {
    let cfg = Config::load(config)?;
    let app = App::build(cfg)?;
    tracing::info!(
        sources = app.sources.len(),
        profiles = app.profiles.len(),
        "karld starting"
    );
    for id in app.sources.keys() {
        tokio::spawn(sync::run_source(app.clone(), id.clone()));
    }
    tokio::spawn(sync::purge_loop(app.clone()));
    for idx in 0..app.profiles.len() {
        let a = app.clone();
        tokio::spawn(async move {
            if let Err(e) = server::serve_profile(a, idx).await {
                tracing::error!("profile server: {e:#}");
            }
        });
    }
    let a = app.clone();
    tokio::spawn(async move {
        if let Err(e) = admin::serve(a).await {
            tracing::error!("admin server: {e:#}");
        }
    });
    wait_for_signal().await;
    tracing::info!("karld stopping");
    app.cleanup_sockets();
    Ok(())
}

async fn wait_for_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("sigterm");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

async fn check(config: &Path) -> Result<()> {
    let cfg = Config::load(config)?;
    let app = App::build(cfg)?;
    let mut failed = 0;
    for (id, src) in &app.sources {
        match src.preflight().await {
            Ok(()) => println!("source {id} ({}): ok", src.kind()),
            Err(e) => {
                failed += 1;
                println!("source {id} ({}): FAIL {e:#}", src.kind());
            }
        }
    }
    for p in &app.profiles {
        println!(
            "profile {}: socket {} uids {:?} sources {:?} rate {} max {}L/{}B",
            p.policy.name,
            p.policy.cfg.socket.display(),
            p.uids,
            p.policy.sources(),
            p.policy.cfg.rate,
            p.policy.cfg.max_lines,
            p.policy.cfg.max_bytes
        );
    }
    if failed > 0 {
        bail!("{failed} source(s) failed preflight");
    }
    println!("ok");
    Ok(())
}

async fn sync_once(config: &Path, only: Option<&str>) -> Result<()> {
    let cfg = Config::load(config)?;
    let app = App::build(cfg)?;
    for (id, src) in &app.sources {
        if only.is_some_and(|o| o != id) {
            continue;
        }
        let t0 = std::time::Instant::now();
        match src.sync(&app.store).await {
            Ok(r) => println!(
                "{id}: fetched {} inserted {} updated {} deleted {} in {:.1}s",
                r.fetched,
                r.inserted,
                r.updated,
                r.deleted,
                t0.elapsed().as_secs_f32()
            ),
            Err(e) => println!("{id}: FAIL {e:#}"),
        }
    }
    Ok(())
}

async fn auth(config: &Path, args: &[String]) -> Result<()> {
    let name = args
        .first()
        .context("usage: karld auth <secret-name> [--port N] [--scopes a,b]")?;
    let mut port: u16 = 0;
    let mut scopes = vec![
        "https://www.googleapis.com/auth/gmail.readonly".to_string(),
        "https://www.googleapis.com/auth/calendar.readonly".to_string(),
    ];
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--port" => {
                port = args
                    .get(i + 1)
                    .context("--port N")?
                    .parse()
                    .context("port")?;
                i += 2;
            }
            "--scopes" => {
                scopes = args
                    .get(i + 1)
                    .context("--scopes a,b")?
                    .split(',')
                    .map(String::from)
                    .collect();
                i += 2;
            }
            other => bail!("unknown flag {other}"),
        }
    }
    let cfg = Config::load(config)?;
    let secrets = karl_core::policy::Secrets::load(&cfg.daemon.secrets)?;
    let client_id = secrets.get(name, "client_id")?.to_string();
    let client_secret = secrets.get(name, "client_secret")?.to_string();
    let refresh =
        karl_sources::google::oauth::loopback_flow(&client_id, &client_secret, &scopes, port)
            .await?;
    karl_core::policy::Secrets::set_key(&cfg.daemon.secrets, name, "refresh_token", &refresh)?;
    println!(
        "refresh_token written to [{name}] in {}. Restart karld.",
        cfg.daemon.secrets.display()
    );
    Ok(())
}

/// Microsoft: device code, no port, works over ssh. `[secret]` needs `client_id`
/// (an app registration with "allow public client flows") and optionally `tenant`.
async fn auth_ms(config: &Path, args: &[String]) -> Result<()> {
    let name = args
        .first()
        .context("usage: karld auth-ms <secret-name> [--scopes a,b]")?;
    let mut scopes = karl_sources::microsoft::teams::default_scopes();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--scopes" => {
                scopes = args
                    .get(i + 1)
                    .context("--scopes a,b")?
                    .split(',')
                    .map(String::from)
                    .collect();
                i += 2;
            }
            other => bail!("unknown flag {other}"),
        }
    }
    let cfg = Config::load(config)?;
    let secrets = karl_core::policy::Secrets::load(&cfg.daemon.secrets)?;
    let client_id = secrets.get(name, "client_id")?.to_string();
    let tenant = secrets
        .get(name, "tenant")
        .unwrap_or("organizations")
        .to_string();
    let refresh =
        karl_sources::microsoft::oauth::device_code_flow(&client_id, &tenant, &scopes).await?;
    karl_core::policy::Secrets::set_key(&cfg.daemon.secrets, name, "refresh_token", &refresh)?;
    println!(
        "refresh_token written to [{name}] in {}. Restart karld.",
        cfg.daemon.secrets.display()
    );
    Ok(())
}
