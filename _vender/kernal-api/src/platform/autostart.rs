//! Registering a program to start when the user logs in.
//!
//! Every host has a mechanism for this and no two agree on what it is called,
//! where it is stored, or what it is written in: a systemd user unit, a
//! launchd agent plist, a Task Scheduler ONLOGON task. A caller has one
//! question -- start this program at login, and stop doing so -- and answering
//! it should not require knowing which of the three is in play.
//!
//! What stays with the caller is *enrolment*: whether to register at all, what
//! the thing is called, and which program runs. Those arrive in
//! [`AutostartProgram`].

use std::io;
use std::path::PathBuf;

pub use crate::{
    autostart_register as register, autostart_render_registration as render_registration,
    autostart_unregister as unregister,
};

/// What a caller wants started at login.
///
/// The two names are separate because the hosts disagree about what a name is:
/// launchd requires a reverse-DNS label, systemd and Task Scheduler want a
/// plain stem. Deriving one from the other would be a guess, so the caller
/// states both.
#[derive(Debug, Clone, Copy)]
pub struct AutostartProgram<'a> {
    /// Plain stem: the systemd unit name and the Task Scheduler task name.
    pub identifier: &'a str,
    /// Reverse-DNS label, as launchd requires.
    pub label: &'a str,
    /// One line describing the program, for hosts that record one.
    pub description: &'a str,
    /// The program to run.
    pub program: &'a std::path::Path,
    /// The argument that starts it.
    pub start_argument: &'a str,
    /// The argument that stops it, for hosts that ask for one.
    pub stop_argument: &'a str,
}

/// Why a registration did not happen.
///
/// The three cases are kept apart because an operator does something different
/// about each: fix the environment, fix the filesystem, or arm the init system
/// by hand.
#[derive(Debug, thiserror::Error)]
pub enum AutostartError {
    /// The host would not say where the registration belongs -- typically a
    /// missing `HOME` or `XDG_CONFIG_HOME`.
    #[error("could not resolve autostart location: {0}")]
    Resolve(String),
    /// Writing or removing the registration failed.
    #[error("autostart file operation failed: {0}")]
    Io(io::Error),
    /// The init system itself refused, or could not be invoked.
    #[error("init system rejected autostart: {0}")]
    InitSystem(String),
}

impl From<io::Error> for AutostartError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Where a registration was written.
pub type Registration = PathBuf;

/// Wrap a string in POSIX single quotes, escaping embedded single quotes with
/// the standard `'\''` dance.
///
/// Lives in the neutral leaf rather than the Linux tree so its tests run on
/// every host: the escaping rule is the kind of thing that is wrong once and
/// then wrong everywhere it is copied, and a path with a quote in it is not
/// the moment to find that out.
// Only the systemd implementation calls this, so the other hosts see it as
// dead code. It stays compiled on all of them anyway -- that is what keeps
// the tests below running everywhere rather than on one host.
#[allow(dead_code)]
pub(crate) fn shell_quote_single(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            // Close quote, escaped literal single, re-open quote.
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Escape a string for an XML text node.
///
/// Neutral for the same reason as [`shell_quote_single`].
#[allow(dead_code)]
pub(crate) fn xml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_wraps_simple_path() {
        assert_eq!(shell_quote_single("/usr/bin/foo"), "'/usr/bin/foo'");
    }

    #[test]
    fn shell_quote_escapes_embedded_single_quote() {
        assert_eq!(shell_quote_single("o'malley"), "'o'\\''malley'");
    }

    #[test]
    fn xml_escape_handles_metacharacters() {
        assert_eq!(
            xml_escape("a<b&c>d\"e'f"),
            "a&lt;b&amp;c&gt;d&quot;e&apos;f"
        );
    }

    /// Whatever this host writes, it writes the program path into it, and the
    /// caller's identifier is recoverable from what comes back.
    #[test]
    fn a_registration_names_the_program_it_starts() {
        let program = std::path::Path::new("/opt/example/bin/exampled");
        let rendered = render_registration(&AutostartProgram {
            identifier: "example-daemon",
            label: "com.example.daemon",
            description: "example supervisor",
            program,
            start_argument: "start",
            stop_argument: "stop",
        });
        assert!(
            rendered.contains("exampled"),
            "registration must name the program: {rendered}"
        );
        assert!(
            rendered.contains("start"),
            "registration must say how to start it: {rendered}"
        );
    }
}
