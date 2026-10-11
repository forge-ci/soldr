//! binstall `[package.metadata.binstall.signing]` minisign verification
//! (soldr#3735).
//!
//! Fail-closed: once a crate declares `signing`, an unsupported algorithm, a
//! malformed public key, a missing or undecodable signature, or a signature
//! that does not verify over the downloaded asset is always fatal, even when
//! `SOLDR_CHECKSUMS_FILE` pins the asset. Only prehashed (BLAKE2b) minisign
//! signatures are accepted; legacy signatures are refused.
//!
//! Trust semantics: the public key comes from the same crates.io `.crate`
//! that names the asset URL, so it is not an independent trust root. A
//! verified signature proves the asset was signed by whoever controls the
//! crate's published metadata (it defends against a tampered or swapped
//! asset host), and is logged as `trust: minisign-verified`. It is not a
//! pin: it never satisfies `SOLDR_TRUST_MODE=strict` on its own.

use std::io::Read as _;
use std::path::Path;

use crate::core::{SoldrError, TargetTriple};

use super::binstall_template::{render, PkgFmt, Signing, TemplateVars};

/// Upper bound on a `.sig` file read.
const MAX_SIG_BYTES: u64 = 64 * 1024;

fn sig_err(crate_name: &str, what: impl std::fmt::Display) -> SoldrError {
    SoldrError::Other(format!("binstall: minisign: {crate_name}: {what}"))
}

/// A parsed, ready-to-check `signing` declaration.
pub(crate) struct Verifier {
    crate_name: String,
    key: minisign_verify::PublicKey,
    /// The signature URL (rendered from `signing.file` or `{ url }.sig`).
    pub sig_url: String,
}

impl Verifier {
    /// Validate the declaration for `asset_url`. Errors are fatal.
    pub(crate) fn new(
        signing: &Signing,
        crate_name: &str,
        version: &str,
        target: &TargetTriple,
        pkg_fmt: PkgFmt,
        asset_url: &str,
    ) -> Result<Self, SoldrError> {
        if signing.algorithm != "minisign" {
            return Err(sig_err(
                crate_name,
                format!(
                    "unsupported signing algorithm `{}` (only `minisign`)",
                    signing.algorithm
                ),
            ));
        }
        let key = minisign_verify::PublicKey::from_base64(signing.pubkey.trim())
            .map_err(|e| sig_err(crate_name, format!("invalid public key: {e}")))?;
        let sig_url = match &signing.file {
            Some(template) => render(
                template,
                &TemplateVars {
                    name: crate_name,
                    version,
                    target,
                    pkg_fmt,
                    repo: None,
                    bin: None,
                    url: Some(asset_url),
                },
            )?,
            None => format!("{asset_url}.sig"),
        };
        Ok(Self {
            crate_name: crate_name.to_string(),
            key,
            sig_url,
        })
    }

    /// Decode a downloaded `.sig` file.
    pub(crate) fn decode_signature(
        &self,
        sig_file: &Path,
    ) -> Result<minisign_verify::Signature, SoldrError> {
        let mut text = String::new();
        std::fs::File::open(sig_file)?
            .take(MAX_SIG_BYTES)
            .read_to_string(&mut text)
            .map_err(|e| sig_err(&self.crate_name, format!("unreadable signature: {e}")))?;
        minisign_verify::Signature::decode(&text)
            .map_err(|e| sig_err(&self.crate_name, format!("invalid signature file: {e}")))
    }

    /// Verify `asset` against `signature`, streaming. Errors are fatal.
    pub(crate) fn verify_file(
        &self,
        asset: &Path,
        signature: &minisign_verify::Signature,
    ) -> Result<(), SoldrError> {
        let mut stream = self
            .key
            .verify_stream(signature)
            .map_err(|e| sig_err(&self.crate_name, format!("unusable signature: {e}")))?;
        let mut file = std::fs::File::open(asset)?;
        let mut buf = vec![0_u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            stream.update(&buf[..n]);
        }
        stream.finalize().map_err(|e| {
            sig_err(
                &self.crate_name,
                format!("signature verification FAILED: {e}"),
            )
        })?;
        eprintln!(
            "soldr: trust: minisign-verified {} (publisher key from the crate's own metadata; not a pin)",
            self.crate_name
        );
        Ok(())
    }
}
