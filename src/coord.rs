//! `tblink coordinator`: runs on the oMLX coordinator Mac as the
//! logged-in user. It replaces the zsh watchdog:
//!   - keeps the local oMLX brew service up and bound to every configured
//!     address that exists on this Mac,
//!   - works out the deployment the current ports call for (JACCL over the
//!     active RDMA ports on both Macs, or TCP Ring over the bridge addresses),
//!   - rebuilds and loads that deployment when the ports change, and falls back
//!     to Ring when JACCL will not load, retrying JACCL later.
//! The only secret it reads is the local admin API key from oMLX's settings;
//! it is never logged or put on a command line.

use crate::net::{self, Status};
use crate::{log, sleep_secs};
use serde::Deserialize;
use serde_json::{json, Value};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

#[derive(Debug, Deserialize, Clone)]
pub struct Node {
    pub node_id: String,
    pub rdma_ip: String,
    pub bridge_ip: String,
    /// SSH user on the peer (empty for the local node).
    #[serde(default)]
    pub ssh_user: String,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub api: String,
    pub settings_path: String,
    pub brew: String,
    pub service: String,
    pub ssh_key: String,
    #[serde(default = "default_peer_tblink")]
    pub peer_tblink: String,
    pub local: Node,
    pub peer: Node,
    /// The body for POST /admin/api/cluster/plan (model_path, nodes, profile,
    /// context, tensor_parallel_size). Deployments reuse it.
    pub plan_request: Value,
    #[serde(default = "d15")]
    pub interval_secs: u64,
    #[serde(default = "d120")]
    pub backoff_secs: u64,
    #[serde(default = "d1800")]
    pub jaccl_retry_secs: u64,
    #[serde(default = "d2")]
    pub jaccl_failures_before_ring: u32,
}
fn default_peer_tblink() -> String { "/usr/local/bin/tblink".into() }
fn d15() -> u64 { 15 }
fn d120() -> u64 { 120 }
fn d1800() -> u64 { 1800 }
fn d2() -> u32 { 2 }

pub fn expand(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => format!("{}/{rest}", std::env::var("HOME").unwrap_or_default()),
        None => p.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Backend { Jaccl, Ring }

/// The `hosts` list a deployment needs for the given port state.
pub fn desired_hosts(cfg: &Config, backend: &Backend, local: &Status, peer: &Status) -> Value {
    let peer_ssh = format!("{}@{}", cfg.peer.ssh_user, cfg.peer.bridge_ip);
    match backend {
        Backend::Jaccl => {
            let l = &local.active.as_ref().expect("jaccl needs local port").rdma_device;
            let p = &peer.active.as_ref().expect("jaccl needs peer port").rdma_device;
            json!([
                {"node_id": cfg.local.node_id, "ssh": "127.0.0.1", "ips": [cfg.local.rdma_ip], "rdma": [null, l]},
                {"node_id": cfg.peer.node_id, "ssh": peer_ssh, "ips": [cfg.peer.rdma_ip], "rdma": [p, null]},
            ])
        }
        Backend::Ring => json!([
            {"node_id": cfg.local.node_id, "ssh": "127.0.0.1", "ips": [cfg.local.bridge_ip], "rdma": []},
            {"node_id": cfg.peer.node_id, "ssh": peer_ssh, "ips": [cfg.peer.bridge_ip], "rdma": []},
        ]),
    }
}

/// A port is usable for JACCL once the port daemon has put the RDMA address on it.
pub fn jaccl_ready(cfg: &Config, local: &Status, peer: &Status) -> bool {
    local.active_has(&cfg.local.rdma_ip) && peer.active_has(&cfg.peer.rdma_ip)
}

/// Compare only the fields that define the link, in the shape oMLX stores them.
pub fn hosts_match(current: &Value, want: &Value) -> bool {
    let norm = |v: &Value| -> Vec<(String, String, Value, Value)> {
        let mut out: Vec<_> = v.as_array().into_iter().flatten().map(|h| (
            h["node_id"].as_str().unwrap_or("").to_string(),
            h["ssh"].as_str().unwrap_or("").to_string(),
            h["ips"].clone(),
            h.get("rdma").cloned().unwrap_or(json!([])),
        )).collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    };
    norm(current) == norm(want)
}

struct Api { base: String, key: String }

impl Api {
    fn req(&self, method: &str, path: &str, timeout: u64) -> ureq::Request {
        ureq::request(method, &format!("{}{}", self.base, path))
            .set("Authorization", &format!("Bearer {}", self.key))
            .timeout(Duration::from_secs(timeout))
    }
    fn get(&self, path: &str) -> Result<Value, String> {
        self.req("GET", path, 20).call().map_err(err)?.into_json().map_err(|e| e.to_string())
    }
    fn post(&self, path: &str, body: Option<&Value>, timeout: u64) -> Result<Value, String> {
        let r = self.req("POST", path, timeout);
        let resp = match body { Some(b) => r.send_json(b.clone()), None => r.call() }.map_err(err)?;
        resp.into_json().map_err(|e| e.to_string())
    }
}

fn err(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, r) => {
            let body = r.into_string().unwrap_or_default();
            format!("HTTP {code}: {}", body.chars().take(300).collect::<String>())
        }
        other => other.to_string(),
    }
}

fn read_key(path: &str) -> Option<String> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(expand(path)).ok()?).ok()?;
    v["auth"]["api_key"].as_str().map(str::to_string)
}

fn healthy(base: &str) -> bool {
    ureq::get(&format!("{base}/health")).timeout(Duration::from_secs(5)).call()
        .ok().and_then(|r| r.into_string().ok()).map(|s| s.contains("\"healthy\"")).unwrap_or(false)
}

fn listening(addr: &str, port: u16) -> bool {
    addr.parse().map(|ip| TcpStream::connect_timeout(&SocketAddr::new(ip, port), Duration::from_secs(2)).is_ok()).unwrap_or(false)
}

/// Addresses oMLX is configured to listen on, and its port.
fn listen_config(settings_path: &str) -> (Vec<String>, u16) {
    let v: Value = std::fs::read_to_string(expand(settings_path)).ok()
        .and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
    let hosts = v["server"]["host"].as_str().unwrap_or("127.0.0.1")
        .split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    (hosts, v["server"]["port"].as_u64().unwrap_or(8000) as u16)
}

fn local_has_addr(addr: &str) -> bool {
    let (_, out) = net::run(net::IFCONFIG, &["-a"]);
    net::parse_inet(&out).iter().any(|a| a == addr)
}

fn peer_status(cfg: &Config) -> Option<Status> {
    for ip in [&cfg.peer.bridge_ip, &cfg.peer.rdma_ip] {
        let target = format!("{}@{}", cfg.peer.ssh_user, ip);
        let (ok, out) = net::run("/usr/bin/ssh", &[
            "-i", &expand(&cfg.ssh_key), "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes",
            "-o", "ConnectTimeout=3", "-o", "StrictHostKeyChecking=accept-new",
            &target, &cfg.peer_tblink, "status", "--json",
        ]);
        if ok {
            if let Ok(s) = serde_json::from_str::<Status>(out.trim()) {
                return Some(s);
            }
        }
    }
    None
}

#[derive(Default)]
struct State {
    last_service: Option<Instant>,
    last_load: Option<Instant>,
    jaccl_failures: u32,
    ring_until: Option<Instant>,
    down_since: Option<Instant>,
    last_note: String,
}

impl State {
    fn due(t: Option<Instant>, secs: u64) -> bool {
        t.map(|t| t.elapsed().as_secs() >= secs).unwrap_or(true)
    }
    /// Log a steady-state message only when it changes.
    fn note(&mut self, msg: String) {
        if msg != self.last_note {
            log(&msg);
            self.last_note = msg;
        }
    }
}

pub fn main(config_path: &str, once: bool, check: bool) -> i32 {
    let cfg: Config = match std::fs::read_to_string(expand(config_path)).map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string())) {
        Ok(c) => c,
        Err(e) => { eprintln!("cannot read {config_path}: {e}"); return 1; }
    };
    if check {
        // Validate without changing anything: config, key, API, peer.
        let mut ok = true;
        if cfg.plan_request["model_path"].as_str().is_none() { eprintln!("plan_request.model_path missing"); ok = false; }
        if read_key(&cfg.settings_path).is_none() { eprintln!("no auth.api_key in {}", cfg.settings_path); ok = false; }
        println!("oMLX at {}: {}", cfg.api, if healthy(&cfg.api) { "healthy" } else { "not answering" });
        match peer_status(&cfg) {
            Some(p) => println!("peer {}: active port {}", p.hostname, p.active.map(|a| a.name).unwrap_or("none".into())),
            None => println!("peer: not reachable over SSH, or {} missing there", cfg.peer_tblink),
        }
        return if ok { 0 } else { 1 };
    }
    let mut st = State::default();
    loop {
        tick(&cfg, &mut st);
        if once { return 0; }
        sleep_secs(cfg.interval_secs);
    }
}

fn brew(cfg: &Config, verb: &str, st: &mut State, why: &str) {
    if !State::due(st.last_service, cfg.backoff_secs) { return; }
    st.last_service = Some(Instant::now());
    log(&format!("{why}; brew services {verb} {}", cfg.service));
    let (ok, out) = net::run(&cfg.brew, &["services", verb, &cfg.service]);
    if !ok { log(&format!("brew services {verb} failed: {}", out.trim())); }
}

fn tick(cfg: &Config, st: &mut State) {
    // 1. Local service.
    if !healthy(&cfg.api) {
        brew(cfg, "start", st, "oMLX not healthy");
        return;
    }
    // 2. oMLX binds at start; an address that appeared later is not served.
    let (hosts, port) = listen_config(&cfg.settings_path);
    if let Some(miss) = hosts.iter().find(|h| local_has_addr(h) && !listening(h, port)) {
        brew(cfg, "restart", st, &format!("{miss} is configured and present but oMLX is not listening on it"));
        return;
    }
    let Some(key) = read_key(&cfg.settings_path) else { st.note("cannot read the oMLX API key".into()); return };
    let api = Api { base: cfg.api.clone(), key };

    // 3. Port state on both Macs.
    let local = net::status();
    let Some(peer) = peer_status(cfg) else { st.note("peer unreachable over SSH; waiting".into()); return };

    let use_ring = !jaccl_ready(cfg, &local, &peer)
        || st.ring_until.map(|t| Instant::now() < t).unwrap_or(false);
    let backend = if use_ring { Backend::Ring } else { Backend::Jaccl };
    let want_hosts = desired_hosts(cfg, &backend, &local, &peer);
    let backend_name = if backend == Backend::Jaccl { "jaccl" } else { "ring" };

    // 4. Current deployment for this model, and whether it is live.
    let model = cfg.plan_request["model_path"].as_str().unwrap_or_default();
    let deps = match api.get("/admin/api/cluster/deployments") { Ok(v) => v, Err(e) => { st.note(format!("deployments: {e}")); return } };
    let current = deps["deployments"].as_array().into_iter().flatten()
        .find(|d| d["model"].as_str() == Some(model)).cloned();
    let status = match api.get("/admin/api/cluster/status") { Ok(v) => v, Err(e) => { st.note(format!("status: {e}")); return } };
    let live_ids: Vec<String> = status["runtime_jobs"]["jobs"].as_array().into_iter().flatten()
        .filter(|j| j["live"].as_bool() == Some(true))
        .filter_map(|j| j["deployment_id"].as_str().map(str::to_string)).collect();

    let matches = current.as_ref().map(|d| d["backend"].as_str() == Some(backend_name) && hosts_match(&d["hosts"], &want_hosts)).unwrap_or(false);
    let cur_id = current.as_ref().and_then(|d| d["deployment_id"].as_str()).unwrap_or("").to_string();

    if matches && live_ids.contains(&cur_id) {
        if let Some(t) = st.down_since.take() { log(&format!("{cur_id} live again (down {} s)", t.elapsed().as_secs())); }
        if backend == Backend::Jaccl { st.jaccl_failures = 0; }
        st.note(format!("ok: {cur_id} live on {backend_name} ({} ↔ {})",
            local.active.as_ref().map(|p| p.rdma_device.as_str()).unwrap_or("-"),
            peer.active.as_ref().map(|p| p.rdma_device.as_str()).unwrap_or("-")));
        return;
    }
    st.down_since.get_or_insert_with(Instant::now);
    if !State::due(st.last_load, cfg.backoff_secs) { return; }
    st.last_load = Some(Instant::now());

    // 5. Rebuild when the ports (or backend) changed.
    let id = if matches { cur_id } else {
        log(&format!("ports call for {backend_name} with hosts {want_hosts}; rebuilding (was {})", if cur_id.is_empty() { "none" } else { &cur_id }));
        for live in live_ids.iter().filter(|i| **i == cur_id) {
            match api.post(&format!("/admin/api/cluster/deployments/{live}/unload"), None, 300) {
                Ok(_) => log(&format!("unloaded {live}")),
                Err(e) => { log(&format!("unload {live}: {e}")); return; }
            }
        }
        let plan = match api.post("/admin/api/cluster/plan", Some(&cfg.plan_request), 180) { Ok(v) => v, Err(e) => { log(&format!("plan: {e}")); return } };
        let mut body = cfg.plan_request.clone();
        body["backend"] = json!(backend_name);
        body["auto_tune"] = json!(true);
        body["approved_placement"] = plan["placement_signature"].clone();
        body["hosts"] = want_hosts.clone();
        match api.post("/admin/api/cluster/deployments", Some(&body), 300) {
            Ok(v) => v["deployment"]["deployment_id"].as_str().unwrap_or_default().to_string(),
            Err(e) => { log(&format!("create deployment: {e}")); return; }
        }
    };

    // 6. Load.
    let t0 = Instant::now();
    match api.post(&format!("/admin/api/cluster/deployments/{id}/load"), None, 900) {
        Ok(v) if v["loaded"].as_bool() == Some(true) => log(&format!("loaded {id} on {backend_name} in {} s", t0.elapsed().as_secs())),
        r => {
            let why = match r { Ok(v) => v.to_string().chars().take(300).collect(), Err(e) => e };
            log(&format!("load {id} on {backend_name} failed after {} s: {why}", t0.elapsed().as_secs()));
            if backend == Backend::Jaccl {
                st.jaccl_failures += 1;
                if st.jaccl_failures >= cfg.jaccl_failures_before_ring {
                    log(&format!("JACCL failed {} times; falling back to Ring for {} s", st.jaccl_failures, cfg.jaccl_retry_secs));
                    st.ring_until = Some(Instant::now() + Duration::from_secs(cfg.jaccl_retry_secs));
                    st.jaccl_failures = 0;
                    st.last_load = None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::Port;

    fn cfg() -> Config {
        serde_json::from_value(json!({
            "api": "http://127.0.0.1:8000", "settings_path": "~/.omlx/settings.json",
            "brew": "/opt/homebrew/bin/brew", "service": "jundot/omlx/omlx", "ssh_key": "~/.ssh/omlx_cluster",
            "local": {"node_id": "A", "rdma_ip": "10.0.1.1", "bridge_ip": "10.77.0.1"},
            "peer": {"node_id": "B", "rdma_ip": "10.0.1.2", "bridge_ip": "10.77.0.2", "ssh_user": "peeruser"},
            "plan_request": {"model_path": "/m"}
        })).unwrap()
    }
    fn status(dev: &str, inet: &[&str]) -> Status {
        let p = Port { name: dev.trim_start_matches("rdma_").into(), rdma_device: dev.into(), active: true, bridge_member: false, inet: inet.iter().map(|s| s.to_string()).collect() };
        Status { hostname: "h".into(), active: Some(p.clone()), ports: vec![p], bridge_inet: vec![], bridge_members: vec![] }
    }

    #[test]
    fn jaccl_hosts_follow_the_active_ports() {
        let c = cfg();
        let h = desired_hosts(&c, &Backend::Jaccl, &status("rdma_en6", &["10.0.1.1"]), &status("rdma_en1", &["10.0.1.2"]));
        assert_eq!(h, json!([
            {"node_id": "A", "ssh": "127.0.0.1", "ips": ["10.0.1.1"], "rdma": [null, "rdma_en6"]},
            {"node_id": "B", "ssh": "peeruser@10.77.0.2", "ips": ["10.0.1.2"], "rdma": ["rdma_en1", null]},
        ]));
    }

    #[test]
    fn port_change_breaks_the_match() {
        let c = cfg();
        let (l, p) = (status("rdma_en6", &["10.0.1.1"]), status("rdma_en1", &["10.0.1.2"]));
        let old = desired_hosts(&c, &Backend::Jaccl, &l, &p);
        assert!(hosts_match(&old, &desired_hosts(&c, &Backend::Jaccl, &l, &p)));
        let moved = desired_hosts(&c, &Backend::Jaccl, &status("rdma_en1", &["10.0.1.1"]), &p);
        assert!(!hosts_match(&old, &moved));
    }

    #[test]
    fn jaccl_needs_rdma_addresses_on_both_active_ports() {
        let c = cfg();
        assert!(jaccl_ready(&c, &status("rdma_en6", &["10.0.1.1"]), &status("rdma_en1", &["10.0.1.2"])));
        assert!(!jaccl_ready(&c, &status("rdma_en6", &["10.0.1.1"]), &status("rdma_en1", &[])));
    }
}
