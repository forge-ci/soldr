#!/usr/bin/env bash
# Install the native build tools soldr's Linux lanes assume (soldr#3557).
#
# GitHub's ubuntu-24.04 image ships clang and lld, so this is a no-op there.
# Other runner images -- act2's catthehacker image when `bosn ci run` replays
# the PR workflow locally -- do not, and every linker-driver test then fails
# with "the linker driver needs `clang`". Declaring the dependency here keeps
# the workflow honest about what it needs instead of relying on the image.
set -euo pipefail

# ubuntu-24.04 ships lld only as the versioned /usr/lib/llvm-<N>/bin/ld.lld;
# the unversioned `ld.lld` is an 11 kB symlink package, and fetching it cost
# ~40 s of `apt-get update` per Linux job. Link the installed one instead.
if ! command -v ld.lld >/dev/null && [ -n "${GITHUB_PATH:-}" ]; then
  shopt -s nullglob
  candidates=(/usr/lib/llvm-*/bin/ld.lld)
  shopt -u nullglob
  installed=""
  if [ "${#candidates[@]}" -gt 0 ]; then
    installed=$(printf '%s\n' "${candidates[@]}" | sort -V | tail -n1)
  fi
  if [ -n "$installed" ]; then
    link_dir="${RUNNER_TEMP:-/tmp}/native-build-tools"
    mkdir -p "$link_dir"
    ln -sf "$installed" "$link_dir/ld.lld"
    echo "$link_dir" >>"$GITHUB_PATH"
    export PATH="$link_dir:$PATH"
    echo "linked installed $installed as ld.lld"
  fi
fi

missing=()
for tool in clang ld.lld; do
  command -v "$tool" >/dev/null || missing+=("$tool")
done
if [ "${#missing[@]}" -eq 0 ]; then
  echo "native build tools present: $(clang --version | head -n1)"
  exit 0
fi
echo "installing missing native build tools: ${missing[*]}"
sudo_cmd=()
[ "$(id -u)" -eq 0 ] || sudo_cmd=(sudo -n)
"${sudo_cmd[@]}" apt-get update -q
"${sudo_cmd[@]}" apt-get install -y -q --no-install-recommends clang lld
clang --version | head -n1
