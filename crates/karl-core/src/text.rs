//! Text cleaning. Every byte that reaches the model costs time on a CPU model,
//! so bodies are stripped of HTML, quoted replies and signatures before they
//! are stored, and cut to a budget when rendered.

/// Strip HTML to plain text: drop `script`/`style`, turn block tags into
/// newlines, remove the rest, decode the common entities, collapse whitespace.
pub fn html_to_text(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len() / 2);
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // skip script/style blocks whole
            for tag in ["script", "style", "head"] {
                if lower[i..].starts_with(&format!("<{tag}")) {
                    if let Some(end) = lower[i..].find(&format!("</{tag}>")) {
                        i += end + tag.len() + 3;
                    } else {
                        i = bytes.len();
                    }
                }
            }
            if i >= bytes.len() {
                break;
            }
            if bytes[i] != b'<' {
                continue;
            }
            let close = lower[i..]
                .find('>')
                .map(|c| i + c)
                .unwrap_or(bytes.len() - 1);
            let tag = &lower[i + 1..close];
            let name: String = tag
                .trim_start_matches('/')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect();
            if matches!(
                name.as_str(),
                "p" | "br"
                    | "div"
                    | "tr"
                    | "li"
                    | "h1"
                    | "h2"
                    | "h3"
                    | "h4"
                    | "h5"
                    | "h6"
                    | "blockquote"
                    | "table"
            ) {
                out.push('\n');
            } else if name == "td" || name == "th" {
                out.push(' ');
            }
            i = close + 1;
        } else {
            let ch = html[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    let out = decode_entities(&out);
    // HTML block structure is not worth blank lines: one line per block, no empty ones.
    collapse_ws(&out)
        .lines()
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        let end = rest.find(';').filter(|e| *e <= 10);
        match end {
            Some(e) => {
                let ent = &rest[1..e];
                let rep = match ent {
                    "amp" => Some("&".to_string()),
                    "lt" => Some("<".to_string()),
                    "gt" => Some(">".to_string()),
                    "quot" => Some("\"".to_string()),
                    "apos" | "#39" => Some("'".to_string()),
                    "nbsp" | "#160" => Some(" ".to_string()),
                    _ if ent.starts_with('#') => {
                        let n = if let Some(h) =
                            ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X"))
                        {
                            u32::from_str_radix(h, 16).ok()
                        } else {
                            ent[1..].parse().ok()
                        };
                        n.and_then(char::from_u32).map(|c| c.to_string())
                    }
                    _ => None,
                };
                match rep {
                    Some(r) => {
                        out.push_str(&r);
                        rest = &rest[e + 1..];
                    }
                    None => {
                        out.push('&');
                        rest = &rest[1..];
                    }
                }
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Collapse runs of spaces and tabs, trim lines, drop runs of blank lines.
pub fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for line in s.lines() {
        let line: Vec<&str> = line.split_whitespace().collect();
        if line.is_empty() {
            blank += 1;
            if blank == 1 && !out.is_empty() {
                out.push('\n');
            }
            continue;
        }
        blank = 0;
        out.push_str(&line.join(" "));
        out.push('\n');
    }
    out.trim().to_string()
}

/// Remove quoted replies and signatures from a mail body.
/// Cuts at the first `>` block, at "On … wrote:", at `-- ` and at the
/// usual "From: / Sent:" forwarded headers.
pub fn strip_quotes(body: &str) -> String {
    let mut kept = Vec::new();
    for line in body.lines() {
        let t = line.trim_start();
        if t.starts_with('>') {
            break;
        }
        if line.trim_end() == "--" || line == "-- " {
            break;
        }
        let l = t.to_ascii_lowercase();
        if (l.starts_with("on ") && l.ends_with("wrote:"))
            || l.starts_with("-----original message-----")
            || l.starts_with("from:") && kept.len() > 2
            || l.starts_with("________________________________")
        {
            break;
        }
        kept.push(line);
    }
    collapse_ws(&kept.join("\n"))
}

/// One line: newlines become ` ⏎ `, then cut to `max` chars with a `(+Nc)` tail.
pub fn one_line(s: &str, max: usize) -> String {
    let flat: String = s
        .split('\n')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ⏎ ");
    truncate_chars(&flat, max)
}

/// Cut to `max` chars; if cut, append ` …(+Nc)` so the model knows there is more.
pub fn truncate_chars(s: &str, max: usize) -> String {
    let total = s.chars().count();
    if total <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    while out.ends_with(' ') {
        out.pop();
    }
    out.push_str(&format!("…(+{}c)", total - out.chars().count()));
    out
}

/// Cut text to a byte budget on a char boundary, with a tail that says how much is left.
pub fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let tail_room = 20;
    let mut cut = max.saturating_sub(tail_room);
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let left = s.len() - cut;
    format!("{}…(+{}B)", &s[..cut], left)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_basic() {
        let h = "<html><head><style>p{}</style></head><body><p>Hi&nbsp;<b>Bob</b>,</p><div>see &lt;x&gt; &amp; y</div><script>x()</script></body></html>";
        assert_eq!(html_to_text(h), "Hi Bob,\nsee <x> & y");
    }

    #[test]
    fn quotes_cut() {
        let b = "Thanks!\nSee you.\n\nOn Mon, Alice wrote:\n> old stuff\n";
        assert_eq!(strip_quotes(b), "Thanks!\nSee you.");
        let b = "ok\n-- \nAlex\nCTO";
        assert_eq!(strip_quotes(b), "ok");
    }

    #[test]
    fn truncation() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        let t = truncate_chars("hello world this is long", 11);
        assert!(t.starts_with("hello worl"), "{t}");
        assert!(t.ends_with("c)"), "{t}");
        assert_eq!(one_line("a\n\nb\n", 20), "a ⏎ b");
        let t = truncate_bytes(&"é".repeat(100), 50);
        assert!(t.len() <= 60);
        assert!(t.contains("…(+"));
    }
}
