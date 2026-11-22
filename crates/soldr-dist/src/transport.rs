//! Bounded protobuf control frames and independent streaming blob transfers.
use crate::{cas::Cas, proto::Blob, trust::node_key, Result};
use iroh::{
    endpoint::{presets, Connection, QuicTransportConfig, RecvStream, SendStream},
    Endpoint, EndpointAddr, SecretKey,
};
use prost::Message;
use std::{path::Path, time::Duration};

pub const ALPN: &[u8] = b"soldr-dist/0";
pub const VERSION: u32 = 1;
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
        // Ephemeral UDP sockets allow outbound relay traffic and hole-punch return packets;
        // no fixed listener, TCP service or router port-mapping is configured.
        builder = builder.bind_addr("0.0.0.0:0")?.bind_addr("[::]:0")?;
        if let Some(url) = relay_url {
            let url: iroh::RelayUrl = url.parse()?;
            if url.scheme() != "https" {
                return Err("dist relay URL must use HTTPS".into());
            }
            builder = builder.relay_mode(iroh::RelayMode::custom([url]));
        }
    }
    Ok(builder.bind().await?)
}
/// Human-portable dial ticket. JSON is configuration, never the job wire format.
pub fn ticket(addr: &EndpointAddr) -> Result<String> {
    Ok(serde_json::to_string(addr)?)
}
pub fn parse_ticket(ticket: &str) -> Result<EndpointAddr> {
    Ok(serde_json::from_str(ticket)?)
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
pub async fn upload(conn: &Connection, digest: String, file: std::fs::File) -> Result<()> {
    let size = file.metadata()?.len();
    let mut send_stream = conn.open_uni().await?;
    send(&mut send_stream, &Blob { digest, size }).await?;
    let mut file = tokio::fs::File::from_std(file);
    tokio::io::copy(&mut file, &mut send_stream).await?;
    send_stream.finish()?;
    Ok(())
}
pub async fn receive_blob(
    conn: &Connection,
    cas: &Cas,
    expected: &[(String, u64)],
) -> Result<String> {
    let mut stream = conn.accept_uni().await?;
    let blob: Blob = recv(&mut stream).await?;
    if blob.size > BLOB_LIMIT
        || !expected
            .iter()
            .any(|(d, s)| d == &blob.digest && *s == blob.size)
    {
        return Err("unsolicited blob or incorrect size".into());
    }
    cas.receive_stream(&blob.digest, blob.size, &mut stream)
        .await?;
    Ok(blob.digest)
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
