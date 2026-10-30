//! Paths, the config file, and machine descriptions.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

pub const LOCAL: &str = "local";
pub const ALL: &str = "all";
pub const DEFAULT_REMOTE_COMMAND: &str = "~/.sessift/bin/sessift";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_BROKER_IDLE: Duration = Duration::from_secs(30 * 60);
const DEFAULT_PROBE_AFTER_IDLE: Duration = Duration::from_secs(20);
/// macOS limits Unix socket paths to 104 bytes including the terminator.
const MAX_SOCKET_PATH: usize = 100;

#[derive(Debug, Clone)]
pub struct Paths {
    pub home: PathBuf,
}

impl Paths {
    pub fn from_env() -> Result<Self> {
        if let Some(home) = std::env::var_os("SESSIFT_HOME").filter(|v| !v.is_empty()) {
            return Ok(Self {
                home: PathBuf::from(home),
            });
        }
        let user_home = std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .ok_or_else(|| anyhow!("HOME is not set; set SESSIFT_HOME"))?;
        Ok(Self {
            home: PathBuf::from(user_home).join(".sessift"),
        })
    }

    pub fn config_file(&self) -> PathBuf {
        self.home.join("config.toml")
    }
    pub fn run_dir(&self) -> PathBuf {
        self.home.join("run")
    }
    pub fn log_dir(&self) -> PathBuf {
        self.home.join("log")
    }
    pub fn broker_socket(&self) -> PathBuf {
        self.run_dir().join("broker.sock")
    }
    pub fn broker_lock(&self) -> PathBuf {
        self.run_dir().join("broker.lock")
    }
    pub fn broker_log(&self) -> PathBuf {
        self.log_dir().join("broker.log")
    }
    pub fn index_dir(&self) -> PathBuf {
        self.home.join("index")
    }

    /// Whether the broker socket path fits the platform limit.
    pub fn broker_socket_usable(&self) -> bool {
        self.broker_socket().as_os_str().len() <= MAX_SOCKET_PATH
    }

    /// ControlPath for OpenSSH connection sharing, or `None` when it would be too long.
    pub fn ssh_control_path(&self) -> Option<String> {
        // `%C` expands to a 40-character hash.
        let path = self.run_dir().join("ssh-%C");
        let text = path.to_str()?.to_string();
        (text.len() + 40 - 2 <= MAX_SOCKET_PATH).then_some(text)
    }
}

/// Create a directory (and parents) readable only by the current user.
pub fn ensure_private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod {}", path.display()))?;
    Ok(())
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub backend: BackendConfig,
    #[serde(default)]
    pub broker: BrokerConfig,
    #[serde(default, rename = "machine")]
    pub machines: Vec<MachineConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    /// Machines queried when `--machine` is not given. Empty means local plus every enabled machine.
    #[serde(default)]
    pub machines: Vec<String>,
    #[serde(default, deserialize_with = "de_opt_duration")]
    pub timeout: Option<Duration>,
    #[serde(default, deserialize_with = "de_opt_duration")]
    pub connect_timeout: Option<Duration>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    /// Our own index of the transcripts (`crate::index`).
    #[default]
    Native,
    /// The installed memex binary (kept for comparison).
    Memex,
    Fixture,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
    #[serde(default)]
    pub kind: BackendKind,
    /// Path to the memex binary; discovered when absent.
    #[serde(default)]
    pub memex: Option<String>,
    /// memex `--root`.
    #[serde(default)]
    pub memex_root: Option<String>,
    #[serde(default)]
    pub fixture: Option<PathBuf>,
    #[serde(default)]
    pub max_concurrency: Option<usize>,
    /// Native backend: how stale the index may get before a query updates it ("0" never).
    #[serde(default, deserialize_with = "de_opt_duration")]
    pub refresh_every: Option<Duration>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerConfig {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default, deserialize_with = "de_opt_duration")]
    pub idle_timeout: Option<Duration>,
    /// A connection idle this long gets a liveness probe alongside its next request.
    #[serde(default, deserialize_with = "de_opt_duration")]
    pub probe_after_idle: Option<Duration>,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            idle_timeout: None,
            probe_after_idle: None,
        }
    }
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineConfig {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
    /// SSH destination, usually a `Host` alias from ~/.ssh/config.
    #[serde(default)]
    pub ssh: Option<String>,
    /// Path of sessift on the remote machine.
    #[serde(default)]
    pub remote_command: Option<String>,
    /// Extra arguments passed to ssh before the destination.
    #[serde(default)]
    pub ssh_options: Vec<String>,
    /// Run this local command instead of SSH (testing and custom tunnels).
    #[serde(default)]
    pub command: Option<Vec<String>>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

/// Everything needed to reach a machine. Sent to the broker, which keys connections by it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MachineSpec {
    pub id: String,
    pub transport: TransportSpec,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TransportSpec {
    Ssh {
        target: String,
        remote_command: String,
        #[serde(default)]
        options: Vec<String>,
    },
    Command {
        argv: Vec<String>,
    },
}

impl MachineSpec {
    pub fn key(&self) -> String {
        serde_json::to_string(self).expect("machine specs serialize")
    }

    pub fn describe(&self) -> String {
        match &self.transport {
            TransportSpec::Ssh { target, .. } => format!("ssh {target}"),
            TransportSpec::Command { argv } => format!("command {}", argv.join(" ")),
        }
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = paths.config_file();
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text).with_context(|| format!("invalid {}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        let config: Config = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        let mut seen = HashSet::new();
        for machine in &self.machines {
            validate_id(&machine.id)?;
            if !seen.insert(machine.id.as_str()) {
                bail!("machine '{}' is defined twice", machine.id);
            }
            match (&machine.ssh, &machine.command) {
                (Some(_), Some(_)) => bail!("machine '{}': set either ssh or command", machine.id),
                (None, None) => bail!("machine '{}': set ssh or command", machine.id),
                (Some(target), None) => validate_ssh_target(&machine.id, target)?,
                (None, Some(argv)) if argv.is_empty() => {
                    bail!("machine '{}': command must not be empty", machine.id)
                }
                _ => {}
            }
            if let Some(command) = &machine.remote_command
                && (command.trim().is_empty() || command.contains('\n'))
            {
                bail!("machine '{}': invalid remote_command", machine.id);
            }
        }
        for id in &self.defaults.machines {
            if id != LOCAL && id != ALL && self.machine(id).is_none() {
                bail!("defaults.machines lists unknown machine '{id}'");
            }
        }
        Ok(())
    }

    pub fn machine(&self, id: &str) -> Option<&MachineConfig> {
        self.machines.iter().find(|machine| machine.id == id)
    }

    pub fn timeout(&self) -> Duration {
        self.defaults.timeout.unwrap_or(DEFAULT_TIMEOUT)
    }

    pub fn connect_timeout(&self) -> Duration {
        self.defaults
            .connect_timeout
            .unwrap_or(DEFAULT_CONNECT_TIMEOUT)
    }

    pub fn broker_idle_timeout(&self) -> Duration {
        self.broker.idle_timeout.unwrap_or(DEFAULT_BROKER_IDLE)
    }

    pub fn broker_probe_after_idle(&self) -> Duration {
        self.broker
            .probe_after_idle
            .unwrap_or(DEFAULT_PROBE_AFTER_IDLE)
    }

    /// Resolve `--machine` values (or the defaults) into an ordered, de-duplicated id list.
    pub fn select(&self, requested: &[String]) -> Result<Vec<String>> {
        let wanted: Vec<String> = if !requested.is_empty() {
            requested.to_vec()
        } else if !self.defaults.machines.is_empty() {
            self.defaults.machines.clone()
        } else {
            vec![ALL.to_string()]
        };
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for id in wanted {
            let expanded: Vec<String> = if id == ALL {
                std::iter::once(LOCAL.to_string())
                    .chain(
                        self.machines
                            .iter()
                            .filter(|m| m.enabled)
                            .map(|m| m.id.clone()),
                    )
                    .collect()
            } else if id == LOCAL || self.machine(&id).is_some() {
                vec![id]
            } else {
                let known: Vec<&str> = std::iter::once(LOCAL)
                    .chain(self.machines.iter().map(|m| m.id.as_str()))
                    .collect();
                bail!("unknown machine '{id}' (known: {})", known.join(", "));
            };
            for id in expanded {
                if seen.insert(id.clone()) {
                    out.push(id);
                }
            }
        }
        Ok(out)
    }

    pub fn spec(&self, id: &str) -> Result<MachineSpec> {
        let machine = self
            .machine(id)
            .ok_or_else(|| anyhow!("unknown machine '{id}'"))?;
        Ok(machine.spec())
    }
}

impl MachineConfig {
    pub fn spec(&self) -> MachineSpec {
        let transport = match (&self.command, &self.ssh) {
            (Some(argv), _) => TransportSpec::Command { argv: argv.clone() },
            (None, Some(target)) => TransportSpec::Ssh {
                target: target.clone(),
                remote_command: self
                    .remote_command
                    .clone()
                    .unwrap_or_else(|| DEFAULT_REMOTE_COMMAND.to_string()),
                options: self.ssh_options.clone(),
            },
            (None, None) => unreachable!("validated: ssh or command is set"),
        };
        MachineSpec {
            id: self.id.clone(),
            transport,
        }
    }
}

pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id == LOCAL
        || id == ALL
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!(
            "invalid machine id '{id}' (use letters, digits, '-' or '_'; 'local' and 'all' are reserved)"
        );
    }
    Ok(())
}

fn validate_ssh_target(id: &str, target: &str) -> Result<()> {
    if target.is_empty() || target.starts_with('-') || target.chars().any(char::is_whitespace) {
        bail!("machine '{id}': unsafe ssh target '{target}'");
    }
    Ok(())
}

/// Parse `500ms`, `5s`, `2m`, `1h`; a bare number is milliseconds.
pub fn parse_duration(text: &str) -> Result<Duration> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let value: f64 = number
        .parse()
        .map_err(|_| anyhow!("invalid duration '{text}'"))?;
    let ms = match unit.trim() {
        "" | "ms" => value,
        "s" => value * 1000.0,
        "m" | "min" => value * 60_000.0,
        "h" => value * 3_600_000.0,
        other => bail!("invalid duration unit '{other}' in '{text}'"),
    };
    if !ms.is_finite() || ms < 0.0 {
        bail!("invalid duration '{text}'");
    }
    Ok(Duration::from_millis(ms.round() as u64))
}

fn de_opt_duration<'de, D>(de: D) -> std::result::Result<Option<Duration>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Ms(u64),
        Text(String),
    }
    match Option::<Raw>::deserialize(de)? {
        None => Ok(None),
        Some(Raw::Ms(ms)) => Ok(Some(Duration::from_millis(ms))),
        Some(Raw::Text(text)) => parse_duration(&text)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

/// Identifies this exact binary, so a broker started by another build gets replaced.
pub fn build_id() -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    if let Ok(exe) = std::env::current_exe() {
        exe.hash(&mut hasher);
        if let Ok(meta) = std::fs::metadata(&exe) {
            meta.len().hash(&mut hasher);
            if let Ok(modified) = meta.modified()
                && let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH)
            {
                since.as_nanos().hash(&mut hasher);
            }
        }
    }
    format!("{}-{:016x}", crate::VERSION, hasher.finish())
}

/// A machine found in a memex config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedMachine {
    pub id: String,
    pub label: Option<String>,
    pub ssh: String,
}

/// Machines and default selection from `~/.memex/config.toml`.
pub fn import_memex_config(text: &str) -> Result<(Vec<ImportedMachine>, Vec<String>)> {
    let value: toml::Value = toml::from_str(text)?;
    let mut machines = Vec::new();
    for entry in value
        .get("machines")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(id) = entry.get("id").and_then(toml::Value::as_str) else {
            continue;
        };
        let ssh = entry.get("ssh").and_then(toml::Value::as_str).or_else(|| {
            let control = entry.get("control")?;
            let kind = control.get("type").and_then(toml::Value::as_str);
            (kind.is_none() || kind == Some("ssh"))
                .then(|| control.get("host").and_then(toml::Value::as_str))
                .flatten()
        });
        let Some(ssh) = ssh else { continue };
        if validate_id(id).is_err() || validate_ssh_target(id, ssh).is_err() {
            continue;
        }
        machines.push(ImportedMachine {
            id: id.to_string(),
            label: entry
                .get("label")
                .and_then(toml::Value::as_str)
                .map(str::to_string),
            ssh: ssh.to_string(),
        });
    }
    let defaults = value
        .get("multi_machine")
        .and_then(|m| m.get("default"))
        .and_then(toml::Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(toml::Value::as_str)
                .filter(|id| *id == LOCAL || machines.iter().any(|m| m.id == *id))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    Ok((machines, defaults))
}

/// Render a starter config file.
pub fn render_config(machines: &[ImportedMachine], defaults: &[String]) -> String {
    let mut out = String::from(
        "# sessift configuration. See docs/plan.md.\n\n[defaults]\n# Machines queried when --machine is not given.\n",
    );
    let defaults = if defaults.is_empty() {
        std::iter::once(LOCAL.to_string())
            .chain(machines.iter().map(|m| m.id.clone()))
            .collect::<Vec<_>>()
    } else {
        defaults.to_vec()
    };
    let list = defaults
        .iter()
        .map(|id| format!("\"{id}\""))
        .collect::<Vec<_>>()
        .join(", ");
    out.push_str(&format!("machines = [{list}]\n"));
    out.push_str("# Per-request deadline for remote machines.\ntimeout = \"5s\"\n\n");
    out.push_str(
        "[backend]\n# sessift's own index (~/.sessift/index); \"memex\" uses an installed memex.\nkind = \"native\"\n",
    );
    for machine in machines {
        out.push_str("\n[[machine]]\n");
        out.push_str(&format!("id = \"{}\"\n", machine.id));
        if let Some(label) = &machine.label {
            out.push_str(&format!("label = {}\n", toml_string(label)));
        }
        out.push_str(&format!("ssh = {}\n", toml_string(&machine.ssh)));
        out.push_str(&format!(
            "# remote_command = \"{DEFAULT_REMOTE_COMMAND}\"\n"
        ));
    }
    out
}

fn toml_string(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[defaults]
machines = ["local", "mm"]
timeout = "3s"

[backend]
kind = "memex"

[[machine]]
id = "mm"
ssh = "mm"

[[machine]]
id = "box"
command = ["sessift", "serve", "--stdio"]
enabled = false
"#;

    #[test]
    fn parses_and_selects_machines() {
        let config = Config::parse(SAMPLE).unwrap();
        assert_eq!(config.timeout(), Duration::from_secs(3));
        assert_eq!(config.select(&[]).unwrap(), vec!["local", "mm"]);
        assert_eq!(config.select(&["all".into()]).unwrap(), vec!["local", "mm"]);
        assert_eq!(
            config
                .select(&["box".into(), "local".into(), "box".into()])
                .unwrap(),
            vec!["box", "local"]
        );
        assert!(config.select(&["nope".into()]).is_err());
        let spec = config.spec("mm").unwrap();
        assert_eq!(
            spec.transport,
            TransportSpec::Ssh {
                target: "mm".into(),
                remote_command: DEFAULT_REMOTE_COMMAND.into(),
                options: vec![],
            }
        );
    }

    #[test]
    fn rejects_bad_machines() {
        assert!(Config::parse("[[machine]]\nid = \"local\"\nssh = \"x\"").is_err());
        assert!(Config::parse("[[machine]]\nid = \"a\"\nssh = \"-oProxyCommand=x\"").is_err());
        assert!(Config::parse("[[machine]]\nid = \"a\"").is_err());
        assert!(Config::parse("[defaults]\nmachines = [\"ghost\"]").is_err());
        assert!(Config::parse("[defaults]\ntypo = 1").is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(parse_duration("1.5s").unwrap(), Duration::from_millis(1500));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("250").unwrap(), Duration::from_millis(250));
        assert!(parse_duration("fast").is_err());
        assert!(parse_duration("5 parsecs").is_err());
    }

    #[test]
    fn imports_memex_machines() {
        let memex = r#"
[multi_machine]
default = ["local", "mm", "gone"]
timeout_seconds = 10

[[machines]]
id = "mm"
label = "Mac mini"
[machines.control]
type = "ssh"
host = "mm"
[machines.index]
type = "remote"

[[machines]]
id = "short"
ssh = "short-host"
"#;
        let (machines, defaults) = import_memex_config(memex).unwrap();
        assert_eq!(machines.len(), 2);
        assert_eq!(machines[0].ssh, "mm");
        assert_eq!(machines[0].label.as_deref(), Some("Mac mini"));
        assert_eq!(machines[1].ssh, "short-host");
        assert_eq!(defaults, vec!["local", "mm"]);
        let rendered = render_config(&machines, &defaults);
        let config = Config::parse(&rendered).unwrap();
        assert_eq!(config.machines.len(), 2);
        assert_eq!(config.select(&[]).unwrap(), vec!["local", "mm"]);
    }
}
