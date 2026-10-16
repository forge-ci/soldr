#!/usr/bin/env bash
# Runs the README-facing bare cargo vs sccache vs soldr comparison
# benchmarks and emits benchmark-output/comparison.json.

set -euo pipefail

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${HERE}/.." && pwd)"
# shellcheck source=../perf/lib/common.sh
. "${REPO_ROOT}/perf/lib/common.sh"

OUT_DIR="${REPO_ROOT}/benchmark-output"
WORK_DIR="$(mktemp -d)"
WORKTREES_TO_REMOVE=()
CREATED_PROJECT=""
COMPARISON_BUILD_TIMEOUT_SECONDS="${COMPARISON_BUILD_TIMEOUT_SECONDS:-60}"
trap 'for wt in "${WORKTREES_TO_REMOVE[@]:-}"; do git -C "${REPO_ROOT}" worktree remove --force "$wt" >&2 || true; done; measure::teardown_work_dir "${WORK_DIR}"' EXIT

mkdir -p "${OUT_DIR}"

# Keep setup-soldr's toolchain/PATH benefits, but remove wrapper and cache
# state so each comparison cell owns its cache and target dirs.
measure::clear_inherited_cache_env

now_ms() { date +%s%3N; }

elapsed_ms() {
    local start="$1"
    local end
    end="$(now_ms)"
    echo "$(( end - start ))"
}

dir_size_bytes() {
    local path="$1"
    if [[ ! -d "${path}" ]]; then
        echo 0
        return
    fi
    if du -sb "${path}" >/dev/null 2>&1; then
        du -sb "${path}" | awk '{print $1}'
    else
        find "${path}" -type f -printf '%s\n' 2>/dev/null | awk '{sum += $1} END {print sum + 0}'
    fi
}

json_escape() {
    jq -Rn --arg v "$1" '$v'
}

append_result() {
    local benchmark="$1"
    local fixture="$2"
    local scenario_key="$3"
    local scenario="$4"
    local mode="$5"
    local tool="$6"
    local tool_label="$7"
    local command_text="$8"
    local wall_ms="$9"
    local cache_bytes="${10}"
    local project_path="${11}"
    # soldr#3802: one-time tool provisioning, timed outside wall_ms. JSON null
    # for bare cargo / sccache and for every non-cold scenario.
    local provision_ms="${12:-null}"
    local provision_bytes="${13:-null}"

    cat >>"${RESULTS_JSONL}" <<EOF
{"benchmark":$(json_escape "$benchmark"),"fixture":$(json_escape "$fixture"),"scenario_key":$(json_escape "$scenario_key"),"scenario":$(json_escape "$scenario"),"mode":$(json_escape "$mode"),"tool":$(json_escape "$tool"),"tool_label":$(json_escape "$tool_label"),"command":$(json_escape "$command_text"),"wall_ms":${wall_ms},"cache_bytes":${cache_bytes},"provision_ms":${provision_ms},"provision_bytes":${provision_bytes},"cache_scope":$(json_escape "tool-isolated-after-measured-run"),"project_path":$(json_escape "$project_path")}
EOF
}

run_logged() {
    local label="$1"
    shift
    echo "+ $label" >&2
    "$@" >&2
}

measure_command() {
    local label="$1"
    shift
    local t0 rc elapsed
    echo "::group::comparison ${label}" >&2
    echo "+ $*" >&2
    t0="$(now_ms)"
    set +e
    "$@" >&2
    rc=$?
    set -e
    elapsed="$(elapsed_ms "${t0}")"
    if (( rc == 0 )); then
        echo "comparison ${label}: ok (${elapsed} ms)" >&2
        echo "::endgroup::" >&2
        echo "${elapsed}"
    else
        echo "comparison ${label}: FAILED (rc=${rc}, ${elapsed} ms) - recording 0" >&2
        echo "::endgroup::" >&2
        echo "0"
    fi
}

median_ms() {
    printf '%s\n' "$@" | sort -n | awk 'NF { values[NR] = $1 } END { if (NR == 0) print 0; else print values[int((NR + 1) / 2)] }'
}

# Directory that holds one canonical extraction of each tarball
# fixture. Populated lazily by make_fixture_project on first hit; every
# subsequent cell for the same fixture cp -a's from here. See #1158.
FIXTURE_SEED_ROOT="${WORK_DIR}/fixture-seeds"

make_fixture_project() {
    local fixture="$1"
    local dest="$2"
    mkdir -p "${dest}"
    if [[ "${fixture}" == "soldr" ]]; then
        git -C "${REPO_ROOT}" worktree add --detach "${dest}/soldr" HEAD >/dev/null
        WORKTREES_TO_REMOVE+=("${dest}/soldr")
        CREATED_PROJECT="${dest}/soldr"
    else
        # #1158: extract the tarball once per fixture into a seed dir,
        # then copy into each cell instead of re-decompressing every
        # time. 24 calls per Benchmark Stats run -> 22 cp -a's instead
        # of 22 tar extracts on the hot path.
        local seed="${FIXTURE_SEED_ROOT}/${fixture}"
        if [[ ! -d "${seed}/${fixture}" ]]; then
            mkdir -p "${seed}"
            bash "${REPO_ROOT}/perf/lib/extract.sh" "${fixture}" "${seed}" >/dev/null
        fi
        cp -a "${seed}/${fixture}" "${dest}/"
        CREATED_PROJECT="${dest}/${fixture}"
    fi
}

tool_label() {
    case "$1" in
        bare) echo "Bare cargo" ;;
        sccache) echo "sccache" ;;
        soldr) echo "soldr" ;;
        *) echo "$1" ;;
    esac
}

command_text() {
    case "$1" in
        bare) echo "cargo build --release" ;;
        sccache) echo "RUSTC_WRAPPER=sccache cargo build --release" ;;
        soldr) echo "soldr cargo build --release" ;;
        *) echo "$1" ;;
    esac
}

run_with_timeout() {
    local seconds="$1"
    shift
    if command -v timeout >/dev/null 2>&1; then
        timeout --kill-after=15s --preserve-status "${seconds}s" "$@"
    else
        "$@"
    fi
}

run_build() {
    local tool="$1"
    local project="$2"
    local cache_dir="$3"
    local target_dir="$4"
    case "${tool}" in
        bare)
            (cd "${project}" && CARGO_TARGET_DIR="${target_dir}" run_with_timeout "${COMPARISON_BUILD_TIMEOUT_SECONDS}" cargo build --release)
            ;;
        sccache)
            mkdir -p "${cache_dir}"
            (cd "${project}" && SCCACHE_DIR="${cache_dir}" RUSTC_WRAPPER=sccache CARGO_TARGET_DIR="${target_dir}" run_with_timeout "${COMPARISON_BUILD_TIMEOUT_SECONDS}" cargo build --release)
            ;;
        soldr)
            mkdir -p "${cache_dir}"
            # soldr#3607: the build auto-starts a daemon owning this root; the
            # EXIT trap must stop it before deleting WORK_DIR.
            measure::register_soldr_root "${WORK_DIR}" "${cache_dir}"
            (cd "${project}" && SOLDR_CACHE_DIR="${cache_dir}" CARGO_TARGET_DIR="${target_dir}" run_with_timeout "${COMPARISON_BUILD_TIMEOUT_SECONDS}" soldr cargo build --release)
            ;;
        *)
            echo "unknown tool: ${tool}" >&2
            return 2
            ;;
    esac
}

# soldr#3802: pay soldr's one-time tool provisioning (linker-driver LLVM, shims,
# cmake/ninja) before the timed build, under the IDENTICAL env run_build uses
# (same SOLDR_CACHE_DIR, CARGO_TARGET_DIR and PATH) so the build finds exactly
# what prepare put there. The compile cache stays empty: prepare starts no daemon.
run_prepare() {
    local project="$1"
    local cache_dir="$2"
    local target_dir="$3"
    local unpinned=()
    if [[ ! -f "${project}/rust-toolchain.toml" && ! -f "${project}/rust-toolchain" ]]; then
        unpinned=(--allow-unpinned)
    fi
    mkdir -p "${cache_dir}"
    measure::register_soldr_root "${WORK_DIR}" "${cache_dir}"
    (cd "${project}" && SOLDR_CACHE_DIR="${cache_dir}" CARGO_TARGET_DIR="${target_dir}" run_with_timeout "${COMPARISON_BUILD_TIMEOUT_SECONDS}" soldr ${unpinned[@]+"${unpinned[@]}"} toolchain prepare)
}

clean_project() {
    local project="$1"
    local target_dir="$2"
    (cd "${project}" && CARGO_TARGET_DIR="${target_dir}" cargo clean >/dev/null 2>&1 || true)
    rm -rf "${target_dir}"
}

# Prints "<wall_ms> <cache_bytes> <provision_ms> <provision_bytes>"; the last
# two are "null" except for soldr, whose tools are pre-provisioned (soldr#3802).
measure_cold() {
    local benchmark="$1"
    local fixture="$2"
    local tool="$3"
    local samples=()
    local cache_bytes=0
    local provision_ms=null
    local provision_bytes=null
    for idx in 1; do
        local cell="${WORK_DIR}/${benchmark}/cold/${tool}/${idx}"
        local project cache target ms
        make_fixture_project "${fixture}" "${cell}/src"
        project="${CREATED_PROJECT}"
        cache="${cell}/cache"
        target="${cell}/target"
        if [[ "${tool}" == "soldr" ]]; then
            provision_ms="$(measure_command "${benchmark} cold ${tool} provision ${idx}" run_prepare "${project}" "${cache}" "${target}")"
            provision_bytes="$(dir_size_bytes "${cache}")"
        fi
        ms="$(measure_command "${benchmark} cold ${tool} sample ${idx}" run_build "${tool}" "${project}" "${cache}" "${target}")"
        samples+=("${ms}")
        cache_bytes="$(dir_size_bytes "${cache}")"
        if [[ "${provision_bytes}" != "null" ]]; then
            # The root after the build still holds the provisioned tools; report
            # only what the build itself added so cache_bytes is not inflated.
            cache_bytes=$(( cache_bytes > provision_bytes ? cache_bytes - provision_bytes : 0 ))
        fi
    done
    printf '%s %s %s %s\n' "$(median_ms "${samples[@]}")" "${cache_bytes}" "${provision_ms}" "${provision_bytes}"
}

measure_warm() {
    local benchmark="$1"
    local fixture="$2"
    local tool="$3"
    local cell="${WORK_DIR}/${benchmark}/warm/${tool}"
    local project cache target
    make_fixture_project "${fixture}" "${cell}/src"
    project="${CREATED_PROJECT}"
    cache="${cell}/cache"
    target="${cell}/target"
    run_logged "${benchmark} warm ${tool} populate" run_build "${tool}" "${project}" "${cache}" "${target}"
    clean_project "${project}" "${target}"
    local ms
    ms="$(measure_command "${benchmark} warm ${tool}" run_build "${tool}" "${project}" "${cache}" "${target}")"
    printf '%s %s\n' "${ms}" "$(dir_size_bytes "${cache}")"
}

measure_worktree_share() {
    local benchmark="$1"
    local fixture="$2"
    local tool="$3"
    local cell="${WORK_DIR}/${benchmark}/worktree-share/${tool}"
    local project_a project_b cache target_a target_b
    make_fixture_project "${fixture}" "${cell}/src-a"
    project_a="${CREATED_PROJECT}"
    make_fixture_project "${fixture}" "${cell}/src-b"
    project_b="${CREATED_PROJECT}"
    cache="${cell}/cache"
    target_a="${cell}/target-a"
    target_b="${cell}/target-b"
    run_logged "${benchmark} worktree-share ${tool} populate A" run_build "${tool}" "${project_a}" "${cache}" "${target_a}"
    local ms
    ms="$(measure_command "${benchmark} worktree-share ${tool} build B" run_build "${tool}" "${project_b}" "${cache}" "${target_b}")"
    printf '%s %s\n' "${ms}" "$(dir_size_bytes "${cache}")"
}

RESULTS_JSONL="${WORK_DIR}/comparison-results.jsonl"
: >"${RESULTS_JSONL}"

# bash 3.2 has no associative arrays. A case statement is the portable
# lookup and reads no worse for two entries.
fixture_for() {
    case "$1" in
        rust-only) printf '%s
' 'demo-small' ;;
        rust-c)    printf '%s
' 'rust-native' ;;
        *) echo "unknown benchmark: $1" >&2; return 1 ;;
    esac
}

for benchmark in rust-only rust-c; do
    fixture="$(fixture_for "${benchmark}")"
    for tool in bare sccache soldr; do
        label="$(tool_label "${tool}")"
        command="$(command_text "${tool}")"

        read -r ms cache_bytes provision_ms provision_bytes < <(measure_cold "${benchmark}" "${fixture}" "${tool}")
        append_result "${benchmark}" "${fixture}" "cold" "Cold build (tools pre-provisioned)" "cold" "${tool}" "${label}" "${command}" "${ms}" "${cache_bytes}" "single-sample" "${provision_ms}" "${provision_bytes}"

        read -r ms cache_bytes < <(measure_warm "${benchmark}" "${fixture}" "${tool}")
        append_result "${benchmark}" "${fixture}" "warm" "Clean-target rebuild (same workspace; warm compiler cache)" "warm" "${tool}" "${label}" "${command}" "${ms}" "${cache_bytes}" "single-sample"

        read -r ms cache_bytes < <(measure_worktree_share "${benchmark}" "${fixture}" "${tool}")
        append_result "${benchmark}" "${fixture}" "worktree-share" "Agent worktree / parent-child share" "worktree-share" "${tool}" "${label}" "${command}" "${ms}" "${cache_bytes}" "single-sample"
    done
done

ran_at="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
soldr_version="$(soldr --version 2>/dev/null || echo unknown)"
rustc_version="$(rustc --version 2>/dev/null || echo unknown)"
sccache_version="$(sccache --version 2>/dev/null || echo unknown)"

jq -s \
    --arg ran_at "${ran_at}" \
    --arg soldr_version "${soldr_version}" \
    --arg rustc_version "${rustc_version}" \
    --arg sccache_version "${sccache_version}" \
    '{
        ran_at: $ran_at,
        soldr_version: $soldr_version,
        rustc_version: $rustc_version,
        sccache_version: $sccache_version,
        schema_version: 2,
        scenarios: [
            {key: "cold", label: "Cold build (tools pre-provisioned)", samples: 1},
            {key: "warm", label: "Clean-target rebuild (same workspace; warm compiler cache)", samples: 1},
            {key: "worktree-share", label: "Agent worktree / parent-child share", samples: 1}
        ],
        tools: [
            {key: "bare", label: "Bare cargo"},
            {key: "sccache", label: "sccache"},
            {key: "soldr", label: "soldr"}
        ],
        results: .
    }' "${RESULTS_JSONL}" >"${OUT_DIR}/comparison.json"

echo "comparison.json written to ${OUT_DIR}/comparison.json" >&2
cat "${OUT_DIR}/comparison.json" >&2
