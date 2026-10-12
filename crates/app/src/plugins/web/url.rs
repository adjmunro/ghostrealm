//! Turning URL-bar input into a URL.

/// Turn URL-bar input into a URL: explicit schemes pass through, things that
/// look like hosts get `https://` (`http://` for localhost), everything else
/// becomes a search using `search` (with `{}` replaced by the encoded query).
pub fn normalize_url(input: &str, search: &str) -> String {
    let s = input.trim();
    if s.is_empty() {
        return "about:blank".to_string();
    }
    let has_scheme = s.contains("://")
        || [
            "about:",
            "data:",
            "file:",
            "javascript:",
            "view-source:",
            "chrome:",
        ]
        .iter()
        .any(|p| s.to_ascii_lowercase().starts_with(p));
    if has_scheme {
        return s.to_string();
    }
    let host = s.split(['/', '?', '#']).next().unwrap_or("");
    let bare = host.rsplit_once(':').map_or(host, |(h, port)| {
        if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
            h
        } else {
            host
        }
    });
    let local = bare.eq_ignore_ascii_case("localhost")
        || bare.parse::<std::net::Ipv4Addr>().is_ok()
        || bare.ends_with(".localhost")
        || bare.ends_with(".test");
    if !s.contains(char::is_whitespace) && local {
        return format!("http://{s}");
    }
    if !s.contains(char::is_whitespace)
        && bare.contains('.')
        && !bare.starts_with('.')
        && !bare.ends_with('.')
    {
        return format!("https://{s}");
    }
    search.replace("{}", &encode_query(s))
}

/// Percent-encode a search query (spaces as `+`).
fn encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH: &str = "https://duckduckgo.com/?q={}";

    #[test]
    fn urls_hosts_and_searches() {
        assert_eq!(normalize_url("https://a.b/c", SEARCH), "https://a.b/c");
        assert_eq!(normalize_url("example.com", SEARCH), "https://example.com");
        assert_eq!(
            normalize_url("example.com/x?y=1", SEARCH),
            "https://example.com/x?y=1"
        );
        assert_eq!(
            normalize_url("localhost:3000", SEARCH),
            "http://localhost:3000"
        );
        assert_eq!(
            normalize_url("127.0.0.1:8080/a", SEARCH),
            "http://127.0.0.1:8080/a"
        );
        assert_eq!(normalize_url("about:blank", SEARCH), "about:blank");
        assert_eq!(
            normalize_url("rust lifetimes", SEARCH),
            "https://duckduckgo.com/?q=rust+lifetimes"
        );
        assert_eq!(
            normalize_url("c++", SEARCH),
            "https://duckduckgo.com/?q=c%2B%2B"
        );
        assert_eq!(normalize_url("  ", SEARCH), "about:blank");
    }
}
