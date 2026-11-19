//! Read-only view of the Thunderbolt ports, plus the few ifconfig/route
//! changes the port daemon makes. Everything shells out to the stock macOS
//! tools by absolute path, so the binary has no PATH dependence under launchd.

use serde::{Deserialize, Serialize};
use std::process::Command;

pub const IFCONFIG: &str = "/sbin/ifconfig";
pub const ROUTE: &str = "/sbin/route";
pub const IBV_DEVICES: &str = "/usr/bin/ibv_devices";
pub const BRIDGE: &str = "bridge0";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Port {
    pub name: String,
    pub rdma_device: String,
    pub active: bool,
    pub bridge_member: bool,
    pub inet: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub hostname: String,
    /// The active RDMA-capable Thunderbolt port, if exactly one is chosen.
    pub active: Option<Port>,
    pub ports: Vec<Port>,
    pub bridge_inet: Vec<String>,
    pub bridge_members: Vec<String>,
}

impl Status {
    pub fn active_has(&self, addr: &str) -> bool {
        self.active
            .as_ref()
            .map(|p| p.inet.iter().any(|a| a == addr))
            .unwrap_or(false)
    }
}

pub fn run(cmd: &str, args: &[&str]) -> (bool, String) {
    match Command::new(cmd).args(args).output() {
        Ok(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.success(), s)
        }
        Err(e) => (false, e.to_string()),
    }
}

/// Parse `inet A.B.C.D` lines from one interface's ifconfig output.
pub fn parse_inet(ifconfig: &str) -> Vec<String> {
    ifconfig
        .lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            (w.next() == Some("inet")).then(|| w.next().map(str::to_string)).flatten()
        })
        .collect()
}

pub fn parse_active(ifconfig: &str) -> bool {
    ifconfig.lines().any(|l| l.trim() == "status: active")
}

pub fn parse_members(bridge_ifconfig: &str) -> Vec<String> {
    bridge_ifconfig
        .lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            (w.next() == Some("member:")).then(|| w.next().map(str::to_string)).flatten()
        })
        .collect()
}

/// `ibv_devices` rows look like `    rdma_en1     a067dd5dad30ac05`.
pub fn parse_rdma_devices(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|w| w.starts_with("rdma_"))
        .map(str::to_string)
        .collect()
}

pub fn ifconfig(name: &str) -> String {
    run(IFCONFIG, &[name]).1
}

pub fn status() -> Status {
    let bridge = ifconfig(BRIDGE);
    let members = parse_members(&bridge);
    let mut ports: Vec<Port> = parse_rdma_devices(&run(IBV_DEVICES, &[]).1)
        .into_iter()
        .map(|dev| {
            let name = dev.trim_start_matches("rdma_").to_string();
            let out = ifconfig(&name);
            Port {
                bridge_member: members.contains(&name),
                active: parse_active(&out),
                inet: parse_inet(&out),
                rdma_device: dev,
                name,
            }
        })
        .collect();
    ports.sort_by(|a, b| a.name.cmp(&b.name));
    let active = ports.iter().find(|p| p.active).cloned();
    Status {
        hostname: run("/bin/hostname", &[]).1.trim().to_string(),
        active,
        ports,
        bridge_inet: parse_inet(&bridge),
        bridge_members: members,
    }
}

/// Interface the kernel would use to reach `addr` (`route -n get`).
pub fn route_interface(addr: &str) -> Option<String> {
    let (ok, out) = run(ROUTE, &["-n", "get", addr]);
    if !ok {
        return None;
    }
    out.lines()
        .find_map(|l| l.trim().strip_prefix("interface:").map(|s| s.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const EN6: &str = "en6: flags=8963<UP,BROADCAST> mtu 1500\n\toptions=460<TSO4>\n\tether 36:c4:47:75:9e:84\n\tinet 10.0.1.1 netmask 0xffffff00 broadcast 10.0.1.255\n\tinet 10.77.0.1 netmask 0xffffff00 broadcast 10.77.0.255\n\tmedia: autoselect <full-duplex>\n\tstatus: active\n";
    const BR: &str = "bridge0: flags=8863<UP>\n\tether 02:00:00:00:00:00\n\tinet 10.77.0.1 netmask 0xffffff00 broadcast 10.77.0.255\n\tmember: en1 flags=3<LEARNING,DISCOVER>\n\tmember: en2 flags=3<LEARNING,DISCOVER>\n\tstatus: inactive\n";

    #[test]
    fn parses_interface() {
        assert_eq!(parse_inet(EN6), vec!["10.0.1.1", "10.77.0.1"]);
        assert!(parse_active(EN6));
        assert!(!parse_active(BR));
        assert_eq!(parse_members(BR), vec!["en1", "en2"]);
        assert_eq!(parse_inet(BR), vec!["10.77.0.1"]);
    }

    #[test]
    fn parses_ibv_devices() {
        let out = "    device          \t   node GUID\n    ------          \t----------------\n    rdma_en1        \ta067dd5dad30ac05\n    rdma_en6        \ta167dd5dad30ac05\n";
        assert_eq!(parse_rdma_devices(out), vec!["rdma_en1", "rdma_en6"]);
    }
}
