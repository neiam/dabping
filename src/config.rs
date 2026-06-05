use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::store::{RraSpec, default_rras, validate_target_path};

/// Inheritable per-node settings while flattening the tree.
struct Inherited<'a> {
    probe: &'a str,
    port: Option<u16>,
    lookup: Option<&'a str>,
    alerts: Option<&'a [String]>,
    agents: Option<&'a [String]>,
    nomasterpoll: bool,
}

/// Top-level config file model. See PLAN.md and dabping.toml for the shape.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub database: Database,
    #[serde(default)]
    pub web: Web,
    #[serde(default)]
    pub probes: BTreeMap<String, ProbeConfig>,
    #[serde(default)]
    pub emitters: BTreeMap<String, EmitterConfig>,
    #[serde(default)]
    pub alerts: BTreeMap<String, AlertConfig>,
    #[serde(default)]
    pub smtp: Option<SmtpConfig>,
    #[serde(default)]
    pub status: Option<StatusConfig>,
    /// Remote measurement agents (SmokePing slaves).
    #[serde(default)]
    pub agents: BTreeMap<String, AgentConfig>,
    pub targets: BTreeMap<String, TargetNode>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Shared secret the agent presents in X-Dabping-Secret.
    pub secret: String,
}

/// Public status page (Statuspage-style).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusConfig {
    /// Page heading; defaults to "dabping status".
    pub title: Option<String>,
    /// Days of uptime bars per component.
    #[serde(default = "default_history_days")]
    pub history_days: u32,
    /// Enables the incident-administration API when set
    /// (X-Dabping-Token header).
    pub admin_token: Option<String>,
    #[serde(default)]
    pub components: BTreeMap<String, StatusComponentConfig>,
}

fn default_history_days() -> u32 {
    90
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusComponentConfig {
    /// Display name; defaults to the component key.
    pub name: Option<String>,
    pub group: Option<String>,
    /// Target paths backing this component (all must be healthy).
    pub targets: Vec<String>,
    /// Alert-pattern DSL over loss%; default ">5%,>5%".
    pub degraded_loss: Option<String>,
    /// Default "==100%,==100%,==100%".
    pub down_loss: Option<String>,
    /// Optional pattern over median ms.
    pub degraded_rtt: Option<String>,
    /// Severity that auto-opens an incident; default "partial_outage".
    pub incident_at: Option<String>,
}

/// One alert definition — SmokePing's Alerts section.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertConfig {
    #[serde(rename = "type")]
    pub kind: AlertKind,
    /// e.g. ">10%,>10%,>10%" — see alert/pattern.rs.
    pub pattern: String,
    #[serde(default)]
    pub comment: String,
    /// "log:", "email:addr", "exec:cmd", "webhook:url".
    pub to: Vec<String>,
    /// Re-notify while still raised, e.g. "1h". Default: raise edge only.
    pub repeat_every: Option<String>,
    #[serde(default = "default_true")]
    pub notify_clear: bool,
}

/// Which per-round value the pattern compares.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertKind {
    /// Loss percentage (always known).
    Loss,
    /// Median RTT in ms; unknown (==U) when the whole round was lost.
    Rtt,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SmtpConfig {
    pub server: String,
    #[serde(default = "default_smtp_port")]
    pub port: u16,
    pub from: String,
    /// none | starttls | implicit
    #[serde(default = "default_smtp_tls")]
    pub tls: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

fn default_smtp_port() -> u16 {
    25
}
fn default_smtp_tls() -> String {
    "none".into()
}

/// Web UI / API server settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Web {
    #[serde(default = "default_listen")]
    pub listen: std::net::SocketAddr,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_listen() -> std::net::SocketAddr {
    "127.0.0.1:8420".parse().expect("valid default listen addr")
}
fn default_true() -> bool {
    true
}

impl Default for Web {
    fn default() -> Self {
        Web { listen: default_listen(), enabled: true }
    }
}

/// Round timing / shape / storage — mirrors SmokePing's Database section.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Database {
    /// Seconds between rounds for each target.
    #[serde(default = "default_step")]
    pub step: u64,
    /// Measurements per round (the smoke). SmokePing requires >= 3.
    #[serde(default = "default_pings")]
    pub pings: u32,
    /// Directory for the round-robin series files.
    #[serde(default = "default_data_dir")]
    pub dir: PathBuf,
    /// Consolidation table; defaults to SmokePing's.
    #[serde(default = "default_rras", rename = "rra")]
    pub rras: Vec<RraSpec>,
}

fn default_step() -> u64 {
    300
}
fn default_pings() -> u32 {
    20
}
fn default_data_dir() -> PathBuf {
    PathBuf::from("data")
}

impl Default for Database {
    fn default() -> Self {
        Database {
            step: default_step(),
            pings: default_pings(),
            dir: default_data_dir(),
            rras: default_rras(),
        }
    }
}

/// Where rounds get exported, beyond the native store (vaping-style).
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum EmitterConfig {
    /// Enables GET /metrics on the web server (pull).
    Prometheus,
    Graphite(GraphiteConfig),
    Influx(InfluxConfig),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GraphiteConfig {
    /// host:port of the plaintext (2003) listener.
    pub addr: String,
    #[serde(default = "default_metric_prefix")]
    pub prefix: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InfluxConfig {
    /// Complete write endpoint, v1 or v2 style:
    /// "http://influx:8086/write?db=dabping" or
    /// "http://influx:8086/api/v2/write?org=x&bucket=net".
    pub url: String,
    /// Sent as "Authorization: Token …" when set.
    pub token: Option<String>,
    #[serde(default = "default_metric_prefix")]
    pub measurement: String,
    #[serde(default = "default_flush_secs")]
    pub flush_secs: u64,
}

fn default_metric_prefix() -> String {
    "dabping".into()
}
fn default_flush_secs() -> u64 {
    5
}

/// A named probe instance. Multiple instances of the same type with
/// different parameters are allowed, as in SmokePing.
#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum ProbeConfig {
    Icmp(IcmpConfig),
    Tcp(TcpConfig),
    Dns(DnsConfig),
    Http(HttpConfig),
    Exec(ExecConfig),
    Phoenix(PhoenixConfig),
}

#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct IcmpConfig {
    /// ICMP data bytes per echo request (like ping -s).
    #[serde(default = "default_payload_size")]
    pub payload_size: usize,
    /// How long to wait for each reply.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Pacing gap between pings within a round.
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,
}

fn default_payload_size() -> usize {
    56
}
fn default_timeout_ms() -> u64 {
    1500
}
fn default_interval_ms() -> u64 {
    300
}

impl Default for IcmpConfig {
    fn default() -> Self {
        IcmpConfig {
            payload_size: default_payload_size(),
            timeout_ms: default_timeout_ms(),
            interval_ms: default_interval_ms(),
        }
    }
}

impl IcmpConfig {
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }
    pub fn interval(&self) -> Duration {
        Duration::from_millis(self.interval_ms)
    }
}

/// TCP connect probe. The port comes from here or from the target.
#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct TcpConfig {
    pub port: Option<u16>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,
}

impl TcpConfig {
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }
    pub fn interval(&self) -> Duration {
        Duration::from_millis(self.interval_ms)
    }
}

/// DNS probe: the target is the server; `lookup` is the queried name
/// (default: the target's own hostname — any response counts).
#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct DnsConfig {
    #[serde(default = "default_dns_port")]
    pub port: u16,
    pub lookup: Option<String>,
    #[serde(default = "default_qtype")]
    pub qtype: String,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,
}

fn default_dns_port() -> u16 {
    53
}
fn default_qtype() -> String {
    "A".into()
}

impl DnsConfig {
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }
    pub fn interval(&self) -> Duration {
        Duration::from_millis(self.interval_ms)
    }
}

/// HTTP(S) probe; `%host%` in the url is replaced by the target host.
#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct HttpConfig {
    #[serde(default = "default_http_url")]
    pub url: String,
    #[serde(default = "default_http_method")]
    pub method: String,
    #[serde(default = "default_http_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_http_interval_ms")]
    pub interval_ms: u64,
}

fn default_http_url() -> String {
    "http://%host%/".into()
}
fn default_http_method() -> String {
    "GET".into()
}
fn default_http_timeout_ms() -> u64 {
    5000
}
fn default_http_interval_ms() -> u64 {
    500
}

impl HttpConfig {
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }
    pub fn interval(&self) -> Duration {
        Duration::from_millis(self.interval_ms)
    }
}

/// Phoenix-socket probe: RTT = channel heartbeat round-trip.
/// The target's port override applies (e.g. a dev server on 4000).
#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct PhoenixConfig {
    /// Socket mount point; "/socket/websocket" for user sockets.
    #[serde(default = "default_phx_path")]
    pub path: String,
    #[serde(default = "default_true")]
    pub tls: bool,
    /// Session cookie for auth-gated sockets, e.g. "_app_key=…".
    pub cookie: Option<String>,
    #[serde(default = "default_http_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,
}

fn default_phx_path() -> String {
    "/live/websocket".into()
}

impl PhoenixConfig {
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }
    pub fn interval(&self) -> Duration {
        Duration::from_millis(self.interval_ms)
    }
}

/// External-command probe; see probe/exec.rs for the output contract.
#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecConfig {
    pub command: String,
    #[serde(default = "default_round_timeout_ms")]
    pub round_timeout_ms: u64,
}

fn default_round_timeout_ms() -> u64 {
    60_000
}

/// A node in the hierarchical target tree. Any key that isn't a known
/// field is a child section/target, nested arbitrarily deep.
#[derive(Debug, Deserialize)]
pub struct TargetNode {
    // title/menu are read by the web UI (milestone 3)
    #[allow(dead_code)]
    pub title: Option<String>,
    #[allow(dead_code)]
    pub menu: Option<String>,
    pub host: Option<String>,
    /// Probe instance name; inherited by children when unset.
    pub probe: Option<String>,
    /// Per-target probe parameters (tcp/dns port, dns lookup); inherited.
    pub port: Option<u16>,
    pub lookup: Option<String>,
    /// Alert names watching this target (and its children; inherited).
    pub alerts: Option<Vec<String>>,
    /// Agent names that also measure this target (inherited).
    pub agents: Option<Vec<String>>,
    /// The master itself does not probe this target (inherited);
    /// only the listed agents do. SmokePing's nomasterpoll.
    pub nomasterpoll: Option<bool>,
    #[serde(default, flatten)]
    pub children: BTreeMap<String, TargetNode>,
}

/// A leaf target after flattening the tree and resolving inheritance.
#[derive(Debug, Clone)]
pub struct FlatTarget {
    /// Slash-separated path in the tree, e.g. "isp/gw".
    pub path: String,
    pub host: String,
    pub probe: String,
    pub port: Option<u16>,
    pub lookup: Option<String>,
    pub alerts: Vec<String>,
    pub agents: Vec<String>,
    pub nomasterpoll: bool,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config file {}", path.display()))?;
        Self::load_str(&raw).with_context(|| format!("cannot parse {}", path.display()))
    }

    pub fn load_str(raw: &str) -> Result<Config> {
        let mut cfg: Config = toml::from_str(raw)?;
        // Implicit default: targets fall back to a probe named "icmp", so
        // provide a stock instance whenever the config doesn't define one
        // (defining other probes must not break icmp-by-default targets).
        cfg.probes
            .entry("icmp".into())
            .or_insert(ProbeConfig::Icmp(IcmpConfig::default()));
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        if self.database.pings < 3 {
            bail!("database.pings must be >= 3 (got {})", self.database.pings);
        }
        if self.database.step == 0 {
            bail!("database.step must be > 0");
        }
        if self.targets.is_empty() {
            bail!("no targets configured");
        }
        if self.database.rras.is_empty() {
            bail!("database.rra table must have at least one archive");
        }
        for (i, r) in self.database.rras.iter().enumerate() {
            if r.steps == 0 || r.rows < 2 {
                bail!("database.rra[{i}]: steps must be >= 1 and rows >= 2");
            }
            if !(0.0..1.0).contains(&r.xff) {
                bail!("database.rra[{i}]: xff must be in [0, 1)");
            }
        }
        // probe configs must actually build (bad qtype/method/url fail here)
        for (name, pc) in &self.probes {
            crate::probe::ProbeInstance::from_config(pc)
                .with_context(|| format!("probe {name:?}"))?;
        }
        // alert patterns, notify specs, references and smtp presence
        crate::alert::Alerter::new(self)?;
        // status components: patterns parse, targets exist
        crate::status::validate(self)?;
        // flatten() checks probe references and leaf shapes
        self.flatten_targets().map(|_| ())
    }

    /// Walk the target tree into a flat list of probeable leaves,
    /// resolving probe/port/lookup inheritance along the way.
    pub fn flatten_targets(&self) -> Result<Vec<FlatTarget>> {
        let mut out = Vec::new();
        let root = Inherited {
            probe: "icmp",
            port: None,
            lookup: None,
            alerts: None,
            agents: None,
            nomasterpoll: false,
        };
        for (name, node) in &self.targets {
            self.flatten_node(name, node, &root, &mut out)?;
        }
        if out.is_empty() {
            bail!("target tree contains no hosts");
        }
        Ok(out)
    }

    fn flatten_node(
        &self,
        path: &str,
        node: &TargetNode,
        inherited: &Inherited,
        out: &mut Vec<FlatTarget>,
    ) -> Result<()> {
        let here = Inherited {
            probe: node.probe.as_deref().unwrap_or(inherited.probe),
            port: node.port.or(inherited.port),
            lookup: node.lookup.as_deref().or(inherited.lookup),
            alerts: node.alerts.as_deref().or(inherited.alerts),
            agents: node.agents.as_deref().or(inherited.agents),
            nomasterpoll: node.nomasterpoll.unwrap_or(inherited.nomasterpoll),
        };
        if node.probe.is_some() && !self.probes.contains_key(here.probe) {
            bail!("target {path}: unknown probe {:?}", here.probe);
        }
        if let Some(host) = &node.host {
            let probe_cfg = self
                .probes
                .get(here.probe)
                .with_context(|| format!("target {path}: unknown probe {:?}", here.probe))?;
            // probes that need a port must get one from somewhere
            if let ProbeConfig::Tcp(t) = probe_cfg {
                if t.port.or(here.port).is_none() {
                    bail!("target {path}: tcp probe {:?} needs a port (on the probe or the target)", here.probe);
                }
            }
            validate_target_path(path)?; // paths become series file names
            if path.contains('@') {
                bail!("target {path}: '@' is reserved for agent series");
            }
            let agents = here.agents.map(<[String]>::to_vec).unwrap_or_default();
            for a in &agents {
                if !self.agents.contains_key(a) {
                    bail!("target {path}: unknown agent {a:?} (no [agents.{a}] section)");
                }
            }
            if here.nomasterpoll && agents.is_empty() {
                bail!("target {path}: nomasterpoll but no agents — nobody would measure it");
            }
            out.push(FlatTarget {
                path: path.to_string(),
                host: host.clone(),
                probe: here.probe.to_string(),
                port: here.port,
                lookup: here.lookup.map(str::to_string),
                alerts: here.alerts.map(<[String]>::to_vec).unwrap_or_default(),
                agents,
                nomasterpoll: here.nomasterpoll,
            });
        } else if node.children.is_empty() {
            bail!("target {path}: section has neither a host nor children");
        }
        for (name, child) in &node.children {
            self.flatten_node(&format!("{path}/{name}"), child, &here, out)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // go through the real loader so tests exercise defaulting + validation
    fn parse(s: &str) -> Config {
        Config::load_str(s).unwrap()
    }

    #[test]
    fn flattens_nested_targets_with_probe_inheritance() {
        let cfg = parse(
            r#"
            [probes.icmp]
            type = "icmp"
            [probes.slow]
            type = "icmp"
            timeout_ms = 5000

            [targets.isp]
            title = "Upstream"
            probe = "slow"
              [targets.isp.gw]
              host = "192.0.2.1"
              [targets.isp.dns]
              host = "192.0.2.53"
              probe = "icmp"
            [targets.web]
            host = "example.com"
            "#,
        );
        let flat = cfg.flatten_targets().unwrap();
        let by_path: BTreeMap<_, _> =
            flat.iter().map(|t| (t.path.as_str(), t.probe.as_str())).collect();
        assert_eq!(by_path["isp/gw"], "slow"); // inherited
        assert_eq!(by_path["isp/dns"], "icmp"); // overridden
        assert_eq!(by_path["web"], "icmp"); // default
        assert_eq!(flat.len(), 3);
    }

    #[test]
    fn rejects_unknown_probe_reference() {
        let err = Config::load_str(
            r#"
            [targets.a]
            host = "192.0.2.1"
            probe = "nope"
            "#,
        );
        assert!(err.is_err());
    }

    #[test]
    fn implicit_icmp_survives_other_probes() {
        // regression: defining any probe used to remove the icmp default,
        // breaking every target that relied on it
        let cfg = Config::load_str(
            r#"
            [probes.web]
            type = "http"

            [targets.a]
            host = "192.0.2.1"
            [targets.b]
            host = "192.0.2.2"
            probe = "web"
            "#,
        )
        .unwrap();
        let flat = cfg.flatten_targets().unwrap();
        let by_path: BTreeMap<_, _> =
            flat.iter().map(|t| (t.path.as_str(), t.probe.as_str())).collect();
        assert_eq!(by_path["a"], "icmp");
        assert_eq!(by_path["b"], "web");
    }

    #[test]
    fn agents_inherit_and_validate() {
        let cfg = parse(
            r#"
            [agents.lon1]
            secret = "s"

            [targets.isp]
            agents = ["lon1"]
              [targets.isp.gw]
              host = "192.0.2.1"
              [targets.isp.faraway]
              host = "192.0.2.2"
              nomasterpoll = true
            [targets.local]
            host = "127.0.0.1"
            "#,
        );
        let flat = cfg.flatten_targets().unwrap();
        let by_path: BTreeMap<_, _> = flat.iter().map(|t| (t.path.as_str(), t)).collect();
        assert_eq!(by_path["isp/gw"].agents, vec!["lon1"]); // inherited
        assert!(!by_path["isp/gw"].nomasterpoll);
        assert!(by_path["isp/faraway"].nomasterpoll);
        assert!(by_path["local"].agents.is_empty());
    }

    #[test]
    fn rejects_unknown_agent_and_orphan_nomasterpoll() {
        let err = Config::load_str(
            r#"
            [targets.a]
            host = "192.0.2.1"
            agents = ["ghost"]
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("unknown agent"));

        let err = Config::load_str(
            r#"
            [targets.a]
            host = "192.0.2.1"
            nomasterpoll = true
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("nobody would measure"));
    }

    #[test]
    fn rejects_at_sign_in_target_names() {
        assert!(
            Config::load_str(
                r#"
                [targets."a@b"]
                host = "192.0.2.1"
                "#,
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_empty_section() {
        assert!(
            Config::load_str(
                r#"
                [targets.empty]
                title = "nothing here"
                "#,
            )
            .is_err()
        );
    }
}
