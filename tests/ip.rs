use swisha::util::net::is_public;

#[test]
fn private_ranges_rejected() {
    for ip in ["10.0.0.1", "172.16.0.1", "172.31.255.255", "192.168.1.1", "127.0.0.1"] {
        assert!(!is_public(ip.parse().unwrap()), "{ip} should be private");
    }
}

#[test]
fn public_ips_accepted() {
    for ip in ["1.1.1.1", "8.8.8.8", "203.0.113.5"] {
        assert!(is_public(ip.parse().unwrap()), "{ip} should be public");
    }
}

#[test]
fn ipv6_private_ranges_rejected() {
    for ip in ["fe80::1", "fe80::abcd:ef01", "fc00::1", "fd00::1"] {
        assert!(!is_public(ip.parse().unwrap()), "{ip} should be private");
    }
}

#[test]
fn ipv6_public_accepted() {
    assert!(is_public("2001:db8::1".parse().unwrap()));
}

// The private ranges are bounded, and the octet either side of each boundary is public. An
// off-by-one here silently discards a real forwarded address or trusts a private one.
#[test]
fn the_edges_of_each_private_range_are_public() {
    for ip in ["9.255.255.255", "11.0.0.0", "172.15.255.255", "172.32.0.0", "192.167.255.255", "192.169.0.0"] {
        assert!(is_public(ip.parse().unwrap()), "{ip} sits outside every private range");
    }
    for ip in ["10.0.0.0", "10.255.255.255", "172.16.0.0", "172.31.255.255", "192.168.0.0", "192.168.255.255"] {
        assert!(!is_public(ip.parse().unwrap()), "{ip} is inside a private range");
    }
}

#[test]
fn unspecified_and_broadcast_are_not_public() {
    assert!(!is_public("0.0.0.0".parse().unwrap()));
    assert!(!is_public("255.255.255.255".parse().unwrap()));
    assert!(!is_public("::".parse().unwrap()));
    assert!(!is_public("::1".parse().unwrap()));
}

// A forwarded header is believed only when it names a routable address. IPv4 link-local is not
// one, any more than IPv6 link-local is.
#[test]
fn ipv4_link_local_is_not_public() {
    for ip in ["169.254.0.1", "169.254.169.254", "169.254.255.255"] {
        assert!(!is_public(ip.parse().unwrap()), "{ip} is link-local");
    }
    for ip in ["169.253.255.255", "169.255.0.0"] {
        assert!(is_public(ip.parse().unwrap()), "{ip} sits outside link-local");
    }
}
