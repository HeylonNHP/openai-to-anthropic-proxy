use super::*;

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

fn v4(o: [u8; 4]) -> IpAddr {
    IpAddr::V4(Ipv4Addr::from(o))
}

fn v6(s: &str) -> IpAddr {
    IpAddr::V6(s.parse::<Ipv6Addr>().expect("ipv6"))
}

#[test]
fn public_addresses_are_allowed() {
    assert!(is_public_ip(v4([93, 184, 216, 34])));
    assert!(is_public_ip(v4([8, 8, 8, 8])));
    assert!(is_public_ip(v4([1, 1, 1, 1])));
}

#[test]
fn private_ranges_are_blocked() {
    // 10.0.0.0/8
    assert!(!is_public_ip(v4([10, 0, 0, 1])));
    assert!(!is_public_ip(v4([10, 255, 255, 255])));
    // 172.16.0.0/12
    assert!(!is_public_ip(v4([172, 16, 0, 1])));
    assert!(!is_public_ip(v4([172, 31, 255, 255])));
    // 192.168.0.0/16
    assert!(!is_public_ip(v4([192, 168, 1, 1])));
}

#[test]
fn loopback_is_blocked() {
    assert!(!is_public_ip(v4([127, 0, 0, 1])));
    assert!(!is_public_ip(v4([127, 255, 255, 254])));
}

#[test]
fn link_local_and_the_metadata_address_are_blocked() {
    // 169.254.0.0/16 — includes the cloud metadata endpoint.
    assert!(!is_public_ip(v4([169, 254, 169, 254])));
    assert!(!is_public_ip(v4([169, 254, 0, 1])));
}

#[test]
fn carrier_grade_nat_is_blocked() {
    // 100.64.0.0/10
    assert!(!is_public_ip(v4([100, 64, 0, 1])));
    assert!(!is_public_ip(v4([100, 127, 255, 255])));
}

#[test]
fn unspecified_multicast_and_reserved_are_blocked() {
    assert!(!is_public_ip(v4([0, 0, 0, 0])));
    assert!(!is_public_ip(v4([0, 1, 2, 3])));
    assert!(!is_public_ip(v4([224, 0, 0, 1])));
    assert!(!is_public_ip(v4([239, 255, 255, 250])));
    assert!(!is_public_ip(v4([255, 255, 255, 255])));
}

#[test]
fn documentation_and_benchmark_ranges_are_blocked() {
    assert!(!is_public_ip(v4([198, 51, 100, 1])));
    assert!(!is_public_ip(v4([203, 0, 113, 1])));
    assert!(!is_public_ip(v4([198, 18, 0, 1])));
}

#[test]
fn ipv6_private_ranges_are_blocked() {
    assert!(!is_public_ip(v6("::1")), "loopback");
    assert!(!is_public_ip(v6("::")), "unspecified");
    assert!(!is_public_ip(v6("fe80::1")), "link-local");
    assert!(!is_public_ip(v6("fc00::1")), "unique local");
    assert!(!is_public_ip(v6("fd00::1")), "unique local");
    assert!(!is_public_ip(v6("ff02::1")), "multicast");
}

#[test]
fn ipv4_mapped_ipv6_is_judged_as_ipv4() {
    // ::ffff:127.0.0.1 must be blocked even though the outer address is
    // not in an IPv6 private range.
    assert!(!is_public_ip(v6("::ffff:127.0.0.1")));
    assert!(!is_public_ip(v6("::ffff:10.0.0.1")));
    assert!(is_public_ip(v6("::ffff:93.184.216.34")));
}

#[test]
fn public_ipv6_is_allowed() {
    assert!(is_public_ip(v6("2606:2800:220:1:248:1893:25c8:1946")));
}

#[test]
fn a_non_http_scheme_is_rejected() {
    let err = ValidatedUrl::parse("file:///etc/passwd").expect_err("must be rejected");
    assert_eq!(err.code, "invalid_tool_input");

    let err = ValidatedUrl::parse("gopher://example.com/").expect_err("must be rejected");
    assert_eq!(err.code, "invalid_tool_input");

    let err = ValidatedUrl::parse("data:text/html,<h1>hi</h1>").expect_err("must be rejected");
    assert_eq!(err.code, "invalid_tool_input");
}

#[test]
fn credentials_in_the_url_are_rejected() {
    let err = ValidatedUrl::parse("https://user:pass@example.com/x").expect_err("must be rejected");
    assert_eq!(err.code, "invalid_tool_input");
    // The detail must NOT contain the credentials.
    assert!(!err.detail.contains("pass"), "leaked a credential");
}

#[test]
fn a_malformed_url_is_rejected() {
    let err = ValidatedUrl::parse("not a url").expect_err("must be rejected");
    assert_eq!(err.code, "invalid_tool_input");
}

#[test]
fn a_valid_url_is_split_into_its_parts() {
    let v = ValidatedUrl::parse("https://Example.COM:8443/a/b?q=1").expect("valid");
    assert_eq!(v.scheme, "https");
    assert_eq!(v.host, "example.com");
    assert_eq!(v.port, 8443);
    assert_eq!(v.path, "/a/b?q=1");
}

#[test]
fn the_default_port_is_filled_in() {
    let v = ValidatedUrl::parse("https://example.com/").expect("valid");
    assert_eq!(v.port, 443);
    let v = ValidatedUrl::parse("http://example.com/").expect("valid");
    assert_eq!(v.port, 80);
}

#[test]
fn an_ipv6_host_loses_its_brackets() {
    let v = ValidatedUrl::parse("http://[2606:2800::1]/x").expect("valid");
    assert_eq!(v.host, "2606:2800::1");
    assert!(v.to_url().contains("[2606:2800::1]"), "must re-bracket");
}

#[test]
fn a_redirect_to_a_private_address_is_resolved_but_refused_at_connect_time() {
    // The URL PARSE only checks syntax (scheme, credentials). The private
    // address is caught later, when the redirect target is resolved and
    // its address checked — which is where the SSRF defence lives.
    let next = resolve_redirect("https://example.com/", "http://127.0.0.1/admin")
        .expect("the redirect resolves syntactically");
    assert!(next.contains("127.0.0.1"), "got {next}");

    // And the address itself is refused by the range check.
    let v = ValidatedUrl::parse(&next).expect("valid");
    assert!(!is_public_ip(
        v.host.parse::<IpAddr>().expect("an IP literal")
    ));
}

#[test]
fn a_relative_redirect_resolves_against_the_base() {
    let next = resolve_redirect("https://example.com/a/b", "/c").expect("valid");
    assert!(next.starts_with("https://example.com/c"), "got {next}");
}

#[test]
fn a_redirect_to_a_bad_scheme_is_rejected() {
    let err = resolve_redirect("https://example.com/", "file:///etc/passwd")
        .expect_err("must be rejected");
    assert_eq!(err.code, "invalid_tool_input");
}

#[test]
fn truncation_is_a_no_op_without_a_limit() {
    let text = "a".repeat(1000);
    assert_eq!(truncate(text.clone(), None), text);
}

#[test]
fn truncation_leaves_short_text_alone() {
    let text = "short".to_owned();
    assert_eq!(truncate(text.clone(), Some(100)), text);
}

#[test]
fn truncation_cuts_at_a_paragraph_boundary() {
    let text = "first para\n\nsecond para".to_owned();
    let out = truncate(text, Some(20));
    assert!(out.starts_with("first para"), "got {out}");
    assert!(out.contains("[content truncated]"), "got {out}");
}
