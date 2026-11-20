//! `tblink setup`: a guided setup, run once on each Mac.
//!
//! It detects what it can (this Mac's oMLX node id, its Thunderbolt ports and
//! speeds, oMLX's model folder and port, the other Mac over SSH) and asks only
//! for the rest: which Mac is the coordinator, the login on the other Mac, and
//! the model. On the coordinator it writes ~/.config/tblink/coordinator.json
//! (with memory budgets measured by oMLX when it is running). It installs the
//! port daemon (with sudo) and, on the coordinator, the coordinator agent, via
//! install.sh, after asking. It reads one secret, oMLX's admin key, only to
//! call oMLX's own API, exactly as the coordinator does; the key is never
//! printed, logged or put on a command line.

use crate::coord::{self, expand};
use crate::net;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

pub const DEFAULT_KEY: &str = "~/.ssh/omlx_cluster";
pub const SETTINGS: &str = "~/.omlx/settings.json";

/// This Mac's and the other Mac's addresses.
#[derive(Debug, Clone, PartialEq)]
pub struct Addresses {
    pub rdma: String,
    pub bridge: String,
    pub peer_rdma: String,
    pub peer_bridge: String,
}

/// The standard plan: the coordinator is .1, the other Mac .2.
pub fn standard_addresses(coordinator: bool) -> Addresses {
    let (me, them) = if coordinator { (1, 2) } else { (2, 1) };
    Addresses {
        rdma: format!("10.0.1.{me}"),
        bridge: format!("10.77.0.{me}"),
        peer_rdma: format!("10.0.1.{them}"),
        peer_bridge: format!("10.77.0.{them}"),
    }
}

/// The install.sh arguments for the port daemon.
pub fn port_install_args(a: &Addresses) -> Vec<String> {
    vec!["port".into(), a.rdma.clone(), a.bridge.clone(), a.peer_rdma.clone(), a.peer_bridge.clone()]
}

/// What the coordinator config needs beyond the defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct Answers {
    pub addresses: Addresses,
    pub local_node: String,
    pub peer_node: String,
    pub peer_user: String,
    pub ssh_key: String,
    pub api_port: u16,
    pub brew: String,
    pub model_path: String,
    /// Bytes each Mac can give the model: (this Mac, the other Mac).
    pub capacity: (u64, u64),
    pub context_tokens: u64,
}

/// The coordinator config for `a`, keeping anything else an existing config had.
pub fn build_config(a: &Answers, existing: Option<&Value>) -> Value {
    let nodes = json!([
        {"node_id": a.local_node, "capacity_bytes": a.capacity.0, "role": "headless", "memory_guard_tier": "aggressive"},
        {"node_id": a.peer_node, "capacity_bytes": a.capacity.1, "role": "headless", "memory_guard_tier": "aggressive"},
    ]);
    let fresh = json!({
        "api": format!("http://127.0.0.1:{}", a.api_port),
        "settings_path": SETTINGS,
        "brew": a.brew,
        "service": "jundot/omlx/omlx",
        "ssh_key": a.ssh_key,
        "peer_tblink": "/usr/local/bin/tblink",
        "local": {"node_id": a.local_node, "rdma_ip": a.addresses.rdma, "bridge_ip": a.addresses.bridge},
        "peer": {"node_id": a.peer_node, "rdma_ip": a.addresses.peer_rdma, "bridge_ip": a.addresses.peer_bridge, "ssh_user": a.peer_user},
        "plan_request": {
            "model_path": a.model_path,
            "nodes": nodes,
            "execution_profile": "interactive",
            "target_context_tokens": a.context_tokens,
            "tensor_parallel_size": 2
        },
        "interval_secs": 15,
        "backoff_secs": 120,
        "jaccl_retry_secs": 1800,
        "jaccl_failures_before_ring": 2
    });
    let Some(Value::Object(old)) = existing else { return fresh };
    // Keep the old file's other settings (and tuning it already had); replace what setup decides.
    let mut out = old.clone();
    for (k, v) in fresh.as_object().into_iter().flatten() {
        let keep_old_tuning = matches!(k.as_str(), "interval_secs" | "backoff_secs" | "jaccl_retry_secs" | "jaccl_failures_before_ring")
            && out.contains_key(k);
        if k == "plan_request" {
            let mut pr = out.get(k).and_then(|p| p.as_object()).cloned().unwrap_or_default();
            for (pk, pv) in v.as_object().into_iter().flatten() {
                let keep = matches!(pk.as_str(), "execution_profile" | "tensor_parallel_size") && pr.contains_key(pk);
                if !keep {
                    pr.insert(pk.clone(), pv.clone());
                }
            }
            out.insert(k.clone(), Value::Object(pr));
        } else if !keep_old_tuning {
            out.insert(k.clone(), v.clone());
        }
    }
    Value::Object(out)
}

/// Capacities for (local, peer) from a `POST /admin/api/cluster/node-budgets` response.
pub fn capacities(resp: &Value, local: &str, peer: &str) -> Option<(u64, u64)> {
    let get = |id: &str| {
        resp["nodes"]
            .as_array()?
            .iter()
            .find(|n| n["node_id"].as_str() == Some(id))
            .and_then(|n| n["capacity_bytes"].as_u64())
            .filter(|b| *b > 0)
    };
    Some((get(local)?, get(peer)?))
}

/// The body for node-budgets: this Mac locally, the other Mac over SSH.
pub fn budgets_request(local: &str, peer: &str, peer_ssh: &str) -> Value {
    json!({
        "hosts": [{"node_id": local, "ssh": "127.0.0.1"}, {"node_id": peer, "ssh": peer_ssh}],
        "roles": {local: "headless", peer: "headless"},
    })
}

/// Model folders under oMLX's model directory: those with a config.json.
pub fn model_choices(model_dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(model_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("config.json").is_file())
        .collect();
    v.sort();
    v
}

/// oMLX's model folder and port from settings.json text.
pub fn omlx_settings(text: Option<&str>) -> (Option<String>, u16) {
    let v: Value = text.and_then(|t| serde_json::from_str(t).ok()).unwrap_or(Value::Null);
    let dir = v["model"]["model_dir"].as_str().map(str::to_string);
    (dir, v["server"]["port"].as_u64().unwrap_or(8000) as u16)
}

/// The install.sh that came with this binary: next to it, two levels up
/// (target/release/tblink), or in the current folder.
pub fn find_install_sh(exe: Option<&Path>, cwd: Option<&Path>) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = exe.and_then(|e| e.parent()) {
        dirs.push(d.to_path_buf());
        if let Some(up) = d.parent().and_then(|p| p.parent()) {
            dirs.push(up.to_path_buf());
        }
    }
    dirs.extend(cwd.map(Path::to_path_buf));
    dirs.into_iter()
        .map(|d| d.join("install.sh"))
        .find(|p| std::fs::read_to_string(p).map(|t| t.contains("software.skepticdog.tblink")).unwrap_or(false))
}

/// Ask questions on `input`, print on `output`.
pub struct Prompt<'a> {
    pub input: &'a mut dyn BufRead,
    pub output: &'a mut dyn Write,
}

impl Prompt<'_> {
    pub fn say(&mut self, s: &str) {
        let _ = writeln!(self.output, "{s}");
    }
    /// A free-text answer; Enter takes `default`. `None` at end of input.
    pub fn ask(&mut self, q: &str, default: &str) -> Option<String> {
        if default.is_empty() {
            let _ = write!(self.output, "{q}: ");
        } else {
            let _ = write!(self.output, "{q} [{default}]: ");
        }
        let _ = self.output.flush();
        let mut line = String::new();
        if self.input.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim();
        Some(if line.is_empty() { default.to_string() } else { line.to_string() })
    }
    pub fn yes(&mut self, q: &str, default: bool) -> Option<bool> {
        loop {
            let a = self.ask(&format!("{q} {}", if default { "[Y/n]" } else { "[y/N]" }), "")?;
            match a.to_ascii_lowercase().as_str() {
                "" => return Some(default),
                "y" | "yes" => return Some(true),
                "n" | "no" => return Some(false),
                _ => self.say("please answer y or n"),
            }
        }
    }
    /// A choice from 1..=n.
    pub fn choose(&mut self, q: &str, n: usize, default: usize) -> Option<usize> {
        loop {
            let a = self.ask(q, &default.to_string())?;
            match a.parse::<usize>() {
                Ok(i) if (1..=n).contains(&i) => return Some(i),
                _ => self.say(&format!("please enter a number from 1 to {n}")),
            }
        }
    }
    /// A number of bytes, typed in GB.
    pub fn gigabytes(&mut self, q: &str) -> Option<u64> {
        loop {
            let a = self.ask(q, "")?;
            match a.trim_end_matches(['G', 'B', 'g', 'b', ' ']).parse::<f64>() {
                Ok(g) if g > 0.0 => return Some((g * 1e9) as u64),
                _ => self.say("please enter a size in GB, e.g. 34"),
            }
        }
    }
}

fn gb(b: u64) -> String {
    format!("{:.1} GB", b as f64 / 1e9)
}

/// One line per Thunderbolt port, for the opening summary.
pub fn describe_ports(st: &net::Status) -> Vec<String> {
    st.ports
        .iter()
        .map(|p| match (&p.peer, p.active) {
            (Some(m), true) => {
                format!("  {}: cable to {m}, {}", p.name, p.speed_gbps.map(|s| format!("{s} Gb/s")).unwrap_or("speed unknown".into()))
            }
            (_, true) => format!("  {}: link up", p.name),
            _ => format!("  {}: nothing connected", p.name),
        })
        .collect()
}

/// Run a command with this terminal (for sudo's password prompt).
fn interactive(cmd: &str, args: &[&str]) -> bool {
    std::process::Command::new(cmd).args(args).status().map(|s| s.success()).unwrap_or(false)
}

fn remote_login_on() -> bool {
    std::net::TcpStream::connect_timeout(&"127.0.0.1:22".parse().unwrap(), std::time::Duration::from_secs(1)).is_ok()
}

pub fn main(config_path: &str) -> i32 {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut output = std::io::stdout();
    let mut p = Prompt { input: &mut input, output: &mut output };
    match run(&mut p, config_path) {
        Some(code) => code,
        None => {
            p.say("\nsetup stopped; nothing more was changed");
            1
        }
    }
}

fn run(p: &mut Prompt, config_path: &str) -> Option<i32> {
    let st = net::status();
    let node = std::fs::read_to_string(expand(net::IDENTITY)).ok().and_then(|t| net::parse_node_id(&t));
    let existing: Option<Value> = std::fs::read_to_string(expand(config_path)).ok().and_then(|t| serde_json::from_str(&t).ok());
    p.say(&format!("tblink {} setup on {}", net::version(), st.hostname));
    match &node {
        Some(n) => p.say(&format!("oMLX node id: {n}")),
        None => {
            p.say("oMLX node id: not found (~/.omlx/cluster/identity.json); start oMLX once and enable clustering, then run setup again")
        }
    }
    if st.ports.is_empty() {
        p.say("No RDMA-capable Thunderbolt ports. Enable RDMA first: start up in Recovery, open Terminal, run `rdma_ctl enable`, restart.");
        return Some(1);
    }
    p.say("Thunderbolt ports:");
    for l in describe_ports(&st) {
        p.say(&l);
    }
    let cables = st.ports.iter().filter(|x| x.active && x.peer.is_some()).count();
    if cables >= 2 {
        p.say("Two cables: the faster one will carry only inference (RDMA), the other management (SSH, API, Ring).");
    }
    if !remote_login_on() {
        p.say("Note: Remote Login looks off on this Mac. Turn it on (System Settings > General > Sharing): the coordinator uses SSH, and the port daemons use port 22 to confirm both Macs use the same cable.");
    }

    p.say("\nWhich Mac is this?\n  1) the coordinator: runs the oMLX API and the coordinator\n  2) the other Mac");
    let coordinator = p.choose("choice", 2, if existing.is_some() { 1 } else { 2 })? == 1;
    let mut addr = standard_addresses(coordinator);
    if let Some(e) = &existing {
        if coordinator {
            addr = Addresses {
                rdma: e["local"]["rdma_ip"].as_str().unwrap_or(&addr.rdma).into(),
                bridge: e["local"]["bridge_ip"].as_str().unwrap_or(&addr.bridge).into(),
                peer_rdma: e["peer"]["rdma_ip"].as_str().unwrap_or(&addr.peer_rdma).into(),
                peer_bridge: e["peer"]["bridge_ip"].as_str().unwrap_or(&addr.peer_bridge).into(),
            };
        }
    }
    p.say(&format!(
        "Addresses: this Mac {} (RDMA) and {} (management); the other Mac {} and {}.",
        addr.rdma, addr.bridge, addr.peer_rdma, addr.peer_bridge
    ));
    if !p.yes("Use these?", true)? {
        addr.rdma = p.ask("this Mac's RDMA address", &addr.rdma)?;
        addr.bridge = p.ask("this Mac's management address", &addr.bridge)?;
        addr.peer_rdma = p.ask("the other Mac's RDMA address", &addr.peer_rdma)?;
        addr.peer_bridge = p.ask("the other Mac's management address", &addr.peer_bridge)?;
    }

    let install = find_install_sh(std::env::current_exe().ok().as_deref(), std::env::current_dir().ok().as_deref());
    let port_args = port_install_args(&addr);
    let port_cmd = format!("sudo ./install.sh {}", port_args.join(" "));
    let mut installed_port = false;
    match &install {
        Some(sh) => {
            if p.yes("Install (or update) the port daemon now? It runs install.sh with sudo, which asks for your password.", true)? {
                let sh = sh.to_string_lossy().to_string();
                let mut args: Vec<&str> = vec!["/bin/sh", &sh];
                args.extend(port_args.iter().map(String::as_str));
                installed_port = interactive("/usr/bin/sudo", &args);
                p.say(if installed_port { "port daemon installed" } else { "install.sh port failed; see its message above" });
            }
        }
        None => p.say(&format!("install.sh not found next to this binary; from the tblink folder run:\n  {port_cmd}")),
    }

    if !coordinator {
        p.say("\nNext steps:");
        if !installed_port {
            p.say(&format!("  - install the port daemon: {port_cmd}"));
        }
        p.say("  - make sure Remote Login is on here, and run `tblink setup` on the coordinator Mac");
        p.say("  - then run `tblink doctor` on the coordinator Mac to check both Macs");
        return Some(0);
    }
    coordinator_setup(p, config_path, &addr, node, existing, install.as_deref(), installed_port, &port_cmd)
}

#[allow(clippy::too_many_arguments)]
fn coordinator_setup(
    p: &mut Prompt,
    config_path: &str,
    addr: &Addresses,
    node: Option<String>,
    existing: Option<Value>,
    install: Option<&Path>,
    installed_port: bool,
    port_cmd: &str,
) -> Option<i32> {
    let e = existing.clone().unwrap_or(Value::Null);
    let local_node = match node {
        Some(n) => n,
        None => p.ask("this Mac's oMLX node id", e["local"]["node_id"].as_str().unwrap_or(""))?,
    };
    let me = std::env::var("USER").unwrap_or_default();
    let peer_user = p.ask("\nLogin name on the other Mac", e["peer"]["ssh_user"].as_str().unwrap_or(&me))?;
    let ssh_key = p.ask("SSH key for the cluster", e["ssh_key"].as_str().unwrap_or(DEFAULT_KEY))?;
    if !Path::new(&expand(&ssh_key)).exists()
        && p.yes(&format!("{ssh_key} does not exist. Create it now (ssh-keygen, no passphrase)?"), true)?
    {
        interactive("/usr/bin/ssh-keygen", &["-t", "ed25519", "-N", "", "-C", "tblink cluster", "-f", &expand(&ssh_key)]);
    }
    let target = format!("{peer_user}@{}", addr.peer_bridge);
    let mut ok = coord::ssh_run(&peer_user, &addr.peer_bridge, &ssh_key, &["true"]).0;
    if !ok {
        p.say(&format!("SSH to {target} with {ssh_key} does not work yet."));
        if p.yes("Copy the key to the other Mac now (ssh-copy-id; asks for that Mac's password)?", true)? {
            interactive("/usr/bin/ssh-copy-id", &["-i", &format!("{}.pub", expand(&ssh_key)), &target]);
            ok = coord::ssh_run(&peer_user, &addr.peer_bridge, &ssh_key, &["true"]).0;
        }
    }
    p.say(if ok { "SSH to the other Mac works" } else { "SSH to the other Mac still fails; the coordinator needs it. Continuing." });

    // The other Mac's node id: from its tblink status, else its identity file.
    let mut peer_node = None;
    if ok {
        let (_, out) = coord::ssh_run(&peer_user, &addr.peer_bridge, &ssh_key, &["/usr/local/bin/tblink", "status", "--json"]);
        if let Some(s) = coord::json_line(&out).and_then(|l| serde_json::from_str::<net::Status>(l).ok()) {
            p.say(&format!("the other Mac: {}, tblink {}", s.hostname, if s.version.is_empty() { "0.1 (update it)" } else { &s.version }));
            if let Some(w) = coord::version_warning(&net::version(), &s.version) {
                p.say(&format!("  {w}"));
            }
            peer_node = s.node_id;
        }
        if peer_node.is_none() {
            let (_, out) = coord::ssh_run(&peer_user, &addr.peer_bridge, &ssh_key, &["/bin/cat", ".omlx/cluster/identity.json"]);
            peer_node = coord::json_line(&out).and_then(net::parse_node_id);
        }
    }
    let peer_node = match peer_node {
        Some(n) => {
            p.say(&format!("the other Mac's oMLX node id: {n}"));
            n
        }
        None => {
            p.ask("the other Mac's oMLX node id (in ~/.omlx/cluster/identity.json there)", e["peer"]["node_id"].as_str().unwrap_or(""))?
        }
    };

    // Model.
    let settings_text = std::fs::read_to_string(expand(SETTINGS)).ok();
    let (model_dir, api_port) = omlx_settings(settings_text.as_deref());
    let models = model_dir.as_deref().map(|d| model_choices(Path::new(d))).unwrap_or_default();
    let current_model = e["plan_request"]["model_path"].as_str().unwrap_or("").to_string();
    let model_path = if models.is_empty() {
        p.ask("\nFull path of the model folder (the same path on both Macs)", &current_model)?
    } else {
        p.say("\nModels:");
        for (i, m) in models.iter().enumerate() {
            p.say(&format!("  {}) {}", i + 1, m.display()));
        }
        p.say(&format!("  {}) another path", models.len() + 1));
        let default = models.iter().position(|m| m.to_string_lossy() == current_model).map(|i| i + 1).unwrap_or(1);
        let i = p.choose("model", models.len() + 1, default)?;
        if i <= models.len() {
            models[i - 1].to_string_lossy().to_string()
        } else {
            p.ask("full path of the model folder", &current_model)?
        }
    };
    p.say("The model must be at this same path on the other Mac.");

    // Memory budgets, measured by oMLX.
    let mut capacity = None;
    let api_base = format!("http://127.0.0.1:{api_port}");
    if coord::healthy(&api_base) {
        if let Some(key) = coord::read_key(SETTINGS) {
            let api = coord::Api { base: api_base.clone(), key };
            p.say("measuring how much memory each Mac can give the model (oMLX node-budgets)...");
            match api.post(
                "/admin/api/cluster/node-budgets",
                Some(&budgets_request(&local_node, &peer_node, &format!("{peer_user}@{}", addr.peer_bridge))),
                120,
            ) {
                Ok(v) => capacity = capacities(&v, &local_node, &peer_node),
                Err(err) => p.say(&format!("  node-budgets failed: {err}")),
            }
            if let Ok(status) = api.get("/admin/api/cluster/status") {
                if !coord::live_ids(&status).is_empty() {
                    p.say("  note: a model is loaded now, so these budgets are lower than with nothing loaded; run setup again later if they look small");
                }
            }
        }
    } else {
        p.say(&format!("oMLX is not running at {api_base}, so the memory budgets cannot be measured."));
    }
    let capacity = match capacity {
        Some((a, b)) => {
            p.say(&format!("  this Mac {}, the other Mac {}", gb(a), gb(b)));
            (a, b)
        }
        None => (p.gigabytes("GB this Mac can give the model")?, p.gigabytes("GB the other Mac can give the model")?),
    };
    let context: u64 = loop {
        let default = e["plan_request"]["target_context_tokens"].as_u64().unwrap_or(32768).to_string();
        if let Ok(n) = p.ask("Context length in tokens", &default)?.parse() {
            break n;
        }
    };

    let brew =
        ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"].into_iter().find(|b| Path::new(b).exists()).unwrap_or("/opt/homebrew/bin/brew");
    let answers = Answers {
        addresses: addr.clone(),
        local_node,
        peer_node,
        peer_user,
        ssh_key,
        api_port,
        brew: brew.into(),
        model_path,
        capacity,
        context_tokens: context,
    };
    let cfg = build_config(&answers, existing.as_ref());
    let path = PathBuf::from(expand(config_path));
    p.say(&format!("\nThe config:\n{}", serde_json::to_string_pretty(&cfg).unwrap_or_default()));
    if !p.yes(&format!("Write {}{}?", path.display(), if existing.is_some() { " (the old one is kept as .bak)" } else { "" }), true)? {
        return Some(0);
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if path.exists() {
        let _ = std::fs::copy(&path, path.with_extension("json.bak"));
    }
    if let Err(err) = std::fs::write(&path, serde_json::to_string_pretty(&cfg).unwrap_or_default() + "\n") {
        p.say(&format!("cannot write {}: {err}", path.display()));
        return Some(1);
    }
    p.say(&format!("wrote {}", path.display()));

    let mut installed_agent = false;
    if let Some(sh) = install {
        if p.yes("Install (or restart) the coordinator agent now?", true)? {
            installed_agent = interactive("/bin/sh", &[&sh.to_string_lossy(), "coordinator", &path.to_string_lossy()]);
            p.say(if installed_agent { "coordinator agent installed" } else { "install.sh coordinator failed; see its message above" });
        }
    }
    p.say("\nNext steps:");
    if !installed_port {
        p.say(&format!("  - install the port daemon here: {port_cmd}"));
    }
    if !installed_agent {
        p.say(&format!("  - install the coordinator agent: ./install.sh coordinator {}", path.display()));
    }
    p.say("  - run `tblink setup` on the other Mac too (choose \"the other Mac\"), if you have not");
    p.say("  - then `tblink doctor` here checks both Macs, the cables, roles, versions and oMLX");
    p.say("  - logs: /var/log/tblink.log (port daemon), ~/Library/Logs/tblink.log (coordinator)");
    Some(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn answers() -> Answers {
        Answers {
            addresses: standard_addresses(true),
            local_node: "A".into(),
            peer_node: "B".into(),
            peer_user: "me".into(),
            ssh_key: DEFAULT_KEY.into(),
            api_port: 8000,
            brew: "/opt/homebrew/bin/brew".into(),
            model_path: "/Users/Shared/omlx-models/M".into(),
            capacity: (37_462_008_324, 34_155_749_856),
            context_tokens: 32768,
        }
    }

    #[test]
    fn standard_addresses_mirror_each_other() {
        let c = standard_addresses(true);
        let o = standard_addresses(false);
        assert_eq!(
            c,
            Addresses {
                rdma: "10.0.1.1".into(),
                bridge: "10.77.0.1".into(),
                peer_rdma: "10.0.1.2".into(),
                peer_bridge: "10.77.0.2".into()
            }
        );
        assert_eq!((o.rdma, o.bridge, o.peer_rdma, o.peer_bridge), (c.peer_rdma, c.peer_bridge, c.rdma, c.bridge));
    }

    #[test]
    fn port_install_args_match_install_sh() {
        assert_eq!(port_install_args(&standard_addresses(false)), vec!["port", "10.0.1.2", "10.77.0.2", "10.0.1.1", "10.77.0.1"]);
    }

    #[test]
    fn built_config_is_a_valid_coordinator_config() {
        let v = build_config(&answers(), None);
        let c: coord::Config = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(c.local.node_id, "A");
        assert_eq!(c.peer.ssh_user, "me");
        assert_eq!(c.peer.bridge_ip, "10.77.0.2");
        assert_eq!(c.plan_request["nodes"][1]["capacity_bytes"], 34_155_749_856u64);
        assert_eq!(c.plan_request["nodes"][0]["node_id"], "A");
        assert_eq!(c.plan_request["tensor_parallel_size"], 2);
        assert!(coord::config_problems(&c, true).is_empty());
        // Same shape as the documented example.
        let example: Value = serde_json::from_str(include_str!("../examples/coordinator.json")).unwrap();
        let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        assert_eq!(keys(&v), keys(&example));
        assert_eq!(keys(&v["plan_request"]), keys(&example["plan_request"]));
    }

    #[test]
    fn rerunning_setup_keeps_other_settings() {
        let mut old = build_config(&answers(), None);
        old["interval_secs"] = json!(5);
        old["rebuild_settle_secs"] = json!(90);
        old["plan_request"]["execution_profile"] = json!("throughput");
        old["plan_request"]["mtp"] = json!(true);
        let mut a = answers();
        a.model_path = "/Users/Shared/omlx-models/N".into();
        a.context_tokens = 65536;
        let v = build_config(&a, Some(&old));
        assert_eq!(v["interval_secs"], 5);
        assert_eq!(v["rebuild_settle_secs"], 90);
        assert_eq!(v["plan_request"]["execution_profile"], "throughput");
        assert_eq!(v["plan_request"]["mtp"], true);
        assert_eq!(v["plan_request"]["model_path"], "/Users/Shared/omlx-models/N");
        assert_eq!(v["plan_request"]["target_context_tokens"], 65536);
    }

    #[test]
    fn capacities_from_node_budgets() {
        let resp = json!({"nodes": [
            {"node_id": "B", "ssh": "me@10.77.0.2", "capacity_bytes": 2, "reserve_bytes": 0},
            {"node_id": "A", "ssh": "127.0.0.1", "capacity_bytes": 1, "reserve_bytes": 0},
        ]});
        assert_eq!(capacities(&resp, "A", "B"), Some((1, 2)));
        assert_eq!(capacities(&resp, "A", "C"), None);
        assert_eq!(
            capacities(&json!({"nodes": [{"node_id": "A", "capacity_bytes": 0}, {"node_id": "B", "capacity_bytes": 2}]}), "A", "B"),
            None
        );
        assert_eq!(capacities(&json!({}), "A", "B"), None);
    }

    #[test]
    fn budgets_request_shape() {
        assert_eq!(
            budgets_request("A", "B", "me@10.77.0.2"),
            json!({"hosts": [{"node_id": "A", "ssh": "127.0.0.1"}, {"node_id": "B", "ssh": "me@10.77.0.2"}], "roles": {"A": "headless", "B": "headless"}})
        );
    }

    #[test]
    fn omlx_settings_cases() {
        let s =
            r#"{"server":{"host":"127.0.0.1","port":8080},"model":{"model_dir":"/Users/Shared/omlx-models"},"auth":{"api_key":"secret"}}"#;
        assert_eq!(omlx_settings(Some(s)), (Some("/Users/Shared/omlx-models".into()), 8080));
        assert_eq!(omlx_settings(None), (None, 8000));
        assert_eq!(omlx_settings(Some("nope")), (None, 8000));
    }

    #[test]
    fn model_choices_need_a_config_json() {
        let d = tempfile::tempdir().unwrap();
        for m in ["b-model", "a-model", "not-a-model"] {
            std::fs::create_dir(d.path().join(m)).unwrap();
        }
        std::fs::write(d.path().join("a-model/config.json"), "{}").unwrap();
        std::fs::write(d.path().join("b-model/config.json"), "{}").unwrap();
        std::fs::write(d.path().join("stray.txt"), "").unwrap();
        let names: Vec<String> = model_choices(d.path()).iter().map(|p| p.file_name().unwrap().to_string_lossy().into()).collect();
        assert_eq!(names, vec!["a-model", "b-model"]);
        assert!(model_choices(&d.path().join("missing")).is_empty());
    }

    #[test]
    fn finds_the_tblink_install_sh() {
        let d = tempfile::tempdir().unwrap();
        let rel = d.path().join("target/release");
        std::fs::create_dir_all(&rel).unwrap();
        assert_eq!(find_install_sh(Some(&rel.join("tblink")), None), None);
        std::fs::write(d.path().join("install.sh"), "#!/bin/sh\nPORT_LABEL=software.skepticdog.tblink.port\n").unwrap();
        assert_eq!(find_install_sh(Some(&rel.join("tblink")), None), Some(d.path().join("install.sh")));
        // Someone else's install.sh in the current folder is not ours.
        let other = tempfile::tempdir().unwrap();
        std::fs::write(other.path().join("install.sh"), "echo hi\n").unwrap();
        assert_eq!(find_install_sh(None, Some(other.path())), None);
    }

    #[test]
    fn repo_install_sh_is_recognised() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert_eq!(find_install_sh(None, Some(here)), Some(here.join("install.sh")));
    }

    fn prompt_with(input: &str) -> (Cursor<Vec<u8>>, Vec<u8>) {
        (Cursor::new(input.as_bytes().to_vec()), Vec::new())
    }

    #[test]
    fn prompt_defaults_and_answers() {
        let (mut i, mut o) = prompt_with("\nalice\n");
        let mut p = Prompt { input: &mut i, output: &mut o };
        assert_eq!(p.ask("login", "me"), Some("me".into()));
        assert_eq!(p.ask("login", "me"), Some("alice".into()));
        assert_eq!(p.ask("login", "me"), None, "end of input stops setup");
        assert_eq!(String::from_utf8(o).unwrap(), "login [me]: login [me]: login [me]: ");
    }

    #[test]
    fn prompt_yes_no() {
        let (mut i, mut o) = prompt_with("\nmaybe\nN\nyes\n");
        let mut p = Prompt { input: &mut i, output: &mut o };
        assert_eq!(p.yes("ok?", true), Some(true));
        assert_eq!(p.yes("ok?", true), Some(false));
        assert_eq!(p.yes("ok?", false), Some(true));
        assert!(String::from_utf8(o).unwrap().contains("please answer y or n"));
    }

    #[test]
    fn prompt_choose_and_gigabytes() {
        let (mut i, mut o) = prompt_with("3\nx\n2\n\nabc\n34GB\n12.5\n");
        let mut p = Prompt { input: &mut i, output: &mut o };
        assert_eq!(p.choose("pick", 2, 1), Some(2));
        assert_eq!(p.choose("pick", 2, 1), Some(1));
        assert_eq!(p.gigabytes("GB"), Some(34_000_000_000));
        assert_eq!(p.gigabytes("GB"), Some(12_500_000_000));
    }

    #[test]
    fn port_summary_lines() {
        let st = net::Status {
            ports: vec![
                net::Port {
                    name: "en1".into(),
                    active: true,
                    peer: Some("MacBook Pro".into()),
                    speed_gbps: Some(80),
                    ..net::Port::default()
                },
                net::Port { name: "en2".into(), active: true, ..net::Port::default() },
                net::Port { name: "en6".into(), ..net::Port::default() },
            ],
            ..net::Status::default()
        };
        assert_eq!(describe_ports(&st), vec!["  en1: cable to MacBook Pro, 80 Gb/s", "  en2: link up", "  en6: nothing connected"]);
    }
}
