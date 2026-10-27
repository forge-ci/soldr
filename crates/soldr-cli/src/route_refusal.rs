//! Explaining a broker route refusal (soldr#3401).
//!
//! A `Refused` Hello reply used to surface as one flattened line
//! (`broker refused the daemon route: <reason> (code=8, retry_after_ms=30000,
//! details={})`), repeated once per in-flight crate. It named neither the
//! wrapper that was refused nor the route it dialed, and its retry hint invited
//! a retry that a version-policy refusal can never satisfy. The client knows
//! its own image, wanted version and dialed service, which is most of the
//! diagnostic.

use running_process::broker::client::RefusalKind;
use running_process::broker::protocol::ErrorCode;

/// A refusal no retry can fix: the wrapper and the route disagree by policy or
/// configuration, so the caller should stop rather than back off.
pub(crate) fn refusal_is_fatal(kind: RefusalKind) -> bool {
    matches!(
        kind,
        RefusalKind::VersionBlocked
            | RefusalKind::VersionUnsupported
            | RefusalKind::ServiceUnknown
            | RefusalKind::Other(ErrorCode::ErrorPeerRejected)
    )
}

/// The wire error code as a fatal/transient decision, for callers that only
/// hold the raw `i32` from the reply.
pub(crate) fn code_is_fatal(code: i32) -> bool {
    let error_code = ErrorCode::try_from(code).unwrap_or(ErrorCode::Unspecified);
    refusal_is_fatal(RefusalKind::from_code(error_code))
}

/// A broker route refusal carried through the `io::Error` chain as a value, not
/// a flattened string (soldr#3401), so the dispatcher can tell a deterministic
/// policy refusal from a transient failure and report it once per build.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{text}")]
pub(crate) struct RouteRefusal {
    /// The explanation to show the user.
    pub(crate) text: String,
    /// No retry can fix it: stop the build's remaining dials.
    pub(crate) fatal: bool,
    /// This build already reported the full block; `text` is the one-line
    /// pointer back to it.
    pub(crate) repeat: bool,
}

impl RouteRefusal {
    /// The refusal inside `error`, if it carries one.
    pub(crate) fn from_io(error: &std::io::Error) -> Option<&RouteRefusal> {
        error.get_ref()?.downcast_ref::<RouteRefusal>()
    }
}

/// What the client knows about itself and the route it dialed.
pub(crate) struct RefusalContext<'a> {
    pub wrapper: &'a str,
    pub wrapper_version: &'a str,
    pub wanted_version: &'a str,
    pub service: Option<&'a str>,
}

/// This process's wrapper path and the route service it dialed.
pub(crate) fn wrapper_and_service() -> (String, Option<String>) {
    let wrapper = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|_| String::from("<unknown>"));
    let service =
        std::env::var(crate::daemon::backend_handle_adoption::SOLDR_BROKER_SERVICE_ENV_VAR)
            .ok()
            .filter(|value| !value.is_empty());
    (wrapper, service)
}

/// The text of a refused Hello. Fatal refusals get one explanatory block and no
/// retry hint; transient ones (rate limiting, shutdown) keep the raw fields,
/// including `retry_after_ms`, because retrying is the right response.
pub(crate) fn describe_refusal(
    code: i32,
    reason: &str,
    retry_after_ms: u64,
    details: &dyn std::fmt::Debug,
    context: &RefusalContext<'_>,
) -> String {
    let error_code = ErrorCode::try_from(code).unwrap_or(ErrorCode::Unspecified);
    let kind = RefusalKind::from_code(error_code);
    if !refusal_is_fatal(kind) {
        return format!(
            "broker refused the daemon route: {reason} (code={code}, retry_after_ms={retry_after_ms}, details={details:?})"
        );
    }
    let service = context.service.unwrap_or("<SOLDR_BROKER_SERVICE not set>");
    let cause = match kind {
        RefusalKind::VersionBlocked | RefusalKind::VersionUnsupported => {
            "two soldr versions in one build -- RUSTC_WRAPPER resolved to a different\n\
             \x20             soldr than the front door that started this build."
        }
        RefusalKind::ServiceUnknown => {
            "the broker has no route by this name -- the front door that registered it\n\
             \x20             is gone or is a different broker."
        }
        _ => "the broker rejected this wrapper as a peer.",
    };
    format!(
        "broker refused the daemon route ({error_code:?}, not retryable): {reason}\n  \
         wrapper:  {} (v{}, wants route version {})\n  \
         route:    {service} (from SOLDR_BROKER_SERVICE)\n  \
         cause:    {cause}\n  \
         fix:      make RUSTC_WRAPPER an absolute path to the front-door soldr, fix PATH order,\n  \
         \x20         or align the PEP 517 backend requirement with the installed soldr.",
        context.wrapper, context.wrapper_version, context.wanted_version
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> RefusalContext<'static> {
        RefusalContext {
            wrapper: "/env/bin/soldr",
            wrapper_version: "0.9.6",
            wanted_version: "0.9.6",
            service: Some("soldr-daemon-abc"),
        }
    }

    #[test]
    fn version_policy_refusals_are_fatal_and_rate_limits_are_not() {
        for code in [
            ErrorCode::ErrorVersionBlocked,
            ErrorCode::ErrorVersionUnsupported,
            ErrorCode::ErrorServiceUnknown,
            ErrorCode::ErrorPeerRejected,
        ] {
            assert!(refusal_is_fatal(RefusalKind::from_code(code)), "{code:?}");
        }
        for code in [ErrorCode::ErrorRateLimited, ErrorCode::ErrorShuttingDown] {
            assert!(!refusal_is_fatal(RefusalKind::from_code(code)), "{code:?}");
        }
    }

    #[test]
    fn version_blocked_names_wrapper_route_and_drops_the_retry_hint() {
        let text = describe_refusal(
            ErrorCode::ErrorVersionBlocked as i32,
            "wanted_version is below min_version",
            30000,
            &"{}",
            &context(),
        );
        assert!(text.contains("/env/bin/soldr"), "{text}");
        assert!(text.contains("v0.9.6, wants route version 0.9.6"), "{text}");
        assert!(text.contains("soldr-daemon-abc"), "{text}");
        assert!(text.contains("not retryable"), "{text}");
        assert!(!text.contains("retry_after_ms"), "{text}");
    }

    #[test]
    fn rate_limited_keeps_the_retry_hint() {
        let text = describe_refusal(
            ErrorCode::ErrorRateLimited as i32,
            "slow down",
            250,
            &"{}",
            &context(),
        );
        assert!(text.contains("retry_after_ms=250"), "{text}");
        assert!(!text.contains("not retryable"), "{text}");
    }

    #[test]
    fn a_refusal_survives_the_io_error_chain_as_a_value() {
        let refusal = RouteRefusal {
            text: "block".into(),
            fatal: code_is_fatal(ErrorCode::ErrorVersionBlocked as i32),
            repeat: false,
        };
        let error = std::io::Error::other(refusal.clone());
        assert_eq!(RouteRefusal::from_io(&error), Some(&refusal));
        assert!(RouteRefusal::from_io(&std::io::Error::other("plain")).is_none());
        assert!(!code_is_fatal(ErrorCode::ErrorRateLimited as i32));
        assert!(code_is_fatal(ErrorCode::ErrorVersionBlocked as i32));
    }
}
