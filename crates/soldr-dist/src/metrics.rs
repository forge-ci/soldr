//! Per-joblet counters and the last selected iroh 1.5 network path.
use crate::{DistJob, DistOutcome, JobMetrics};
use iroh::endpoint::Connection;
use std::collections::HashSet;

pub fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

impl JobMetrics {
    pub fn for_job(worker_id: String, job: &DistJob) -> Self {
        let unique: HashSet<_> = job.inputs.iter().map(|input| &input.blake3).collect();
        Self {
            worker_id,
            path: "unknown".into(),
            input_files: job.inputs.len() as u64,
            input_digests: unique.len() as u64,
            dedup_inputs: job.inputs.len().saturating_sub(unique.len()) as u64,
            ..Self::default()
        }
    }

    pub(crate) fn capture(&mut self, conn: &Connection, before: (u64, u64)) {
        let stats = conn.stats();
        self.connection_bytes_sent = stats.udp_tx.bytes;
        self.connection_bytes_received = stats.udp_rx.bytes;
        self.bytes_sent = stats.udp_tx.bytes.saturating_sub(before.0);
        self.bytes_received = stats.udp_rx.bytes.saturating_sub(before.1);
        self.connection_side = format!("{:?}", conn.side());
        let (mut direct, mut relay) = (false, false);
        let paths = conn.paths();
        let mut addresses = Vec::new();
        for path in paths.iter().filter(|path| path.is_selected()) {
            direct |= path.is_ip();
            relay |= path.is_relay();
            addresses.push(format!("{:?}", path.remote_addr()));
            self.rtt_known = true;
            self.rtt_micros = self
                .rtt_micros
                .max(u64::try_from(path.rtt().as_micros()).unwrap_or(u64::MAX));
        }
        self.path = match (direct, relay) {
            (true, true) => "mixed",
            (true, false) => "direct",
            (false, true) => "relay",
            _ => "unknown",
        }
        .into();
        self.remote_addr = addresses.join(", ");
    }

    pub(crate) fn finish(&mut self, outcome: &DistOutcome) {
        self.completed_unix_millis = unix_millis();
        match outcome {
            DistOutcome::Executed { exit_code, .. } => {
                self.outcome = "executed".into();
                self.exit_code = *exit_code;
            }
            DistOutcome::Declined { reason, .. } => {
                self.outcome = "declined".into();
                self.decline_reason = reason.clone();
                self.exit_code = -1;
            }
        }
    }

    pub fn summary(&self) -> String {
        let rtt = if self.rtt_known {
            format!("{}us", self.rtt_micros)
        } else {
            "unknown".into()
        };
        let hits = if self.cas_hits_known {
            self.cas_hits.to_string()
        } else {
            "unknown".into()
        };
        format!("worker={} outcome={} path={} remote={} rtt={} sent={} received={} need={} cas_hits={} uploaded={} dedup={} elapsed={}ms",
            self.worker_id, self.outcome, self.path, self.remote_addr, rtt, self.bytes_sent,
            self.bytes_received, self.need_digests, hits, self.uploaded_blobs,
            self.dedup_inputs, self.elapsed_millis)
    }
}
