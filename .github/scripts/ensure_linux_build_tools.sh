#!/usr/bin/env bash
# Install the native build tools soldr's Linux lanes assume (soldr#3557).
#
# GitHub's ubuntu-24.04 image ships clang and lld, so this is a no-op there.
# Other runner images -- act2's catthehacker image when `bosn ci run` replays
# the PR workflow locally -- do not, and every linker-driver test then fails
# with "the linker driver needs `clang`". Declaring the dependency here keeps
# the workflow honest about what it needs instead of relying on the image.
set -euo pipefail

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
