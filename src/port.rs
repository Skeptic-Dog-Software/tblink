//! `tblink port`: the per-Mac root daemon.
//!
//! Each tick it makes the link look the way the JACCL backend needs it:
//!   1. the active RDMA-capable Thunderbolt port carries this Mac's RDMA
//!      address (10.0.1.N/24) and, optionally, its old bridge address
//!      (10.77.0.N/24) so SSH, Screen Sharing and API clients keep working;
//!   2. no inactive port still holds either address (a stale copy owns the
//!      subnet route and silently breaks the link);
//!   3. the active port is not a bridge0 member, and bridge0 does not hold
//!      the bridge address while its members are gone;
//!   4. the subnet routes to the peer go out the active port.
//! It only ever adds or removes these two addresses, removes the active port
//! from bridge0, and fixes those two routes.

use crate::net::{self, Status, BRIDGE, IFCONFIG, ROUTE};
use crate::{log, sleep_secs};

pub struct PortArgs {
    pub rdma_ip: String,
    pub bridge_ip: Option<String>,
    pub peer_ips: Vec<String>,
    pub interval: u64,
    pub once: bool,
    pub dry_run: bool,
}

/// One planned change, as the command that makes it.
#[derive(Debug, PartialEq)]
pub struct Action {
    pub cmd: &'static str,
    pub args: Vec<String>,
    pub why: String,
}

fn act(cmd: &'static str, args: &[&str], why: String) -> Action {
    Action { cmd, args: args.iter().map(|s| s.to_string()).collect(), why }
}

fn subnet24(addr: &str) -> String {
    let mut o: Vec<&str> = addr.split('.').take(3).collect();
    o.push("0");
    format!("{}/24", o.join("."))
}

/// Pure planning step, so it can be unit-tested against fixed snapshots.
pub fn plan(st: &Status, a: &PortArgs, routes: &dyn Fn(&str) -> Option<String>) -> Vec<Action> {
    let mut out = Vec::new();
    let Some(active) = st.active.as_ref() else { return out };
    let ours: Vec<&str> = std::iter::once(a.rdma_ip.as_str()).chain(a.bridge_ip.as_deref()).collect();

    for p in st.ports.iter().filter(|p| p.name != active.name) {
        for addr in &ours {
            if p.inet.iter().any(|x| x == addr) {
                out.push(act(IFCONFIG, &[&p.name, "inet", addr, "-alias"], format!("stale {addr} on inactive {}", p.name)));
            }
        }
    }
    if active.bridge_member {
        out.push(act(IFCONFIG, &[BRIDGE, "deletem", &active.name], format!("take {} out of {BRIDGE}", active.name)));
    }
    if let Some(b) = a.bridge_ip.as_deref() {
        if st.bridge_inet.iter().any(|x| x == b) {
            out.push(act(IFCONFIG, &[BRIDGE, "inet", b, "-alias"], format!("{b} moves from {BRIDGE} to {}", active.name)));
        }
    }
    let mut added_nets = Vec::new();
    for addr in &ours {
        if !active.inet.iter().any(|x| x == addr) {
            out.push(act(IFCONFIG, &[&active.name, "inet", addr, "netmask", "255.255.255.0", "alias"], format!("add {addr} to {}", active.name)));
            added_nets.push(subnet24(addr));
        }
    }
    // Adding an address installs its subnet route; judge routes on the next tick.
    for peer in a.peer_ips.iter().filter(|p| !added_nets.contains(&subnet24(p))) {
        match routes(peer) {
            Some(ifc) if ifc == active.name => {}
            other => {
                let net = subnet24(peer);
                if other.is_some() {
                    out.push(act(ROUTE, &["-n", "delete", "-net", &net], format!("route to {peer} uses {}", other.as_deref().unwrap_or("?"))));
                }
                out.push(act(ROUTE, &["-n", "add", "-net", &net, "-interface", &active.name], format!("route {net} via {}", active.name)));
            }
        }
    }
    out
}

pub fn main(a: PortArgs) -> i32 {
    if !a.dry_run && unsafe_geteuid() != 0 {
        eprintln!("tblink port must run as root (or pass --dry-run)");
        return 1;
    }
    let mut last_active: Option<String> = None;
    loop {
        let st = net::status();
        let now_active = st.active.as_ref().map(|p| p.name.clone());
        if now_active != last_active {
            match &now_active {
                Some(n) => log(&format!("active Thunderbolt RDMA port: {n}")),
                None => log("no active Thunderbolt RDMA port (cable unplugged?)"),
            }
            last_active = now_active;
        }
        // Planned against a fresh route view: route state can lag the ifconfig change.
        let actions = plan(&st, &a, &|p| net::route_interface(p));
        for x in &actions {
            if a.dry_run {
                log(&format!("would run: {} {}   ({})", x.cmd, x.args.join(" "), x.why));
                continue;
            }
            let args: Vec<&str> = x.args.iter().map(String::as_str).collect();
            let (ok, out) = net::run(x.cmd, &args);
            log(&format!("{} {}: {}  ({}){}", x.cmd, x.args.join(" "), if ok { "ok" } else { "FAILED" }, x.why,
                if ok { String::new() } else { format!(": {}", out.trim()) }));
        }
        if a.once {
            return 0;
        }
        sleep_secs(a.interval);
    }
}

fn unsafe_geteuid() -> u32 {
    extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::Port;

    fn port(name: &str, active: bool, member: bool, inet: &[&str]) -> Port {
        Port { name: name.into(), rdma_device: format!("rdma_{name}"), active, bridge_member: member, inet: inet.iter().map(|s| s.to_string()).collect() }
    }
    fn args() -> PortArgs {
        PortArgs { rdma_ip: "10.0.1.1".into(), bridge_ip: Some("10.77.0.1".into()), peer_ips: vec!["10.0.1.2".into()], interval: 5, once: true, dry_run: true }
    }
    fn st(ports: Vec<Port>, bridge_inet: &[&str]) -> Status {
        Status { hostname: "t".into(), active: ports.iter().find(|p| p.active).cloned(), bridge_members: ports.iter().filter(|p| p.bridge_member).map(|p| p.name.clone()).collect(), ports, bridge_inet: bridge_inet.iter().map(|s| s.to_string()).collect() }
    }
    fn cmds(v: &[Action]) -> Vec<String> {
        v.iter().map(|a| a.args.join(" ")).collect()
    }

    #[test]
    fn cable_moved_after_link_setup() {
        // Observed after a cable move: address left on en1, cable now on en6, en6 in the bridge.
        let s = st(vec![port("en1", false, true, &["10.0.1.1"]), port("en6", true, true, &[])], &["10.77.0.1"]);
        let v = plan(&s, &args(), &|_| Some("en1".into()));
        assert_eq!(cmds(&v), vec![
            "en1 inet 10.0.1.1 -alias",
            "bridge0 deletem en6",
            "bridge0 inet 10.77.0.1 -alias",
            "en6 inet 10.0.1.1 netmask 255.255.255.0 alias",
            "en6 inet 10.77.0.1 netmask 255.255.255.0 alias",
        ]);
    }

    #[test]
    fn stale_route_is_replaced() {
        let s = st(vec![port("en1", false, true, &[]), port("en6", true, false, &["10.0.1.1", "10.77.0.1"])], &[]);
        assert_eq!(cmds(&plan(&s, &args(), &|_| Some("en1".into()))), vec![
            "-n delete -net 10.0.1.0/24",
            "-n add -net 10.0.1.0/24 -interface en6",
        ]);
    }

    #[test]
    fn settled_link_is_left_alone() {
        let s = st(vec![port("en1", false, true, &[]), port("en6", true, false, &["10.0.1.1", "10.77.0.1"])], &[]);
        assert!(plan(&s, &args(), &|_| Some("en6".into())).is_empty());
    }

    #[test]
    fn unplugged_does_nothing() {
        let s = st(vec![port("en1", false, true, &["10.0.1.1"]), port("en6", false, false, &[])], &[]);
        assert!(plan(&s, &args(), &|_| None).is_empty());
    }

    #[test]
    fn missing_route_is_added_without_delete() {
        let s = st(vec![port("en6", true, false, &["10.0.1.1", "10.77.0.1"])], &[]);
        assert_eq!(cmds(&plan(&s, &args(), &|_| None)), vec!["-n add -net 10.0.1.0/24 -interface en6"]);
    }
}
