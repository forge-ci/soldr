use crate::{proto::Role, Result};
use serde::Deserialize;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub name: String,
    pub role: Role,
    pub signal_url: String,
    #[serde(default = "default_key")]
    pub key: PathBuf,
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    #[serde(default = "default_bind")]
    pub health_bind: SocketAddr,
    #[serde(default)]
    pub units: Vec<String>,
    #[serde(default)]
    pub host_load_file: Option<PathBuf>,
    /// Trusted executable plus arguments, never a shell command.
    #[serde(default)]
    pub dist_status_cmd: Option<Vec<String>>,
    #[serde(default = "default_soldr_root")]
    pub soldr_root: PathBuf,
}
pub fn system_service() -> bool {
    std::env::var_os("INVOCATION_ID").is_some_and(|v| !v.is_empty())
        || std::env::var_os("SYSTEMD_EXEC_PID").is_some_and(|v| !v.is_empty())
}
fn user_root() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".soldr")
}
pub fn default_key() -> PathBuf {
    if system_service() {
        PathBuf::from("/var/lib/soldr/host.key")
    } else {
        user_root().join("host.key")
    }
}
pub fn default_state_dir() -> PathBuf {
    if system_service() {
        PathBuf::from("/var/lib/soldr/agent")
    } else {
        user_root().join("agent")
    }
}
fn default_soldr_root() -> PathBuf {
    if system_service() {
        PathBuf::from("/var/lib/soldr")
    } else {
        user_root()
    }
}
fn default_bind() -> SocketAddr {
    ([0, 0, 0, 0], 9099).into()
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        Self::parse(&std::fs::read_to_string(path)?)
    }
    pub fn parse(text: &str) -> Result<Self> {
        // Do not echo the config or parser context into logs.
        let config: Self = toml::from_str(text).map_err(|_| "invalid soldr agent configuration")?;
        if config.name.is_empty()
            || config.name.len() > 128
            || !config
                .name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            || config.units.len() > 32
            || config
                .units
                .iter()
                .any(|u| u.is_empty() || u.starts_with('-') || u.len() > 256)
            || config
                .dist_status_cmd
                .as_ref()
                .is_some_and(|c| c.is_empty() || c[0].is_empty())
        {
            return Err("invalid agent name, units or dist command".into());
        }
        config.connect_url()?;
        let ip = config.health_bind.ip();
        // Unspecified allows a LAN firewall policy; explicit public binds are refused.
        let allowed = match ip {
            std::net::IpAddr::V4(v) => {
                v.is_unspecified() || v.is_loopback() || v.is_private() || v.is_link_local()
            }
            std::net::IpAddr::V6(v) => {
                v.is_loopback() || v.is_unique_local() || v.is_unicast_link_local()
            }
        };
        if !allowed {
            return Err("health_bind must be a LAN or loopback address".into());
        }
        Ok(config)
    }
    pub fn connect_url(&self) -> Result<url::Url> {
        let mut url = url::Url::parse(&self.signal_url).map_err(|_| "invalid signal_url")?;
        let loopback = url.host_str().is_some_and(|s| {
            s.parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
        });
        if !(url.scheme() == "wss" || url.scheme() == "https" || (url.scheme() == "ws" && loopback))
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("signal_url requires wss/https (ws only for loopback), without credentials or query".into());
        }
        if url.scheme() == "https" {
            url.set_scheme("wss")
                .map_err(|()| "invalid signal_url scheme")?;
        }
        url.set_path("/v1/hosts/connect");
        Ok(url)
    }
}
