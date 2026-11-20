//! Read-only view of the Thunderbolt ports, plus the few ifconfig/route
//! changes the port daemon makes. Everything shells out to the stock macOS
//! tools by absolute path, so the binary has no PATH dependence under launchd.

use serde::{Deserialize, Serialize};
use std::process::Command;

pub const IFCONFIG: &str = "/sbin/ifconfig";
pub const ROUTE: &str = "/sbin/route";
pub const IBV_DEVICES: &str = "/usr/bin/ibv_devices";
pub const BRIDGE: &str = "bridge0";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Port {
    pub name: String,
    pub rdma_device: String,
    pub active: bool,
    pub bridge_member: bool,
    pub inet: Vec<String>,
    /// The Thunderbolt receptacle ("Thunderbolt N" is receptacle N).
    #[serde(default)]
    pub receptacle: Option<u32>,
    /// Negotiated Thunderbolt speed while a cable is in.
    #[serde(default)]
    pub speed_gbps: Option<u32>,
    /// The Mac on the other end of the cable, if one is there.
    #[serde(default)]
    pub peer: Option<String>,
    /// The same on both Macs for the same cable (see `tb`).
    #[serde(default)]
    pub cable_id: Option<String>,
    /// What this port carries now, judged by this Mac's addresses on it.
    /// `None` when the port daemon has not said which addresses are ours.
    #[serde(default)]
    pub role: Option<Role>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum Role {
    #[serde(rename = "inference")]
    Inference,
    #[serde(rename = "management")]
    Management,
    #[serde(rename = "inference+management")]
    Both,
    #[serde(rename = "unused")]
    Unused,
}

impl Role {
    pub fn name(&self) -> &'static str {
        match self {
            Role::Inference => "inference",
            Role::Management => "management",
            Role::Both => "inference+management",
            Role::Unused => "unused",
        }
    }
}

/// What the port daemon last decided. It writes this to `STATE_PATH` every
/// tick; `tblink status` reads it back.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct DaemonState {
    pub version: String,
    pub rdma_ip: String,
    #[serde(default)]
    pub bridge_ip: Option<String>,
    #[serde(default)]
    pub peer_ips: Vec<String>,
    pub layout: crate::roles::Layout,
    #[serde(default)]
    pub adapt: Option<crate::roles::Adapt>,
    /// A better layout waiting out its settle time, and the seconds left.
    #[serde(default)]
    pub pending: Option<(crate::roles::Layout, u64)>,
    #[serde(default)]
    pub probe: crate::roles::Probe,
    #[serde(default)]
    pub leader: bool,
    /// Unix time of the write.
    pub updated: u64,
    /// Seconds since `updated`, filled in by the reader.
    #[serde(default)]
    pub age_secs: Option<u64>,
}

/// Where the port daemon writes its `DaemonState`. Cleared at boot.
pub const STATE_PATH: &str = "/var/run/tblink.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Status {
    pub hostname: String,
    /// The active RDMA-capable Thunderbolt port that carries the RDMA address
    /// (with no port daemon state: the first active port by name).
    pub active: Option<Port>,
    pub ports: Vec<Port>,
    pub bridge_inet: Vec<String>,
    pub bridge_members: Vec<String>,
    /// The tblink version that produced this status ("" from tblink 0.1).
    #[serde(default)]
    pub version: String,
    /// This Mac's oMLX node id, from ~/.omlx/cluster/identity.json.
    #[serde(default)]
    pub node_id: Option<String>,
    /// The port daemon's last decision, if it is running a version that writes one.
    #[serde(default)]
    pub daemon: Option<DaemonState>,
}

impl Status {
    #[cfg(test)]
    pub fn active_has(&self, addr: &str) -> bool {
        self.active.as_ref().map(|p| p.inet.iter().any(|a| a == addr)).unwrap_or(false)
    }
    /// The active port holding `addr`.
    pub fn port_with(&self, addr: &str) -> Option<&Port> {
        self.ports.iter().find(|p| p.active && p.inet.iter().any(|a| a == addr))
    }
}

/// This build's version, e.g. `0.2.0 (d979c9f)`.
pub fn version() -> String {
    match option_env!("TBLINK_GIT") {
        Some(g) if !g.is_empty() => format!("{} ({g})", env!("CARGO_PKG_VERSION")),
        _ => env!("CARGO_PKG_VERSION").to_string(),
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
    out.lines().filter_map(|l| l.split_whitespace().next()).filter(|w| w.starts_with("rdma_")).map(str::to_string).collect()
}

/// The `interface:` line of `route -n get` output.
pub fn parse_route_interface(out: &str) -> Option<String> {
    out.lines().find_map(|l| l.trim().strip_prefix("interface:").map(|s| s.trim().to_string()))
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
                ..Port::default()
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
        ..Status::default()
    }
}

/// Add what `system_profiler` knows about each port, the roles the port
/// daemon's addresses give it, and pick `active` by the RDMA address.
pub fn annotate(st: &mut Status, hardware_ports: &[(String, u32)], receptacles: &[crate::tb::Receptacle], daemon: Option<&DaemonState>) {
    for p in &mut st.ports {
        let info = crate::tb::port_info(&p.name, hardware_ports, receptacles);
        p.receptacle = info.receptacle;
        p.speed_gbps = info.speed_gbps;
        p.peer = info.peer;
        p.cable_id = info.cable_id;
        p.role = daemon.map(|d| {
            let rdma = p.inet.contains(&d.rdma_ip);
            let bridge = d.bridge_ip.as_ref().is_some_and(|b| p.inet.contains(b));
            match (rdma, bridge) {
                (true, true) => Role::Both,
                (true, false) => Role::Inference,
                (false, true) => Role::Management,
                (false, false) => Role::Unused,
            }
        });
    }
    if let Some(d) = daemon {
        if let Some(p) = st.port_with(&d.rdma_ip).cloned() {
            st.active = Some(p);
        }
    }
    if let Some(a) = st.active.as_ref().map(|a| a.name.clone()) {
        st.active = st.ports.iter().find(|p| p.name == a).cloned();
    }
    st.daemon = daemon.cloned();
}

/// The usable links for role selection: active RDMA-capable ports.
pub fn links(st: &Status) -> Vec<crate::roles::Link> {
    st.ports
        .iter()
        .filter(|p| p.active)
        .map(|p| crate::roles::Link { port: p.name.clone(), speed_gbps: p.speed_gbps, cable_id: p.cable_id.clone() })
        .collect()
}

/// Parse the port daemon's state file, filling in its age.
pub fn parse_daemon_state(text: &str, now_unix: u64) -> Option<DaemonState> {
    let mut d: DaemonState = serde_json::from_str(text).ok()?;
    d.age_secs = Some(now_unix.saturating_sub(d.updated));
    Some(d)
}

/// The `node_id` in oMLX's identity.json text.
pub fn parse_node_id(text: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    v["node_id"].as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn read_status(daemon: Option<&DaemonState>) -> Status {
    let bridge = ifconfig(BRIDGE);
    let ibv = run(IBV_DEVICES, &[]).1;
    let hostname = run("/bin/hostname", &[]).1;
    let mut st = build_status(&hostname, &ibv, &bridge, &|name| ifconfig(name));
    let (hw, rec) = crate::tb::read();
    annotate(&mut st, &hw, &rec, daemon);
    st.version = version();
    st
}

/// The ports with addresses and Thunderbolt details, without the daemon state (the port daemon's own view).
pub fn port_status() -> Status {
    read_status(None)
}

/// The port daemon's last state, if it wrote one.
pub fn daemon_state() -> Option<DaemonState> {
    std::fs::read_to_string(STATE_PATH).ok().and_then(|t| parse_daemon_state(&t, unix_now()))
}

/// Everything `tblink status` shows.
pub fn status() -> Status {
    let mut st = read_status(daemon_state().as_ref());
    st.node_id = std::fs::read_to_string(crate::coord::expand(IDENTITY)).ok().and_then(|t| parse_node_id(&t));
    st
}

/// One line on what the port daemon decided, for people.
pub fn describe_daemon(d: &DaemonState) -> String {
    let mut line = d.layout.describe();
    match &d.adapt {
        Some(crate::roles::Adapt::Fallback { .. }) => line.push_str(" (following the other Mac, which uses one cable for both)"),
        Some(crate::roles::Adapt::Swapped { .. }) => line.push_str(" (swapped to match the other Mac)"),
        None => {}
    }
    if let Some((l, secs)) = &d.pending {
        line.push_str(&format!("; {} in {secs} s if the link stays up", l.describe()));
    }
    line
}

/// `tblink status` for people.
pub fn render(st: &Status) -> String {
    let mut out = format!("{}  tblink {}", st.hostname, if st.version.is_empty() { "?" } else { &st.version });
    if let Some(n) = &st.node_id {
        out.push_str(&format!("  oMLX node {n}"));
    }
    out.push('\n');
    match &st.daemon {
        Some(d) => out.push_str(&format!(
            "roles: {}  [port daemon {}, {} s ago]\n",
            describe_daemon(d),
            if d.leader { "leads" } else { "follows" },
            d.age_secs.unwrap_or(0)
        )),
        None => out.push_str("roles: unknown (no port daemon state; is `tblink port` 0.2 or later running?)\n"),
    }
    let rows: Vec<[String; 6]> = st
        .ports
        .iter()
        .map(|p| {
            [
                p.name.clone(),
                p.receptacle.map(|r| r.to_string()).unwrap_or("-".into()),
                if p.active { p.speed_gbps.map(|s| format!("{s} Gb/s")).unwrap_or("up".into()) } else { "down".into() },
                p.peer.clone().unwrap_or("-".into()),
                p.role.map(|r| r.name().to_string()).unwrap_or("?".into()),
                if p.inet.is_empty() { "-".into() } else { p.inet.join(" ") },
            ]
        })
        .collect();
    let head = ["PORT", "RECEPTACLE", "LINK", "OTHER END", "ROLE", "ADDRESSES"].map(String::from);
    let mut w = [0usize; 6];
    for r in std::iter::once(&head).chain(rows.iter()) {
        for (i, c) in r.iter().enumerate() {
            w[i] = w[i].max(c.chars().count());
        }
    }
    for r in std::iter::once(&head).chain(rows.iter()) {
        let cells: Vec<String> = r.iter().enumerate().map(|(i, c)| format!("{c:<width$}", width = w[i])).collect();
        out.push_str(cells.join("  ").trim_end());
        out.push('\n');
    }
    if st.ports.is_empty() {
        out.push_str("no RDMA-capable Thunderbolt ports (is RDMA enabled? `rdma_ctl status`)\n");
    }
    out.push_str(&format!(
        "{BRIDGE}: members {}; addresses {}\n",
        if st.bridge_members.is_empty() { "-".into() } else { st.bridge_members.join(" ") },
        if st.bridge_inet.is_empty() { "-".into() } else { st.bridge_inet.join(" ") }
    ));
    out
}

/// oMLX's node identity file.
pub const IDENTITY: &str = "~/.omlx/cluster/identity.json";

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
        Port {
            name: name.into(),
            rdma_device: format!("rdma_{name}"),
            active,
            bridge_member: member,
            inet: inet.iter().map(|s| s.to_string()).collect(),
            ..Port::default()
        }
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
        let out =
            "   route to: 10.0.1.1\ndestination: 10.0.1.0\n       mask: 255.255.255.0\n  interface: en1\n      flags: <UP,DONE,CLONING>\n";
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
        build_status("h", IBV, BR, &|n| {
            asked.borrow_mut().push(n.to_string());
            String::new()
        });
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
            ..Status::default()
        };
        assert!(s.active_has("10.0.1.1"));
        assert!(!s.active_has("10.0.1.9"));
        assert!(!s.active_has("10.77.0.1"));
        assert!(!s.active_has("10.0.1"));
    }

    #[test]
    fn active_has_without_active_port_is_false() {
        let s =
            Status { hostname: "h".into(), active: None, ports: vec![], bridge_inet: vec![], bridge_members: vec![], ..Status::default() };
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
        let s = Status {
            hostname: "h".into(),
            active: Some(port("en1", true, false, &["10.0.1.2"])),
            ports: vec![],
            bridge_inet: vec![],
            bridge_members: vec![],
            ..Status::default()
        };
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

    // ---- the real Mac (fixtures captured read-only from it) ----

    fn fixture(name: &str) -> String {
        std::fs::read_to_string(format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }
    fn real_status(profiler: &str, daemon: Option<&DaemonState>) -> Status {
        let ibv = fixture("ibv-devices.txt");
        let bridge = fixture("ifconfig-bridge0.txt");
        let mut st = build_status("Mac-B", &ibv, &bridge, &|n| {
            std::fs::read_to_string(format!("{}/tests/fixtures/ifconfig-{n}.txt", env!("CARGO_MANIFEST_DIR"))).unwrap_or_default()
        });
        let hw = crate::tb::parse_hardware_ports(&fixture("hardware-ports.txt"));
        annotate(&mut st, &hw, &crate::tb::parse_profiler(&fixture(profiler)), daemon);
        st
    }
    fn state(rdma: &str, bridge: &str, layout: crate::roles::Layout) -> DaemonState {
        DaemonState {
            version: "v".into(),
            rdma_ip: rdma.into(),
            bridge_ip: Some(bridge.into()),
            layout,
            updated: 1,
            ..DaemonState::default()
        }
    }

    #[test]
    fn real_mac_one_cable() {
        let d = state("10.0.1.2", "10.77.0.2", crate::roles::Layout::Single { port: "en2".into() });
        let st = real_status("profiler-one-cable.json", Some(&d));
        let names: Vec<_> = st.ports.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["en1", "en2", "en6"]);
        let en2 = &st.ports[1];
        assert!(en2.active);
        assert_eq!((en2.receptacle, en2.speed_gbps, en2.peer.as_deref()), (Some(3), Some(40), Some("MacBook Pro")));
        assert_eq!(en2.role, Some(Role::Both));
        assert_eq!(st.ports[0].role, Some(Role::Unused));
        assert_eq!(st.active.as_ref().unwrap().name, "en2");
        assert_eq!(st.active.as_ref().unwrap().speed_gbps, Some(40), "active carries the annotations too");
        let l = links(&st);
        assert_eq!(l.len(), 1);
        assert_eq!(crate::roles::ideal(&l, true), crate::roles::Layout::Single { port: "en2".into() });
    }

    #[test]
    fn real_mac_without_daemon_state_has_unknown_roles() {
        let st = real_status("profiler-one-cable.json", None);
        assert!(st.ports.iter().all(|p| p.role.is_none()));
        assert_eq!(st.active.as_ref().unwrap().name, "en2");
        assert!(render(&st).contains("roles: unknown"));
    }

    #[test]
    fn active_is_the_port_with_the_rdma_address() {
        // en1 sorts first, but the RDMA address is on en6.
        let s = build_status("h", IBV, "", &|n| {
            if n == "en1" {
                "\tinet 10.77.0.1 netmask 0xffffff00\n\tstatus: active\n".into()
            } else {
                EN6.replace("\tinet 10.77.0.1 netmask 0xffffff00 broadcast 10.77.0.255\n", "")
            }
        });
        assert_eq!(s.active.as_ref().unwrap().name, "en1", "without state: first by name, as before");
        let mut s2 = s.clone();
        let d = state("10.0.1.1", "10.77.0.1", crate::roles::Layout::Split { inference: "en6".into(), management: "en1".into() });
        annotate(&mut s2, &[], &[], Some(&d));
        assert_eq!(s2.active.as_ref().unwrap().name, "en6");
        assert_eq!(s2.ports[0].role, Some(Role::Management));
        assert_eq!(s2.ports[1].role, Some(Role::Inference));
        assert_eq!(s2.port_with("10.77.0.1").unwrap().name, "en1");
        assert!(s2.port_with("10.0.1.9").is_none());
    }

    #[test]
    fn two_cables_rank_from_the_fixtures() {
        // Two cables up (TB5 on receptacle 1 = en1, TB4 on receptacle 3 = en2).
        let mut st = real_status("profiler-two-cables.json", None);
        st.ports[0].active = true;
        let l = links(&st);
        assert_eq!(crate::roles::ideal(&l, true), crate::roles::Layout::Split { inference: "en1".into(), management: "en2".into() });
        let mut same = real_status("profiler-two-identical.json", None);
        same.ports[0].active = true;
        let l = links(&same);
        assert!(crate::roles::tied(&l));
        assert!(matches!(crate::roles::ideal(&l, true), crate::roles::Layout::Split { .. }));
    }

    #[test]
    fn render_shows_speed_peer_and_role() {
        let mut d = state("10.0.1.2", "10.77.0.2", crate::roles::Layout::Single { port: "en2".into() });
        d.age_secs = Some(2);
        let mut st = real_status("profiler-one-cable.json", Some(&d));
        st.version = "0.2.0".into();
        let r = render(&st);
        assert!(r.starts_with("Mac-B  tblink 0.2.0\nroles: en2 carries inference and management  [port daemon follows, 2 s ago]\n"), "{r}");
        assert!(r.contains("en2   3           40 Gb/s  MacBook Pro  inference+management  10.0.1.2 10.77.0.2"), "{r}");
        assert!(r.contains("en6   2           down     -            unused"), "{r}");
        assert!(r.ends_with("bridge0: members -; addresses -\n"), "{r}");
    }

    #[test]
    fn render_mentions_fallback_and_pending() {
        let mut d = state("10.0.1.1", "10.77.0.1", crate::roles::Layout::Single { port: "en2".into() });
        d.adapt = Some(crate::roles::Adapt::Fallback {
            to: crate::roles::Layout::Single { port: "en2".into() },
            base: crate::roles::Layout::None,
        });
        assert!(describe_daemon(&d).contains("following the other Mac"));
        d.adapt = None;
        d.pending = Some((crate::roles::Layout::Split { inference: "en1".into(), management: "en2".into() }, 25));
        assert_eq!(
            describe_daemon(&d),
            "en2 carries inference and management; inference on en1 (dedicated), management on en2 in 25 s if the link stays up"
        );
    }

    #[test]
    fn daemon_state_parsing() {
        let d = state("10.0.1.1", "10.77.0.1", crate::roles::Layout::Single { port: "en2".into() });
        let text = serde_json::to_string(&d).unwrap();
        let back = parse_daemon_state(&text, 31).unwrap();
        assert_eq!(back.age_secs, Some(30));
        assert_eq!(back.layout, d.layout);
        assert_eq!(parse_daemon_state("{", 0), None);
        assert_eq!(parse_daemon_state(&text, 0).unwrap().age_secs, Some(0), "clock skew never goes negative");
    }

    #[test]
    fn node_id_from_identity() {
        assert_eq!(
            parse_node_id(r#"{"created_at": 1.5, "friendly_name": "Mac-B", "node_id": "26cb-x", "schema_version": 1}"#),
            Some("26cb-x".into())
        );
        assert_eq!(parse_node_id(r#"{"node_id": ""}"#), None);
        assert_eq!(parse_node_id("nope"), None);
    }

    #[test]
    fn new_status_fields_round_trip_and_old_ones_stay() {
        let d = state("10.0.1.2", "10.77.0.2", crate::roles::Layout::Single { port: "en2".into() });
        let mut st = real_status("profiler-one-cable.json", Some(&d));
        st.version = "0.2.0".into();
        st.node_id = Some("B".into());
        let v = serde_json::to_value(&st).unwrap();
        assert_eq!(v["ports"][1]["speed_gbps"], 40);
        assert_eq!(v["ports"][1]["role"], "inference+management");
        assert_eq!(v["ports"][1]["peer"], "MacBook Pro");
        assert_eq!(v["daemon"]["layout"]["mode"], "single");
        assert_eq!(serde_json::from_value::<Status>(v).unwrap(), st);
    }

    #[test]
    fn version_has_the_package_version() {
        assert!(version().starts_with(env!("CARGO_PKG_VERSION")));
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
