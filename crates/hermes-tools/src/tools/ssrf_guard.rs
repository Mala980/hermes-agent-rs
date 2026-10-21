//! SSRF guard for the web tools (web_extract).
//!
//! Policy: reject URLs that carry credentials (`user:pass@`) and reject hosts
//! that point at private/internal addresses. Pure-string classification
//! ([`classify_url`]) touches no network and is fully hermetic in tests; the
//! DNS step ([`ensure_url_allowed`]) only runs for hostnames that passed the
//! literal checks.
//!
//! SECURITY NOTE — DNS rebinding (TOCTOU) residual risk: the DNS check and the
//! actual HTTP connection happen at different times. A malicious hostname can
//! change its DNS answer between the check and the connection (rebinding), so
//! this guard *reduces* but does not *eliminate* SSRF risk. Full mitigation
//! requires pinning the resolved address to the connection (e.g. a custom
//! resolver in the HTTP client), which is not implemented here.

use std::net::{IpAddr, Ipv4Addr};

/// Classification result for a URL's host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlTarget {
    /// Host is an IP literal — no DNS needed.
    Ip(IpAddr),
    /// Host is a DNS name — needs a DNS check before fetching.
    Name(String),
}

/// Classify a URL purely by string (no network).
///
/// Returns `Err` with a denial reason when the URL is forbidden:
/// non-http(s) scheme, embedded credentials, `localhost` / `*.localhost`
/// host, or a private/internal IP literal.
pub fn classify_url(url: &str) -> Result<UrlTarget, String> {
    // --- Scheme ---
    let rest = url.split_once("://").and_then(|(scheme, rest)| {
        if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
            Some(rest)
        } else {
            None
        }
    });
    let rest = match rest {
        Some(r) => r,
        None => return Err(format!("blocked: only http(s) URLs are allowed: {url}")),
    };

    // --- Authority: [userinfo@]host[:port], stopping at /, ?, or # ---
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() {
        return Err(format!("blocked: URL has no host: {url}"));
    }

    // --- Credentials: reject any userinfo (user or user:pass) ---
    // Take the LAST '@' so `http://user@evil@example.com/` cannot slip through.
    let hostport = match authority.rfind('@') {
        Some(at) => {
            let userinfo = &authority[..at];
            if !userinfo.is_empty() {
                return Err("blocked: URL contains credentials (userinfo@)".to_string());
            }
            &authority[at + 1..]
        }
        None => authority,
    };

    // --- Extract host (drop port; handle bracketed IPv6) ---
    let host = if let Some(stripped) = hostport.strip_prefix('[') {
        match stripped.find(']') {
            Some(end) => &stripped[..end],
            None => {
                return Err(format!(
                    "blocked: invalid IPv6 literal (unbalanced brackets): {url}"
                ))
            }
        }
    } else if let Some(colon) = hostport.rfind(':') {
        // Only treat as a port when the entire suffix is digits.
        let port_part = &hostport[colon + 1..];
        if !port_part.is_empty() && port_part.bytes().all(|b| b.is_ascii_digit()) {
            &hostport[..colon]
        } else {
            hostport
        }
    } else {
        hostport
    };
    // Drop a raw IPv6 zone id (`%eth0`) if present; the rest is still checked.
    let host = host.split('%').next().unwrap_or(host);
    if host.is_empty() {
        return Err(format!("blocked: URL has no host: {url}"));
    }

    // --- localhost and *.localhost: reject (subdomains resolve to loopback
    // and are used to obscure logs/audits) ---
    let normalized = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    if normalized == "localhost" || normalized.ends_with(".localhost") {
        return Err(format!("blocked: host '{host}' is localhost"));
    }

    // --- IP literal: check directly against the forbidden ranges ---
    if let Ok(ip) = normalized.parse::<IpAddr>() {
        if is_blocked_ip(&ip) {
            return Err(format!(
                "blocked: host '{host}' is a private/internal IP ({ip})"
            ));
        }
        return Ok(UrlTarget::Ip(ip));
    }

    // Not an IP literal recognized by std (e.g. quirks like `2130706433`
    // which getaddrinfo still parses as 127.0.0.1): defer to the DNS check
    // in `ensure_url_allowed`.
    Ok(UrlTarget::Name(normalized))
}

/// True when the IP is in a range forbidden for web tools:
/// loopback, RFC1918 private, link-local (including cloud metadata
/// 169.254.0.0/16), 0.0.0.0, and their IPv6 equivalents.
fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 127 // 127.0.0.0/8 loopback
                || o[0] == 10 // 10.0.0.0/8
                || (o[0] == 172 && (16..=31).contains(&o[1])) // 172.16.0.0/12
                || (o[0] == 192 && o[1] == 168) // 192.168.0.0/16
                || (o[0] == 169 && o[1] == 254) // 169.254.0.0/16 link-local / cloud metadata
                || *v4 == Ipv4Addr::UNSPECIFIED // 0.0.0.0
        }
        IpAddr::V6(v6) => {
            // IPv4-mapped (::ffff:127.0.0.1) is checked as IPv4.
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_blocked_ip(&IpAddr::V4(mapped));
            }
            v6.is_loopback() // ::1
                || v6.is_unspecified() // ::
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 unique-local (IPv6 private)
        }
    }
}

/// Ensure a URL may be fetched: string classification + DNS check.
///
/// For DNS names, every resolved address is checked against the forbidden
/// ranges; if ANY of them is blocked, the URL is denied. See the DNS
/// rebinding note at the top of this module.
pub async fn ensure_url_allowed(url: &str) -> Result<(), String> {
    match classify_url(url)? {
        UrlTarget::Ip(_) => Ok(()), // already checked in classify_url
        UrlTarget::Name(name) => {
            let addrs: Vec<IpAddr> = tokio::net::lookup_host((name.as_str(), 443))
                .await
                .map_err(|e| format!("blocked: DNS resolution failed for '{name}': {e}"))?
                .map(|sock| sock.ip())
                .collect();
            for ip in addrs {
                if is_blocked_ip(&ip) {
                    return Err(format!(
                        "blocked: host '{name}' resolves to a private/internal IP ({ip})"
                    ));
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocked_msg(url: &str) -> String {
        classify_url(url).expect_err(&format!("expected {url} to be blocked"))
    }

    #[test]
    fn ssrf_blocks_localhost_names() {
        for url in [
            "http://localhost/",
            "http://localhost:8080/admin",
            "http://sub.localhost/",
            "http://LOCALHOST/",
            "http://localhost./",
        ] {
            let msg = blocked_msg(url);
            assert!(
                msg.contains("localhost"),
                "reason for {url} must mention localhost, got: {msg}"
            );
        }
    }

    #[test]
    fn ssrf_blocks_private_ipv4_literals() {
        for url in [
            "http://127.0.0.1/",
            "http://127.0.0.2:8080/",
            "http://10.1.2.3/",
            "http://10.255.255.255/admin",
            "http://172.16.0.1/",
            "http://172.31.255.254/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://0.0.0.0/",
        ] {
            let msg = blocked_msg(url);
            assert!(
                msg.contains("private/internal"),
                "reason for {url} must mention private/internal IP, got: {msg}"
            );
        }
    }

    #[test]
    fn ssrf_blocks_private_ipv6_literals() {
        for url in [
            "http://[::1]/",
            "http://[fe80::1]/",
            "http://[FE80::abcd]/",
            "http://[::]/",
            "http://[fc00::1]/",
            // IPv4-mapped: ::ffff:127.0.0.1 is checked as 127.0.0.1.
            "http://[::ffff:127.0.0.1]/",
        ] {
            let msg = blocked_msg(url);
            assert!(
                msg.contains("private/internal"),
                "reason for {url} must mention private/internal IP, got: {msg}"
            );
        }
    }

    #[test]
    fn ssrf_blocks_non_http_schemes() {
        for url in [
            "ftp://example.com/file",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "data:text/plain,hello",
        ] {
            let msg = blocked_msg(url);
            assert!(
                msg.contains("http(s)"),
                "reason for {url} must mention http(s), got: {msg}"
            );
        }
    }

    #[test]
    fn ssrf_blocks_urls_with_credentials() {
        for url in [
            "http://user@example.com/",
            "http://user:pass@example.com/",
            "http://user@evil@example.com/",
        ] {
            let msg = blocked_msg(url);
            assert!(
                msg.contains("credentials"),
                "reason for {url} must mention credentials, got: {msg}"
            );
        }
    }

    #[test]
    fn ssrf_allows_public_urls() {
        // Public hostnames: pass literal classification (no DNS in tests).
        assert_eq!(
            classify_url("https://example.com/").unwrap(),
            UrlTarget::Name("example.com".to_string())
        );
        assert_eq!(
            classify_url("https://api.github.com/repos").unwrap(),
            UrlTarget::Name("api.github.com".to_string())
        );
        // Public IP literals: pass as Ip.
        assert!(matches!(
            classify_url("http://93.184.216.34/").unwrap(),
            UrlTarget::Ip(_)
        ));
        assert!(matches!(
            classify_url("http://8.8.8.8/").unwrap(),
            UrlTarget::Ip(_)
        ));
        // 172.16.0.0/12 boundaries: 172.15.x.x and 172.32.x.x are public.
        assert!(matches!(
            classify_url("http://172.15.255.255/").unwrap(),
            UrlTarget::Ip(_)
        ));
        assert!(matches!(
            classify_url("http://172.32.0.1/").unwrap(),
            UrlTarget::Ip(_)
        ));
    }

    #[test]
    fn ssrf_rejects_malformed_urls() {
        assert!(classify_url("http:///").is_err());
        assert!(classify_url("https://").is_err());
        assert!(classify_url("http://[::1/").is_err()); // unbalanced brackets
    }

    #[tokio::test]
    async fn ssrf_ensure_url_allowed_blocks_private_literal_without_dns() {
        // IP literals never trigger DNS: hermetic.
        let err = ensure_url_allowed("http://169.254.169.254/latest/meta-data/")
            .await
            .expect_err("cloud metadata must be blocked");
        assert!(err.contains("private/internal"), "got: {err}");

        let err = ensure_url_allowed("http://[::1]/")
            .await
            .expect_err("IPv6 loopback must be blocked");
        assert!(err.contains("private/internal"), "got: {err}");

        ensure_url_allowed("http://93.184.216.34/")
            .await
            .expect("public IP literal must pass without DNS");
    }

    #[tokio::test]
    async fn ssrf_ensure_url_allowed_blocks_credentialed_url() {
        let err = ensure_url_allowed("https://user:pass@93.184.216.34/")
            .await
            .expect_err("credentialed URL must be blocked");
        assert!(err.contains("credentials"), "got: {err}");
    }
}
