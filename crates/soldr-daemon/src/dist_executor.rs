//! Static peers for zccache's post-lookup miss executor. No discovery or registry.
use crate::core::{SoldrConfig, SoldrPaths};
use soldr_dist::{compiler::Compiler, transport, DistClient, DistJob, DistOutcome, InputFile};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use zccache::embedded::{
    CompileExecutor, CompileJob, ExecutorFuture, ExecutorOutcome, ZccacheService,
};
mod namespace;

struct Executor {
    clients: Vec<DistClient>,
    next: AtomicUsize,
    compiler: Compiler,
    cache_identity: [u8; 32],
    _lease: transport::EndpointLease,
    root: std::path::PathBuf,
    inputs: tokio::sync::Mutex<soldr_dist::cas::DigestMemo>,
}

pub(crate) async fn register(
    service: ZccacheService,
    paths: &SoldrPaths,
) -> soldr_dist::Result<ZccacheService> {
    let config = SoldrConfig::load(&paths.config_file)?.dist;
    let peers = peer_list(
        std::env::var("SOLDR_DIST_PEERS").ok().as_deref(),
        config.peers,
    )?;
    if peers.is_empty() {
        return Ok(service);
    }
    let root = std::env::var_os("SOLDR_DIST_ROOT")
        .map(std::path::PathBuf::from)
        .or(config.root)
        .unwrap_or_else(|| paths.root.join("dist"));
    let relay_disabled = std::env::var("SOLDR_DIST_RELAY").ok().as_deref() == Some("none");
    let relay = if relay_disabled {
        None
    } else {
        std::env::var("SOLDR_DIST_RELAY_URL")
            .ok()
            .or(config.relay_url)
    };
    let addresses = peers
        .iter()
        .map(|peer| transport::parse_peer(peer, relay.as_deref()))
        .collect::<soldr_dist::Result<Vec<_>>>()?;
    let relay = if relay_disabled {
        None
    } else {
        relay.or_else(|| {
            addresses
                .iter()
                .flat_map(|peer| peer.relay_urls())
                .next()
                .map(ToString::to_string)
        })
    };
    let identity = soldr_dist::trust::node_key(&root)?;
    let lease = transport::EndpointLease::acquire_with_key(&root, &root.join("node.key"))?;
    let endpoint = transport::endpoint_with_identity(identity, false, relay.as_deref()).await?;
    let clients = addresses
        .into_iter()
        .map(|peer| DistClient::new(endpoint.clone(), peer, &root))
        .collect::<soldr_dist::Result<Vec<_>>>()?;
    let compiler = Compiler::rustc().await?;
    let output = compiler.command().arg("-vV").capture().await?;
    if !output.status.success() || output.stdout.is_empty() {
        return Err("dist executor could not verify the local rustc cache identity".into());
    }
    let cache_identity = *zccache::hash::hash_bytes(&output.stdout).as_bytes();
    tracing::info!(peers = clients.len(), root = %root.display(), "soldr dist miss executor enabled");
    Ok(service.with_executor(Arc::new(Executor {
        clients,
        next: AtomicUsize::new(0),
        compiler,
        cache_identity,
        inputs: tokio::sync::Mutex::new(soldr_dist::cas::DigestMemo::default()),
        _lease: lease,
        root,
    }))?)
}

impl Executor {
    fn declined(&self, job: &CompileJob, reason: String) -> ExecutorOutcome {
        let digests: std::collections::HashSet<_> =
            job.inputs.iter().map(|input| input.digest).collect();
        let metrics = soldr_dist::JobMetrics {
            outcome: "declined".into(),
            completed_unix_millis: soldr_dist::metrics::unix_millis(),
            path: "unknown".into(),
            decline_reason: reason.clone(),
            exit_code: -1,
            input_files: job.inputs.len() as u64,
            input_digests: digests.len() as u64,
            dedup_inputs: job.inputs.len().saturating_sub(digests.len()) as u64,
            ..Default::default()
        };
        if let Err(error) = soldr_dist::status::record_job(&self.root, &metrics) {
            tracing::warn!(%error, "soldr dist cannot persist policy decline metrics");
        }
        ExecutorOutcome::Declined { reason }
    }
}

fn relay_config(
    mode: Option<&str>,
    configured: Option<String>,
) -> soldr_dist::Result<Option<String>> {
    transport::relay_config(mode, configured)
}

fn reverse_mode(value: Option<&str>, configured: bool) -> soldr_dist::Result<bool> {
    match value {
        None => Ok(configured),
        Some("true" | "1") => Ok(true),
        Some("false" | "0") => Ok(false),
        Some(_) => Err("SOLDR_DIST_REVERSE_ONLY must be true or false".into()),
    }
}

fn peer_list(value: Option<&str>, configured: Vec<String>) -> soldr_dist::Result<Vec<String>> {
    match value.map(str::trim) {
        None => Ok(configured),
        Some("") => Ok(Vec::new()),
        Some(value) if value.starts_with('[') => Ok(serde_json::from_str(value)?),
        Some(value) => Ok(vec![value.to_owned()]),
    }
}

fn manifest(job: &CompileJob, compiler: &Compiler, identity: &[u8; 32]) -> Result<DistJob, String> {
    if job.compiler_kind != zccache::compiler::CompilerFamily::Rustc {
        return Err("dist miss bring-up currently supports rustc only".into());
    }
    if job.compiler_name != "rustc" && job.compiler_name != "rustc.exe" {
        return Err("dist miss adapter requires the plain rustc driver".into());
    }
    if &job.compiler_identity != identity {
        return Err("requested rustc differs from the dist client's resolved toolchain".into());
    }
    let path = |p: &std::path::Path| {
        p.to_str()
            .map(str::to_owned)
            .ok_or_else(|| "non-UTF8 dist path".to_owned())
    };
    let inputs = job
        .inputs
        .iter()
        .map(|input| {
            Ok(InputFile {
                abs_path: path(input.path.as_path())?,
                blake3: hex::encode(input.digest),
                size: input.size,
                executable: input.executable,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(DistJob {
        rewrite_env: vec![],
        compiler_kind: compiler.kind.clone(),
        compiler_identity: compiler.identity.clone(),
        argv: job.args.clone(),
        env: execution_env(job),
        cwd: path(job.cwd.as_path())?,
        stdin: job.stdin.clone(),
        inputs,
        outputs: job
            .outputs
            .iter()
            .map(|p| path(p.as_path()))
            .collect::<Result<_, _>>()?,
    })
}

fn execution_env(job: &CompileJob) -> std::collections::HashMap<String, String> {
    job.exec_env.iter().cloned().collect()
}

impl CompileExecutor for Executor {
    fn capacity(&self) -> usize {
        // Only concurrent remote offers: compiler capacity remains node-owned.
        64
    }

    fn execute<'a>(&'a self, job: &'a CompileJob) -> ExecutorFuture<'a> {
        Box::pin(async move {
            let manifest = match manifest(job, &self.compiler, &self.cache_identity) {
                Ok(job) => job,
                Err(reason) => return self.declined(job, reason),
            };
            if let Err(reason) = namespace::check(&manifest, true) {
                return self.declined(job, reason);
            }
            let manifest = match soldr_dist::rust_inputs::construct(manifest, &self.inputs).await {
                Ok(manifest) => manifest,
                Err(error) => {
                    return self.declined(job, format!("dist input construction: {error}"))
                }
            };
            let index = self.next.fetch_add(1, Ordering::Relaxed) % self.clients.len();
            match self.clients[index].execute(manifest).await {
                DistOutcome::Executed {
                    exit_code,
                    stdout,
                    stderr,
                    metrics,
                } => {
                    tracing::info!(exit_code, path = %metrics.path, rtt_micros = metrics.rtt_micros,
                        sent = metrics.bytes_sent, received = metrics.bytes_received,
                        need = metrics.need_digests, cas_hits = metrics.cas_hits,
                        "soldr dist remote cache miss executed");
                    ExecutorOutcome::Executed {
                        exit_code,
                        stdout,
                        stderr,
                    }
                }
                DistOutcome::Declined { reason, .. } => {
                    tracing::warn!(%reason, "soldr dist declined cache miss; executing locally");
                    ExecutorOutcome::Declined { reason }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::peer_list;
    #[test]
    fn reverse_and_public_relay_modes_are_explicit_overrides() {
        assert!(super::relay_config(Some("default"), None).is_err());
        assert!(
            super::relay_config(Some("none"), Some("https://custom.example".into()))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            super::relay_config(None, None).unwrap().as_deref(),
            Some(soldr_dist::transport::DEFAULT_RELAY_URL)
        );
        assert_eq!(
            super::relay_config(Some("https://signal.forge-ci.com"), None)
                .unwrap()
                .as_deref(),
            Some("https://signal.forge-ci.com")
        );
        assert!(super::relay_config(Some("auto"), None).is_err());
        assert!(super::reverse_mode(None, true).unwrap());
        assert!(super::reverse_mode(Some("true"), false).unwrap());
        assert!(!super::reverse_mode(Some("false"), true).unwrap());
        assert!(super::reverse_mode(Some("typo"), false).is_err());
    }
    #[test]
    fn static_peers_preserve_json_tickets_and_env_precedence() {
        let ticket = r#"{"id":"node", "addrs":["127.0.0.1:7"]}"#;
        assert_eq!(
            peer_list(Some(ticket), vec!["config".into()]).unwrap(),
            [ticket]
        );
        assert_eq!(peer_list(None, vec!["config".into()]).unwrap(), ["config"]);
        assert!(peer_list(Some(" "), vec!["config".into()])
            .unwrap()
            .is_empty());
        assert_eq!(peer_list(Some(r#"["a","b"]"#), vec![]).unwrap(), ["a", "b"]);
        assert!(peer_list(Some("[broken"), vec![]).is_err());
    }

    #[test]
    fn manifest_accepts_cold_inputs_and_requires_matching_cache_identity() {
        use super::manifest;
        let compiler = soldr_dist::compiler::Compiler {
            kind: "rustc".into(),
            identity: "commit-hash: same\nhost: same".into(),
            program: "soldr".into(),
            prefix: vec![],
            toolchain: None,
        };
        let mut job = zccache::embedded::CompileJob {
            compiler_kind: zccache::compiler::CompilerFamily::Rustc,
            compiler_name: "rustc".into(),
            compiler_identity: [7; 32],
            args: vec!["--emit=metadata".into()],
            env: vec![],
            exec_env: vec![],
            request_args: vec![],
            cwd: std::path::PathBuf::from("/tmp").into(),
            stdin: vec![],
            inputs: vec![],
            inputs_complete: false,
            outputs: vec![std::path::PathBuf::from("/tmp/test.rmeta").into()],
        };
        assert!(manifest(&job, &compiler, &[7; 32]).is_ok());
        job.inputs_complete = true;
        assert!(manifest(&job, &compiler, &[8; 32])
            .unwrap_err()
            .contains("toolchain"));
        let mapped = manifest(&job, &compiler, &[7; 32]).unwrap();
        assert_eq!(mapped.compiler_identity, compiler.identity);
        assert_eq!(mapped.outputs, ["/tmp/test.rmeta"]);
        job.compiler_kind = zccache::compiler::CompilerFamily::Gcc;
        assert!(manifest(&job, &compiler, &[7; 32])
            .unwrap_err()
            .contains("rustc only"));
    }
}
