#!/usr/bin/env bash
# Print a structured banner before every cargo build invocation so the
# workflow log shows what's being built, in what profile, against which
# target, and via which Soldr build tool.
#
# Usage:
#   .github/scripts/print_build_banner.sh \
#       <package-or-workdir>   # e.g. "soldr-cli" or "crgx@v0.4.2"
#       <profile>              # "release" / "debug"
#       <target>               # e.g. "aarch64-pc-windows-msvc"
#       <tool>                 # "soldr cargo" / "soldr cargo zigbuild" / ...
#       [<manifest_path>]      # optional, e.g. "$work_dir/Cargo.toml"

set -euo pipefail

package="${1:?package required}"
profile="${2:?profile required}"
target="${3:?target required}"
tool="${4:?tool required}"
manifest="${5:-}"

# Tool probes are best-effort. A missing Soldr or failed compiler probe
# should leave a useful banner without invoking an unmanaged compiler.
rustc_version="$(soldr rustc --version || echo '(soldr rustc unavailable)')"
soldr_version="$(soldr --version 2>/dev/null || echo '(soldr not on PATH yet)')"

echo "==================================================================="
echo "build      : ${package}"
echo "profile    : ${profile}"
echo "target     : ${target}"
echo "tool       : ${tool}"
[ -n "${manifest}" ] && echo "manifest   : ${manifest}"
echo "host       : $(uname -srm)"
echo "rustc      : ${rustc_version}"
echo "soldr      : ${soldr_version}"
echo "started    : $(date -u +"%Y-%m-%dT%H:%M:%SZ")"
echo "==================================================================="
