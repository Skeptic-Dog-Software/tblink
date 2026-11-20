//! What each Thunderbolt port is plugged into, and how fast.
//!
//! Two read-only sources:
//!   - `networksetup -listallhardwareports` names each network port
//!     "Thunderbolt N" and gives its device (`en1`, `en6`, ...);
//!   - `system_profiler SPThunderboltDataType -json` lists one bus per
//!     receptacle, with the receptacle number (`receptacle_id_key`), the
//!     negotiated speed, and the devices on the other end.
//!
//! "Thunderbolt N" is receptacle N. On the Mac these fixtures come from,
//! en1 = Thunderbolt 1, en6 = Thunderbolt 2 and en2 = Thunderbolt 3, and the
//! peer Mac sat on receptacle 3, the port carrying en2's live link.
//!
//! A cable is named by the two Thunderbolt domain UUIDs it joins: this Mac's
//! bus and the remote Mac's. Both Macs see the same pair (each from its own
//! side), and the negotiated speed is the same at both ends, so both Macs can
//! rank their cables identically without talking to each other.

use serde_json::Value;

pub const NETWORKSETUP: &str = "/usr/sbin/networksetup";
pub const SYSTEM_PROFILER: &str = "/usr/sbin/system_profiler";

/// One Thunderbolt receptacle as `system_profiler` reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Receptacle {
    pub id: u32,
    pub connected: bool,
    /// Negotiated speed while something is connected.
    pub speed_gbps: Option<u32>,
    /// This bus's Thunderbolt domain UUID.
    pub domain: String,
    /// The Mac on the other end, when it offers IP over Thunderbolt.
    pub peer: Option<RemoteMac>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RemoteMac {
    pub name: String,
    pub domain: String,
}

impl Receptacle {
    /// A name for the cable that is the same on both Macs.
    pub fn cable_id(&self) -> Option<String> {
        let p = self.peer.as_ref()?;
        if self.domain.is_empty() || p.domain.is_empty() {
            return None;
        }
        let mut pair = [self.domain.to_ascii_uppercase(), p.domain.to_ascii_uppercase()];
        pair.sort();
        Some(pair.join("+"))
    }
}

/// `(device, N)` for every "Thunderbolt N" network port.
pub fn parse_hardware_ports(out: &str) -> Vec<(String, u32)> {
    let mut found = Vec::new();
    let mut number: Option<u32> = None;
    for line in out.lines() {
        let line = line.trim();
        if let Some(name) = line.strip_prefix("Hardware Port:") {
            number = name.trim().strip_prefix("Thunderbolt ").and_then(|n| n.trim().parse().ok());
        } else if let Some(dev) = line.strip_prefix("Device:") {
            if let Some(n) = number.take() {
                found.push((dev.trim().to_string(), n));
            }
        }
    }
    found
}

/// The number in "40 Gb/s" or "Up to 120 Gb/s".
pub fn parse_speed(s: &str) -> Option<u32> {
    let s = s.trim();
    let num = s.strip_suffix("Gb/s")?.trim();
    let num = num.rsplit(' ').next()?;
    num.parse::<f64>().ok().filter(|n| *n > 0.0).map(|n| n.round() as u32)
}

fn is_ip_peer(item: &Value) -> bool {
    item["services_title"].as_array().into_iter().flatten().any(|s| s["_name"].as_str() == Some("service_ip"))
}

/// Every host receptacle in `system_profiler SPThunderboltDataType -json` output.
pub fn parse_profiler(json: &str) -> Vec<Receptacle> {
    let v: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    let mut out = Vec::new();
    for bus in v["SPThunderboltDataType"].as_array().into_iter().flatten() {
        let Some(obj) = bus.as_object() else { continue };
        // The host port: a `receptacle_*_tag` that carries a receptacle number.
        let Some((id, tag)) = obj
            .iter()
            .filter(|(k, _)| k.starts_with("receptacle_") && k.ends_with("_tag"))
            .find_map(|(_, t)| t["receptacle_id_key"].as_str().and_then(|n| n.trim().parse::<u32>().ok()).map(|n| (n, t)))
        else {
            continue;
        };
        let connected = tag["receptacle_status_key"].as_str() == Some("receptacle_connected");
        // Devices directly on this receptacle are the bus's own items.
        let peer = bus["_items"].as_array().into_iter().flatten().find(|i| is_ip_peer(i)).map(|i| RemoteMac {
            name: i["_name"].as_str().unwrap_or("Mac").to_string(),
            domain: i["domain_uuid_key"].as_str().unwrap_or("").to_string(),
        });
        out.push(Receptacle {
            id,
            connected,
            speed_gbps: if connected { tag["current_speed_key"].as_str().and_then(parse_speed) } else { None },
            domain: bus["domain_uuid_key"].as_str().unwrap_or("").to_string(),
            peer: if connected { peer } else { None },
        });
    }
    out.sort_by_key(|r| r.id);
    out
}

/// What one network device is plugged into.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PortInfo {
    pub receptacle: Option<u32>,
    pub speed_gbps: Option<u32>,
    pub peer: Option<String>,
    pub cable_id: Option<String>,
}

/// Look up `device` (e.g. `en2`) in the two tool outputs.
pub fn port_info(device: &str, hardware_ports: &[(String, u32)], receptacles: &[Receptacle]) -> PortInfo {
    let Some(n) = hardware_ports.iter().find(|(d, _)| d == device).map(|(_, n)| *n) else { return PortInfo::default() };
    let Some(r) = receptacles.iter().find(|r| r.id == n) else { return PortInfo { receptacle: Some(n), ..PortInfo::default() } };
    PortInfo { receptacle: Some(n), speed_gbps: r.speed_gbps, peer: r.peer.as_ref().map(|p| p.name.clone()), cable_id: r.cable_id() }
}

/// The topology of this Mac, read with the stock tools.
pub fn read() -> (Vec<(String, u32)>, Vec<Receptacle>) {
    let hw = crate::net::run(NETWORKSETUP, &["-listallhardwareports"]).1;
    let (ok, sp) = crate::net::run(SYSTEM_PROFILER, &["SPThunderboltDataType", "-json"]);
    (parse_hardware_ports(&hw), if ok { parse_profiler(&sp) } else { Vec::new() })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const HW: &str = include_str!("../tests/fixtures/hardware-ports.txt");
    pub const ONE: &str = include_str!("../tests/fixtures/profiler-one-cable.json");
    pub const TWO: &str = include_str!("../tests/fixtures/profiler-two-cables.json");
    pub const SAME: &str = include_str!("../tests/fixtures/profiler-two-identical.json");
    pub const PEER_TWO: &str = include_str!("../tests/fixtures/profiler-peer-two-cables.json");
    /// The peer Mac's ports, as a Mac of the same model names them.
    pub const PEER_HW: &str = "Hardware Port: Thunderbolt 1\nDevice: en1\n\nHardware Port: Thunderbolt 2\nDevice: en6\n\nHardware Port: Thunderbolt 3\nDevice: en2\n";

    #[test]
    fn hardware_ports_from_the_real_output() {
        assert_eq!(parse_hardware_ports(HW), vec![("en1".into(), 1), ("en6".into(), 2), ("en2".into(), 3)]);
    }

    #[test]
    fn hardware_ports_skip_the_bridge_and_other_ports() {
        let out = "Hardware Port: Thunderbolt Bridge\nDevice: bridge0\n\nHardware Port: Wi-Fi\nDevice: en0\n";
        assert!(parse_hardware_ports(out).is_empty());
        assert!(parse_hardware_ports("").is_empty());
    }

    #[test]
    fn hardware_port_without_a_device_line_is_dropped() {
        let out = "Hardware Port: Thunderbolt 1\nEthernet Address: x\n\nHardware Port: Wi-Fi\nDevice: en0\n";
        assert!(parse_hardware_ports(out).is_empty());
    }

    #[test]
    fn speeds() {
        assert_eq!(parse_speed("40 Gb/s"), Some(40));
        assert_eq!(parse_speed("80 Gb/s"), Some(80));
        assert_eq!(parse_speed("Up to 120 Gb/s"), Some(120));
        assert_eq!(parse_speed("20.625 Gb/s"), Some(21));
        assert_eq!(parse_speed("0 Gb/s"), None);
        assert_eq!(parse_speed("fast"), None);
        assert_eq!(parse_speed(""), None);
    }

    #[test]
    fn profiler_one_cable_from_the_real_output() {
        let r = parse_profiler(ONE);
        assert_eq!(r.iter().map(|r| r.id).collect::<Vec<_>>(), vec![1, 2, 3]);
        // Receptacle 1: empty TB5 port. Its "Up to 120 Gb/s" is not a link speed.
        assert!(!r[0].connected);
        assert_eq!(r[0].speed_gbps, None);
        assert_eq!(r[0].peer, None);
        // Receptacle 2: a USB4 display, not a Mac.
        assert!(r[1].connected);
        assert_eq!(r[1].speed_gbps, Some(40));
        assert_eq!(r[1].peer, None);
        assert_eq!(r[1].cable_id(), None);
        // Receptacle 3: the other Mac at 40 Gb/s.
        assert!(r[2].connected);
        assert_eq!(r[2].speed_gbps, Some(40));
        assert_eq!(r[2].peer.as_ref().unwrap().name, "MacBook Pro");
        assert_eq!(r[2].cable_id().unwrap(), "AAAAAAAA-0000-4000-8000-000000000003+BBBBBBBB-0000-4000-8000-000000000003");
    }

    #[test]
    fn mapping_rule_on_the_real_mac() {
        // en2 is the port with the live link (see ifconfig-en2.txt), and it maps to receptacle 3, where the Mac is.
        let hw = parse_hardware_ports(HW);
        let r = parse_profiler(ONE);
        let en2 = port_info("en2", &hw, &r);
        assert_eq!(en2.receptacle, Some(3));
        assert_eq!(en2.peer.as_deref(), Some("MacBook Pro"));
        assert_eq!(en2.speed_gbps, Some(40));
        let en6 = port_info("en6", &hw, &r);
        assert_eq!((en6.receptacle, en6.peer), (Some(2), None));
        let en1 = port_info("en1", &hw, &r);
        assert_eq!((en1.receptacle, en1.speed_gbps, en1.peer), (Some(1), None, None));
    }

    #[test]
    fn unknown_device_has_no_info() {
        assert_eq!(port_info("en9", &parse_hardware_ports(HW), &parse_profiler(ONE)), PortInfo::default());
        let only_number = port_info("en1", &parse_hardware_ports(HW), &[]);
        assert_eq!(only_number, PortInfo { receptacle: Some(1), ..PortInfo::default() });
    }

    #[test]
    fn both_macs_name_each_cable_the_same() {
        let (hw_b, r_b) = (parse_hardware_ports(HW), parse_profiler(TWO));
        let (hw_a, r_a) = (parse_hardware_ports(PEER_HW), parse_profiler(PEER_TWO));
        // TB5 cable: this Mac's en1 (receptacle 1), the peer's en6 (receptacle 2).
        let b1 = port_info("en1", &hw_b, &r_b);
        let a6 = port_info("en6", &hw_a, &r_a);
        assert_eq!(b1.speed_gbps, Some(80));
        assert_eq!(b1.speed_gbps, a6.speed_gbps);
        assert_eq!(b1.cable_id, a6.cable_id);
        // TB4 cable: this Mac's en2 (receptacle 3), the peer's en1 (receptacle 1).
        let b2 = port_info("en2", &hw_b, &r_b);
        let a1 = port_info("en1", &hw_a, &r_a);
        assert_eq!((b2.speed_gbps, a1.speed_gbps), (Some(40), Some(40)));
        assert_eq!(b2.cable_id, a1.cable_id);
        assert_ne!(b1.cable_id, b2.cable_id);
    }

    #[test]
    fn identical_cables_still_have_distinct_ids() {
        let r = parse_profiler(SAME);
        let ids: Vec<_> = r.iter().filter_map(|r| r.cable_id()).collect();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0], ids[1]);
        assert!(r.iter().filter(|r| r.peer.is_some()).all(|r| r.speed_gbps == Some(40)));
    }

    #[test]
    fn cable_id_ignores_case_and_side() {
        let a = Receptacle {
            id: 1,
            connected: true,
            speed_gbps: Some(40),
            domain: "aa".into(),
            peer: Some(RemoteMac { name: "m".into(), domain: "BB".into() }),
        };
        let b = Receptacle { domain: "bb".into(), peer: Some(RemoteMac { name: "m".into(), domain: "AA".into() }), ..a.clone() };
        assert_eq!(a.cable_id(), b.cable_id());
        assert_eq!(a.cable_id().unwrap(), "AA+BB");
    }

    #[test]
    fn cable_id_needs_both_domains() {
        let r = Receptacle {
            id: 1,
            connected: true,
            speed_gbps: None,
            domain: "".into(),
            peer: Some(RemoteMac { name: "m".into(), domain: "B".into() }),
        };
        assert_eq!(r.cable_id(), None);
        let r = Receptacle { domain: "A".into(), peer: Some(RemoteMac { name: "m".into(), domain: "".into() }), ..r };
        assert_eq!(r.cable_id(), None);
    }

    #[test]
    fn profiler_garbage_is_empty() {
        assert!(parse_profiler("").is_empty());
        assert!(parse_profiler("{}").is_empty());
        assert!(parse_profiler(r#"{"SPThunderboltDataType":[{"_name":"bus"}]}"#).is_empty());
        assert!(parse_profiler(r#"{"SPThunderboltDataType":[{"receptacle_1_tag":{"receptacle_id_key":"x"}}]}"#).is_empty());
    }

    #[test]
    fn disconnected_receptacle_reports_no_peer_even_if_items_linger() {
        let j = r#"{"SPThunderboltDataType":[{"domain_uuid_key":"A","receptacle_1_tag":{"receptacle_id_key":"4","receptacle_status_key":"receptacle_no_devices_connected","current_speed_key":"Up to 80 Gb/s"},"_items":[{"_name":"Mac","domain_uuid_key":"B","services_title":[{"_name":"service_ip"}]}]}]}"#;
        let r = parse_profiler(j);
        assert_eq!(r.len(), 1);
        assert_eq!((r[0].id, r[0].connected, r[0].speed_gbps, r[0].peer.clone()), (4, false, None, None));
    }

    #[test]
    fn a_device_without_ip_service_is_not_a_peer() {
        let j = r#"{"SPThunderboltDataType":[{"domain_uuid_key":"A","receptacle_1_tag":{"receptacle_id_key":"1","receptacle_status_key":"receptacle_connected","current_speed_key":"40 Gb/s"},"_items":[{"_name":"Dock","domain_uuid_key":"B"}]}]}"#;
        assert_eq!(parse_profiler(j)[0].peer, None);
    }
}
