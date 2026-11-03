//! Thin helpers around the PipeWire command-line tools.
//!
//! * `ParamWriter` keeps ONE long-lived `pw-cli` process and feeds it `set-param` lines on stdin,
//!   so live slider/ducking updates cost a pipe write instead of a process spawn.
//! * Node lookups go through `pw-dump`; links through `pw-link`.
//!
//! Verified on PipeWire 1.6.9 (see docs/verified-apis.md).

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------------------------
// dB helpers (0 dB = 100 %, -6 dB ~ 50 %, -60 dB and below = mute)
// ---------------------------------------------------------------------------------------------

pub fn db_to_lin(db: f32) -> f32 {
    if db <= -60.0 { 0.0 } else { 10f32.powf(db / 20.0) }
}

pub fn lin_to_db(lin: f32) -> f32 {
    if lin <= 0.001 { -60.0 } else { (20.0 * lin.log10()).max(-60.0) }
}

// ---------------------------------------------------------------------------------------------
// Persistent pw-cli parameter writer
// ---------------------------------------------------------------------------------------------

struct WriterInner {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
}

pub struct ParamWriter {
    inner: Mutex<WriterInner>,
}

pub static PARAMS: LazyLock<ParamWriter> =
    LazyLock::new(|| ParamWriter { inner: Mutex::new(WriterInner { child: None, stdin: None }) });

impl ParamWriter {
    fn ensure(inner: &mut WriterInner) -> bool {
        if let Some(child) = inner.child.as_mut() {
            if let Ok(None) = child.try_wait() {
                return inner.stdin.is_some();
            }
        }
        match Command::new("pw-cli")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(mut c) => {
                inner.stdin = c.stdin.take();
                inner.child = Some(c);
                true
            }
            Err(e) => {
                eprintln!("Cannot start pw-cli parameter writer: {}", e);
                false
            }
        }
    }

    fn send(&self, line: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        for _ in 0..2 {
            if !Self::ensure(&mut inner) {
                return false;
            }
            if let Some(stdin) = inner.stdin.as_mut() {
                if stdin.write_all(line.as_bytes()).is_ok() && stdin.write_all(b"\n").is_ok() && stdin.flush().is_ok() {
                    return true;
                }
            }
            // Broken pipe: restart pw-cli and retry once.
            if let Some(mut c) = inner.child.take() {
                let _ = c.kill();
                let _ = c.wait();
            }
            inner.stdin = None;
        }
        false
    }

    /// Sets filter-chain control values on a node: `"<node>:<port>" value` pairs.
    pub fn set_controls(&self, node_id: u32, params: &[(String, f32)]) -> bool {
        if params.is_empty() {
            return true;
        }
        let mut s = String::with_capacity(64 + params.len() * 32);
        s.push_str(&format!("set-param {} Props {{ params = [ ", node_id));
        for (k, v) in params {
            let v = if v.is_finite() { *v } else { 0.0 };
            s.push_str(&format!("\"{}\" {:.6} ", k.replace('"', ""), v));
        }
        s.push_str("] }");
        self.send(&s)
    }

    /// Sets node volume/mute directly as LINEAR amplitude (wpctl uses a cubic scale).
    pub fn set_volume(&self, node_id: u32, channels: usize, linear: f32, mute: bool) -> bool {
        let vols = vec![format!("{:.6}", linear.max(0.0)); channels.max(1)].join(" ");
        self.send(&format!(
            "set-param {} Props {{ channelVolumes = [ {} ] mute = {} }}",
            node_id, vols, mute
        ))
    }

    pub fn shutdown(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.stdin = None;
        if let Some(mut c) = inner.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}


// ---------------------------------------------------------------------------------------------
// Short-lived graph snapshot cache. One maintenance pass asks "which ports / links / nodes exist"
// many times; each answer used to spawn pw-link / pw-dump. Results are reused for 250 ms and
// dropped as soon as Studio Mic itself links, unlinks, creates or destroys anything.
// ---------------------------------------------------------------------------------------------

const CACHE_TTL: Duration = Duration::from_millis(250);

#[derive(Default)]
struct GraphCache {
    ports: Option<(Instant, (Vec<String>, Vec<String>))>,
    links: Option<(Instant, HashSet<(String, String)>)>,
    nodes: Option<(Instant, Vec<NodeInfo>)>,
}

static GRAPH: LazyLock<Mutex<GraphCache>> = LazyLock::new(|| Mutex::new(GraphCache::default()));

/// Forget every cached graph query (called after any change we make).
pub fn invalidate_cache() {
    *GRAPH.lock().unwrap() = GraphCache::default();
}

fn fresh<T: Clone>(slot: &Option<(Instant, T)>) -> Option<T> {
    slot.as_ref().filter(|(t, _)| t.elapsed() < CACHE_TTL).map(|(_, v)| v.clone())
}

// ---------------------------------------------------------------------------------------------
// Graph inspection
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct NodeInfo {
    pub id: u32,
    pub name: String,
    pub class: String,
    pub props: serde_json::Value,
    pub params: serde_json::Value,
}

pub fn dump(kind: &str) -> Vec<serde_json::Value> {
    let mut cmd = Command::new("pw-dump");
    if !kind.is_empty() {
        cmd.arg(kind);
    }
    match cmd.output() {
        Ok(o) => serde_json::from_slice::<serde_json::Value>(&o.stdout)
            .ok()
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

pub fn nodes() -> Vec<NodeInfo> {
    if let Some(v) = fresh(&GRAPH.lock().unwrap().nodes) {
        return v;
    }
    let v = nodes_uncached();
    GRAPH.lock().unwrap().nodes = Some((Instant::now(), v.clone()));
    v
}

fn nodes_uncached() -> Vec<NodeInfo> {
    dump("Node")
        .into_iter()
        .filter_map(|n| {
            let id = n.get("id")?.as_u64()? as u32;
            let props = n.pointer("/info/props")?.clone();
            let name = props.get("node.name").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let class = props.get("media.class").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let params = n.pointer("/info/params").cloned().unwrap_or(serde_json::Value::Null);
            Some(NodeInfo { id, name, class, props, params })
        })
        .collect()
}

pub fn node_ids_by_name() -> HashMap<String, u32> {
    nodes().into_iter().map(|n| (n.name, n.id)).collect()
}

pub fn find_node(name: &str) -> Option<NodeInfo> {
    nodes().into_iter().find(|n| n.name == name)
}

/// Polls until every name exists (or the timeout passes). Returns the ids that were found.
pub fn wait_for_nodes(names: &[String], timeout: Duration) -> HashMap<String, u32> {
    let start = Instant::now();
    loop {
        invalidate_cache();
        let map = node_ids_by_name();
        let found: HashMap<String, u32> =
            names.iter().filter_map(|n| map.get(n).map(|id| (n.clone(), *id))).collect();
        if found.len() == names.len() || start.elapsed() >= timeout {
            return found;
        }
        std::thread::sleep(Duration::from_millis(80));
    }
}

/// Every port as "node:port" -> direction ("in"/"out").
pub fn ports() -> (Vec<String>, Vec<String>) {
    if let Some(v) = fresh(&GRAPH.lock().unwrap().ports) {
        return v;
    }
    let v = ports_uncached();
    GRAPH.lock().unwrap().ports = Some((Instant::now(), v.clone()));
    v
}

fn ports_uncached() -> (Vec<String>, Vec<String>) {
    let run = |flag: &str| -> Vec<String> {
        Command::new("pw-link")
            .arg(flag)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
            .unwrap_or_default()
    };
    (run("-o"), run("-i"))
}

pub fn ports_of(node: &str, outputs: bool) -> Vec<String> {
    let (o, i) = ports();
    let list = if outputs { o } else { i };
    let prefix = format!("{}:", node);
    let mut v: Vec<String> = list
        .into_iter()
        .filter(|p| p.starts_with(&prefix))
        .map(|p| p[prefix.len()..].to_string())
        .filter(|p| !p.starts_with("monitor_"))
        .collect();
    v.sort();
    v
}

pub fn wait_for_port(full_port: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        invalidate_cache();
        let (o, i) = ports();
        if o.iter().chain(i.iter()).any(|p| p == full_port) {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(60));
    }
}

/// Current links as (output "node:port", input "node:port").
pub fn links() -> HashSet<(String, String)> {
    if let Some(v) = fresh(&GRAPH.lock().unwrap().links) {
        return v;
    }
    let v = links_uncached();
    GRAPH.lock().unwrap().links = Some((Instant::now(), v.clone()));
    v
}

fn links_uncached() -> HashSet<(String, String)> {
    let out = match Command::new("pw-link").arg("-l").output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(_) => return HashSet::new(),
    };
    let mut set = HashSet::new();
    let mut current: Option<String> = None;
    for line in out.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("|->") {
            if let Some(src) = &current {
                set.insert((src.clone(), rest.trim().to_string()));
            }
        } else if t.starts_with("|<-") {
            // input-side listing of the same link; ignore
        } else if !t.is_empty() {
            current = Some(t.to_string());
        }
    }
    set
}

pub fn link(out_port: &str, in_port: &str) -> bool {
    invalidate_cache();
    Command::new("pw-link")
        .args([out_port, in_port])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn unlink(out_port: &str, in_port: &str) -> bool {
    invalidate_cache();
    Command::new("pw-link")
        .args(["-d", out_port, in_port])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Waits until every link touching the given nodes reports `state = active` (format and buffers
/// negotiated, audio flowing). `pw-link` returns before that, which is why a crossfade started
/// right after linking can leave a few periods of silence.
pub fn wait_links_active(node_names: &[String], timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        invalidate_cache();
        let ids: HashSet<u64> = nodes().into_iter().filter(|n| node_names.contains(&n.name)).map(|n| n.id as u64).collect();
        let links = dump("Link");
        let mut any = false;
        let all_active = links.iter().all(|l| {
            let o = l.pointer("/info/output-node-id").and_then(|v| v.as_u64()).unwrap_or(0);
            let i = l.pointer("/info/input-node-id").and_then(|v| v.as_u64()).unwrap_or(0);
            if !ids.contains(&o) && !ids.contains(&i) {
                return true;
            }
            any = true;
            l.pointer("/info/state").and_then(|v| v.as_str()) == Some("active")
        });
        if (any && all_active) || start.elapsed() >= timeout {
            return any && all_active;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
}

/// Links only the pairs that are missing.
pub fn ensure_links(pairs: &[(String, String)]) -> usize {
    if pairs.is_empty() {
        return 0;
    }
    let existing = links();
    let mut made = 0;
    for (o, i) in pairs {
        if !existing.contains(&(o.clone(), i.clone())) && link(o, i) {
            made += 1;
        }
    }
    made
}

/// Removes every link feeding `in_port` (e.g. "StudioMic_Mic_Pre_In:input_FL") except `keep`.
pub fn unlink_inputs_of(node: &str, keep: &[(String, String)]) {
    let prefix = format!("{}:", node);
    for (o, i) in links() {
        if i.starts_with(&prefix) && !keep.contains(&(o.clone(), i.clone())) {
            unlink(&o, &i);
        }
    }
}

/// Removes every link leaving `node` except `keep`.
pub fn unlink_outputs_of(node: &str, keep: &[(String, String)]) {
    let prefix = format!("{}:", node);
    for (o, i) in links() {
        if o.starts_with(&prefix) && !keep.contains(&(o.clone(), i.clone())) {
            unlink(&o, &i);
        }
    }
}

/// Maps a source's capture ports onto a stereo input pair. Mono sources feed both sides.
pub fn stereo_pairs(src_node: &str, src_ports: &[String], dst_node: &str, dst_l: &str, dst_r: &str) -> Vec<(String, String)> {
    let find = |names: &[&str]| src_ports.iter().find(|p| names.iter().any(|n| p.ends_with(n))).cloned();
    let l = find(&["_FL", "_AUX0", "_1"]);
    let r = find(&["_FR", "_AUX1", "_2"]);
    let mono = find(&["_MONO"]);
    let (sl, sr) = match (l, r, mono) {
        (Some(l), Some(r), _) => (l, r),
        (_, _, Some(m)) => (m.clone(), m),
        (Some(l), None, None) => (l.clone(), l),
        _ => match src_ports.first() {
            Some(p) => (p.clone(), p.clone()),
            None => return Vec::new(),
        },
    };
    vec![
        (format!("{}:{}", src_node, sl), format!("{}:{}", dst_node, dst_l)),
        (format!("{}:{}", src_node, sr), format!("{}:{}", dst_node, dst_r)),
    ]
}

// ---------------------------------------------------------------------------------------------
// Lingering virtual nodes created in the main PipeWire daemon (survive Studio Mic restarts)
// ---------------------------------------------------------------------------------------------

pub fn create_lingering_null_node(name: &str, class: &str, description: &str) -> bool {
    invalidate_cache();
    let props = format!(
        "{{ factory.name=support.null-audio-sink node.name=\"{}\" node.description=\"{}\" media.class={} \
         audio.position=[FL FR] audio.rate=48000 object.linger=true node.pause-on-idle=false \
         monitor.channel-volumes=true priority.session=0 priority.driver=0 node.group=studiomic }}",
        name, crate::audio::filter_chain::spa_string_safe(description), class
    );
    Command::new("pw-cli")
        .args(["create-node", "adapter", &props])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// A private, invisible-to-routing null sink that DRIVES its own group: used to run a new chain
/// generation (and its denoiser) on an isolated clock before it joins the live graph.
pub fn destroy_node(id: u32) -> bool {
    invalidate_cache();
    Command::new("pw-cli")
        .args(["destroy", &id.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Moves an application stream to another sink the way pavucontrol does (WirePlumber metadata).
/// A plain node.name string is used: verified to move cleanly with no duplicate links.
pub fn move_stream(stream_id: u32, target_node_name: &str) -> bool {
    Command::new("pw-metadata")
        .args(["-n", "default", &stream_id.to_string(), "target.object", target_node_name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn clear_stream_target(stream_id: u32) {
    let _ = Command::new("pw-metadata")
        .args(["-n", "default", "-d", &stream_id.to_string(), "target.object"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

pub fn default_node_name(which: &str) -> Option<String> {
    static CACHE: LazyLock<Mutex<HashMap<String, (Instant, Option<String>)>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
    if let Some((t, v)) = CACHE.lock().unwrap().get(which) {
        if t.elapsed() < Duration::from_millis(1900) {
            return v.clone();
        }
    }
    let v = default_node_name_uncached(which);
    CACHE.lock().unwrap().insert(which.to_string(), (Instant::now(), v.clone()));
    v
}

/// Reads WirePlumber's current default from the `default` metadata (~9 ms; `wpctl inspect`
/// took ~42 ms and ran on every maintenance pass).
fn default_node_name_uncached(which: &str) -> Option<String> {
    let key = match which {
        "@DEFAULT_AUDIO_SOURCE@" => "default.audio.source",
        _ => "default.audio.sink",
    };
    let output = Command::new("pw-metadata").args(["-n", "default", "0", key]).output().ok()?;
    parse_default_name(&String::from_utf8_lossy(&output.stdout), key)
}

/// `update: id:0 key:'default.audio.sink' value:'{"name":"alsa_output.x"}' type:'Spa:String:JSON'`
fn parse_default_name(text: &str, key: &str) -> Option<String> {
    let line = text.lines().find(|l| l.contains(&format!("key:'{}'", key)))?;
    let value = line.split("value:'").nth(1)?.rsplit_once("' type:")?.0;
    serde_json::from_str::<serde_json::Value>(value).ok()?.get("name")?.as_str().map(|s| s.to_string())
}

/// Reads a node's linear channel volume and mute from its Props param.
pub fn node_volume(node: &NodeInfo) -> Option<(f32, bool)> {
    let props = node.params.get("Props")?.as_array()?;
    for p in props {
        if let Some(vols) = p.get("channelVolumes").and_then(|v| v.as_array()) {
            let vs: Vec<f32> = vols.iter().filter_map(|v| v.as_f64().map(|f| f as f32)).collect();
            if vs.is_empty() {
                continue;
            }
            let avg = vs.iter().sum::<f32>() / vs.len() as f32;
            let mute = p.get("mute").and_then(|m| m.as_bool()).unwrap_or(false);
            return Some((avg, mute));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_percent_math() {
        assert!((db_to_lin(0.0) - 1.0).abs() < 1e-6);
        assert!((db_to_lin(-6.0) - 0.501).abs() < 0.01);
        assert_eq!(db_to_lin(-60.0), 0.0);
        assert!((lin_to_db(0.5) + 6.02).abs() < 0.05);
        assert_eq!(lin_to_db(0.0), -60.0);
    }

    #[test]
    fn default_name_from_metadata() {
        let t = "Found \"default\" metadata 43\nupdate: id:0 key:'default.audio.sink' value:'{\"name\":\"alsa_output.usb-x\"}' type:'Spa:String:JSON'\n";
        assert_eq!(parse_default_name(t, "default.audio.sink").as_deref(), Some("alsa_output.usb-x"));
        assert_eq!(parse_default_name(t, "default.audio.source"), None);
    }

    #[test]
    fn mono_source_feeds_both_sides() {
        let p = stereo_pairs("hw", &["capture_MONO".to_string()], "dst", "input_FL", "input_FR");
        assert_eq!(p[0], ("hw:capture_MONO".to_string(), "dst:input_FL".to_string()));
        assert_eq!(p[1], ("hw:capture_MONO".to_string(), "dst:input_FR".to_string()));
        let s = stereo_pairs("hw", &["capture_FL".to_string(), "capture_FR".to_string()], "d", "a", "b");
        assert_eq!(s[1].0, "hw:capture_FR");
    }
}
