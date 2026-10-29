//! `soldr wheel [--release] [--target <triple>]` — the blessed Python wheel
//! surface (soldr#2139 gap 1).
//!
//! # Why a verb rather than a flag
//!
//! `soldr build --wheel` would overload the blessed-toolchain verb with a
//! different *output artifact*, and `soldr maturin build` looks like a
//! passthrough while silently doing sysroot preparation. `wheel` says what it
//! produces and is honest about doing work first.
//!
//! # What it is
//!
//! A thin front end over the existing `soldr maturin ...` execution path. It
//! resolves the friendly target alias, picks maturin's `--compatibility`
//! value from the target family, and hands a `maturin build ...` argument
//! vector back to the ordinary dispatcher — which already owns maturin
//! provisioning, toolchain env pinning, the build lease, target preparation
//! (`target_lifecycle::prepare_for_invocation`), and the PyO3 plan. Nothing
//! about the wheel *naming* contract is touched; that is downstream-visible
//! and is maturin's to decide.
//!
//! # Scope: abi3 only
//!
//! A non-abi3 extension module has to link against a CPython built for the
//! *target*, which is a materially harder problem than mounting a sysroot.
//! abi3 needs no target-side interpreter, and it covers soldr's own wheel.
//! Anything the PyO3 planner cannot place in an interpreter-free mode is
//! refused with a message naming `soldr maturin build` as the escape hatch,
//! rather than silently degrading into a wheel built against the host's
//! Python.
//!
//! # Grammar
//!
//! ```text
//! soldr wheel                              # quick dev wheel, host target
//! soldr wheel --release                    # release wheel, host target
//! soldr wheel --release --target XXX       # release wheel, cross target
//! soldr wheel --release --host-glibc       # release wheel, host glibc floor
//! ```
//!
//! `--release` is opt-in, matching `cargo` and `soldr build`: the default is a
//! fast dev-profile wheel. `--target` defaults to the host triple.
//!
//! # Note on glibc floors — only claim a floor soldr enforced
//!
//! `--compatibility manylinux_2_17` is a *tag*, and the suffixed-triple floor
//! (`...-linux-gnu.2.17`, soldr#2202) means "ask zig for this floor", never
//! "guarantee this floor" — the effective floor is the max of what zig was
//! asked for and every symbol the vendored C dependencies reference. The
//! suffixed spelling is therefore rejected here rather than being quietly
//! accepted into a wheel tag that would read as a promise.
//!
//! The same honesty rule governs the tag soldr emits at all: soldr claims
//! `manylinux_2_17` only for a build in which
//! `target_lifecycle::prepare_for_invocation` ran and mounted the catalogue
//! glibc-2.17 sysroot that *creates* the floor. `verify_wheel_glibc.py` exists
//! precisely because pip *trusts* that claim and installs the wheel anyway.
//!
//! The maturin execution path prepares a target on its own only when the
//! target differs from the host. soldr#3432 closed the gap that left: a
//! `--release` `*-linux-gnu` wheel **always** gets target preparation, host
//! target included, via `maturin_target_needs_prep`. A release wheel is the
//! thing that goes to PyPI, and it must never silently inherit the build
//! machine's glibc floor (2.39 on ubuntu-24.04). So "release + linux-gnu"
//! means an enforced 2.17 floor, and soldr says so on stderr with one green
//! `info` line ([`GlibcNotice`]) so the floor is never a silent choice.
//!
//! The opt-out is explicit: `--host-glibc` skips the forced preparation for a
//! host-target build, links against this host's glibc, and passes
//! `--compatibility pypi` — maturin's "work the tag out from the bytes"
//! pseudo-option — because that is the only claim soldr can back. A dev wheel
//! gets the same `pypi` treatment: it is a local artifact, and a dev wheel
//! tagged `manylinux_2_17` without the sysroot would be a lie pip acts on.
//!
//! A host that cannot run the catalogue GNU bundle (every bundle is
//! x86_64-hosted, soldr#2874 — so a native aarch64 Linux host) cannot enforce
//! the floor for a host-target release wheel. soldr refuses rather than
//! falling back: the same rule `target_lifecycle::decide_gnu_bundle` applies to
//! an explicit `.2.17` floor request, where a silent success is worse than a
//! failure because the artifact would read as a promise. The refusal names both
//! remedies (cross-build from x86_64, or `--host-glibc`).
//!
//! musl is deliberately unchanged: a host-target musl wheel still gets `pypi`.
//! The catalogue musl bundle is hosted on `x86_64-unknown-linux-gnu`, which a
//! musl host (the only place a host-target musl build happens) is not
//! guaranteed to be able to execute, so the glibc fix does not carry over
//! (soldr#3435).
//!
//! Note that maturin does **not** paper over a bad claim: with an explicit
//! `--compatibility manylinux_2_17` and an ELF needing `GLIBC_2.39`,
//! `auditwheel_rs` (maturin `src/auditwheel/linux.rs`) returns
//! `VersionedSymbolTooNewError` from the explicit-tag branch and the build
//! fails with "Error ensuring manylinux_2_17 compliance" — it downgrades only
//! when *no* tag was requested. `AuditWheelMode::Repair` is maturin's
//! `#[default]`, so `auto-release.yml`'s explicit `--auditwheel repair`
//! restates the default and its absence here changes nothing.

use crate::core::SoldrError;
use crate::fetch::gnu_linux_toolchain::GNU_LINUX_GLIBC_BASELINE;
use crate::pyo3_detect::PlanMode;

/// Arguments for `soldr wheel`.
///
/// `--target`, `--release` and `--host-glibc` must precede any passthrough
/// arguments, because everything after the first free argument is forwarded
/// to maturin verbatim.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct WheelArgs {
    /// Target triple or friendly alias (for example `linux-arm64`).
    /// Defaults to the host triple.
    #[arg(long, value_name = "TRIPLE")]
    pub target: Option<String>,
    /// Build with the release profile. Default is a quick dev-profile wheel,
    /// matching `cargo` and `soldr build`. A release `*-linux-gnu` wheel is
    /// always built against the catalogue glibc 2.17 sysroot and tagged
    /// `manylinux_2_17`, including when the target is the host.
    #[arg(long)]
    pub release: bool,
    /// Link a host-target `*-linux-gnu` wheel against this host's glibc
    /// instead of the catalogue glibc 2.17 sysroot. The wheel then requires
    /// this host's glibc version or newer, and maturin tags it from its bytes
    /// (for example `manylinux_2_39`) rather than `manylinux_2_17`. Refused
    /// with a cross target or a non-glibc target.
    #[arg(long)]
    pub host_glibc: bool,
    /// Extra arguments forwarded verbatim to `maturin build`
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub rest: Vec<String>,
}

/// The host facts the wheel plan depends on, injected so the policy can be
/// tested from any machine rather than only from the host it describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WheelHost {
    /// The host triple (`pyo3_detect::host_triple`).
    pub triple: String,
    /// Whether the catalogue GNU/Linux bundle's compilers execute here
    /// (`catalogue_linux_host::bundle_host_fitness` with `GNU_BUNDLE_HOST`).
    /// The bundle's pinned sysroot is what enforces the 2.17 floor.
    pub gnu_bundle_fitness: crate::fetch::catalogue_linux_host::BundleHostFitness,
    /// The running glibc's version, when it can be read cheaply. Only used in
    /// the `--host-glibc` notice.
    pub glibc_version: Option<String>,
}

impl WheelHost {
    /// The facts of the machine soldr is running on.
    pub fn current() -> Self {
        use crate::platform::host::facts;
        Self {
            triple: crate::pyo3_detect::host_triple().to_string(),
            gnu_bundle_fitness: crate::fetch::catalogue_linux_host::current_host_fitness(
                crate::fetch::catalogue_linux_host::GNU_BUNDLE_HOST,
            ),
            glibc_version: facts::glibc_version(),
        }
    }
}

/// Everything `soldr wheel` decided before re-entering the maturin path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WheelPlan {
    /// `maturin build ...`, handed to the ordinary dispatcher.
    pub argv: Vec<String>,
    /// The resolved Rust target triple.
    pub triple: String,
    /// soldr#3432: run target preparation even though the target is the
    /// host, because this is a release `*-linux-gnu` wheel.
    pub prepare_host_target: bool,
    /// The one `info` line printed before the build, if any.
    pub notice: Option<GlibcNotice>,
    /// How `[tool.soldr.pep517] bundle-bins` entries are built so they match
    /// the extension maturin built (zackees/soldr#3468).
    pub bundle: crate::wheel_bundle::BundleBuild,
}

/// The glibc-floor `info` line `soldr wheel` prints before building, so the
/// floor a Linux wheel gets is never a silent choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlibcNotice {
    /// Built against the catalogue glibc 2.17 sysroot.
    Catalogue {
        /// `None` for a host-target build, the triple for a cross build
        /// (where `--host-glibc` has no meaning, so it is not offered).
        cross_target: Option<String>,
        /// The tag soldr asked maturin for; `None` when the caller supplied
        /// their own `--compatibility` / `--manylinux`.
        tag: Option<&'static str>,
    },
    /// `--host-glibc`: built against this host's glibc.
    HostGlibc {
        /// This host's glibc version, when known.
        version: Option<String>,
    },
}

impl GlibcNotice {
    /// The plain-text line.
    pub fn message(&self) -> String {
        match self {
            Self::Catalogue { cross_target, tag } => {
                let tag = tag.map(|tag| format!(" ({tag})")).unwrap_or_default();
                match cross_target {
                    None => format!(
                        "soldr: info: building release wheel against glibc \
                         {GNU_LINUX_GLIBC_BASELINE}{tag} for maximum Linux compatibility; \
                         pass --host-glibc to link against this host's glibc instead"
                    ),
                    Some(triple) => format!(
                        "soldr: info: building release wheel for {triple} against glibc \
                         {GNU_LINUX_GLIBC_BASELINE}{tag} for maximum Linux compatibility \
                         (catalogue cross toolchain; --host-glibc applies only to a \
                         host-target build)"
                    ),
                }
            }
            Self::HostGlibc { version } => {
                let floor = match version {
                    Some(version) => format!("glibc {version} or newer"),
                    None => "this host's glibc version or newer (version not detected)".to_string(),
                };
                format!(
                    "soldr: info: --host-glibc: building wheel against this host's glibc, \
                     not glibc {GNU_LINUX_GLIBC_BASELINE}; it will require {floor}, and \
                     maturin tags it from its bytes instead of manylinux_2_17"
                )
            }
        }
    }

    /// The line as printed: green when `use_color`, plain otherwise.
    pub fn render(&self, use_color: bool) -> String {
        crate::color_choice::paint(&self.message(), crate::color_choice::GREEN, use_color)
    }
}

/// maturin's `--compatibility` value for a resolved Rust target triple.
///
/// When the floor claim is backed this mirrors the release lane
/// (`auto-release.yml`): linux-gnu wheels are tagged `manylinux_2_17`,
/// linux-musl wheels `musllinux_1_2`. Otherwise — and for every non-Linux
/// target, which has no such floor to claim — soldr passes `pypi`, maturin's
/// pseudo-option meaning "derive the platform tag from the bytes and validate
/// the resulting filename for PyPI". That is a description, not a promise.
pub fn compatibility_for_target(triple: &str, floor_backed: bool) -> &'static str {
    if !floor_backed {
        "pypi"
    } else if triple.contains("-linux-musl") {
        "musllinux_1_2"
    } else if is_linux_gnu(triple) {
        "manylinux_2_17"
    } else {
        "pypi"
    }
}

fn is_linux_gnu(triple: &str) -> bool {
    triple.contains("-linux-gnu")
}

fn has_flag(args: &[String], flag: &str) -> bool {
    let prefix = format!("{flag}=");
    args.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == flag || arg.starts_with(&prefix))
}

/// soldr#3636: every forwarded spelling of maturin's release profile -- `--release`,
/// `-r`, `--profile release`, `--profile=release` -- before any `--`.
fn release_requested_in(args: &[String]) -> bool {
    let mut iter = args.iter().take_while(|arg| arg.as_str() != "--");
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--release" | "-r" | "--profile=release" => return true,
            "--profile" if iter.next().is_some_and(|value| value == "release") => return true,
            _ => {}
        }
    }
    false
}

/// Pure planner: `(args, host) -> WheelPlan`.
///
/// No I/O and no env reads — the host is injected — so this is the piece worth
/// unit-testing, and it is the only place the wheel surface decides anything.
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
pub fn plan_for_host(args: &WheelArgs, host: &WheelHost) -> Result<WheelPlan, SoldrError> {
    let rest = args.rest.as_slice();
    let requested = args
        .target
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(&host.triple);

    if has_flag(rest, "--target") {
        return Err(SoldrError::Other(
            "soldr wheel: pass the target once, as `soldr wheel --target <triple> [...]`; \
             a second --target in the forwarded arguments would silently disagree with the \
             sysroot soldr prepared."
                .to_string(),
        ));
    }

    if let Some((base, floor)) = crate::target_alias::split_glibc_floor(requested) {
        return Err(SoldrError::Other(format!(
            "soldr wheel: glibc-floor targets (`{base}.{floor}`) are not supported by the \
             wheel surface. A floor is a request to zig, not a guarantee — the effective \
             floor is also bounded by every symbol the vendored C dependencies reference — \
             so folding it into a manylinux tag would publish a promise soldr cannot keep. \
             Use `soldr wheel --release --target {base}` (tagged \
             `{}`), or `soldr build --target {base}.{floor}` for a bare binary.",
            compatibility_for_target(base, true)
        )));
    }

    let resolved = crate::target_alias::resolve_soldr_target(requested)
        .map_err(|err| err.into_soldr_error(crate::target_alias::TargetSurface::Wheel))?;
    let triple = resolved.rust_triple;
    let host_target = triple == host.triple;
    let gnu = is_linux_gnu(&triple);

    if args.host_glibc && !host_target {
        return Err(SoldrError::Other(format!(
            "soldr wheel: --host-glibc links the wheel against this host's glibc, which \
             only means something for a host-target build. `{triple}` is a cross target \
             (this host is `{}`), so the wheel is built against the catalogue glibc \
             {GNU_LINUX_GLIBC_BASELINE} sysroot for that target. Drop --host-glibc.",
            host.triple
        )));
    }
    if args.host_glibc && !gnu {
        return Err(SoldrError::Other(format!(
            "soldr wheel: --host-glibc only applies to a `*-linux-gnu` wheel; `{triple}` \
             does not link against glibc. Drop --host-glibc."
        )));
    }

    // `--debug` is maturin's spelling for "not --release". A caller who wrote
    // both is asking for two different profiles; say so rather than picking
    // one and building something they did not ask for.
    let release_in_rest = release_requested_in(rest) || has_flag(rest, "--release");
    let debug_in_rest = has_flag(rest, "--debug");
    if args.release && debug_in_rest {
        return Err(SoldrError::Other(
            "soldr wheel: `--release` and a forwarded `--debug` ask for different profiles. \
             Drop one — `soldr wheel` alone already builds the dev profile."
                .to_string(),
        ));
    }
    if release_in_rest && debug_in_rest {
        return Err(SoldrError::Other(
            "soldr wheel: a forwarded release profile (`-r`/`--release`/`--profile release`) \
             and `--debug` ask for different profiles. Drop one."
                .to_string(),
        ));
    }
    let is_release = args.release || release_in_rest;

    // soldr#3432: a release linux-gnu wheel always gets the catalogue 2.17
    // sysroot, host target included, unless the caller opted out.
    let prepare_host_target = is_release && host_target && gnu && !args.host_glibc;
    if prepare_host_target && !host.gnu_bundle_fitness.is_runnable() {
        let bundle_host = crate::fetch::catalogue_linux_host::GNU_BUNDLE_HOST.host;
        let reason = match host.gnu_bundle_fitness {
            crate::fetch::catalogue_linux_host::BundleHostFitness::WrongLibc => {
                crate::fetch::catalogue_linux_host::wrong_libc_reason(&host.triple)
            }
            _ => format!(
                "every catalogue GNU/Linux bundle is hosted on `{bundle_host}` (soldr#2874)"
            ),
        };
        return Err(SoldrError::Other(format!(
            "soldr wheel: a release wheel for `{triple}` is built against the catalogue \
             glibc {GNU_LINUX_GLIBC_BASELINE} sysroot so that its manylinux_2_17 tag is \
             enforced, but that toolchain cannot run on this host: {reason}. soldr will \
             not silently fall back to this host's glibc for a release wheel (soldr#3432). \
             Build on an `{bundle_host}` host instead — `soldr wheel --release --target \
             {triple}` cross-builds it at glibc {GNU_LINUX_GLIBC_BASELINE} — or pass \
             --host-glibc to link against this host's glibc and have the wheel tagged \
             from its bytes.",
        )));
    }
    // Cross builds are prepared by the maturin path's own `target != host`
    // gate; host-target ones only when this plan asks for it.
    let floor_backed = is_release && !args.host_glibc && (!host_target || prepare_host_target);

    let mut argv = vec!["maturin".to_string(), "build".to_string()];
    if is_release && !release_in_rest {
        argv.push("--release".to_string());
    }
    let caller_tagged = has_flag(rest, "--compatibility") || has_flag(rest, "--manylinux");
    let compatibility = compatibility_for_target(&triple, floor_backed);
    if !caller_tagged {
        argv.push("--compatibility".to_string());
        argv.push(compatibility.to_string());
    }
    argv.push("--target".to_string());
    argv.push(triple.clone());
    argv.extend(rest.iter().cloned());

    let notice = if args.host_glibc {
        Some(GlibcNotice::HostGlibc {
            version: host.glibc_version.clone(),
        })
    } else if floor_backed && gnu {
        Some(GlibcNotice::Catalogue {
            cross_target: (!host_target).then(|| triple.clone()),
            tag: (!caller_tagged).then_some(compatibility),
        })
    } else {
        None
    };

    // zackees/soldr#3468: bundled bins get the same toolchain as the
    // extension. When the maturin path prepared the target (a cross build, or
    // a host-target release gnu wheel at glibc 2.17), `soldr build --target`
    // prepares the same catalogue target; otherwise the extension was a
    // native host build, so the bins are too.
    let bundle = crate::wheel_bundle::BundleBuild {
        target: (!host_target || prepare_host_target).then(|| triple.clone()),
        profile_args: crate::wheel_bundle::profile_args(is_release, rest),
    };

    Ok(WheelPlan {
        argv,
        triple,
        prepare_host_target,
        notice,
        bundle,
    })
}

/// The target a `soldr wheel` plan asked the maturin path to prepare although
/// it is the host (soldr#3432). `soldr wheel` re-enters the dispatcher
/// in-process, so a process-local slot carries the request without an
/// environment variable that would leak into maturin, cargo and build scripts.
static HOST_TARGET_PREP: crate::platform::sync::Mutex<Option<String>> =
    crate::platform::sync::Mutex::new(None);

fn request_host_target_prep(triple: &str) {
    *HOST_TARGET_PREP.lock() = Some(triple.to_string());
}

/// Whether the maturin execution path must run
/// `target_lifecycle::prepare_for_invocation` for `target`: always for a
/// cross target, and for the host target when `soldr wheel` planned a release
/// `*-linux-gnu` wheel (soldr#3432).
pub(crate) fn maturin_target_needs_prep(target: &str, host: &str) -> bool {
    target != host || HOST_TARGET_PREP.lock().as_deref() == Some(target)
}

/// The abi3-only scope gate, expressed over the PyO3 planner's decision.
///
/// Every allowed mode is one where the build needs no CPython built for the
/// target: a host-native build, a workspace with no PyO3 at all, abi3
/// (`PYO3_NO_PYTHON`), modern Windows `raw-dylib`, an explicitly opted-in
/// Python sysroot, or a caller who configured `PYO3_*` themselves.
pub fn abi3_scope_check(
    mode: PlanMode,
    target: &str,
    diagnostic: Option<&str>,
) -> Result<(), SoldrError> {
    match mode {
        PlanMode::Native
        | PlanMode::NoPyo3
        | PlanMode::Abi3NoPython
        | PlanMode::ModernWindowsRawDylib
        | PlanMode::CompatibilitySysroot
        | PlanMode::CallerConfigured => Ok(()),
        PlanMode::ExtensionDefault | PlanMode::RequiresExplicitCompatibility => {
            Err(SoldrError::Other(format!(
                "soldr wheel: cross-building a wheel for `{target}` needs a CPython built \
                 for that target, because this workspace's PyO3 extension is not proven \
                 abi3. The first cut of `soldr wheel` is abi3-only (soldr#2139): enable the \
                 `abi3-py310` feature on pyo3 (the fleet-wide policy, docs/API.md \"PyO3 ABI \
                 policy\"), or set SOLDR_PYO3_COMPATIBILITY=sysroot, or \
                 drive maturin yourself with `soldr maturin build`."
            )))
        }
        PlanMode::Unresolved => {
            // Surface the probe's real failure (soldr#2576): the summary
            // alone sent users chasing `cargo metadata` by hand, which
            // succeeds through the front door and proves nothing about
            // this probe's environment.
            let detail = diagnostic
                .map(|text| format!("\n  probe error: {text}"))
                .unwrap_or_default();
            Err(SoldrError::Other(format!(
                "soldr wheel: could not read Cargo metadata, so soldr cannot prove this \
                 workspace is abi3-safe for `{target}`. The first cut of `soldr wheel` is \
                 abi3-only (soldr#2139); run `soldr maturin build --target {target}` to build \
                 anyway.{detail}"
            )))
        }
    }
}

/// Build the full soldr argv that `soldr wheel` re-enters as.
///
/// The returned vector is a complete `soldr` command line minus argv[0]:
/// leading global flags, then `maturin build ...`. Re-entering the dispatcher
/// is what keeps the maturin provisioning ladder, toolchain pinning, build
/// lease, target preparation, and PyO3 planning in exactly one place.
///
/// Side effects, both deliberate and both after every refusal has had its
/// chance: a host-target release linux-gnu plan registers its target with
/// [`maturin_target_needs_prep`], and the plan's [`GlibcNotice`] is printed to
/// stderr (green on a terminal, plain when `NO_COLOR` is set or stderr is
/// redirected).
pub(crate) fn maturin_invocation(
    args: &WheelArgs,
    no_cache: bool,
    trust_inherited_soldr_env: bool,
) -> Result<Vec<String>, SoldrError> {
    let plan = plan_for_host(args, &WheelHost::current())?;

    // The gate only has something to say about cross builds; a host build
    // resolves to `PlanMode::Native` without touching Cargo metadata anyway,
    // so skip the `cargo metadata` round trip entirely.
    if plan.triple != crate::pyo3_detect::host_triple() {
        let workspace_root =
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let pyo3_plan = crate::pyo3_detect::resolve_for_invocation(
            &workspace_root,
            &plan.argv,
            Some(&plan.triple),
        );
        abi3_scope_check(
            pyo3_plan.mode,
            &plan.triple,
            pyo3_plan.diagnostic.as_deref(),
        )?;
    }

    if plan.prepare_host_target {
        request_host_target_prep(&plan.triple);
    }
    // zackees/soldr#3468: validate `bundle-bins` before the (long) maturin
    // build, and register the request the maturin path runs after it.
    let workspace_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    crate::wheel_bundle::request_for_workspace(&workspace_root, &plan.bundle)?;
    if let Some(notice) = &plan.notice {
        eprintln!("{}", notice.render(crate::color_choice::stderr_enabled()));
    }

    let mut argv = Vec::with_capacity(plan.argv.len() + 2);
    if no_cache {
        argv.push("--no-cache".to_string());
    }
    if trust_inherited_soldr_env {
        argv.push("--trust-inherited-soldr-env".to_string());
    }
    argv.extend(plan.argv);
    Ok(argv)
}

#[cfg(test)]
#[path = "wheel_cmd_tests.rs"]
mod tests;
