//! Name → uid/gid from /etc/passwd and /etc/group. Numeric strings pass through.

pub fn uid_of(name: &str) -> Option<u32> {
    lookup("/etc/passwd", name)
}

pub fn gid_of(name: &str) -> Option<u32> {
    lookup("/etc/group", name)
}

fn lookup(file: &str, name: &str) -> Option<u32> {
    if let Ok(n) = name.parse() {
        return Some(n);
    }
    let text = std::fs::read_to_string(file).ok()?;
    text.lines().find_map(|l| {
        let mut f = l.split(':');
        (f.next()? == name).then(|| f.nth(1)?.parse().ok())?
    })
}

pub fn my_uid() -> u32 {
    // the daemon never runs as root; /proc/self/status is enough and dependency-free
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    #[test]
    fn root_is_zero() {
        assert_eq!(super::uid_of("root"), Some(0));
        assert_eq!(super::uid_of("12"), Some(12));
        assert_eq!(super::uid_of("no-such-user-xyz"), None);
    }
}
