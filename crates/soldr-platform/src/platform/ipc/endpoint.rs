//! Endpoint representation and OS-safe name/path derivation.

pub use crate::platform_imp::ipc::endpoint::{
    ephemeral, legacy_daemon_endpoint, machine_runtime_dir, path_is_on_non_bindable_filesystem,
    socket_path_bytes, socket_path_fits, sun_path_capacity,
};

/// A Windows pipe name derived from an executable path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsPipeName {
    /// The canonical logical socket path (the `.sock` sibling of the
    /// executable, lowercased ASCII).
    pub logical_socket_path: String,
    /// The percent-encoded pipe leaf for `\\.\pipe\` APIs.
    pub pipe_leaf: String,
    /// The pre-fallback encoded leaf when it overflowed the pipe-name
    /// limit, for diagnostics.
    pub oversized_leaf: Option<String>,
    /// True when the complete pipe name exceeded the Windows limit and
    /// the deterministic hash fallback was used.
    pub overflowed: bool,
}

/// Maximum complete Windows named-pipe path length.
pub const WINDOWS_PIPE_NAME_LIMIT: usize = 256;
/// The Windows named-pipe path prefix.
pub const WINDOWS_PIPE_PREFIX: &str = r"\\.\pipe\";

/// Canonicalize and percent-encode a Windows executable path into the
/// pipe leaf for an endpoint beside it, without relying on host-platform
/// `Path` parsing — the complete Windows matrix runs under the Linux
/// development harness too.
///
/// Kept in the neutral facade (cfg-free string logic) so its sanitizer
/// tests compile and run on every host; the Windows concrete tree calls
/// it for its endpoint naming.
pub fn windows_pipe_from_executable_with_suffix(
    executable: &str,
    socket_suffix: &str,
    overflow_prefix: &str,
) -> Result<WindowsPipeName, String> {
    let original = executable.to_string();
    let mut normalized = executable.replace('/', "\\");

    if ascii_starts_with_ignore_case(&normalized, r"\\?\UNC\") {
        normalized = format!(r"\\{}", &normalized[8..]);
    } else if ascii_starts_with_ignore_case(&normalized, r"\\?\") {
        normalized = normalized[4..].to_string();
    }

    let (root, remainder, minimum_components) = if normalized.len() >= 3
        && normalized.as_bytes()[0].is_ascii_alphabetic()
        && normalized.as_bytes()[1] == b':'
        && normalized.as_bytes()[2] == b'\\'
    {
        (normalized[..3].to_string(), &normalized[3..], 0_usize)
    } else if let Some(remainder) = normalized.strip_prefix(r"\\") {
        (r"\\".to_string(), remainder, 2_usize)
    } else {
        return Err(format!(
            "windows executable path must be absolute: {original}"
        ));
    };

    let mut components = Vec::new();
    for component in remainder.split('\\') {
        match component {
            "" | "." => {}
            ".." => {
                return Err(format!(
                    "windows executable path contains unresolved '..': {original}"
                ))
            }
            value => components.push(value.to_string()),
        }
    }
    if components.len() < minimum_components {
        return Err(format!("unsupported windows executable path: {original}"));
    }
    let Some(last) = components.last_mut() else {
        return Err(format!(
            "windows executable path must end in .exe: {original}"
        ));
    };
    if last.len() < 4 || !last[last.len() - 4..].eq_ignore_ascii_case(".exe") {
        return Err(format!(
            "windows executable path must end in .exe: {original}"
        ));
    }
    last.truncate(last.len() - 4);
    last.push_str(socket_suffix);

    let joined = components.join("\\");
    let logical = if root == r"\\" {
        format!(r"\\{joined}")
    } else {
        format!("{root}{joined}")
    };
    let logical = ascii_lowercase(&logical);
    let encoded = percent_encode_pipe_leaf(logical.as_bytes());
    let overflowed = WINDOWS_PIPE_PREFIX.len() + encoded.len() > WINDOWS_PIPE_NAME_LIMIT;
    let (pipe_leaf, oversized_leaf) = if overflowed {
        let digest = kernal_api::hash::blake3_bytes(logical.as_bytes());
        let mut short = String::with_capacity(16);
        for byte in digest.as_bytes().iter().take(8) {
            use std::fmt::Write as _;
            let _ = write!(short, "{byte:02x}");
        }
        (format!("{overflow_prefix}-ovf-{short}"), Some(encoded))
    } else {
        (encoded, None)
    };

    Ok(WindowsPipeName {
        logical_socket_path: logical,
        pipe_leaf,
        oversized_leaf,
        overflowed,
    })
}

fn ascii_starts_with_ignore_case(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn ascii_lowercase(value: &str) -> String {
    let mut bytes = value.as_bytes().to_vec();
    bytes.make_ascii_lowercase();
    String::from_utf8(bytes).expect("ASCII folding preserves valid UTF-8")
}

fn percent_encode_pipe_leaf(bytes: &[u8]) -> String {
    let mut encoded = String::with_capacity(bytes.len());
    for byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-') {
            encoded.push(*byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_pipe_sanitizer_normalizes_supported_spellings() {
        let expected = windows_pipe_from_executable_with_suffix(
            r"C:\Users\Me\soldr-broker.exe",
            ".sock",
            "soldr-broker",
        )
        .expect("baseline");
        for spelling in [
            r"c:/users/me/soldr-broker.EXE",
            r"\\?\C:\Users\.\Me\soldr-broker.ExE",
        ] {
            let endpoint =
                windows_pipe_from_executable_with_suffix(spelling, ".sock", "soldr-broker")
                    .expect(spelling);
            assert_eq!(endpoint, expected, "{spelling}");
        }
        assert_eq!(
            expected.logical_socket_path,
            r"c:\users\me\soldr-broker.sock"
        );
        assert_eq!(expected.pipe_leaf, r"c%3A%5Cusers%5Cme%5Csoldr-broker.sock");
        assert!(!expected.overflowed);
    }

    #[test]
    fn windows_pipe_sanitizer_rejects_relative_and_parent_paths() {
        assert!(windows_pipe_from_executable_with_suffix(
            r"Users\me\soldr-broker.exe",
            ".sock",
            "soldr-broker"
        )
        .is_err());
        assert!(windows_pipe_from_executable_with_suffix(
            r"C:\Users\me\..\other\soldr-broker.exe",
            ".sock",
            "soldr-broker"
        )
        .is_err());
    }

    #[test]
    fn windows_pipe_overflow_falls_back_to_deterministic_hash() {
        let path = format!(r"C:\Users\{}\soldr-broker.exe", "long-profile-".repeat(30));
        let first = windows_pipe_from_executable_with_suffix(&path, ".sock", "soldr-broker")
            .expect("first");
        let second = windows_pipe_from_executable_with_suffix(&path, ".sock", "soldr-broker")
            .expect("second");
        assert_eq!(first, second);
        assert!(first.overflowed);
        assert!(first.pipe_leaf.starts_with("soldr-broker-ovf-"));
        assert_eq!(first.pipe_leaf.len(), "soldr-broker-ovf-".len() + 16);
    }

    #[test]
    fn distinct_windows_paths_produce_distinct_leaves() {
        let cases = [
            r"C:\Users\a\soldr-broker.exe",
            r"C:\Users\b\soldr-broker.exe",
            r"D:\Users\a\soldr-broker.exe",
            r"\\server\share\a\soldr-broker.exe",
        ];
        let mut leaves = std::collections::HashSet::new();
        for case in cases {
            let endpoint = windows_pipe_from_executable_with_suffix(case, ".sock", "soldr-broker")
                .expect(case);
            assert!(leaves.insert(endpoint.pipe_leaf));
        }
    }
}
