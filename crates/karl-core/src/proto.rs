//! Wire format on the unix socket. One request per connection.
//!
//! ```text
//! client → {"v":1,"args":["msg","new"]}\n
//! server → {"ok":true,"len":812,"exit":0}\n   followed by exactly `len` bytes of body
//! ```
//!
//! The body is the text the model reads. The header is for the CLI: it prints
//! the body to stdout (or stderr when `ok` is false) and exits with `exit`.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Read, Write};

pub const VERSION: u32 = 1;
/// A request line longer than this is refused unread.
pub const MAX_REQUEST: usize = 4096;

pub const EXIT_OK: i32 = 0;
pub const EXIT_ERROR: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_DENIED: i32 = 3;
pub const EXIT_RATE: i32 = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub v: u32,
    pub args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub ok: bool,
    pub len: usize,
    pub exit: i32,
}

/// A complete reply: status plus body text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub exit: i32,
    pub body: String,
}

impl Reply {
    pub fn ok(body: impl Into<String>) -> Reply {
        Reply {
            exit: EXIT_OK,
            body: body.into(),
        }
    }
    pub fn usage(msg: impl Into<String>) -> Reply {
        Reply {
            exit: EXIT_USAGE,
            body: msg.into(),
        }
    }
    pub fn denied(msg: impl Into<String>) -> Reply {
        Reply {
            exit: EXIT_DENIED,
            body: msg.into(),
        }
    }
    pub fn error(msg: impl Into<String>) -> Reply {
        Reply {
            exit: EXIT_ERROR,
            body: msg.into(),
        }
    }
    pub fn rate_limited(msg: impl Into<String>) -> Reply {
        Reply {
            exit: EXIT_RATE,
            body: msg.into(),
        }
    }
    pub fn is_ok(&self) -> bool {
        self.exit == EXIT_OK
    }
    pub fn encode(&self) -> Vec<u8> {
        let mut body = self.body.clone();
        if !body.is_empty() && !body.ends_with('\n') {
            body.push('\n');
        }
        let h = Header {
            ok: self.is_ok(),
            len: body.len(),
            exit: self.exit,
        };
        let mut out = serde_json::to_vec(&h).expect("header");
        out.push(b'\n');
        out.extend_from_slice(body.as_bytes());
        out
    }
}

pub fn encode_request(args: &[String]) -> Vec<u8> {
    let mut v = serde_json::to_vec(&Request {
        v: VERSION,
        args: args.to_vec(),
    })
    .expect("request");
    v.push(b'\n');
    v
}

pub fn decode_request(line: &[u8]) -> Result<Request> {
    if line.len() > MAX_REQUEST {
        bail!("request too long");
    }
    let r: Request = serde_json::from_slice(line).context("bad request json")?;
    if r.v != VERSION {
        bail!("protocol version {} not supported (want {VERSION})", r.v);
    }
    if r.args.len() > 64 {
        bail!("too many args");
    }
    Ok(r)
}

pub fn decode_header(line: &[u8]) -> Result<Header> {
    serde_json::from_slice(line).context("bad reply header")
}

/// Blocking client side: send args, read the whole reply. Used by the `karl` CLI.
pub fn call<S: Read + Write>(stream: &mut S, args: &[String]) -> Result<Reply> {
    stream.write_all(&encode_request(args))?;
    stream.flush()?;
    let mut reader = std::io::BufReader::new(stream);
    let mut line = Vec::new();
    reader.read_until(b'\n', &mut line)?;
    if line.is_empty() {
        bail!("daemon closed the connection without a reply");
    }
    let h = decode_header(&line)?;
    let mut body = vec![0u8; h.len];
    reader.read_exact(&mut body)?;
    Ok(Reply {
        exit: h.exit,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let req = encode_request(&["msg".into(), "new".into()]);
        assert_eq!(req, b"{\"v\":1,\"args\":[\"msg\",\"new\"]}\n");
        let r = decode_request(&req[..req.len() - 1]).unwrap();
        assert_eq!(r.args, vec!["msg", "new"]);
        let rep = Reply::ok("line Juliette: hi");
        let enc = rep.encode();
        let nl = enc.iter().position(|b| *b == b'\n').unwrap();
        let h = decode_header(&enc[..nl]).unwrap();
        assert_eq!(
            h,
            Header {
                ok: true,
                len: 18,
                exit: 0
            }
        );
        assert_eq!(&enc[nl + 1..], b"line Juliette: hi\n");
    }

    #[test]
    fn call_over_pipe() {
        // a fake server: a buffer that holds the reply
        struct Fake(std::io::Cursor<Vec<u8>>, Vec<u8>);
        impl Read for Fake {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                self.0.read(b)
            }
        }
        impl Write for Fake {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.1.extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut f = Fake(
            std::io::Cursor::new(Reply::denied("writes disabled").encode()),
            vec![],
        );
        let r = call(&mut f, &["jira".into(), "comment".into()]).unwrap();
        assert_eq!(r.exit, EXIT_DENIED);
        assert_eq!(r.body, "writes disabled\n");
        assert!(f.1.starts_with(b"{\"v\":1"));
    }

    #[test]
    fn rejects_bad() {
        assert!(decode_request(b"{\"v\":2,\"args\":[]}").is_err());
        assert!(decode_request(b"nope").is_err());
        assert!(decode_request(&vec![b'x'; MAX_REQUEST + 1]).is_err());
    }
}
