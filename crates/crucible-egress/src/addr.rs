//! What kind of network an address belongs to. An IPv6 address that carries an IPv4 address (mapped,
//! compatible, NAT64, 6to4 or Teredo) is judged by the worse of itself and the address it carries, so no
//! spelling of an address reaches a network that its plain form would not.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

/// The kind of network an address belongs to, worst first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum AddrClass {
    /// A cloud metadata service. Never called, under any setting.
    Metadata,
    /// The node itself: `127.0.0.0/8`, `0.0.0.0/8`, `::1`, `::`.
    Loopback,
    /// Link-local: `169.254.0.0/16`, `fe80::/10`.
    LinkLocal,
    /// A private network: `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `fc00::/7`, `fec0::/10`.
    Private,
    /// Carrier-grade NAT: `100.64.0.0/10`.
    Cgnat,
    /// Multicast, broadcast, documentation, benchmarking and other ranges no receiver lives in.
    Reserved,
    /// Everything else.
    Public,
}

impl AddrClass {
    /// The class in a sentence: "a private network address".
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Metadata => "a cloud metadata address",
            Self::Loopback => "a loopback address",
            Self::LinkLocal => "a link-local address",
            Self::Private => "a private network address",
            Self::Cgnat => "a carrier-grade NAT address",
            Self::Reserved => "a reserved address",
            Self::Public => "a public address",
        }
    }
}

const fn v4(a: u8, b: u8, c: u8, d: u8) -> u32 {
    u32::from_be_bytes([a, b, c, d])
}

const fn in_v4(ip: u32, net: u32, prefix: u32) -> bool {
    prefix == 0 || (ip ^ net) >> (32 - prefix) == 0
}

/// The metadata endpoints of the common clouds: the two link-local `/24`s that hold the instance metadata and
/// the container credential services, and the single addresses outside link-local space.
const METADATA_V4: [(u32, u32); 5] = [
    (v4(169, 254, 169, 0), 24),
    (v4(169, 254, 170, 0), 24),
    (v4(100, 100, 100, 200), 32),
    (v4(168, 63, 129, 16), 32),
    (v4(192, 0, 0, 192), 32),
];

const RESERVED_V4: [(u32, u32); 7] = [
    (v4(192, 0, 0, 0), 24),
    (v4(192, 0, 2, 0), 24),
    (v4(198, 18, 0, 0), 15),
    (v4(198, 51, 100, 0), 24),
    (v4(203, 0, 113, 0), 24),
    (v4(224, 0, 0, 0), 4),
    (v4(240, 0, 0, 0), 4),
];

fn classify_v4(ip: Ipv4Addr) -> AddrClass {
    let ip = u32::from(ip);
    let any = |ranges: &[(u32, u32)]| ranges.iter().any(|(net, prefix)| in_v4(ip, *net, *prefix));
    if any(&METADATA_V4) {
        AddrClass::Metadata
    } else if in_v4(ip, v4(127, 0, 0, 0), 8) || in_v4(ip, 0, 8) {
        AddrClass::Loopback
    } else if in_v4(ip, v4(169, 254, 0, 0), 16) {
        AddrClass::LinkLocal
    } else if in_v4(ip, v4(10, 0, 0, 0), 8)
        || in_v4(ip, v4(172, 16, 0, 0), 12)
        || in_v4(ip, v4(192, 168, 0, 0), 16)
    {
        AddrClass::Private
    } else if in_v4(ip, v4(100, 64, 0, 0), 10) {
        AddrClass::Cgnat
    } else if any(&RESERVED_V4) {
        AddrClass::Reserved
    } else {
        AddrClass::Public
    }
}

const fn in_v6(ip: u128, net: u128, prefix: u32) -> bool {
    prefix == 0 || (ip ^ net) >> (128 - prefix) == 0
}

const fn v6(segments: [u16; 8]) -> u128 {
    let mut out = 0u128;
    let mut i = 0;
    while i < 8 {
        out = (out << 16) | segments[i] as u128;
        i += 1;
    }
    out
}

/// The IPv4 addresses an IPv6 address carries: IPv4-mapped (`::ffff:0:0/96`), IPv4-compatible (`::/96`),
/// NAT64 (`64:ff9b::/96` and `64:ff9b:1::/48`), 6to4 (`2002::/16`) and Teredo (`2001::/32`, whose client address
/// is stored inverted).
pub fn embedded_v4(ip: Ipv6Addr) -> Vec<Ipv4Addr> {
    let bits = u128::from(ip);
    let low = Ipv4Addr::from((bits & 0xffff_ffff) as u32);
    let mut found = Vec::new();
    if in_v6(bits, v6([0, 0, 0, 0, 0, 0xffff, 0, 0]), 96)
        || in_v6(bits, 0, 96)
        || in_v6(bits, v6([0x64, 0xff9b, 0, 0, 0, 0, 0, 0]), 96)
        || in_v6(bits, v6([0x64, 0xff9b, 1, 0, 0, 0, 0, 0]), 48)
    {
        found.push(low);
    }
    if in_v6(bits, v6([0x2002, 0, 0, 0, 0, 0, 0, 0]), 16) {
        found.push(Ipv4Addr::from(((bits >> 80) & 0xffff_ffff) as u32));
    }
    if in_v6(bits, v6([0x2001, 0, 0, 0, 0, 0, 0, 0]), 32) {
        found.push(Ipv4Addr::from(!((bits & 0xffff_ffff) as u32)));
        // The Teredo server address.
        found.push(Ipv4Addr::from(((bits >> 64) & 0xffff_ffff) as u32));
    }
    found
}

fn classify_v6(ip: Ipv6Addr) -> AddrClass {
    let bits = u128::from(ip);
    let own = if in_v6(bits, v6([0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0]), 32)
        || bits == v6([0xfe80, 0, 0, 0, 0, 0, 0xa9fe, 0xa9fe])
        || bits == v6([0xfd20, 0x00ce, 0, 0, 0, 0, 0, 0x254])
    {
        AddrClass::Metadata
    } else if bits <= 1 {
        AddrClass::Loopback
    } else if in_v6(bits, v6([0xfe80, 0, 0, 0, 0, 0, 0, 0]), 10) {
        AddrClass::LinkLocal
    } else if in_v6(bits, v6([0xfc00, 0, 0, 0, 0, 0, 0, 0]), 7)
        || in_v6(bits, v6([0xfec0, 0, 0, 0, 0, 0, 0, 0]), 10)
    {
        AddrClass::Private
    } else if in_v6(bits, v6([0xff00, 0, 0, 0, 0, 0, 0, 0]), 8)
        || in_v6(bits, v6([0x2001, 0x0db8, 0, 0, 0, 0, 0, 0]), 32)
    {
        AddrClass::Reserved
    } else {
        AddrClass::Public
    };
    embedded_v4(ip)
        .into_iter()
        .map(classify_v4)
        .fold(own, Ord::min)
}

/// The class of an address: the worst of its own range and of every IPv4 address it carries.
pub fn classify(ip: IpAddr) -> AddrClass {
    match ip {
        IpAddr::V4(ip) => classify_v4(ip),
        IpAddr::V6(ip) => classify_v6(ip),
    }
}

/// An address range in CIDR notation, for `egress.allow_cidrs`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Cidr {
    addr: IpAddr,
    prefix: u8,
}

/// A range that is not `address/prefix`.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
#[error(
    "`{0}` is not an address range; write it as address/prefix, for example 10.0.0.0/8 or fd00::/8"
)]
pub struct InvalidCidr(pub String);

impl Cidr {
    /// Whether the range holds `ip`. An IPv4 range also holds the IPv6 forms that carry one of its addresses.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                in_v4(u32::from(ip), u32::from(net), u32::from(self.prefix))
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                in_v6(u128::from(ip), u128::from(net), u32::from(self.prefix))
            }
            (IpAddr::V4(_), IpAddr::V6(ip)) => {
                let carried = embedded_v4(ip);
                !carried.is_empty() && carried.iter().all(|ip| self.contains(IpAddr::V4(*ip)))
            }
            (IpAddr::V6(_), IpAddr::V4(_)) => false,
        }
    }
}

impl FromStr for Cidr {
    type Err = InvalidCidr;

    fn from_str(text: &str) -> Result<Self, InvalidCidr> {
        let bad = || InvalidCidr(text.to_owned());
        let (addr, prefix) = match text.trim().split_once('/') {
            Some((addr, prefix)) => (addr, Some(prefix)),
            None => (text.trim(), None),
        };
        let addr: IpAddr = addr.parse().map_err(|_| bad())?;
        let widest = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(prefix) => prefix.parse::<u8>().map_err(|_| bad())?,
            None => widest,
        };
        if prefix > widest {
            return Err(bad());
        }
        Ok(Self { addr, prefix })
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn class(text: &str) -> AddrClass {
        classify(text.parse().unwrap())
    }

    #[test]
    fn the_named_metadata_addresses_are_metadata() {
        for ip in [
            "169.254.169.254",
            "169.254.170.2",
            "169.254.169.123",
            "100.100.100.200",
            "168.63.129.16",
            "192.0.0.192",
            "fd00:ec2::254",
            "fd00:ec2::23",
            "fe80::a9fe:a9fe",
            "fd20:ce::254",
        ] {
            assert_eq!(class(ip), AddrClass::Metadata, "{ip}");
        }
    }

    #[test]
    fn carried_addresses_are_judged_by_what_they_carry() {
        for ip in [
            "::ffff:169.254.169.254",
            "::169.254.169.254",
            "64:ff9b::169.254.169.254",
            "64:ff9b:1::a9fe:a9fe",
            "2002:a9fe:a9fe::1",
            "2001:0:1234:5678:0:0:5601:5601",
        ] {
            assert_eq!(class(ip), AddrClass::Metadata, "{ip}");
        }
        assert_eq!(class("::ffff:10.1.2.3"), AddrClass::Private);
        assert_eq!(class("::ffff:127.0.0.1"), AddrClass::Loopback);
        assert_eq!(class("2002:7f00:1::"), AddrClass::Loopback);
        assert_eq!(class("::ffff:93.184.216.34"), AddrClass::Public);
    }

    #[test]
    fn plain_ranges_have_their_class() {
        for (ip, want) in [
            ("127.0.0.1", AddrClass::Loopback),
            ("0.0.0.0", AddrClass::Loopback),
            ("::1", AddrClass::Loopback),
            ("::", AddrClass::Loopback),
            ("169.254.1.1", AddrClass::LinkLocal),
            ("fe80::1", AddrClass::LinkLocal),
            ("10.0.0.1", AddrClass::Private),
            ("172.31.255.255", AddrClass::Private),
            ("172.32.0.1", AddrClass::Public),
            ("192.168.1.1", AddrClass::Private),
            ("fd12:3456::1", AddrClass::Private),
            ("100.64.0.1", AddrClass::Cgnat),
            ("100.128.0.1", AddrClass::Public),
            ("224.0.0.1", AddrClass::Reserved),
            ("255.255.255.255", AddrClass::Reserved),
            ("ff02::1", AddrClass::Reserved),
            ("93.184.216.34", AddrClass::Public),
            ("2606:4700::1111", AddrClass::Public),
        ] {
            assert_eq!(class(ip), want, "{ip}");
        }
    }

    #[test]
    fn ranges_parse_and_hold_their_addresses() {
        let ten: Cidr = "10.0.0.0/8".parse().unwrap();
        assert!(ten.contains("10.9.8.7".parse().unwrap()));
        assert!(ten.contains("::ffff:10.9.8.7".parse().unwrap()));
        assert!(!ten.contains("11.0.0.1".parse().unwrap()));
        assert!(!ten.contains("fd00::1".parse().unwrap()));
        let one: Cidr = "192.168.1.5".parse().unwrap();
        assert_eq!(one.to_string(), "192.168.1.5/32");
        assert!(one.contains("192.168.1.5".parse().unwrap()));
        let ula: Cidr = "fd00::/8".parse().unwrap();
        assert!(ula.contains("fd12::1".parse().unwrap()));
        for bad in ["10.0.0.0/33", "fd00::/129", "ten", "10.0.0.0/x", ""] {
            assert!(bad.parse::<Cidr>().is_err(), "{bad}");
        }
    }
}
