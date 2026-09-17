//! Shared SSRF (Server-Side Request Forgery) protection.
//!
//! Provides hostname and IP validation to block requests to private/internal
//! network addresses. Used by both `web_fetch` and `browser` tools.

use std::net::{IpAddr, SocketAddr};

/// Result of a successful SSRF check: the URL is safe, and we optionally have
/// the resolved addresses for DNS pinning (prevents DNS rebinding / TOCTOU).
#[derive(Debug)]
pub(crate) struct SsrfCheckResult {
    /// Resolved socket addresses — empty ONLY when the host was a literal
    /// IP (already validated, nothing to pin). A DNS-resolved host always
    /// carries at least one pinned address: an empty DNS answer fails
    /// closed in `validate_answer_set` instead of skipping the pin.
    pub resolved_addrs: Vec<SocketAddr>,
}

/// Validate a URL against SSRF protections: checks scheme, hostname, and DNS resolution.
/// Returns `Ok(SsrfCheckResult)` if the URL is safe, `Err(error_message)` if blocked.
///
/// Fails closed: DNS lookup failures are treated as blocked (prevents bypass
/// by causing DNS resolution to fail at check time but succeed at fetch time).
pub(crate) async fn check_ssrf_with_addrs(url: &str) -> Result<SsrfCheckResult, String> {
    let parsed = reqwest::Url::parse(url).map_err(|_| "Invalid URL".to_string())?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;

    if is_private_host(host) {
        return Err("Requests to private/internal hosts are not allowed".to_string());
    }

    // Literal IPs were already checked by is_private_host — no DNS needed.
    if host.parse::<IpAddr>().is_ok() {
        return Ok(SsrfCheckResult {
            resolved_addrs: vec![],
        });
    }

    let port = parsed.port_or_known_default().unwrap_or(443);
    match tokio::net::lookup_host(format!("{host}:{port}")).await {
        Ok(addrs) => Ok(SsrfCheckResult {
            resolved_addrs: validate_answer_set(host, addrs)?,
        }),
        Err(e) => {
            // Fail closed: if DNS fails, block the request. An attacker could
            // trigger DNS failure at check time, then succeed at fetch time
            // (DNS rebinding variant).
            Err(format!(
                "DNS resolution failed for host '{host}' — blocking request (fail closed): {e}"
            ))
        }
    }
}

/// Validate a DNS answer set for `host`: every address must be public, and
/// the answer set must be non-empty.
///
/// SECURITY (peer-review fix): an EMPTY answer set is a hard failure, never
/// a bypass. The returned list is the DNS-pin set: every consumer of
/// [`SsrfCheckResult::resolved_addrs`] (`web_fetch`'s own hop loop, the
/// MCP remote dispatcher) skips
/// `.resolve()` pinning when the list is empty, on the assumption that
/// "empty = literal-IP host, nothing to pin". If an empty RESOLVED answer
/// could reach those consumers, the connect phase would re-resolve the host
/// unpinned — re-opening the DNS-rebinding TOCTOU (empty answer at check
/// time, private IP at fetch time) that pinning exists to prevent.
fn validate_answer_set(
    host: &str,
    addrs: impl IntoIterator<Item = SocketAddr>,
) -> Result<Vec<SocketAddr>, String> {
    let mut safe_addrs = Vec::new();
    for addr in addrs {
        if is_private_ip(&addr.ip()) {
            return Err(
                "Requests to private/internal hosts are not allowed (DNS resolved to private IP)"
                    .to_string(),
            );
        }
        safe_addrs.push(addr);
    }
    if safe_addrs.is_empty() {
        return Err(format!(
            "DNS resolution returned no addresses for host '{host}' — blocking request (fail closed)"
        ));
    }
    Ok(safe_addrs)
}

/// Check if a hostname is private/internal (string check + IP parse).
pub fn is_private_host(host: &str) -> bool {
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower == "localhost." {
        return true;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_private_ip(&ip);
    }
    false
}

/// Check if an IP address is in a private/internal range.
/// SSRF-relevant IPv4 ranges that are NOT routable public internet but which
/// `Ipv4Addr::is_private()`/`is_link_local()` do not cover. The std predicates
/// for these (`is_shared`, `is_benchmarking`, `is_reserved`, …) are all
/// nightly-only, so match the octets explicitly.
fn is_special_purpose_v4(v4: &std::net::Ipv4Addr) -> bool {
    let [a, b, ..] = v4.octets();
    // Shared address space / CGNAT 100.64.0.0/10 (RFC 6598) — routes to ISP
    // carrier-grade NAT infrastructure.
    (a == 100 && (64..=127).contains(&b))
        // IETF protocol assignments 192.0.0.0/24 (RFC 6890).
        || v4.octets()[..3] == [192, 0, 0]
        // Benchmarking 198.18.0.0/15 (RFC 2544).
        || (a == 198 && (b == 18 || b == 19))
        // Multicast 224.0.0.0/4 and reserved/future 240.0.0.0/4 (RFC 1112),
        // plus the limited-broadcast 255.255.255.255 that 240/4 subsumes.
        || a >= 224
}

pub fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()                 // 127.0.0.0/8
                || v4.is_private()           // 10/8, 172.16/12, 192.168/16
                || v4.is_link_local()        // 169.254/16 (AWS metadata)
                || v4.is_unspecified()       // 0.0.0.0
                || is_special_purpose_v4(v4) // CGNAT/benchmark/reserved/multicast
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()           // ::1
                || v6.is_unspecified() // ::
                || v6.is_multicast()   // ff00::/8
                // ULA fc00::/7
                || matches!(v6.segments()[0], 0xfc00..=0xfdff)
                // Link-local fe80::/10
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // Site-local fec0::/10 (deprecated RFC 3879, still routable)
                || (v6.segments()[0] & 0xffc0) == 0xfec0
                // IPv4-mapped ::ffff:x.x.x.x
                || v6.to_ipv4_mapped().is_some_and(|v4| is_private_ip(&IpAddr::V4(v4)))
                // IPv4-compatible ::x.x.x.x (deprecated RFC 4291)
                || v6.to_ipv4().is_some_and(|v4| is_private_ip(&IpAddr::V4(v4)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-local simple API: `Some(error)` if blocked, `None` if safe.
    async fn check_ssrf(url: &str) -> Option<String> {
        check_ssrf_with_addrs(url).await.err()
    }

    // --- Async check_ssrf() tests ---

    #[tokio::test]
    async fn test_check_ssrf_blocks_localhost() {
        let result = check_ssrf("http://localhost/secret").await;
        assert!(result.is_some(), "localhost should be blocked");
        assert!(result.unwrap().contains("private"));
    }

    #[tokio::test]
    async fn test_check_ssrf_blocks_metadata_endpoint() {
        // AWS metadata endpoint
        let result = check_ssrf("http://169.254.169.254/latest/meta-data/").await;
        assert!(result.is_some(), "AWS metadata IP should be blocked");
    }

    #[tokio::test]
    async fn test_check_ssrf_blocks_private_network() {
        let result = check_ssrf("http://10.0.0.1/internal").await;
        assert!(result.is_some(), "10.x.x.x should be blocked");

        let result = check_ssrf("http://192.168.1.1/router").await;
        assert!(result.is_some(), "192.168.x.x should be blocked");
    }

    #[tokio::test]
    async fn test_check_ssrf_allows_public_ip() {
        // 8.8.8.8 is Google's public DNS — always resolves to itself
        let result = check_ssrf("https://8.8.8.8/").await;
        assert!(result.is_none(), "public IP 8.8.8.8 should be allowed");
    }

    #[tokio::test]
    async fn test_check_ssrf_blocks_ipv6_loopback() {
        let result = check_ssrf("http://[::1]/secret").await;
        assert!(result.is_some(), "IPv6 loopback should be blocked");
    }

    // --- Sync helper tests ---

    #[test]
    fn test_private_host_ipv6() {
        assert!(is_private_host("::1"));
        assert!(is_private_host("::"));
        assert!(is_private_host("fc00::1"));
        assert!(is_private_host("fd12:3456::1"));
        assert!(is_private_host("fe80::1"));
        assert!(is_private_host("::ffff:127.0.0.1"));
        assert!(is_private_host("::ffff:192.168.1.1"));
        assert!(is_private_host("ff02::1"));
        assert!(is_private_host("fec0::1"));
        assert!(is_private_host("::192.168.1.1"));
    }

    #[test]
    fn test_private_host_special_purpose_ranges() {
        // CGNAT / shared address space (RFC 6598) — the carrier-grade-NAT
        // range that routes to ISP infrastructure, previously un-blocked.
        assert!(is_private_host("100.64.0.1"), "CGNAT low edge");
        assert!(is_private_host("100.100.100.100"), "CGNAT middle");
        assert!(is_private_host("100.127.255.255"), "CGNAT high edge");
        // Just OUTSIDE the /10 must stay public (100.64/10 boundaries).
        assert!(!is_private_host("100.63.255.255"), "below CGNAT is public");
        assert!(!is_private_host("100.128.0.0"), "above CGNAT is public");
        // IETF protocol assignments (RFC 6890) 192.0.0.0/24.
        assert!(is_private_host("192.0.0.1"));
        assert!(!is_private_host("192.0.1.1"), "192.0.1/24 is public");
        // Benchmarking (RFC 2544) 198.18.0.0/15.
        assert!(is_private_host("198.18.0.1"));
        assert!(is_private_host("198.19.255.255"));
        assert!(
            !is_private_host("198.20.0.0"),
            "above benchmarking is public"
        );
        // Reserved / future use (RFC 1112) 240.0.0.0/4 + limited broadcast.
        assert!(is_private_host("240.0.0.1"));
        assert!(is_private_host("255.255.255.255"));
        // IPv4 multicast 224.0.0.0/4 as a literal host.
        assert!(is_private_host("224.0.0.1"));
        // Mapped/compat forms of CGNAT must be blocked too (defense in depth).
        assert!(is_private_host("::ffff:100.64.0.1"), "mapped CGNAT");
    }

    /// SECURITY (peer-review finding: empty DNS answer skips pinning): an
    /// empty DNS answer set must be a HARD FAILURE, never a bypass — if it
    /// flowed through as `Ok` with no addresses, every pinning consumer
    /// would skip `.resolve()` ("empty = literal IP, nothing to pin") and
    /// reqwest would re-resolve the host at connect time, letting a
    /// rebinding resolver answer empty at check time and 169.254.169.254 at
    /// fetch time — the exact DNS-rebinding TOCTOU the pin exists to close.
    #[test]
    fn should_fail_closed_when_dns_answer_set_is_empty() {
        let result = validate_answer_set("rebind.example.com", std::iter::empty());
        assert!(
            result.is_err(),
            "empty DNS answer must be blocked, not treated as 'nothing to pin'"
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("no addresses") && err.contains("fail closed"),
            "error must keep the module's fail-closed taxonomy: {err}"
        );
    }

    #[test]
    fn should_block_answer_set_containing_private_ip() {
        // A mixed answer (public + private) is how a rebinding resolver
        // smuggles an internal target past a first-answer-only check.
        let public: SocketAddr = "93.184.216.34:443".parse().unwrap();
        let private: SocketAddr = "10.0.0.1:443".parse().unwrap();
        let result = validate_answer_set("mixed.example.com", [public, private]);
        assert!(
            result
                .expect_err("private answer must block")
                .contains("private"),
            "private answers must be reported with the module's taxonomy"
        );
    }

    // --- check_ssrf_with_addrs tests ---

    #[tokio::test]
    async fn test_with_addrs_fails_closed_on_nonexistent_domain() {
        // This domain should fail DNS resolution → must be blocked (fail closed)
        let result =
            check_ssrf_with_addrs("https://this-domain-does-not-exist-ssrf-test.invalid/foo").await;
        assert!(
            result.is_err(),
            "DNS failure should block request (fail closed)"
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("DNS resolution failed") || err.contains("fail closed"),
            "error message should indicate DNS failure: {err}"
        );
    }

    #[tokio::test]
    async fn test_check_ssrf_blocks_ipv4_mapped_ipv6_url() {
        // IPv4-mapped IPv6 pointing to loopback
        let result = check_ssrf("http://[::ffff:127.0.0.1]/secret").await;
        assert!(
            result.is_some(),
            "IPv4-mapped IPv6 loopback should be blocked"
        );
    }

    #[tokio::test]
    async fn test_check_ssrf_blocks_ipv4_mapped_ipv6_private() {
        let result = check_ssrf("http://[::ffff:192.168.1.1]/internal").await;
        assert!(
            result.is_some(),
            "IPv4-mapped IPv6 private should be blocked"
        );
    }
}
