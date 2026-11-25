//! Deliberate serde copy of forge-ci/signal.forge-ci.com proto/src/lib.rs
//! at 02069c72a3f5cac9d9e48a77dc7e7d0244b23b74 (PR #24).
//! Soldr permits exact crates.io pins, not git dependencies. JSON is the
//! canonical external protocol. Its upstream fixture-test module is replaced
//! by conformance coverage in this crate's single integration target.
//! JSON phone-home protocol. All timestamps are Unix milliseconds.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

pub const TTL_MS: i64 = 15_000;
pub const HEARTBEAT_S: u64 = 5;
pub const MAX_MESSAGE: usize = 16 * 1024;
pub fn encode(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}
pub fn decode(text: &str) -> Result<Vec<u8>, base64::DecodeError> {
    STANDARD.decode(text)
}
/// Sign the UTF-8 challenge string followed immediately by the enrolled name.
pub fn signing_bytes(nonce: &str, name: &str) -> Vec<u8> {
    [nonce.as_bytes(), name.as_bytes()].concat()
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Runner,
    Dist,
    Shared,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Versions {
    pub soldr: String,
    pub agent: String,
    pub os: String,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Load {
    pub cpus: u32,
    pub gh_jobs_running: u32,
    pub gh_cpus_reserved: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Dist {
    pub node_id: String,
    pub relay_url: Option<String>,
    pub slots_total: u32,
    pub slots_free: u32,
}
// Deliberately no Debug: hello carries a signature.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub name: String,
    pub sig: String,
    pub boot_id: String,
    pub role: Role,
    pub ips: Vec<IpAddr>,
    pub uptime_s: u64,
    pub versions: Versions,
    pub load: Load,
    pub dist: Option<Dist>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ToHost {
    Challenge { nonce: String },
    Ping { id: u64 },
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum FromHost {
    Hello(Hello),
    Heartbeat { load: Load, dist: Option<Dist> },
    Pong { id: u64, checks: Vec<Check> },
}
/// WebSocket close control frames, represented as JSON only in fixtures.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CloseReason {
    #[serde(rename = "authentication refused")]
    AuthenticationRefused,
    #[serde(rename = "connection ended")]
    ConnectionEnded,
}
impl CloseReason {
    pub const fn code(self) -> u16 {
        match self {
            Self::AuthenticationRefused => 1008,
            Self::ConnectionEnded => 1000,
        }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AuthenticationRefused => "authentication refused",
            Self::ConnectionEnded => "connection ended",
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServerClose {
    pub code: u16,
    pub reason: CloseReason,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostState {
    Online,
    Offline,
    NeverSeen,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    Online,
    Offline,
    Rebooted,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostEvent {
    pub name: String,
    pub kind: EventKind,
    pub at_ms: i64,
    pub boot_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostView {
    pub name: String,
    pub owner: String,
    pub role: Role,
    pub state: HostState,
    pub last_seen: Option<i64>,
    pub offline_since: Option<i64>,
    pub boot_id: Option<String>,
    pub uptime_s: Option<u64>,
    pub ips: Vec<IpAddr>,
    pub versions: Option<Versions>,
    pub load: Option<Load>,
    pub dist: Option<Dist>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Health {
    pub state: HostState,
    pub offline_since: Option<i64>,
    pub checks: Vec<Check>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DistNode {
    pub name: String,
    pub node_id: String,
    pub relay_url: Option<String>,
    pub slots_free: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn challenge_bytes_are_unambiguous_for_fixed_length_nonces() {
        assert_eq!(signing_bytes("YWJj", "node"), b"YWJjnode");
        assert_eq!(decode(&encode(&[1, 2, 3])).unwrap(), vec![1, 2, 3]);
    }
}
