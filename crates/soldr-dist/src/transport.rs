//! Bounded protobuf control frames and independent streaming blob transfers.
use crate::{proto::Blob, trust::node_key, Result};
use iroh::{
    endpoint::{presets, Connection, QuicTransportConfig, RecvStream, SendStream},
    Endpoint, EndpointAddr, SecretKey,
};
use prost::Message;
use std::{path::Path, time::Duration};

pub const ALPN: &[u8] = b"soldr-dist/0";
pub const VERSION: u32 = 3;
pub const DEFAULT_RELAY_URL: &str = "https://signal.forge-ci.com";
pub const CONTROL_LIMIT: usize = 8 * 1024 * 1024;
pub const BLOB_LIMIT: u64 = 16 * 1024 * 1024 * 1024;

pub async fn endpoint(root: &Path, direct_only: bool) -> Result<Endpoint> {
    endpoint_with_relay(root, direct_only, None).await
}
pub async fn endpoint_with_relay(
    root: &Path,
    direct_only: bool,
    relay_url: Option<&str>,
) -> Result<Endpoint> {
    endpoint_with_identity(node_key(root)?, direct_only, relay_url).await
}

/// Accept an existing host identity without coupling transport to its storage/provider.
pub async fn endpoint_with_identity(
    identity: SecretKey,
    direct_only: bool,
    relay_url: Option<&str>,
) -> Result<Endpoint> {
    let config = QuicTransportConfig::builder()
        .max_idle_timeout(Some(Duration::from_secs(5).try_into()?))
        .keep_alive_interval(Duration::from_secs(1))
        .default_path_max_idle_timeout(Duration::from_secs(5))
        .default_path_keep_alive_interval(Duration::from_secs(1))
        .build();
    let mut builder = Endpoint::builder(presets::Minimal)
        .secret_key(identity)
        .alpns(vec![ALPN.to_vec()])
        .transport_config(config)
        .clear_address_lookup()
        .relay_mode(iroh::RelayMode::Disabled)
        .clear_ip_transports();
    if direct_only {
        builder = builder
            .clear_relay_transports()
            .clear_address_lookup()
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")?;
    }
    if !direct_only {
        // Keep both UDP transports even with a relay: iroh coordinates simultaneous-open
        // hole punching over the relay and can select a direct path after connection setup.
        // No fixed listener, TCP service or router port-mapping is configured.
        builder = builder.bind_addr("0.0.0.0:0")?.bind_addr("[::]:0")?;
        if let Some(url) = relay_url {
            builder = builder.relay_mode(relay_mode(Some(url))?);
        } else {
            builder = builder.clear_relay_transports();
        }
    }
    Ok(builder.bind().await?)
}

/// Resolve CLI/environment policy; low-level `None` still disables relays for tests.
pub fn relay_config(mode: Option<&str>, configured: Option<String>) -> Result<Option<String>> {
    let selected = mode
        .map(str::to_owned)
        .or(configured)
        .unwrap_or_else(|| DEFAULT_RELAY_URL.to_owned());
    if selected == "none" {
        return Ok(None);
    }
    relay_mode(Some(&selected))?;
    Ok(Some(selected))
}

/// Validate a CLI relay selection without enabling public discovery backends.
pub fn parse_relay_selection(value: &str) -> Result<String> {
    relay_config(Some(value), None)?;
    Ok(value.to_owned())
}

/// Forge-owned relays only: peer identity metadata must never reach a third party.
pub fn relay_mode(value: Option<&str>) -> Result<iroh::RelayMode> {
    match value {
        None => Ok(iroh::RelayMode::Disabled),
        Some(value) => Ok(iroh::RelayMode::custom([relay_url(value)?])),
    }
}

pub fn relay_url(value: &str) -> Result<iroh::RelayUrl> {
    let url: iroh::RelayUrl = value.parse().map_err(|_| {
        "\x1b[31mDist privacy rule: relay must be none or HTTPS on signal.forge-ci.com\x1b[0m"
    })?;
    if url.scheme() != "https"
        || url.host_str() != Some("signal.forge-ci.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
    {
        return Err("\x1b[31mDist privacy rule: no third party may know our peers; only HTTPS relays on signal.forge-ci.com are allowed\x1b[0m".into());
    }
    Ok(url)
}

pub fn validate_peer(addr: &EndpointAddr) -> Result<()> {
    for url in addr.relay_urls() {
        relay_url(url.as_str())?;
    }
    Ok(())
}
/// Human-portable dial ticket. JSON is configuration, never the job wire format.
pub fn ticket(addr: &EndpointAddr) -> Result<String> {
    Ok(serde_json::to_string(addr)?)
}
pub fn parse_ticket(ticket: &str) -> Result<EndpointAddr> {
    let addr = serde_json::from_str(ticket)?;
    validate_peer(&addr)?;
    Ok(addr)
}

/// A static peer needs either a full ticket or a node ID plus an explicit relay.
pub fn parse_peer(value: &str, relay_url: Option<&str>) -> Result<EndpointAddr> {
    if let Ok(id) = value.parse::<iroh::EndpointId>() {
        let relay = self::relay_url(
            relay_url.ok_or("a node-id peer needs a Forge relay or a full direct ticket")?,
        )?;
        return Ok(EndpointAddr::new(id).with_relay_url(relay));
    }
    parse_ticket(value)
        .map_err(|e| format!("expected one static node ID or printed JSON ticket: {e}").into())
}

pub fn parse_worker(
    value: &str,
    relay_url: Option<&str>,
    reverse_only: bool,
) -> Result<EndpointAddr> {
    if reverse_only {
        if let Ok(id) = value.parse::<iroh::EndpointId>() {
            return Ok(EndpointAddr::new(id));
        }
    }
    parse_peer(value, relay_url)
}

pub async fn send<M: Message>(stream: &mut SendStream, message: &M) -> Result<()> {
    let bytes = message.encode_to_vec();
    if bytes.len() > CONTROL_LIMIT {
        return Err("Dist control frame too large".into());
    }
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .await?;
    stream.write_all(&bytes).await?;
    Ok(())
}
pub async fn recv<M: Message + Default>(stream: &mut RecvStream) -> Result<M> {
    let mut len = [0; 4];
    stream.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > CONTROL_LIMIT {
        return Err("Dist control frame too large".into());
    }
    let mut bytes = vec![0; len];
    stream.read_exact(&mut bytes).await?;
    Ok(M::decode(bytes.as_slice())?)
}
/// Sends one blob on its own stream, tagged with the joblet it belongs to.
/// Returns the payload size.
pub async fn upload(
    conn: &Connection,
    job_token: u64,
    digest: String,
    file: std::fs::File,
) -> Result<u64> {
    let size = file.metadata()?.len();
    let mut send_stream = conn.open_uni().await?;
    send(
        &mut send_stream,
        &Blob {
            digest,
            size,
            job_token,
        },
    )
    .await?;
    let mut file = tokio::fs::File::from_std(file);
    tokio::io::copy(&mut file, &mut send_stream).await?;
    send_stream.finish()?;
    Ok(size)
}

/// Hold while a CLI endpoint uses this persistent identity. Separate processes
/// with the same key would replace each other's outbound relay registration.
pub struct EndpointLease {
    _identity: Option<std::fs::File>,
    _file: std::fs::File,
}
impl EndpointLease {
    pub fn acquire(root: &Path) -> Result<Self> {
        use fs2::FileExt;
        std::fs::create_dir_all(root)?;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("endpoint.lock"))?;
        file.try_lock_exclusive().map_err(|e| format!("another dist endpoint owns {}: {e}; reuse its library Endpoint or choose a separate --root", root.display()))?;
        Ok(Self {
            _identity: None,
            _file: file,
        })
    }
}

impl EndpointLease {
    /// Lock the selected key as well as the root. A read-only open is sufficient;
    /// advisory ownership does not prevent the agent from reading/signing with it.
    pub fn acquire_with_key(root: &Path, key_path: &Path) -> Result<Self> {
        use fs2::FileExt;
        let mut lease = Self::acquire(root)?;
        let file = std::fs::File::open(key_path)?;
        file.try_lock_exclusive().map_err(|e| format!(
            "another dist endpoint owns identity {}: {e}; reuse its Endpoint instead of starting a second one",
            key_path.display()
        ))?;
        lease._identity = Some(file);
        Ok(lease)
    }
}
