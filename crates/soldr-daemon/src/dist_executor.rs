//! Static peers for zccache's post-lookup miss executor. No discovery or registry.
use crate::core::{SoldrConfig, SoldrPaths};
use soldr_dist::{
    client_connections::ClientConnections, compiler::Compiler, transport, DistClient, DistJob,
    DistOutcome, InputFile,
};
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
    compiler_program: std::path::PathBuf,
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
    std::fs::create_dir_all(&root)?;
    let root = std::fs::canonicalize(root)?;
    let mode = std::env::var("SOLDR_DIST_RELAY").ok();
    let relay_disabled = mode.as_deref() == Some("none");
    let relay = relay_config(
        mode.as_deref(),
        std::env::var("SOLDR_DIST_RELAY_URL")
            .ok()
            .or(config.relay_url),
    )?;
    let reverse_only = reverse_mode(
        std::env::var("SOLDR_DIST_REVERSE_ONLY").ok().as_deref(),
        config.reverse_only,
    )?;
    let addresses = peers
        .iter()
        .map(|peer| transport::parse_worker(peer, relay.as_deref(), reverse_only))
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
    let connections = ClientConnections::new(endpoint, &root)?;
    let clients = addresses
        .into_iter()
        .map(|peer| {
            DistClient::with_connections(connections.clone(), peer, &root)
                .map(|client| client.with_reverse_only(reverse_only))
        })
        .collect::<soldr_dist::Result<Vec<_>>>()?;
    let compiler_program = Compiler::soldr_program(&root, &std::env::current_exe()?)?;
    tracing::info!(peers = clients.len(), root = %root.display(), "soldr dist miss executor enabled");
    Ok(service.with_executor(Arc::new(Executor {
        clients,
        next: AtomicUsize::new(0),
        compiler_program,
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

fn manifest(job: &CompileJob) -> Result<DistJob, String> {
    if job.compiler_kind != zccache::compiler::CompilerFamily::Rustc {
        return Err("dist miss bring-up currently supports rustc only".into());
    }
    if job.compiler_name != "rustc" && job.compiler_name != "rustc.exe" {
        return Err("dist miss adapter requires the plain rustc driver".into());
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
        compiler_kind: "rustc".into(),
        compiler_identity: Compiler::request_identity(&job.compiler_identity),
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
            let manifest = match manifest(job) {
                Ok(job) => job,
                Err(reason) => return self.declined(job, reason),
            };
            if let Err(reason) = namespace::check(&manifest, true) {
                return self.declined(job, reason);
            }
            let manifest = match soldr_dist::rust_inputs::construct_with_soldr(
                manifest,
                &self.inputs,
                &self.compiler_program,
            )
            .await
            {
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
    fn cold_manifest_preserves_the_hooks_exact_compiler_identity() {
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
        let first = super::manifest(&job).unwrap();
        assert_eq!(
            first.compiler_identity,
            soldr_dist::compiler::Compiler::request_identity(&[7; 32])
        );
        job.compiler_identity = [8; 32];
        assert_ne!(
            super::manifest(&job).unwrap().compiler_identity,
            first.compiler_identity
        );
        job.compiler_name = "clippy-driver".into();
        assert!(super::manifest(&job).is_err());
    }
}
