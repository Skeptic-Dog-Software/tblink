//! `tblink doctor`: check this Mac, the cables, the roles, the other Mac and
//! oMLX, and say what is wrong in plain words.
//!
//! Everything it looks at is read-only: `tblink status` here and on the other
//! Mac (over SSH, coordinator only), TCP connects to the other Mac's two
//! addresses bound to the expected ports, and oMLX's admin API.

use crate::coord::{self, Config};
use crate::net::{self, Role, Status};
use crate::port;
use crate::roles::{Adapt, Layout};
use serde_json::Value;

pub const PORT_PLIST: &str = "/Library/LaunchDaemons/software.skepticdog.tblink.port.plist";
pub const COORD_PLIST: &str = "~/Library/LaunchAgents/software.skepticdog.tblink.coordinator.plist";
pub const INSTALLED_BIN: &str = "/usr/local/bin/tblink";
/// A port daemon state older than this means the daemon is not running.
pub const STALE_SECS: u64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Note,
    Warn,
    Problem,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub level: Level,
    pub text: String,
}

fn f(level: Level, text: impl Into<String>) -> Finding {
    Finding { level, text: text.into() }
}

/// One reachability check of the other Mac.
#[derive(Debug, Clone, PartialEq)]
pub struct Reach {
    pub what: &'static str,
    pub addr: String,
    pub port: String,
    pub answered: bool,
}

/// The live deployment, as far as the coordinator's oMLX knows.
#[derive(Debug, Clone, PartialEq)]
pub struct Deployment {
    pub id: String,
    pub backend: String,
    pub live: bool,
}

/// Everything `diagnose` looks at, gathered up front so the judging is pure.
#[derive(Debug, Default)]
pub struct Facts {
    pub binary_version: String,
    /// `tblink version` of the installed binary, if there is one.
    pub installed_version: Option<String>,
    pub port_daemon_installed: bool,
    pub coordinator_installed: bool,
    pub local: Status,
    /// `None`: no coordinator config on this Mac.
    pub config: Option<Result<Config, String>>,
    pub paused: bool,
    pub key_present: bool,
    pub omlx_healthy: Option<bool>,
    /// `None`: not asked (no config). `Some(None)`: asked, no answer.
    pub peer: Option<Option<Status>>,
    pub deployment: Option<Option<Deployment>>,
    pub reach: Vec<Reach>,
    pub probe_port: u16,
}

fn speed(p: &net::Port) -> String {
    p.speed_gbps.map(|s| format!("{s} Gb/s")).unwrap_or_else(|| "unknown speed".into())
}

fn short(id: &Option<String>) -> String {
    id.as_deref()
        .map(|s| s.split('+').map(|u| u.split('-').next().unwrap_or(u)).collect::<Vec<_>>().join("+"))
        .unwrap_or_else(|| "?".into())
}

const INSTALL_PORT: &str =
    "run `tblink setup`, or `sudo ./install.sh port RDMA_IP BRIDGE_IP PEER_RDMA_IP PEER_BRIDGE_IP` from the tblink folder";

/// Judge the facts. Pure.
pub fn diagnose(x: &Facts) -> Vec<Finding> {
    use Level::*;
    let mut out = Vec::new();
    let st = &x.local;

    // ---- this Mac: binary and daemon ----
    out.push(f(Ok, format!("this Mac is {}, running tblink {}", st.hostname, x.binary_version)));
    match &x.installed_version {
        None => out.push(f(Problem, format!("tblink is not installed at {INSTALLED_BIN}: {INSTALL_PORT}"))),
        Some(v) if *v != x.binary_version => out.push(f(
            Warn,
            format!("the installed {INSTALLED_BIN} is {v}, but this one is {}; reinstall so the daemons run this build", x.binary_version),
        )),
        Some(_) => {}
    }
    if !x.port_daemon_installed {
        out.push(f(Problem, format!("the port daemon is not installed ({PORT_PLIST} missing): {INSTALL_PORT}")));
    }
    match &st.daemon {
        None => out.push(f(
            Problem,
            format!(
                "the port daemon has not reported in ({} missing): it is not running, or it is older than tblink 0.2. Install this build: {INSTALL_PORT}",
                net::STATE_PATH
            ),
        )),
        Some(d) if d.age_secs.unwrap_or(0) > STALE_SECS => out.push(f(
            Problem,
            format!(
                "the port daemon last reported {} s ago, so it is stuck or stopped: `sudo launchctl kickstart -k system/software.skepticdog.tblink.port`, then see /var/log/tblink.log",
                d.age_secs.unwrap_or(0)
            ),
        )),
        Some(d) => {
            if d.version != x.binary_version {
                out.push(f(Warn, format!("the running port daemon is tblink {}; restart it to run {} (reinstall with install.sh)", d.version, x.binary_version)));
            }
        }
    }

    // ---- cables ----
    if st.ports.is_empty() {
        out.push(f(
            Problem,
            "this Mac shows no RDMA-capable Thunderbolt ports. RDMA over Thunderbolt is enabled once per Mac: start up in Recovery, open Terminal, run `rdma_ctl enable`, and restart",
        ));
    }
    let cabled: Vec<&net::Port> = st.ports.iter().filter(|p| p.active).collect();
    for p in &st.ports {
        match (p.active, &p.peer) {
            (true, Some(m)) => out.push(f(Ok, format!("{}: Thunderbolt cable to {m} at {} (receptacle {})", p.name, speed(p), p.receptacle.unwrap_or(0)))),
            (true, None) => out.push(f(
                Note,
                format!("{} is up, but system_profiler shows no Mac on its receptacle, so its speed and cable are unknown and it will not be split from another cable", p.name),
            )),
            (false, Some(m)) => out.push(f(Warn, format!("{} has a cable to {m}, but the network link on it is down; reseat the cable", p.name))),
            (false, None) => {}
        }
    }
    if !st.ports.is_empty() && cabled.is_empty() {
        out.push(f(Problem, "no Thunderbolt cable to the other Mac is up: check the cable, and that the other Mac is on and awake"));
    }

    // ---- roles ----
    if let Some(d) = st.daemon.as_ref().filter(|d| d.age_secs.unwrap_or(0) <= STALE_SECS) {
        out.push(f(Ok, format!("roles: {}", net::describe_daemon(d))));
        match &d.adapt {
            Some(Adapt::Fallback { to, .. }) => out.push(f(
                Warn,
                format!(
                    "{} carries both roles because the other Mac is using that cable for both. Usually the other cable is not working at the other Mac's end: run `tblink status` there",
                    to.rdma_port().unwrap_or("?")
                ),
            )),
            Some(Adapt::Swapped { .. }) => out.push(f(Note, "inference and management are swapped to match the other Mac, which broke the speed tie the other way")),
            None => {}
        }
        if cabled.len() >= 2 && matches!(d.layout, Layout::Single { .. }) && d.adapt.is_none() && d.bridge_ip.is_some() {
            out.push(f(
                Warn,
                "two cables are up but tblink cannot tell them apart (no cable ids from system_profiler), so one cable carries both roles",
            ));
        }
        // Are the addresses where the layout says?
        let has = |port: Option<&str>, addr: &str| {
            port.and_then(|n| st.ports.iter().find(|p| p.name == n)).is_some_and(|p| p.inet.iter().any(|a| a == addr))
        };
        let mut settled = has(d.layout.rdma_port(), &d.rdma_ip) || d.layout == Layout::None;
        if let Some(b) = &d.bridge_ip {
            settled &= has(d.layout.bridge_port(), b) || d.layout == Layout::None;
        }
        if !settled {
            out.push(f(
                Warn,
                "the addresses are not yet where the roles put them; the port daemon retries every 5 s (see /var/log/tblink.log)",
            ));
        }
        for name in d.layout.ports() {
            if st.bridge_members.iter().any(|m| m == name) {
                out.push(f(Warn, format!("{name} is still in {}; the port daemon takes it out on its next tick", net::BRIDGE)));
            }
        }
        let stray: Vec<&str> =
            st.ports.iter().filter(|p| !p.active && p.role.is_some_and(|r| r != Role::Unused)).map(|p| p.name.as_str()).collect();
        if !stray.is_empty() {
            out.push(f(Warn, format!("stale addresses on {}; the port daemon removes them on its next tick", stray.join(", "))));
        }
    }

    // ---- reachability ----
    for r in &x.reach {
        if r.answered {
            out.push(f(Ok, format!("the other Mac's {} address {} answers over {}", r.what, r.addr, r.port)));
        } else {
            out.push(f(
                Problem,
                format!(
                    "the other Mac's {} address {} does not answer over {} (TCP port {}). Either the other Mac has it on a different cable (compare `tblink status` on both Macs), or it is off, or Remote Login is off there",
                    r.what, r.addr, r.port, x.probe_port
                ),
            ));
        }
    }

    // ---- coordinator ----
    match &x.config {
        None => out.push(f(
            Note,
            "no coordinator config here, so this is the other (non-coordinator) Mac, or setup has not run. Run `tblink doctor` on the coordinator Mac for the checks across both Macs",
        )),
        Some(Err(e)) => out.push(f(Problem, format!("the coordinator config cannot be read: {e}. Run `tblink setup` to write it again"))),
        Some(Result::Ok(cfg)) => coordinator_findings(x, cfg, &mut out),
    }
    out
}

fn coordinator_findings(x: &Facts, cfg: &Config, out: &mut Vec<Finding>) {
    use Level::*;
    let st = &x.local;
    for p in coord::config_problems(cfg, x.key_present) {
        out.push(f(Problem, format!("coordinator config: {p}")));
    }
    if let Some(n) = &st.node_id {
        if *n != cfg.local.node_id {
            out.push(f(
                Problem,
                format!(
                    "the config says this Mac's oMLX node is {}, but ~/.omlx/cluster/identity.json says {n}; run `tblink setup` again",
                    cfg.local.node_id
                ),
            ));
        }
    }
    if !x.coordinator_installed {
        out.push(f(Warn, "the coordinator LaunchAgent is not installed: `./install.sh coordinator ~/.config/tblink/coordinator.json`"));
    }
    if x.paused {
        out.push(f(Warn, "the coordinator is paused by `tblink shutdown`; run `tblink resume` to bring the model back"));
    }
    match x.omlx_healthy {
        Some(true) => out.push(f(Ok, format!("oMLX answers at {}", cfg.api))),
        Some(false) if x.paused => out.push(f(Note, format!("oMLX is not running at {} (expected while paused)", cfg.api))),
        Some(false) => out.push(f(
            Problem,
            format!(
                "oMLX is not answering at {}; the coordinator starts it on its own within a few minutes, or run `brew services start {}`",
                cfg.api, cfg.service
            ),
        )),
        None => {}
    }

    let peer = match &x.peer {
        Some(Some(p)) => p,
        Some(None) => {
            out.push(f(
                Problem,
                format!(
                    "cannot run `{} status` on the other Mac as {}@{} with {}. Check that Remote Login is on there, that the key is in its ~/.ssh/authorized_keys, and that tblink is installed there",
                    cfg.peer_tblink, cfg.peer.ssh_user, cfg.peer.bridge_ip, cfg.ssh_key
                ),
            ));
            return;
        }
        None => return,
    };
    out.push(f(
        Ok,
        format!("the other Mac is {}, running tblink {}", peer.hostname, if peer.version.is_empty() { "0.1" } else { &peer.version }),
    ));
    if let Some(w) = coord::version_warning(&x.binary_version, &peer.version) {
        out.push(f(Warn, w));
    }
    if let Some(n) = &peer.node_id {
        if *n != cfg.peer.node_id {
            out.push(f(
                Problem,
                format!("the config says the other Mac's oMLX node is {}, but it reports {n}; run `tblink setup` again", cfg.peer.node_id),
            ));
        }
    }
    match &peer.daemon {
        None if !peer.version.is_empty() => {
            out.push(f(Problem, "the other Mac's port daemon has not reported in: install this build there and run `tblink doctor` on it"))
        }
        Some(d) if d.age_secs.unwrap_or(0) > STALE_SECS => {
            out.push(f(Problem, format!("the other Mac's port daemon last reported {} s ago; restart it there", d.age_secs.unwrap_or(0))))
        }
        _ => {}
    }

    // Same physical cable for each role?
    let roles = [("inference", &cfg.local.rdma_ip, &cfg.peer.rdma_ip), ("management", &cfg.local.bridge_ip, &cfg.peer.bridge_ip)];
    for (what, mine, theirs) in roles {
        match (st.port_with(mine), peer.port_with(theirs)) {
            (Some(l), Some(p)) => match (&l.cable_id, &p.cable_id) {
                (Some(a), Some(b)) if a == b => out.push(f(Ok, format!("both Macs use the same cable for {what}: {} here, {} there, {}", l.name, p.name, speed(l)))),
                (Some(_), Some(_)) => out.push(f(
                    Problem,
                    format!(
                        "the Macs use different cables for {what}: {} here (cable {}), {} there (cable {}). The port daemons should agree within a minute; if this persists, unplug and replug the slower cable",
                        l.name,
                        short(&l.cable_id),
                        p.name,
                        short(&p.cable_id)
                    ),
                )),
                _ => out.push(f(Note, format!("{what}: {} here, {} there (cable ids unknown, so not compared)", l.name, p.name))),
            },
            (None, _) => out.push(f(Warn, format!("this Mac's {what} address {mine} is not on an active Thunderbolt port"))),
            (_, None) => out.push(f(Warn, format!("the other Mac's {what} address {theirs} is not on an active Thunderbolt port there"))),
        }
    }

    match &x.deployment {
        Some(Some(d)) if d.live && d.backend == "jaccl" => {
            let port = coord::rdma_port(st, &cfg.local.rdma_ip).map(|p| p.name.clone()).unwrap_or("?".into());
            out.push(f(Ok, format!("deployment {} is live on JACCL (RDMA) over {port}", d.id)));
        }
        Some(Some(d)) if d.live => {
            if coord::jaccl_ready(cfg, st, peer) {
                out.push(f(
                    Warn,
                    format!(
                        "deployment {} is live on {} (TCP over the management link) although RDMA is up: JACCL failed to load and is retried every {} s; see ~/Library/Logs/tblink.log",
                        d.id, d.backend, cfg.jaccl_retry_secs
                    ),
                ));
            } else {
                out.push(f(
                    Note,
                    format!("deployment {} is live on {} over the management link (RDMA is not up on both Macs)", d.id, d.backend),
                ));
            }
        }
        Some(Some(d)) if !x.paused => out.push(f(
            Warn,
            format!("deployment {} is not loaded; the coordinator loads it within a few minutes (see ~/Library/Logs/tblink.log)", d.id),
        )),
        Some(None) if !x.paused => {
            out.push(f(Warn, "there is no deployment for the configured model yet; the coordinator creates one on its next check"))
        }
        _ => {}
    }
}

/// Which addresses of the other Mac to try, over which port.
pub fn reach_targets(st: &Status, peer_rdma: Option<&str>, peer_bridge: Option<&str>) -> Vec<(&'static str, String, String)> {
    let Some(d) = st.daemon.as_ref() else { return Vec::new() };
    let mut v = Vec::new();
    if let (Some(p), Some(ip)) = (d.layout.rdma_port(), peer_rdma) {
        v.push(("RDMA", ip.to_string(), p.to_string()));
    }
    if let (Some(p), Some(ip)) = (d.layout.bridge_port(), peer_bridge) {
        v.push(("management", ip.to_string(), p.to_string()));
    }
    v
}

pub fn render(findings: &[Finding]) -> String {
    let mut s = String::new();
    for x in findings {
        let tag = match x.level {
            Level::Ok => "ok  ",
            Level::Note => "note",
            Level::Warn => "WARN",
            Level::Problem => "FAIL",
        };
        s.push_str(&format!("{tag}  {}\n", x.text));
    }
    let problems = findings.iter().filter(|x| x.level == Level::Problem).count();
    let warnings = findings.iter().filter(|x| x.level == Level::Warn).count();
    s.push_str(&match (problems, warnings) {
        (0, 0) => "all good\n".to_string(),
        (p, w) => format!("{p} problem(s), {w} warning(s)\n"),
    });
    s
}

/// The version an installed binary reports (`tblink 0.2.0 (abc)` → `0.2.0 (abc)`).
pub fn parse_version_output(out: &str) -> Option<String> {
    out.lines()
        .find_map(|l| l.strip_prefix("tblink ").filter(|v| v.starts_with(|c: char| c.is_ascii_digit())).map(|v| v.trim().to_string()))
}

fn gather(config_path: &str) -> Facts {
    let local = net::status();
    let installed_version = if std::path::Path::new(INSTALLED_BIN).exists() {
        let (ok, out) = net::run(INSTALLED_BIN, &["version"]);
        // tblink 0.1 has no `version` command.
        Some(if ok { parse_version_output(&out).unwrap_or_else(|| "0.1".into()) } else { "0.1".into() })
    } else {
        None
    };
    let config_exists = std::path::Path::new(&coord::expand(config_path)).exists();
    let config = config_exists.then(|| coord::load_config(config_path));
    let mut x = Facts {
        binary_version: net::version(),
        installed_version,
        port_daemon_installed: std::path::Path::new(PORT_PLIST).exists(),
        coordinator_installed: std::path::Path::new(&coord::expand(COORD_PLIST)).exists(),
        paused: coord::marker_path(config_path).exists(),
        probe_port: 22,
        ..Facts::default()
    };
    // The other Mac's addresses: from the config, else from the port daemon's arguments.
    let (mut peer_rdma, mut peer_bridge) = (None, None);
    if let Some(d) = &local.daemon {
        let a = port::PortArgs {
            rdma_ip: d.rdma_ip.clone(),
            bridge_ip: d.bridge_ip.clone(),
            peer_ips: d.peer_ips.clone(),
            interval: 5,
            once: true,
            dry_run: true,
            probe_port: 22,
        };
        peer_rdma = port::peer_in_subnet(&a, Some(&a.rdma_ip)).map(str::to_string);
        peer_bridge = port::peer_in_subnet(&a, a.bridge_ip.as_deref()).map(str::to_string);
    }
    if let Some(Ok(cfg)) = &config {
        peer_rdma = Some(cfg.peer.rdma_ip.clone());
        peer_bridge = Some(cfg.peer.bridge_ip.clone());
        x.key_present = coord::read_key(&cfg.settings_path).is_some();
        let healthy = coord::healthy(&cfg.api);
        x.omlx_healthy = Some(healthy);
        x.peer = Some(coord::peer_status(cfg));
        if healthy {
            if let Some(key) = coord::read_key(&cfg.settings_path) {
                let api = coord::Api { base: cfg.api.clone(), key };
                let model = cfg.plan_request["model_path"].as_str().unwrap_or_default();
                if let (Ok(deps), Ok(status)) = (api.get("/admin/api/cluster/deployments"), api.get("/admin/api/cluster/status")) {
                    x.deployment = Some(deployment(&deps, &status, model));
                }
            }
        }
    }
    x.reach = reach_targets(&local, peer_rdma.as_deref(), peer_bridge.as_deref())
        .into_iter()
        .map(|(what, addr, port)| {
            let (ok, out) = net::run(port::NC, &["-z", "-v", "-G", "2", "-b", &port, &addr, "22"]);
            Reach { what, answered: port::probe_answered(ok, &out), addr, port }
        })
        .collect();
    x.local = local;
    x.config = config;
    x
}

/// The configured model's deployment and whether it is live.
pub fn deployment(deps: &Value, status: &Value, model: &str) -> Option<Deployment> {
    let d = coord::current_deployment(deps, model)?;
    let id = d["deployment_id"].as_str().unwrap_or_default().to_string();
    Some(Deployment { live: coord::live_ids(status).contains(&id), backend: d["backend"].as_str().unwrap_or("?").to_string(), id })
}

pub fn main(config_path: &str) -> i32 {
    let x = gather(config_path);
    let findings = diagnose(&x);
    print!("{}", render(&findings));
    if findings.iter().any(|x| x.level == Level::Problem) {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{DaemonState, Port};
    use serde_json::json;

    fn port(name: &str, inet: &[&str], id: &str, speed: u32, role: Role) -> Port {
        Port {
            name: name.into(),
            rdma_device: format!("rdma_{name}"),
            active: true,
            inet: inet.iter().map(|s| s.to_string()).collect(),
            receptacle: Some(1),
            speed_gbps: Some(speed),
            peer: Some("MacBook Pro".into()),
            cable_id: Some(id.into()),
            role: Some(role),
            ..Port::default()
        }
    }
    fn daemon(layout: Layout, rdma: &str, bridge: &str) -> DaemonState {
        DaemonState {
            version: net::version(),
            rdma_ip: rdma.into(),
            bridge_ip: Some(bridge.into()),
            peer_ips: vec![],
            layout,
            updated: 0,
            age_secs: Some(3),
            ..DaemonState::default()
        }
    }
    fn split(i: &str, m: &str) -> Layout {
        Layout::Split { inference: i.into(), management: m.into() }
    }
    /// The coordinator Mac with two good cables.
    fn local() -> Status {
        let ports = vec![port("en1", &["10.77.0.1"], "tb4", 40, Role::Management), port("en6", &["10.0.1.1"], "tb5", 80, Role::Inference)];
        Status {
            hostname: "Mac-A".into(),
            active: Some(ports[1].clone()),
            ports,
            version: net::version(),
            node_id: Some("A".into()),
            daemon: Some(daemon(split("en6", "en1"), "10.0.1.1", "10.77.0.1")),
            ..Status::default()
        }
    }
    fn peer() -> Status {
        let ports = vec![port("en1", &["10.0.1.2"], "tb5", 80, Role::Inference), port("en2", &["10.77.0.2"], "tb4", 40, Role::Management)];
        Status {
            hostname: "Mac-B".into(),
            active: Some(ports[0].clone()),
            ports,
            version: net::version(),
            node_id: Some("B".into()),
            daemon: Some(daemon(split("en1", "en2"), "10.0.1.2", "10.77.0.2")),
            ..Status::default()
        }
    }
    fn cfg() -> Config {
        serde_json::from_value(json!({
            "api": "http://127.0.0.1:8000", "settings_path": "~/.omlx/settings.json",
            "brew": "/opt/homebrew/bin/brew", "service": "jundot/omlx/omlx", "ssh_key": "~/.ssh/omlx_cluster",
            "local": {"node_id": "A", "rdma_ip": "10.0.1.1", "bridge_ip": "10.77.0.1"},
            "peer": {"node_id": "B", "rdma_ip": "10.0.1.2", "bridge_ip": "10.77.0.2", "ssh_user": "u"},
            "plan_request": {"model_path": "/m"}
        }))
        .unwrap()
    }
    fn healthy() -> Facts {
        Facts {
            binary_version: net::version(),
            installed_version: Some(net::version()),
            port_daemon_installed: true,
            coordinator_installed: true,
            local: local(),
            config: Some(Result::Ok(cfg())),
            key_present: true,
            omlx_healthy: Some(true),
            peer: Some(Some(peer())),
            deployment: Some(Some(Deployment { id: "d1".into(), backend: "jaccl".into(), live: true })),
            reach: vec![
                Reach { what: "RDMA", addr: "10.0.1.2".into(), port: "en6".into(), answered: true },
                Reach { what: "management", addr: "10.77.0.2".into(), port: "en1".into(), answered: true },
            ],
            probe_port: 22,
            ..Facts::default()
        }
    }
    fn worst(x: &Facts) -> Level {
        diagnose(x).iter().map(|f| f.level).max().unwrap()
    }
    fn texts(x: &Facts, level: Level) -> Vec<String> {
        diagnose(x).into_iter().filter(|f| f.level == level).map(|f| f.text).collect()
    }

    #[test]
    fn healthy_cluster_is_all_ok() {
        let x = healthy();
        assert_eq!(worst(&x), Level::Ok, "{}", render(&diagnose(&x)));
        let r = render(&diagnose(&x));
        assert!(r.contains("both Macs use the same cable for inference: en6 here, en1 there, 80 Gb/s"), "{r}");
        assert!(r.contains("both Macs use the same cable for management: en1 here, en2 there, 40 Gb/s"), "{r}");
        assert!(r.contains("deployment d1 is live on JACCL (RDMA) over en6"), "{r}");
        assert!(r.contains("roles: inference on en6 (dedicated), management on en1"), "{r}");
        assert!(r.ends_with("all good\n"));
    }

    #[test]
    fn peer_on_an_older_version_is_a_warning() {
        let mut x = healthy();
        let mut p = peer();
        p.version = String::new();
        p.daemon = None;
        x.peer = Some(Some(p));
        assert_eq!(worst(&x), Level::Warn);
        assert!(texts(&x, Level::Warn)[0].contains("older tblink (0.1"));
        let mut p = peer();
        p.version = "0.2.0 (0000000)".into();
        x.peer = Some(Some(p));
        assert!(texts(&x, Level::Warn).iter().any(|t| t.contains("runs tblink 0.2.0 (0000000)")));
    }

    #[test]
    fn installed_binary_older_than_this_one() {
        let mut x = healthy();
        x.installed_version = Some("0.1".into());
        assert!(texts(&x, Level::Warn)[0].contains("the installed /usr/local/bin/tblink is 0.1"));
        x.installed_version = None;
        assert_eq!(worst(&x), Level::Problem);
    }

    #[test]
    fn macs_on_different_cables_is_a_problem() {
        let mut x = healthy();
        let mut p = peer();
        p.ports[0].cable_id = Some("tb4".into());
        p.ports[1].cable_id = Some("tb5".into());
        x.peer = Some(Some(p));
        let t = texts(&x, Level::Problem);
        assert_eq!(t.len(), 2, "{t:?}");
        assert!(t[0].starts_with("the Macs use different cables for inference: en6 here (cable tb5), en1 there (cable tb4)"), "{t:?}");
    }

    #[test]
    fn fallback_is_explained() {
        let mut x = healthy();
        let mut d = daemon(Layout::Single { port: "en1".into() }, "10.0.1.1", "10.77.0.1");
        d.adapt = Some(Adapt::Fallback { to: Layout::Single { port: "en1".into() }, base: split("en6", "en1") });
        x.local.daemon = Some(d);
        x.local.ports[0].inet = vec!["10.0.1.1".into(), "10.77.0.1".into()];
        x.local.ports[1].inet = vec![];
        let w = texts(&x, Level::Warn);
        assert!(w.iter().any(|t| t.contains("en1 carries both roles because the other Mac is using that cable for both")), "{w:?}");
    }

    #[test]
    fn no_rdma_ports_says_how_to_enable_rdma() {
        let mut x = healthy();
        x.config = None;
        x.reach.clear();
        x.local.ports.clear();
        x.local.active = None;
        assert!(texts(&x, Level::Problem)[0].contains("rdma_ctl enable"));
    }

    #[test]
    fn no_cable() {
        let mut x = healthy();
        x.config = None;
        x.reach.clear();
        for p in &mut x.local.ports {
            p.active = false;
            p.peer = None;
        }
        assert!(texts(&x, Level::Problem).iter().any(|t| t.starts_with("no Thunderbolt cable to the other Mac is up")));
    }

    #[test]
    fn cable_present_but_link_down() {
        let mut x = healthy();
        x.local.ports[0].active = false;
        assert!(texts(&x, Level::Warn)
            .iter()
            .any(|t| t == "en1 has a cable to MacBook Pro, but the network link on it is down; reseat the cable"));
    }

    #[test]
    fn daemon_missing_or_stale() {
        let mut x = healthy();
        x.local.daemon = None;
        assert!(texts(&x, Level::Problem).iter().any(|t| t.contains("has not reported in")));
        let mut d = daemon(split("en6", "en1"), "10.0.1.1", "10.77.0.1");
        d.age_secs = Some(600);
        x.local.daemon = Some(d);
        assert!(texts(&x, Level::Problem).iter().any(|t| t.contains("last reported 600 s ago")));
        x.port_daemon_installed = false;
        assert!(texts(&x, Level::Problem).iter().any(|t| t.contains("is not installed")));
    }

    #[test]
    fn running_daemon_older_than_the_binary() {
        let mut x = healthy();
        let mut d = daemon(split("en6", "en1"), "10.0.1.1", "10.77.0.1");
        d.version = "0.2.0 (old)".into();
        x.local.daemon = Some(d);
        assert!(texts(&x, Level::Warn).iter().any(|t| t.contains("the running port daemon is tblink 0.2.0 (old)")));
    }

    #[test]
    fn unsettled_addresses_and_bridge_membership() {
        let mut x = healthy();
        x.local.ports[1].inet.clear();
        x.local.bridge_members = vec!["en6".into()];
        let w = texts(&x, Level::Warn);
        assert!(w.iter().any(|t| t.contains("not yet where the roles put them")), "{w:?}");
        assert!(w.iter().any(|t| t.starts_with("en6 is still in bridge0")), "{w:?}");
    }

    #[test]
    fn unreachable_peer_address() {
        let mut x = healthy();
        x.reach[0].answered = false;
        let t = texts(&x, Level::Problem);
        assert_eq!(t.len(), 1);
        assert!(t[0].starts_with("the other Mac's RDMA address 10.0.1.2 does not answer over en6 (TCP port 22)"));
    }

    #[test]
    fn peer_unreachable_over_ssh() {
        let mut x = healthy();
        x.peer = Some(None);
        let t = texts(&x, Level::Problem);
        assert!(t[0].contains("cannot run `/usr/local/bin/tblink status` on the other Mac as u@10.77.0.2"), "{t:?}");
    }

    #[test]
    fn node_id_mismatch() {
        let mut x = healthy();
        x.local.node_id = Some("Z".into());
        assert!(texts(&x, Level::Problem)[0].contains("identity.json says Z"));
    }

    #[test]
    fn ring_while_rdma_is_up() {
        let mut x = healthy();
        x.deployment = Some(Some(Deployment { id: "r1".into(), backend: "ring".into(), live: true }));
        assert!(texts(&x, Level::Warn)[0].contains("live on ring (TCP over the management link) although RDMA is up"));
    }

    #[test]
    fn paused_cluster() {
        let mut x = healthy();
        x.paused = true;
        x.omlx_healthy = Some(false);
        x.deployment = None;
        assert_eq!(worst(&x), Level::Warn);
        assert!(texts(&x, Level::Note).iter().any(|t| t.contains("expected while paused")));
    }

    #[test]
    fn omlx_down() {
        let mut x = healthy();
        x.omlx_healthy = Some(false);
        x.deployment = None;
        assert!(texts(&x, Level::Problem)[0].starts_with("oMLX is not answering"));
    }

    #[test]
    fn peer_mac_without_config_points_to_the_coordinator() {
        let mut x = healthy();
        x.config = None;
        x.local = peer();
        x.reach.clear();
        assert_eq!(worst(&x), Level::Note, "{}", render(&diagnose(&x)));
        assert!(texts(&x, Level::Note)[0].contains("Run `tblink doctor` on the coordinator Mac"));
    }

    #[test]
    fn broken_config() {
        let mut x = healthy();
        x.config = Some(Err("expected value at line 1".into()));
        assert!(texts(&x, Level::Problem)[0].contains("cannot be read: expected value"));
    }

    #[test]
    fn reach_targets_follow_the_layout() {
        let st = local();
        assert_eq!(
            reach_targets(&st, Some("10.0.1.2"), Some("10.77.0.2")),
            vec![("RDMA", "10.0.1.2".to_string(), "en6".to_string()), ("management", "10.77.0.2".to_string(), "en1".to_string())]
        );
        assert!(reach_targets(&Status::default(), Some("10.0.1.2"), None).is_empty());
    }

    #[test]
    fn version_output() {
        assert_eq!(parse_version_output("tblink 0.2.0 (abc1234)\n"), Some("0.2.0 (abc1234)".into()));
        assert_eq!(parse_version_output("usage:\n  tblink status\n"), None);
    }

    #[test]
    fn deployment_facts() {
        let deps = json!({"deployments": [{"deployment_id": "d1", "model": "/m", "backend": "jaccl"}]});
        let live = json!({"runtime_jobs": {"jobs": [{"deployment_id": "d1", "live": true}]}});
        assert_eq!(deployment(&deps, &live, "/m"), Some(Deployment { id: "d1".into(), backend: "jaccl".into(), live: true }));
        assert!(!deployment(&deps, &json!({}), "/m").unwrap().live);
        assert_eq!(deployment(&deps, &live, "/other"), None);
    }

    #[test]
    fn render_counts() {
        let r = render(&[f(Level::Ok, "a"), f(Level::Warn, "b"), f(Level::Problem, "c")]);
        assert_eq!(r, "ok    a\nWARN  b\nFAIL  c\n1 problem(s), 1 warning(s)\n");
    }
}
