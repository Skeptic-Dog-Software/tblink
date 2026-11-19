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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

/// The `interface:` line of `route -n get` output.
pub fn parse_route_interface(out: &str) -> Option<String> {
    out.lines()
        .find_map(|l| l.trim().strip_prefix("interface:").map(|s| s.trim().to_string()))
}

pub fn ifconfig(name: &str) -> String {
    run(IFCONFIG, &[name]).1
}

/// Assemble a `Status` from the raw tool output. `ifconfig_of` returns one
/// interface's `ifconfig` output. Pure, so it can be tested on fixed text.
pub fn build_status(hostname: &str, ibv_out: &str, bridge_out: &str, ifconfig_of: &dyn Fn(&str) -> String) -> Status {
    let members = parse_members(bridge_out);
    let mut ports: Vec<Port> = parse_rdma_devices(ibv_out)
        .into_iter()
        .map(|dev| {
            let name = dev.trim_start_matches("rdma_").to_string();
            let out = ifconfig_of(&name);
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
        hostname: hostname.trim().to_string(),
        active,
        ports,
        bridge_inet: parse_inet(bridge_out),
        bridge_members: members,
    }
}

pub fn status() -> Status {
    let bridge = ifconfig(BRIDGE);
    let ibv = run(IBV_DEVICES, &[]).1;
    let hostname = run("/bin/hostname", &[]).1;
    build_status(&hostname, &ibv, &bridge, &|name| ifconfig(name))
}

/// Interface the kernel would use to reach `addr` (`route -n get`).
pub fn route_interface(addr: &str) -> Option<String> {
    let (ok, out) = run(ROUTE, &["-n", "get", addr]);
    if !ok {
        return None;
    }
    parse_route_interface(&out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const EN6: &str = "en6: flags=8963<UP,BROADCAST> mtu 1500\n\toptions=460<TSO4>\n\tether 36:c4:47:75:9e:84\n\tinet 10.0.1.1 netmask 0xffffff00 broadcast 10.0.1.255\n\tinet 10.77.0.1 netmask 0xffffff00 broadcast 10.77.0.255\n\tmedia: autoselect <full-duplex>\n\tstatus: active\n";
    const BR: &str = "bridge0: flags=8863<UP>\n\tether 02:00:00:00:00:00\n\tinet 10.77.0.1 netmask 0xffffff00 broadcast 10.77.0.255\n\tmember: en1 flags=3<LEARNING,DISCOVER>\n\tmember: en2 flags=3<LEARNING,DISCOVER>\n\tstatus: inactive\n";
    const IBV: &str = "    device          \t   node GUID\n    ------          \t----------------\n    rdma_en1        \ta067dd5dad30ac05\n    rdma_en6        \ta167dd5dad30ac05\n";

    fn port(name: &str, active: bool, member: bool, inet: &[&str]) -> Port {
        Port { name: name.into(), rdma_device: format!("rdma_{name}"), active, bridge_member: member, inet: inet.iter().map(|s| s.to_string()).collect() }
    }

    // ---- parse_inet ----

    #[test]
    fn parses_interface() {
        assert_eq!(parse_inet(EN6), vec!["10.0.1.1", "10.77.0.1"]);
        assert!(parse_active(EN6));
        assert!(!parse_active(BR));
        assert_eq!(parse_members(BR), vec!["en1", "en2"]);
        assert_eq!(parse_inet(BR), vec!["10.77.0.1"]);
    }

    #[test]
    fn inet_ignores_inet6_and_other_lines() {
        let out = "en1: flags=8863<UP>\n\tinet6 fe80::1%en1 prefixlen 64 scopeid 0x5\n\tether 36:c4:47:75:9e:80\n\tinet 10.0.1.2 netmask 0xffffff00\n\tstatus: active\n";
        assert_eq!(parse_inet(out), vec!["10.0.1.2"]);
    }

    #[test]
    fn inet_of_an_unaddressed_interface_is_empty() {
        assert!(parse_inet("en2: flags=8863<UP>\n\tether 36:c4:47:75:9e:88\n\tstatus: inactive\n").is_empty());
        assert!(parse_inet("").is_empty());
    }

    #[test]
    fn inet_requires_the_keyword_as_the_first_word() {
        // "inet" appearing later in a line, or as a prefix of another word, is not an address line.
        assert!(parse_inet("\toptions=460<inet>\n\tnd6 options=201<PERFORMNUD,inet>\n").is_empty());
        assert!(parse_inet("\tinetx 10.0.0.9\n").is_empty());
    }

    #[test]
    fn inet_keyword_alone_on_a_line_yields_nothing() {
        assert!(parse_inet("\tinet\n").is_empty());
    }

    #[test]
    fn inet_keeps_order_and_duplicates() {
        // ifconfig shows each alias once; the parser must not reorder or dedupe what it sees.
        let out = "\tinet 10.77.0.2 netmask 0xffffff00\n\tinet 10.0.1.2 netmask 0xffffff00\n";
        assert_eq!(parse_inet(out), vec!["10.77.0.2", "10.0.1.2"]);
    }

    // ---- parse_active ----

    #[test]
    fn active_needs_the_exact_status_line() {
        assert!(parse_active("\tstatus: active\n"));
        assert!(parse_active("status: active"));
        assert!(parse_active("   status: active   \n"));
        assert!(!parse_active("\tstatus: inactive\n"));
        assert!(!parse_active("\tstatus: active-ish\n"));
        assert!(!parse_active("\tmedia: autoselect (status: active)\n"));
        assert!(!parse_active(""));
    }

    // ---- parse_members ----

    #[test]
    fn bridge_without_members() {
        assert!(parse_members("bridge0: flags=8863<UP>\n\tstatus: inactive\n").is_empty());
    }

    #[test]
    fn members_ignore_the_word_elsewhere() {
        assert!(parse_members("\tConfiguration: member: none\n").is_empty());
        assert_eq!(parse_members("\tmember: en6 flags=3<LEARNING>\n\t        ifmaxaddr 0 port 11\n"), vec!["en6"]);
    }

    #[test]
    fn member_keyword_alone_yields_nothing() {
        assert!(parse_members("\tmember:\n").is_empty());
    }

    // ---- parse_rdma_devices ----

    #[test]
    fn parses_ibv_devices() {
        assert_eq!(parse_rdma_devices(IBV), vec!["rdma_en1", "rdma_en6"]);
    }

    #[test]
    fn ibv_devices_with_rdma_disabled_is_empty() {
        let out = "    device          \t   node GUID\n    ------          \t----------------\n";
        assert!(parse_rdma_devices(out).is_empty());
        assert!(parse_rdma_devices("").is_empty());
    }

    #[test]
    fn ibv_devices_skips_non_rdma_rows() {
        let out = "    mlx5_0   abc\n    rdma_en2   def\n\n    garbage\n";
        assert_eq!(parse_rdma_devices(out), vec!["rdma_en2"]);
    }

    #[test]
    fn ibv_devices_keeps_tool_order() {
        let out = "    rdma_en6 a\n    rdma_en1 b\n    rdma_en2 c\n";
        assert_eq!(parse_rdma_devices(out), vec!["rdma_en6", "rdma_en1", "rdma_en2"]);
    }

    // ---- parse_route_interface ----

    #[test]
    fn route_interface_from_route_get() {
        let out = "   route to: 10.0.1.1\ndestination: 10.0.1.0\n       mask: 255.255.255.0\n  interface: en1\n      flags: <UP,DONE,CLONING>\n";
        assert_eq!(parse_route_interface(out), Some("en1".into()));
    }

    #[test]
    fn route_interface_missing_line() {
        assert_eq!(parse_route_interface("route: writing to routing socket: not in table\n"), None);
        assert_eq!(parse_route_interface(""), None);
    }

    #[test]
    fn route_interface_takes_the_first_interface_line() {
        assert_eq!(parse_route_interface("interface: en2\ninterface: en1\n"), Some("en2".into()));
    }

    #[test]
    fn route_interface_trims_whitespace() {
        assert_eq!(parse_route_interface("  interface:   bridge0  \n"), Some("bridge0".into()));
    }

    // ---- build_status ----

    fn ifc(map: HashMap<&'static str, &'static str>) -> impl Fn(&str) -> String {
        move |n: &str| map.get(n).copied().unwrap_or("").to_string()
    }

    #[test]
    fn status_marks_active_port_addresses_and_bridge_membership() {
        let en1 = "en1: flags\n\tstatus: inactive\n";
        let f = ifc(HashMap::from([("en1", en1), ("en6", EN6)]));
        let s = build_status("Mac-A.lan\n", IBV, BR, &f);
        assert_eq!(s.hostname, "Mac-A.lan");
        assert_eq!(s.ports, vec![port("en1", false, true, &[]), port("en6", true, false, &["10.0.1.1", "10.77.0.1"])]);
        assert_eq!(s.active, Some(port("en6", true, false, &["10.0.1.1", "10.77.0.1"])));
        assert_eq!(s.bridge_inet, vec!["10.77.0.1"]);
        assert_eq!(s.bridge_members, vec!["en1", "en2"]);
    }

    #[test]
    fn status_sorts_ports_by_name() {
        let ibv = "    rdma_en6 a\n    rdma_en2 b\n    rdma_en1 c\n";
        let s = build_status("h", ibv, "", &|_| String::new());
        let names: Vec<_> = s.ports.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["en1", "en2", "en6"]);
    }

    #[test]
    fn status_with_no_active_port() {
        let s = build_status("h", IBV, BR, &|_| "\tstatus: inactive\n".into());
        assert_eq!(s.active, None);
        assert_eq!(s.ports.len(), 2);
    }

    #[test]
    fn status_with_no_rdma_devices() {
        let s = build_status("h", "", BR, &|_| EN6.into());
        assert!(s.ports.is_empty());
        assert_eq!(s.active, None);
        assert_eq!(s.bridge_members, vec!["en1", "en2"]);
    }

    #[test]
    fn two_active_ports_pick_the_first_by_name() {
        let s = build_status("h", "    rdma_en6 a\n    rdma_en1 b\n", "", &|_| "\tstatus: active\n".into());
        assert_eq!(s.active.unwrap().name, "en1");
    }

    #[test]
    fn status_only_queries_rdma_capable_ports() {
        use std::cell::RefCell;
        let asked = RefCell::new(Vec::new());
        build_status("h", IBV, BR, &|n| { asked.borrow_mut().push(n.to_string()); String::new() });
        assert_eq!(*asked.borrow(), vec!["en1", "en6"]);
    }

    // ---- Status ----

    #[test]
    fn active_has_checks_only_the_active_port() {
        let s = Status {
            hostname: "h".into(),
            active: Some(port("en6", true, false, &["10.0.1.1"])),
            ports: vec![port("en1", false, false, &["10.0.1.9"]), port("en6", true, false, &["10.0.1.1"])],
            bridge_inet: vec!["10.77.0.1".into()],
            bridge_members: vec![],
        };
        assert!(s.active_has("10.0.1.1"));
        assert!(!s.active_has("10.0.1.9"));
        assert!(!s.active_has("10.77.0.1"));
        assert!(!s.active_has("10.0.1"));
    }

    #[test]
    fn active_has_without_active_port_is_false() {
        let s = Status { hostname: "h".into(), active: None, ports: vec![], bridge_inet: vec![], bridge_members: vec![] };
        assert!(!s.active_has("10.0.1.1"));
    }

    #[test]
    fn status_json_round_trips() {
        // The coordinator reads the peer's `tblink status --json` over SSH, so the
        // serialized shape is a wire format between two Macs.
        let f = ifc(HashMap::from([("en6", EN6)]));
        let s = build_status("Mac-B", IBV, BR, &f);
        let text = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Status>(&text).unwrap(), s);
    }

    #[test]
    fn status_json_field_names_are_stable() {
        let s = Status { hostname: "h".into(), active: Some(port("en1", true, false, &["10.0.1.2"])), ports: vec![], bridge_inet: vec![], bridge_members: vec![] };
        let v: serde_json::Value = serde_json::to_value(&s).unwrap();
        for k in ["hostname", "active", "ports", "bridge_inet", "bridge_members"] {
            assert!(v.get(k).is_some(), "missing {k}");
        }
        for k in ["name", "rdma_device", "active", "bridge_member", "inet"] {
            assert!(v["active"].get(k).is_some(), "missing active.{k}");
        }
        assert_eq!(v["active"]["rdma_device"], "rdma_en1");
    }

    #[test]
    fn status_json_with_null_active_parses() {
        let s: Status = serde_json::from_str(r#"{"hostname":"h","active":null,"ports":[],"bridge_inet":[],"bridge_members":[]}"#).unwrap();
        assert_eq!(s.active, None);
    }

    #[test]
    fn run_reports_a_missing_program_as_failure() {
        let (ok, out) = run("/nonexistent/tblink-test-binary", &[]);
        assert!(!ok);
        assert!(!out.is_empty());
    }

    #[test]
    fn run_captures_stdout_stderr_and_exit_status() {
        let (ok, out) = run("/bin/sh", &["-c", "echo out; echo err 1>&2; exit 3"]);
        assert!(!ok);
        assert!(out.contains("out") && out.contains("err"));
        let (ok, out) = run("/bin/sh", &["-c", "echo fine"]);
        assert!(ok);
        assert_eq!(out.trim(), "fine");
    }
}
