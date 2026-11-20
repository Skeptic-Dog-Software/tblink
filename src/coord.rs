//! `tblink coordinator`: runs on the oMLX coordinator Mac as the
//! logged-in user. It replaces the zsh watchdog:
//!   - keeps the local oMLX brew service up and bound to every configured
//!     address that exists on this Mac,
//!   - works out the deployment the current ports call for (JACCL over the
//!     active RDMA ports on both Macs, or TCP Ring over the bridge addresses),
//!   - rebuilds and loads that deployment when the ports change, and falls back
//!     to Ring when JACCL will not load, retrying JACCL later.
//!
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
fn default_peer_tblink() -> String {
    "/usr/local/bin/tblink".into()
}
fn d15() -> u64 {
    15
}
fn d120() -> u64 {
    120
}
fn d1800() -> u64 {
    1800
}
fn d2() -> u32 {
    2
}

/// Expand a leading `~/` against `home`.
pub fn expand_with(p: &str, home: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => format!("{home}/{rest}"),
        None => p.to_string(),
    }
}

pub fn expand(p: &str) -> String {
    expand_with(p, &std::env::var("HOME").unwrap_or_default())
}

#[derive(Debug, Clone, PartialEq)]
pub enum Backend {
    Jaccl,
    Ring,
}

impl Backend {
    /// The name oMLX uses for the backend.
    pub fn name(&self) -> &'static str {
        match self {
            Backend::Jaccl => "jaccl",
            Backend::Ring => "ring",
        }
    }
}

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

/// JACCL when both RDMA ports are ready and no Ring fallback is in force.
pub fn choose_backend(cfg: &Config, local: &Status, peer: &Status, ring_hold: bool) -> Backend {
    if !jaccl_ready(cfg, local, peer) || ring_hold {
        Backend::Ring
    } else {
        Backend::Jaccl
    }
}

/// Compare only the fields that define the link, in the shape oMLX stores them.
pub fn hosts_match(current: &Value, want: &Value) -> bool {
    let norm = |v: &Value| -> Vec<(String, String, Value, Value)> {
        let mut out: Vec<_> = v
            .as_array()
            .into_iter()
            .flatten()
            .map(|h| {
                (
                    h["node_id"].as_str().unwrap_or("").to_string(),
                    h["ssh"].as_str().unwrap_or("").to_string(),
                    h["ips"].clone(),
                    h.get("rdma").cloned().unwrap_or(json!([])),
                )
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    };
    norm(current) == norm(want)
}

/// The admin API key from oMLX's settings.json text.
pub fn key_from_settings(text: &str) -> Option<String> {
    let v: Value = serde_json::from_str(text).ok()?;
    v["auth"]["api_key"].as_str().map(str::to_string)
}

/// Addresses oMLX is configured to listen on, and its port, from settings.json text.
pub fn parse_listen_config(text: Option<&str>) -> (Vec<String>, u16) {
    let v: Value = text.and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null);
    let hosts =
        v["server"]["host"].as_str().unwrap_or("127.0.0.1").split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    (hosts, v["server"]["port"].as_u64().unwrap_or(8000) as u16)
}

/// The stored deployment for `model`, if any.
pub fn current_deployment(deps: &Value, model: &str) -> Option<Value> {
    deps["deployments"].as_array().into_iter().flatten().find(|d| d["model"].as_str() == Some(model)).cloned()
}

/// Deployment ids with a live runtime job, from `/admin/api/cluster/status`.
pub fn live_ids(status: &Value) -> Vec<String> {
    status["runtime_jobs"]["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|j| j["live"].as_bool() == Some(true))
        .filter_map(|j| j["deployment_id"].as_str().map(str::to_string))
        .collect()
}

/// What the coordinator should do about the deployment this tick.
#[derive(Debug, PartialEq)]
pub enum Decision {
    /// The right deployment is live; nothing to do.
    Live(String),
    /// The right deployment exists but is not live; load it.
    Load(String),
    /// The stored deployment does not fit the ports: unload these live ids,
    /// then create and load a new one. `previous` is "" when none existed.
    Rebuild { previous: String, unload: Vec<String> },
}

pub fn decide(backend: &Backend, want_hosts: &Value, current: Option<&Value>, live: &[String]) -> Decision {
    let matches = current.map(|d| d["backend"].as_str() == Some(backend.name()) && hosts_match(&d["hosts"], want_hosts)).unwrap_or(false);
    let cur_id = current.and_then(|d| d["deployment_id"].as_str()).unwrap_or("").to_string();
    if matches && live.contains(&cur_id) {
        Decision::Live(cur_id)
    } else if matches {
        Decision::Load(cur_id)
    } else {
        let unload = live.iter().filter(|i| **i == cur_id).cloned().collect();
        Decision::Rebuild { previous: cur_id, unload }
    }
}

/// Body for POST /admin/api/cluster/deployments.
pub fn deployment_body(plan_request: &Value, backend: &Backend, plan: &Value, hosts: &Value) -> Value {
    let mut body = plan_request.clone();
    body["backend"] = json!(backend.name());
    body["auto_tune"] = json!(true);
    body["approved_placement"] = plan["placement_signature"].clone();
    body["hosts"] = hosts.clone();
    body
}

/// Whether a `/load` response reports the deployment loaded.
pub fn load_ok(resp: &Value) -> bool {
    resp["loaded"].as_bool() == Some(true)
}

/// Problems `--check` reports without contacting anything.
pub fn config_problems(cfg: &Config, key_present: bool) -> Vec<String> {
    let mut out = Vec::new();
    if cfg.plan_request["model_path"].as_str().is_none() {
        out.push("plan_request.model_path missing".to_string());
    }
    if !key_present {
        out.push(format!("no auth.api_key in {}", cfg.settings_path));
    }
    out
}

struct Api {
    base: String,
    key: String,
}

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
        let resp = match body {
            Some(b) => r.send_json(b.clone()),
            None => r.call(),
        }
        .map_err(err)?;
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
    key_from_settings(&std::fs::read_to_string(expand(path)).ok()?)
}

fn healthy(base: &str) -> bool {
    ureq::get(&format!("{base}/health"))
        .timeout(Duration::from_secs(5))
        .call()
        .ok()
        .and_then(|r| r.into_string().ok())
        .map(|s| s.contains("\"healthy\""))
        .unwrap_or(false)
}

fn listening(addr: &str, port: u16) -> bool {
    addr.parse().map(|ip| TcpStream::connect_timeout(&SocketAddr::new(ip, port), Duration::from_secs(2)).is_ok()).unwrap_or(false)
}

fn listen_config(settings_path: &str) -> (Vec<String>, u16) {
    parse_listen_config(std::fs::read_to_string(expand(settings_path)).ok().as_deref())
}

fn local_has_addr(addr: &str) -> bool {
    let (_, out) = net::run(net::IFCONFIG, &["-a"]);
    net::parse_inet(&out).iter().any(|a| a == addr)
}

fn peer_status(cfg: &Config) -> Option<Status> {
    for ip in [&cfg.peer.bridge_ip, &cfg.peer.rdma_ip] {
        let target = format!("{}@{}", cfg.peer.ssh_user, ip);
        let (ok, out) = net::run(
            "/usr/bin/ssh",
            &[
                "-i",
                &expand(&cfg.ssh_key),
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=3",
                "-o",
                "StrictHostKeyChecking=accept-new",
                &target,
                &cfg.peer_tblink,
                "status",
                "--json",
            ],
        );
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
    /// Log a steady-state message only when it changes. Returns whether it logged.
    fn note(&mut self, msg: String) -> bool {
        if msg != self.last_note {
            log(&msg);
            self.last_note = msg;
            return true;
        }
        false
    }
    /// A Ring fallback is in force.
    fn ring_hold(&self) -> bool {
        self.ring_until.map(|t| Instant::now() < t).unwrap_or(false)
    }
    /// Count a failed JACCL load. After enough of them, hold Ring for
    /// `jaccl_retry_secs` and allow an immediate load; returns true then.
    fn record_jaccl_failure(&mut self, cfg: &Config) -> bool {
        self.jaccl_failures += 1;
        if self.jaccl_failures >= cfg.jaccl_failures_before_ring {
            self.ring_until = Some(Instant::now() + Duration::from_secs(cfg.jaccl_retry_secs));
            self.jaccl_failures = 0;
            self.last_load = None;
            return true;
        }
        false
    }
}

pub fn main(config_path: &str, once: bool, check: bool) -> i32 {
    let cfg: Config = match std::fs::read_to_string(expand(config_path))
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot read {config_path}: {e}");
            return 1;
        }
    };
    if check {
        // Validate without changing anything: config, key, API, peer.
        let problems = config_problems(&cfg, read_key(&cfg.settings_path).is_some());
        for p in &problems {
            eprintln!("{p}");
        }
        println!("oMLX at {}: {}", cfg.api, if healthy(&cfg.api) { "healthy" } else { "not answering" });
        match peer_status(&cfg) {
            Some(p) => println!("peer {}: active port {}", p.hostname, p.active.map(|a| a.name).unwrap_or("none".into())),
            None => println!("peer: not reachable over SSH, or {} missing there", cfg.peer_tblink),
        }
        return if problems.is_empty() { 0 } else { 1 };
    }
    let marker = marker_path(config_path);
    let mut st = State::default();
    loop {
        if marker.exists() {
            // `tblink shutdown` paused us: leave oMLX and the deployment alone.
            st.note(format!("paused by `tblink shutdown` ({}); run `tblink resume` to bring the model back", marker.display()));
        } else {
            tick(&cfg, &mut st);
        }
        if once {
            return 0;
        }
        sleep_secs(cfg.interval_secs);
    }
}

/// The default coordinator config path.
pub const DEFAULT_CONFIG: &str = "~/.config/tblink/coordinator.json";

/// The pause marker: a file named `paused` next to the coordinator config.
/// While it exists the coordinator does nothing.
pub fn marker_path(config_path: &str) -> std::path::PathBuf {
    let config = std::path::PathBuf::from(expand(config_path));
    config.parent().map(|d| d.to_path_buf()).unwrap_or_default().join("paused")
}

/// Live deployments to unload on shutdown: this model's, if live.
pub fn to_unload(deps: &Value, status: &Value, model: &str) -> Vec<String> {
    let live = live_ids(status);
    deps["deployments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|d| d["model"].as_str() == Some(model))
        .filter_map(|d| d["deployment_id"].as_str().map(str::to_string))
        .filter(|id| live.contains(id))
        .collect()
}

/// `tblink shutdown`: pause the coordinator, unload the deployment, stop oMLX.
pub fn shutdown(config_path: &str) -> i32 {
    let cfg: Config = match std::fs::read_to_string(expand(config_path))
        .map_err(|e| e.to_string())
        .and_then(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("cannot read {config_path}: {e}");
            return 1;
        }
    };
    let marker = marker_path(config_path);
    if let Err(e) = std::fs::write(&marker, format!("paused by tblink shutdown at {}\n", crate::now_string())) {
        eprintln!("cannot write {}: {e}", marker.display());
        return 1;
    }
    println!("paused the coordinator ({})", marker.display());
    if healthy(&cfg.api) {
        if let Some(key) = read_key(&cfg.settings_path) {
            let api = Api { base: cfg.api.clone(), key };
            let model = cfg.plan_request["model_path"].as_str().unwrap_or_default();
            match (api.get("/admin/api/cluster/deployments"), api.get("/admin/api/cluster/status")) {
                (Ok(deps), Ok(status)) => {
                    for id in to_unload(&deps, &status, model) {
                        match api.post(&format!("/admin/api/cluster/deployments/{id}/unload"), None, 300) {
                            Ok(_) => println!("unloaded {id}"),
                            Err(e) => eprintln!("unload {id}: {e}"),
                        }
                    }
                }
                (Err(e), _) | (_, Err(e)) => eprintln!("could not read deployments: {e}"),
            }
        } else {
            eprintln!("no API key in {}; skipping unload", cfg.settings_path);
        }
    } else {
        println!("oMLX is not running");
    }
    let (ok, out) = net::run(&cfg.brew, &["services", "stop", &cfg.service]);
    if ok {
        println!("stopped {}", cfg.service);
    } else {
        eprintln!("brew services stop {} failed: {}", cfg.service, out.trim());
    }
    println!("run `tblink resume` to bring the model back");
    0
}

/// `tblink resume`: remove the pause marker; the coordinator restarts oMLX
/// and reloads the deployment on its next check.
pub fn resume(config_path: &str) -> i32 {
    let marker = marker_path(config_path);
    match std::fs::remove_file(&marker) {
        Ok(()) => {
            println!("resumed: the coordinator will start oMLX and load the deployment on its next check");
            0
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("not paused ({} does not exist)", marker.display());
            0
        }
        Err(e) => {
            eprintln!("cannot remove {}: {e}", marker.display());
            1
        }
    }
}

fn brew(cfg: &Config, verb: &str, st: &mut State, why: &str) {
    if !State::due(st.last_service, cfg.backoff_secs) {
        return;
    }
    st.last_service = Some(Instant::now());
    log(&format!("{why}; brew services {verb} {}", cfg.service));
    let (ok, out) = net::run(&cfg.brew, &["services", verb, &cfg.service]);
    if !ok {
        log(&format!("brew services {verb} failed: {}", out.trim()));
    }
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
    let Some(key) = read_key(&cfg.settings_path) else {
        st.note("cannot read the oMLX API key".into());
        return;
    };
    let api = Api { base: cfg.api.clone(), key };

    // 3. Port state on both Macs.
    let local = net::status();
    let Some(peer) = peer_status(cfg) else {
        st.note("peer unreachable over SSH; waiting".into());
        return;
    };

    let backend = choose_backend(cfg, &local, &peer, st.ring_hold());
    let want_hosts = desired_hosts(cfg, &backend, &local, &peer);
    let backend_name = backend.name();

    // 4. Current deployment for this model, and whether it is live.
    let model = cfg.plan_request["model_path"].as_str().unwrap_or_default();
    let deps = match api.get("/admin/api/cluster/deployments") {
        Ok(v) => v,
        Err(e) => {
            st.note(format!("deployments: {e}"));
            return;
        }
    };
    let current = current_deployment(&deps, model);
    let status = match api.get("/admin/api/cluster/status") {
        Ok(v) => v,
        Err(e) => {
            st.note(format!("status: {e}"));
            return;
        }
    };
    let live = live_ids(&status);

    let decision = decide(&backend, &want_hosts, current.as_ref(), &live);
    if let Decision::Live(cur_id) = &decision {
        if let Some(t) = st.down_since.take() {
            log(&format!("{cur_id} live again (down {} s)", t.elapsed().as_secs()));
        }
        if backend == Backend::Jaccl {
            st.jaccl_failures = 0;
        }
        st.note(format!(
            "ok: {cur_id} live on {backend_name} ({} ↔ {})",
            local.active.as_ref().map(|p| p.rdma_device.as_str()).unwrap_or("-"),
            peer.active.as_ref().map(|p| p.rdma_device.as_str()).unwrap_or("-")
        ));
        return;
    }
    st.down_since.get_or_insert_with(Instant::now);
    if !State::due(st.last_load, cfg.backoff_secs) {
        return;
    }
    st.last_load = Some(Instant::now());

    // 5. Rebuild when the ports (or backend) changed.
    let id = match decision {
        Decision::Live(_) => unreachable!(),
        Decision::Load(id) => id,
        Decision::Rebuild { previous, unload } => {
            log(&format!(
                "ports call for {backend_name} with hosts {want_hosts}; rebuilding (was {})",
                if previous.is_empty() { "none" } else { &previous }
            ));
            for live in &unload {
                match api.post(&format!("/admin/api/cluster/deployments/{live}/unload"), None, 300) {
                    Ok(_) => log(&format!("unloaded {live}")),
                    Err(e) => {
                        log(&format!("unload {live}: {e}"));
                        return;
                    }
                }
            }
            let plan = match api.post("/admin/api/cluster/plan", Some(&cfg.plan_request), 180) {
                Ok(v) => v,
                Err(e) => {
                    log(&format!("plan: {e}"));
                    return;
                }
            };
            let body = deployment_body(&cfg.plan_request, &backend, &plan, &want_hosts);
            match api.post("/admin/api/cluster/deployments", Some(&body), 300) {
                Ok(v) => v["deployment"]["deployment_id"].as_str().unwrap_or_default().to_string(),
                Err(e) => {
                    log(&format!("create deployment: {e}"));
                    return;
                }
            }
        }
    };

    // 6. Load.
    let t0 = Instant::now();
    match api.post(&format!("/admin/api/cluster/deployments/{id}/load"), None, 900) {
        Ok(v) if load_ok(&v) => log(&format!("loaded {id} on {backend_name} in {} s", t0.elapsed().as_secs())),
        r => {
            let why = match r {
                Ok(v) => v.to_string().chars().take(300).collect(),
                Err(e) => e,
            };
            log(&format!("load {id} on {backend_name} failed after {} s: {why}", t0.elapsed().as_secs()));
            if backend == Backend::Jaccl && st.record_jaccl_failure(cfg) {
                log(&format!("JACCL failed {} times; falling back to Ring for {} s", cfg.jaccl_failures_before_ring, cfg.jaccl_retry_secs));
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
        }))
        .unwrap()
    }
    fn status(dev: &str, inet: &[&str]) -> Status {
        let p = Port {
            name: dev.trim_start_matches("rdma_").into(),
            rdma_device: dev.into(),
            active: true,
            bridge_member: false,
            inet: inet.iter().map(|s| s.to_string()).collect(),
        };
        Status { hostname: "h".into(), active: Some(p.clone()), ports: vec![p], bridge_inet: vec![], bridge_members: vec![] }
    }
    fn unplugged() -> Status {
        Status { hostname: "h".into(), active: None, ports: vec![], bridge_inet: vec![], bridge_members: vec![] }
    }
    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }
    fn jaccl_pair() -> (Status, Status) {
        (status("rdma_en6", &["10.0.1.1", "10.77.0.1"]), status("rdma_en1", &["10.0.1.2", "10.77.0.2"]))
    }

    // ---- config ----

    #[test]
    fn config_defaults() {
        let c = cfg();
        assert_eq!(c.peer_tblink, "/usr/local/bin/tblink");
        assert_eq!((c.interval_secs, c.backoff_secs, c.jaccl_retry_secs, c.jaccl_failures_before_ring), (15, 120, 1800, 2));
        assert_eq!(c.local.ssh_user, "");
        assert_eq!(c.peer.ssh_user, "peeruser");
    }

    #[test]
    fn config_overrides() {
        let mut v = serde_json::to_value(json!({
            "api": "a", "settings_path": "s", "brew": "b", "service": "x", "ssh_key": "k", "peer_tblink": "/opt/tblink",
            "local": {"node_id": "A", "rdma_ip": "1", "bridge_ip": "2"},
            "peer": {"node_id": "B", "rdma_ip": "3", "bridge_ip": "4"},
            "plan_request": {},
        }))
        .unwrap();
        for (k, n) in [("interval_secs", 5), ("backoff_secs", 30), ("jaccl_retry_secs", 60), ("jaccl_failures_before_ring", 1)] {
            v[k] = json!(n);
        }
        let c: Config = serde_json::from_value(v).unwrap();
        assert_eq!(c.peer_tblink, "/opt/tblink");
        assert_eq!((c.interval_secs, c.backoff_secs, c.jaccl_retry_secs, c.jaccl_failures_before_ring), (5, 30, 60, 1));
    }

    #[test]
    fn config_requires_core_fields() {
        for missing in ["api", "settings_path", "brew", "service", "ssh_key", "local", "peer", "plan_request"] {
            let mut v = serde_json::to_value(json!({
                "api": "a", "settings_path": "s", "brew": "b", "service": "x", "ssh_key": "k",
                "local": {"node_id": "A", "rdma_ip": "1", "bridge_ip": "2"},
                "peer": {"node_id": "B", "rdma_ip": "3", "bridge_ip": "4"},
                "plan_request": {},
            }))
            .unwrap();
            v.as_object_mut().unwrap().remove(missing);
            assert!(serde_json::from_value::<Config>(v).is_err(), "{missing} should be required");
        }
    }

    #[test]
    fn example_config_parses() {
        let c: Config = serde_json::from_str(include_str!("../examples/coordinator.json")).unwrap();
        assert_eq!(c.local.rdma_ip, "10.0.1.1");
        assert_eq!(c.peer.bridge_ip, "10.77.0.2");
        assert_eq!(c.plan_request["tensor_parallel_size"], 2);
        assert!(c.plan_request["model_path"].is_string());
    }

    // ---- expand ----

    #[test]
    fn expand_tilde_slash() {
        assert_eq!(expand_with("~/.omlx/settings.json", "/Users/me"), "/Users/me/.omlx/settings.json");
    }

    #[test]
    fn expand_leaves_other_paths() {
        assert_eq!(expand_with("/abs/path", "/Users/me"), "/abs/path");
        assert_eq!(expand_with("relative", "/Users/me"), "relative");
        assert_eq!(expand_with("~", "/Users/me"), "~");
        assert_eq!(expand_with("~other/x", "/Users/me"), "~other/x");
        assert_eq!(expand_with("a/~/b", "/Users/me"), "a/~/b");
    }

    #[test]
    fn expand_with_empty_home() {
        assert_eq!(expand_with("~/x", ""), "/x");
    }

    // ---- backend ----

    #[test]
    fn backend_names() {
        assert_eq!(Backend::Jaccl.name(), "jaccl");
        assert_eq!(Backend::Ring.name(), "ring");
    }

    #[test]
    fn jaccl_needs_rdma_addresses_on_both_active_ports() {
        let c = cfg();
        assert!(jaccl_ready(&c, &status("rdma_en6", &["10.0.1.1"]), &status("rdma_en1", &["10.0.1.2"])));
        assert!(!jaccl_ready(&c, &status("rdma_en6", &["10.0.1.1"]), &status("rdma_en1", &[])));
        assert!(!jaccl_ready(&c, &status("rdma_en6", &[]), &status("rdma_en1", &["10.0.1.2"])));
    }

    #[test]
    fn jaccl_not_ready_when_a_cable_is_out() {
        let c = cfg();
        assert!(!jaccl_ready(&c, &unplugged(), &status("rdma_en1", &["10.0.1.2"])));
        assert!(!jaccl_ready(&c, &status("rdma_en6", &["10.0.1.1"]), &unplugged()));
    }

    #[test]
    fn jaccl_not_ready_with_swapped_addresses() {
        let c = cfg();
        assert!(!jaccl_ready(&c, &status("rdma_en6", &["10.0.1.2"]), &status("rdma_en1", &["10.0.1.1"])));
    }

    #[test]
    fn bridge_address_alone_is_not_enough_for_jaccl() {
        let c = cfg();
        assert!(!jaccl_ready(&c, &status("rdma_en6", &["10.77.0.1"]), &status("rdma_en1", &["10.77.0.2"])));
    }

    #[test]
    fn choose_backend_cases() {
        let c = cfg();
        let (l, p) = jaccl_pair();
        assert_eq!(choose_backend(&c, &l, &p, false), Backend::Jaccl);
        assert_eq!(choose_backend(&c, &l, &p, true), Backend::Ring);
        assert_eq!(choose_backend(&c, &l, &unplugged(), false), Backend::Ring);
        assert_eq!(choose_backend(&c, &unplugged(), &unplugged(), true), Backend::Ring);
    }

    // ---- desired_hosts ----

    #[test]
    fn jaccl_hosts_follow_the_active_ports() {
        let c = cfg();
        let h = desired_hosts(&c, &Backend::Jaccl, &status("rdma_en6", &["10.0.1.1"]), &status("rdma_en1", &["10.0.1.2"]));
        assert_eq!(
            h,
            json!([
                {"node_id": "A", "ssh": "127.0.0.1", "ips": ["10.0.1.1"], "rdma": [null, "rdma_en6"]},
                {"node_id": "B", "ssh": "peeruser@10.77.0.2", "ips": ["10.0.1.2"], "rdma": ["rdma_en1", null]},
            ])
        );
    }

    #[test]
    fn ring_hosts_use_bridge_addresses_and_no_rdma() {
        let c = cfg();
        let h = desired_hosts(&c, &Backend::Ring, &unplugged(), &unplugged());
        assert_eq!(
            h,
            json!([
                {"node_id": "A", "ssh": "127.0.0.1", "ips": ["10.77.0.1"], "rdma": []},
                {"node_id": "B", "ssh": "peeruser@10.77.0.2", "ips": ["10.77.0.2"], "rdma": []},
            ])
        );
    }

    #[test]
    fn ring_hosts_ignore_the_ports() {
        let c = cfg();
        let (l, p) = jaccl_pair();
        assert_eq!(desired_hosts(&c, &Backend::Ring, &l, &p), desired_hosts(&c, &Backend::Ring, &unplugged(), &unplugged()));
    }

    #[test]
    fn peer_is_reached_over_its_bridge_address_on_both_backends() {
        let c = cfg();
        let (l, p) = jaccl_pair();
        for b in [Backend::Jaccl, Backend::Ring] {
            assert_eq!(desired_hosts(&c, &b, &l, &p)[1]["ssh"], "peeruser@10.77.0.2");
            assert_eq!(desired_hosts(&c, &b, &l, &p)[0]["ssh"], "127.0.0.1");
        }
    }

    #[test]
    #[should_panic(expected = "jaccl needs local port")]
    fn jaccl_hosts_without_a_local_port_panic() {
        desired_hosts(&cfg(), &Backend::Jaccl, &unplugged(), &status("rdma_en1", &[]));
    }

    #[test]
    #[should_panic(expected = "jaccl needs peer port")]
    fn jaccl_hosts_without_a_peer_port_panic() {
        desired_hosts(&cfg(), &Backend::Jaccl, &status("rdma_en6", &[]), &unplugged());
    }

    #[test]
    fn choose_backend_never_picks_jaccl_without_ports() {
        // desired_hosts panics for JACCL without ports; choose_backend must rule that out.
        let c = cfg();
        for (l, p) in [(unplugged(), unplugged()), (unplugged(), jaccl_pair().1), (jaccl_pair().0, unplugged())] {
            let b = choose_backend(&c, &l, &p, false);
            desired_hosts(&c, &b, &l, &p);
            assert_eq!(b, Backend::Ring);
        }
    }

    // ---- hosts_match ----

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
    fn hosts_match_ignores_order_and_extra_fields() {
        let want = json!([
            {"node_id": "A", "ssh": "127.0.0.1", "ips": ["10.0.1.1"], "rdma": [null, "rdma_en6"]},
            {"node_id": "B", "ssh": "u@10.77.0.2", "ips": ["10.0.1.2"], "rdma": ["rdma_en1", null]},
        ]);
        let stored = json!([
            {"node_id": "B", "ssh": "u@10.77.0.2", "ips": ["10.0.1.2"], "rdma": ["rdma_en1", null], "hostname": "Mac-B", "chip": "M5 Pro"},
            {"node_id": "A", "ssh": "127.0.0.1", "ips": ["10.0.1.1"], "rdma": [null, "rdma_en6"], "rank": 0},
        ]);
        assert!(hosts_match(&stored, &want));
    }

    #[test]
    fn hosts_match_treats_missing_rdma_as_empty() {
        let want = json!([{"node_id": "A", "ssh": "s", "ips": ["1"], "rdma": []}]);
        assert!(hosts_match(&json!([{"node_id": "A", "ssh": "s", "ips": ["1"]}]), &want));
    }

    #[test]
    fn hosts_match_detects_each_field() {
        let base = json!([{"node_id": "A", "ssh": "s", "ips": ["1"], "rdma": [null, "rdma_en1"]}]);
        for changed in [
            json!([{"node_id": "Z", "ssh": "s", "ips": ["1"], "rdma": [null, "rdma_en1"]}]),
            json!([{"node_id": "A", "ssh": "t", "ips": ["1"], "rdma": [null, "rdma_en1"]}]),
            json!([{"node_id": "A", "ssh": "s", "ips": ["2"], "rdma": [null, "rdma_en1"]}]),
            json!([{"node_id": "A", "ssh": "s", "ips": ["1"], "rdma": ["rdma_en1", null]}]),
            json!([{"node_id": "A", "ssh": "s", "ips": ["1"], "rdma": []}]),
        ] {
            assert!(!hosts_match(&changed, &base), "{changed}");
        }
    }

    #[test]
    fn hosts_match_counts_hosts() {
        let one = json!([{"node_id": "A", "ssh": "s", "ips": ["1"], "rdma": []}]);
        let two = json!([{"node_id": "A", "ssh": "s", "ips": ["1"], "rdma": []}, {"node_id": "B", "ssh": "s", "ips": ["2"], "rdma": []}]);
        assert!(!hosts_match(&one, &two));
        assert!(!hosts_match(&two, &one));
    }

    #[test]
    fn hosts_match_non_array_is_empty() {
        assert!(!hosts_match(&Value::Null, &json!([{"node_id": "A"}])));
        assert!(hosts_match(&Value::Null, &json!([])));
        assert!(hosts_match(&json!({"x": 1}), &json!([])));
    }

    #[test]
    fn ring_and_jaccl_hosts_never_match() {
        let c = cfg();
        let (l, p) = jaccl_pair();
        assert!(!hosts_match(&desired_hosts(&c, &Backend::Ring, &l, &p), &desired_hosts(&c, &Backend::Jaccl, &l, &p)));
    }

    // ---- settings.json ----

    #[test]
    fn key_from_settings_cases() {
        assert_eq!(key_from_settings(r#"{"auth":{"api_key":"k1"}}"#), Some("k1".into()));
        assert_eq!(key_from_settings(r#"{"auth":{}}"#), None);
        assert_eq!(key_from_settings(r#"{"auth":{"api_key":42}}"#), None);
        assert_eq!(key_from_settings(r#"{}"#), None);
        assert_eq!(key_from_settings("not json"), None);
        assert_eq!(key_from_settings(""), None);
    }

    #[test]
    fn key_from_settings_ignores_sub_keys() {
        let s = r#"{"auth":{"api_key":"main","sub_keys":[{"name":"x","key":"sub"}]}}"#;
        assert_eq!(key_from_settings(s), Some("main".into()));
    }

    #[test]
    fn listen_config_multiple_hosts() {
        let s = r#"{"server":{"host":"127.0.0.1, 10.0.1.1 ,10.77.0.1","port":8000}}"#;
        assert_eq!(parse_listen_config(Some(s)), (ids(&["127.0.0.1", "10.0.1.1", "10.77.0.1"]), 8000));
    }

    #[test]
    fn listen_config_custom_port() {
        assert_eq!(parse_listen_config(Some(r#"{"server":{"host":"0.0.0.0","port":8080}}"#)), (ids(&["0.0.0.0"]), 8080));
    }

    #[test]
    fn listen_config_defaults() {
        assert_eq!(parse_listen_config(None), (ids(&["127.0.0.1"]), 8000));
        assert_eq!(parse_listen_config(Some("{")), (ids(&["127.0.0.1"]), 8000));
        assert_eq!(parse_listen_config(Some("{}")), (ids(&["127.0.0.1"]), 8000));
        assert_eq!(parse_listen_config(Some(r#"{"server":{"port":"8000"}}"#)), (ids(&["127.0.0.1"]), 8000));
    }

    #[test]
    fn listen_config_drops_empty_entries() {
        assert_eq!(parse_listen_config(Some(r#"{"server":{"host":"127.0.0.1,,  ,10.0.1.1,"}}"#)).0, ids(&["127.0.0.1", "10.0.1.1"]));
        assert!(parse_listen_config(Some(r#"{"server":{"host":""}}"#)).0.is_empty());
    }

    // ---- deployments and runtime jobs ----

    #[test]
    fn current_deployment_picks_the_model() {
        let deps = json!({"deployments": [
            {"deployment_id": "x-1", "model": "/other"},
            {"deployment_id": "m-1", "model": "/m"},
            {"deployment_id": "m-2", "model": "/m"},
        ]});
        assert_eq!(current_deployment(&deps, "/m").unwrap()["deployment_id"], "m-1");
        assert_eq!(current_deployment(&deps, "/none"), None);
    }

    #[test]
    fn current_deployment_on_odd_responses() {
        assert_eq!(current_deployment(&json!({}), "/m"), None);
        assert_eq!(current_deployment(&json!({"deployments": null}), "/m"), None);
        assert_eq!(current_deployment(&json!({"deployments": [{"deployment_id": "a"}]}), "/m"), None);
    }

    #[test]
    fn live_ids_only_count_live_jobs_with_ids() {
        let st = json!({"runtime_jobs": {"jobs": [
            {"deployment_id": "a", "live": true},
            {"deployment_id": "b", "live": false},
            {"deployment_id": "c"},
            {"live": true},
            {"deployment_id": "d", "live": "true"},
            {"deployment_id": "e", "live": true},
        ]}});
        assert_eq!(live_ids(&st), ids(&["a", "e"]));
    }

    #[test]
    fn live_ids_on_odd_responses() {
        assert!(live_ids(&json!({})).is_empty());
        assert!(live_ids(&json!({"runtime_jobs": {}})).is_empty());
        assert!(live_ids(&json!({"runtime_jobs": {"jobs": []}})).is_empty());
    }

    // ---- decide ----

    fn stored(id: &str, backend: &Backend, hosts: &Value) -> Value {
        json!({"deployment_id": id, "model": "/m", "backend": backend.name(), "hosts": hosts})
    }

    #[test]
    fn decide_live_when_matching_and_running() {
        let c = cfg();
        let (l, p) = jaccl_pair();
        let want = desired_hosts(&c, &Backend::Jaccl, &l, &p);
        let cur = stored("d1", &Backend::Jaccl, &want);
        assert_eq!(decide(&Backend::Jaccl, &want, Some(&cur), &ids(&["d1"])), Decision::Live("d1".into()));
    }

    #[test]
    fn decide_load_when_matching_but_down() {
        // The stale-loaded case after a peer reboot: right deployment, no live job.
        let c = cfg();
        let (l, p) = jaccl_pair();
        let want = desired_hosts(&c, &Backend::Jaccl, &l, &p);
        let cur = stored("d1", &Backend::Jaccl, &want);
        assert_eq!(decide(&Backend::Jaccl, &want, Some(&cur), &[]), Decision::Load("d1".into()));
        assert_eq!(decide(&Backend::Jaccl, &want, Some(&cur), &ids(&["other"])), Decision::Load("d1".into()));
    }

    #[test]
    fn decide_rebuild_unloads_the_live_old_deployment() {
        let c = cfg();
        let (l, p) = jaccl_pair();
        let old = stored("d1", &Backend::Jaccl, &desired_hosts(&c, &Backend::Jaccl, &status("rdma_en1", &[]), &p));
        let want = desired_hosts(&c, &Backend::Jaccl, &l, &p);
        assert_eq!(
            decide(&Backend::Jaccl, &want, Some(&old), &ids(&["d1"])),
            Decision::Rebuild { previous: "d1".into(), unload: ids(&["d1"]) }
        );
    }

    #[test]
    fn decide_rebuild_without_unload_when_old_is_down() {
        let c = cfg();
        let (l, p) = jaccl_pair();
        let old = stored("d1", &Backend::Ring, &desired_hosts(&c, &Backend::Ring, &l, &p));
        let want = desired_hosts(&c, &Backend::Jaccl, &l, &p);
        assert_eq!(decide(&Backend::Jaccl, &want, Some(&old), &[]), Decision::Rebuild { previous: "d1".into(), unload: vec![] });
    }

    #[test]
    fn decide_rebuild_never_unloads_other_models() {
        let c = cfg();
        let (l, p) = jaccl_pair();
        let want = desired_hosts(&c, &Backend::Jaccl, &l, &p);
        let old = stored("d1", &Backend::Ring, &desired_hosts(&c, &Backend::Ring, &l, &p));
        assert_eq!(
            decide(&Backend::Jaccl, &want, Some(&old), &ids(&["other-model", "d1"])),
            Decision::Rebuild { previous: "d1".into(), unload: ids(&["d1"]) }
        );
    }

    #[test]
    fn decide_rebuild_when_backend_changes_even_if_hosts_match() {
        let want = json!([{"node_id": "A", "ssh": "s", "ips": ["1"], "rdma": []}]);
        let cur = json!({"deployment_id": "d1", "backend": "ring", "hosts": want});
        assert_eq!(
            decide(&Backend::Jaccl, &want, Some(&cur), &ids(&["d1"])),
            Decision::Rebuild { previous: "d1".into(), unload: ids(&["d1"]) }
        );
    }

    #[test]
    fn decide_rebuild_from_nothing() {
        let c = cfg();
        let want = desired_hosts(&c, &Backend::Ring, &unplugged(), &unplugged());
        assert_eq!(decide(&Backend::Ring, &want, None, &ids(&["x"])), Decision::Rebuild { previous: "".into(), unload: vec![] });
    }

    #[test]
    fn decide_fallback_to_ring_rebuilds_a_live_jaccl_deployment() {
        let c = cfg();
        let (l, p) = jaccl_pair();
        let cur = stored("j1", &Backend::Jaccl, &desired_hosts(&c, &Backend::Jaccl, &l, &p));
        let want = desired_hosts(&c, &Backend::Ring, &l, &p);
        assert_eq!(
            decide(&Backend::Ring, &want, Some(&cur), &ids(&["j1"])),
            Decision::Rebuild { previous: "j1".into(), unload: ids(&["j1"]) }
        );
    }

    #[test]
    fn decide_with_missing_backend_field_rebuilds() {
        let want = json!([]);
        let cur = json!({"deployment_id": "d1", "hosts": []});
        assert!(matches!(decide(&Backend::Ring, &want, Some(&cur), &[]), Decision::Rebuild { .. }));
    }

    // ---- deployment body and load response ----

    #[test]
    fn deployment_body_extends_the_plan_request() {
        let req = json!({"model_path": "/m", "tensor_parallel_size": 2, "target_context_tokens": 32768});
        let plan = json!({"placement_signature": "sig-1", "other": 1});
        let hosts = json!([{"node_id": "A"}]);
        assert_eq!(
            deployment_body(&req, &Backend::Jaccl, &plan, &hosts),
            json!({
                "model_path": "/m", "tensor_parallel_size": 2, "target_context_tokens": 32768,
                "backend": "jaccl", "auto_tune": true, "approved_placement": "sig-1", "hosts": [{"node_id": "A"}],
            })
        );
    }

    #[test]
    fn deployment_body_overrides_conflicting_keys() {
        let req = json!({"model_path": "/m", "backend": "stale", "auto_tune": false, "hosts": "stale"});
        let b = deployment_body(&req, &Backend::Ring, &json!({"placement_signature": "s"}), &json!([]));
        assert_eq!(b["backend"], "ring");
        assert_eq!(b["auto_tune"], true);
        assert_eq!(b["hosts"], json!([]));
    }

    #[test]
    fn deployment_body_without_a_signature() {
        let b = deployment_body(&json!({}), &Backend::Ring, &json!({}), &json!([]));
        assert_eq!(b["approved_placement"], Value::Null);
    }

    #[test]
    fn load_ok_needs_loaded_true() {
        assert!(load_ok(&json!({"loaded": true, "canary": "passed"})));
        assert!(!load_ok(&json!({"loaded": false})));
        assert!(!load_ok(&json!({"loaded": "true"})));
        assert!(!load_ok(&json!({})));
        assert!(!load_ok(&Value::Null));
    }

    // ---- shutdown / resume ----

    #[test]
    fn marker_sits_next_to_the_config() {
        assert_eq!(marker_path("/etc/tb/coordinator.json"), std::path::PathBuf::from("/etc/tb/paused"));
        assert!(marker_path(DEFAULT_CONFIG).ends_with(".config/tblink/paused"));
    }

    #[test]
    fn shutdown_unloads_only_this_models_live_deployment() {
        let deps = json!({"deployments": [
            {"deployment_id": "m-1", "model": "/m"},
            {"deployment_id": "x-1", "model": "/other"},
        ]});
        let status = json!({"runtime_jobs": {"jobs": [
            {"deployment_id": "m-1", "live": true},
            {"deployment_id": "x-1", "live": true},
        ]}});
        assert_eq!(to_unload(&deps, &status, "/m"), vec!["m-1".to_string()]);
        let idle = json!({"runtime_jobs": {"jobs": []}});
        assert!(to_unload(&deps, &idle, "/m").is_empty(), "nothing live, nothing to unload");
    }

    #[test]
    fn resume_without_a_marker_is_fine() {
        let d = tempfile::tempdir().unwrap();
        let cfg = d.path().join("coordinator.json");
        assert_eq!(resume(cfg.to_str().unwrap()), 0);
        std::fs::write(marker_path(cfg.to_str().unwrap()), "x").unwrap();
        assert_eq!(resume(cfg.to_str().unwrap()), 0);
        assert!(!marker_path(cfg.to_str().unwrap()).exists());
    }

    // ---- --check ----

    #[test]
    fn check_reports_missing_model_and_key() {
        let mut c = cfg();
        assert!(config_problems(&c, true).is_empty());
        assert_eq!(config_problems(&c, false), vec!["no auth.api_key in ~/.omlx/settings.json"]);
        c.plan_request = json!({});
        assert_eq!(config_problems(&c, true), vec!["plan_request.model_path missing"]);
        c.plan_request = json!({"model_path": 5});
        assert_eq!(config_problems(&c, false).len(), 2);
    }

    // ---- state: backoff and fallback ----

    #[test]
    fn due_when_never_done() {
        assert!(State::due(None, 120));
        assert!(State::due(None, 0));
    }

    #[test]
    fn not_due_right_after() {
        assert!(!State::due(Some(Instant::now()), 120));
        assert!(State::due(Some(Instant::now()), 0));
    }

    #[test]
    fn due_after_the_interval() {
        let past = Instant::now().checked_sub(Duration::from_secs(121)).unwrap();
        assert!(State::due(Some(past), 120));
    }

    #[test]
    fn note_logs_only_changes() {
        let mut s = State::default();
        assert!(s.note("a".into()));
        assert!(!s.note("a".into()));
        assert!(s.note("b".into()));
        assert!(s.note("a".into()));
        assert_eq!(s.last_note, "a");
    }

    #[test]
    fn no_ring_hold_by_default() {
        assert!(!State::default().ring_hold());
    }

    #[test]
    fn ring_hold_expires() {
        let mut s = State { ring_until: Some(Instant::now() + Duration::from_secs(60)), ..State::default() };
        assert!(s.ring_hold());
        s.ring_until = Some(Instant::now().checked_sub(Duration::from_secs(1)).unwrap());
        assert!(!s.ring_hold());
    }

    #[test]
    fn jaccl_falls_back_after_the_configured_failures() {
        let c = cfg();
        let mut s = State { last_load: Some(Instant::now()), ..State::default() };
        assert!(!s.record_jaccl_failure(&c));
        assert_eq!(s.jaccl_failures, 1);
        assert!(!s.ring_hold());
        assert!(s.last_load.is_some());

        assert!(s.record_jaccl_failure(&c));
        assert_eq!(s.jaccl_failures, 0);
        assert!(s.ring_hold());
        assert!(s.last_load.is_none(), "the Ring load should not wait for the backoff");
    }

    #[test]
    fn ring_hold_lasts_the_retry_period() {
        let c = cfg();
        let mut s = State::default();
        let before = Instant::now();
        s.record_jaccl_failure(&c);
        s.record_jaccl_failure(&c);
        let held = s.ring_until.unwrap().duration_since(before).as_secs();
        assert!((1799..=1801).contains(&held), "{held}");
    }

    #[test]
    fn single_failure_threshold_falls_back_at_once() {
        let mut c = cfg();
        c.jaccl_failures_before_ring = 1;
        let mut s = State::default();
        assert!(s.record_jaccl_failure(&c));
        assert!(s.ring_hold());
    }

    #[test]
    fn counting_restarts_after_a_fallback() {
        let c = cfg();
        let mut s = State::default();
        s.record_jaccl_failure(&c);
        s.record_jaccl_failure(&c);
        assert!(!s.record_jaccl_failure(&c));
        assert_eq!(s.jaccl_failures, 1);
    }

    #[test]
    fn fallback_then_retry_flow() {
        // Two JACCL failures -> Ring is chosen while held -> JACCL again when the hold expires.
        let c = cfg();
        let (l, p) = jaccl_pair();
        let mut s = State::default();
        assert_eq!(choose_backend(&c, &l, &p, s.ring_hold()), Backend::Jaccl);
        s.record_jaccl_failure(&c);
        assert_eq!(choose_backend(&c, &l, &p, s.ring_hold()), Backend::Jaccl);
        s.record_jaccl_failure(&c);
        assert_eq!(choose_backend(&c, &l, &p, s.ring_hold()), Backend::Ring);
        s.ring_until = Some(Instant::now().checked_sub(Duration::from_secs(1)).unwrap());
        assert_eq!(choose_backend(&c, &l, &p, s.ring_hold()), Backend::Jaccl);
    }
}
