use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use ipnet::{Ipv4Net, Ipv6Net};

const LINK_LOCAL_V4: Ipv4Net = Ipv4Net::new_assert(Ipv4Addr::new(169, 254, 0, 0), 16);
const CGNAT_V4: Ipv4Net = Ipv4Net::new_assert(Ipv4Addr::new(100, 64, 0, 0), 10);
const LINK_LOCAL_V6: Ipv6Net = Ipv6Net::new_assert(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10);

pub(crate) struct IpPolicy;

impl IpPolicy {
    pub(crate) fn is_blocked(&self, addr: IpAddr) -> bool {
        match addr {
            IpAddr::V4(addr) => LINK_LOCAL_V4.contains(&addr) || CGNAT_V4.contains(&addr),
            IpAddr::V6(addr) => match addr.to_ipv4_mapped() {
                Some(mapped) => LINK_LOCAL_V4.contains(&mapped) || CGNAT_V4.contains(&mapped),
                None => LINK_LOCAL_V6.contains(&addr),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: 遮断・許可境界を含む IP 表 / When: policy で分類 / Then: 指定範囲と mapped IPv4 が契約どおりになる
    #[test]
    fn classifies_ip_ranges_fail_closed() {
        let cases = [
            ("169.254.169.254", true),
            ("169.254.1.1", true),
            ("100.64.0.1", true),
            ("100.127.255.254", true),
            ("fe80::1", true),
            ("fe8f::", true),
            ("::ffff:169.254.169.254", true),
            ("127.0.0.1", false),
            ("::1", false),
            ("10.1.2.3", false),
            ("172.16.0.1", false),
            ("172.31.255.255", false),
            ("192.168.1.1", false),
            ("8.8.8.8", false),
            ("203.0.113.1", false),
            ("::ffff:127.0.0.1", false),
            ("::ffff:8.8.8.8", false),
        ];
        let policy = IpPolicy;

        for (input, expected) in cases {
            let addr = input.parse().expect("テスト IP は有効");
            assert_eq!(policy.is_blocked(addr), expected, "{input}");
        }
    }
}
