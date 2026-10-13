//! `karl`: send argv to the daemon, print the reply, exit with its code.
//!
//! Holds nothing: no credentials, no grammar, no database. The socket is the
//! only thing it knows. Inside the agent sandbox it is `/run/karl.sock`
//! (bind-mounted); elsewhere `KARL_SOCK` or `/run/karl/<user>.sock`.

use std::io::{BufRead, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

const VERSION: u32 = 1;

fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("KARL_SOCK") {
        return PathBuf::from(p);
    }
    let sandbox = PathBuf::from("/run/karl.sock");
    if sandbox.exists() {
        return sandbox;
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "default".into());
    PathBuf::from(format!("/run/karl/{user}.sock"))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--version") {
        println!("karl {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let path = socket_path();
    let mut stream = match UnixStream::connect(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("karl: cannot reach daemon at {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(30)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let req = serde_json::json!({ "v": VERSION, "args": args });
    let mut line = serde_json::to_vec(&req).expect("json");
    line.push(b'\n');
    if let Err(e) = stream.write_all(&line).and_then(|_| stream.flush()) {
        eprintln!("karl: send: {e}");
        std::process::exit(1);
    }
    let mut reader = std::io::BufReader::new(stream);
    let mut header = Vec::new();
    if reader.read_until(b'\n', &mut header).is_err() || header.is_empty() {
        eprintln!("karl: no reply from daemon");
        std::process::exit(1);
    }
    let h: serde_json::Value = match serde_json::from_slice(&header) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("karl: bad reply header: {e}");
            std::process::exit(1);
        }
    };
    let len = h["len"].as_u64().unwrap_or(0) as usize;
    let exit = h["exit"].as_i64().unwrap_or(1) as i32;
    let mut body = vec![0u8; len];
    if let Err(e) = reader.read_exact(&mut body) {
        eprintln!("karl: short reply: {e}");
        std::process::exit(1);
    }
    let body = String::from_utf8_lossy(&body);
    if exit == 0 {
        print!("{body}");
    } else {
        eprint!("{body}");
    }
    let _ = std::io::stdout().flush();
    std::process::exit(exit);
}
