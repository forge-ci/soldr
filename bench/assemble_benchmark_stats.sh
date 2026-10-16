#!/usr/bin/env bash
# Assembles the publish dir for `benchmark-stats` from this run's canary
# timings + the prior history fetched from the live raw URL.
#
# Inputs:
#   ./benchmark-output/canaries.json   (produced by run_canaries.sh)
#   ./benchmark-output/comparison.json (produced by run_comparison.sh)
#   ./bench/index.html                 (static Chart.js page)
#   ENV REPO_OWNER REPO_NAME REPO_FULL GIT_SHA RUN_URL
#
# Outputs (in ./benchmark-stats/):
#   manifest.json     fully regenerated discovery index
#   latest.json       rich snapshot of this run
#   history.jsonl     rolling 1000 lines, oldest dropped (one per commit)
#   index.html        copy of bench/index.html
#   .nojekyll         Pages compatibility marker

set -euo pipefail

: "${REPO_OWNER:?missing}"
: "${REPO_NAME:?missing}"
: "${REPO_FULL:?missing}"
: "${GIT_SHA:?missing}"
: "${RUN_URL:?missing}"

HERE="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "${HERE}/.." && pwd)"

IN_FILE="${REPO_ROOT}/benchmark-output/canaries.json"
COMPARISON_FILE="${REPO_ROOT}/benchmark-output/comparison.json"
OUT_DIR="${REPO_ROOT}/benchmark-stats"
RAW_BASE="https://raw.githubusercontent.com/${REPO_FULL}/benchmark-stats"
PAGES_URL="https://${REPO_OWNER}.github.io/${REPO_NAME}/"
HISTORY_MAX=1000

mkdir -p "${OUT_DIR}"

if [[ ! -f "${IN_FILE}" ]]; then
    echo "assemble: ${IN_FILE} not found; did run_canaries.sh complete?" >&2
    exit 1
fi

if [[ ! -f "${COMPARISON_FILE}" ]]; then
    echo "assemble: ${COMPARISON_FILE} not found; did run_comparison.sh complete?" >&2
    exit 1
fi

if ! jq -e '.wall_ms | type == "object"' "${IN_FILE}" >/dev/null; then
    echo "assemble: invalid canary timings in ${IN_FILE}" >&2
    exit 1
fi

# --- Build the new history line --------------------------------------

RAN_AT="$(jq -r '.ran_at' "${IN_FILE}")"

NEW_LINE="$(
    jq -c -n \
        --arg ts "${RAN_AT}" \
        --arg sha "${GIT_SHA}" \
        --slurpfile canaries_doc "${IN_FILE}" \
        --slurpfile comparison_doc "${COMPARISON_FILE}" \
        '{
            ts: $ts,
            sha: $sha,
            canaries: $canaries_doc[0].wall_ms,
            comparison: [
                $comparison_doc[0].results[]
                | {
                    benchmark,
                    fixture,
                    scenario_key,
                    tool,
                    wall_ms,
                    cache_bytes,
                    provision_ms,
                    provision_bytes
                }
            ]
        }'
)"

# --- Fetch prior history.jsonl (404-tolerant) ------------------------

PRIOR="$(mktemp)"
trap 'rm -f "${PRIOR}"' EXIT

if curl --fail --silent --location --max-time 30 \
    -o "${PRIOR}" \
    "${RAW_BASE}/history.jsonl"; then
    echo "assemble: fetched prior history from ${RAW_BASE}/history.jsonl" >&2
    PRIOR_COUNT="$(wc -l <"${PRIOR}" | tr -d ' ')"
    echo "assemble: prior history has ${PRIOR_COUNT} lines" >&2
else
    echo "assemble: no prior history at ${RAW_BASE}/history.jsonl (first run? branch missing?); starting fresh" >&2
    : >"${PRIOR}"
fi

# --- Write history.jsonl: keep last (HISTORY_MAX - 1) of prior, then append new ----

KEEP=$(( HISTORY_MAX - 1 ))
tail -n "${KEEP}" "${PRIOR}" >"${OUT_DIR}/history.jsonl"
printf '%s\n' "${NEW_LINE}" >>"${OUT_DIR}/history.jsonl"

NEW_COUNT="$(wc -l <"${OUT_DIR}/history.jsonl" | tr -d ' ')"
echo "assemble: wrote history.jsonl with ${NEW_COUNT} lines" >&2

# --- Write latest.json ------------------------------------------------

jq -n \
    --arg generated_at "${RAN_AT}" \
    --arg git_sha "${GIT_SHA}" \
    --arg repository "${REPO_FULL}" \
    --arg run_url "${RUN_URL}" \
    --slurpfile canaries_doc "${IN_FILE}" \
    --slurpfile comparison_doc "${COMPARISON_FILE}" \
    '{
        schema_version: 2,
        metadata: {
            generated_at: $generated_at,
            git_sha: $git_sha,
            git_ref: "main",
            repository: $repository,
            run_url: $run_url,
            fixture: "medium",
            soldr_version: $canaries_doc[0].soldr_version,
            rustc_version: $canaries_doc[0].rustc_version,
            sccache_version: $comparison_doc[0].sccache_version
        },
        canaries: $canaries_doc[0].wall_ms,
        comparison: {
            scenarios: $comparison_doc[0].scenarios,
            tools: $comparison_doc[0].tools
        },
        results: $comparison_doc[0].results
    }' >"${OUT_DIR}/latest.json"

echo "assemble: wrote latest.json" >&2

# --- Write manifest.json ---------------------------------------------

jq -n \
    --arg generated_at "${RAN_AT}" \
    --arg git_sha "${GIT_SHA}" \
    --arg repository "${REPO_FULL}" \
    --arg raw_base "${RAW_BASE}" \
    --arg pages_url "${PAGES_URL}" \
    --argjson history_max "${HISTORY_MAX}" \
    '{
        schema_version: 1,
        generated_at: $generated_at,
        git_sha: $git_sha,
        branch: "benchmark-stats",
        repository: $repository,
        artifacts: {
            manifest: {
                description: "This file. Discovery index. Regenerated on every push.",
                url: ($raw_base + "/manifest.json"),
                content_type: "application/json",
                schema_version: 1
            },
            latest: {
                description: "Rich snapshot of the most-recent main-merge benchmark run.",
                url: ($raw_base + "/latest.json"),
                content_type: "application/json",
                schema_version: 1
            },
            history: {
                description: "Slim rolling history of soldr canary timings and README comparison cache/timing rows. One JSONL line per main-commit.",
                url: ($raw_base + "/history.jsonl"),
                content_type: "application/x-ndjson",
                schema_version: 2,
                max_lines: $history_max,
                line_schema: {
                    ts: "ISO-8601 UTC timestamp of the run",
                    sha: "git sha of the main-commit being measured",
                    canaries: "object mapping canary name -> wall-time milliseconds",
                    comparison: "array of compact README comparison rows: benchmark, fixture, scenario_key, tool, wall_ms, cache_bytes, provision_ms, provision_bytes (the last two null except soldr cold, soldr#3802)"
                }
            },
            index_html: {
                description: "Human-facing rendered view with Chart.js interactive graphs.",
                url: $pages_url,
                content_type: "text/html"
            },
            trend_image: {
                description: "Static PNG of the canary trend; retained for the Pages historical deep-dive.",
                url: ($raw_base + "/benchmark-trend.png"),
                content_type: "image/png"
            },
            comparison_rust: {
                description: "Dark zccache-style overlay chart: bare cargo vs sccache vs soldr on a pure-Rust workload. Cold bars are drawn behind warm overlays. Embedded in README.",
                url: ($raw_base + "/benchmark-rust-only.jpg"),
                content_type: "image/jpeg"
            },
            comparison_rust_c: {
                description: "Dark zccache-style overlay chart: bare cargo vs sccache vs soldr on a Rust+C workload (rust-native). Cold bars are drawn behind warm overlays.",
                url: ($raw_base + "/benchmark-rust-c.jpg"),
                content_type: "image/jpeg"
            }
        },
        canaries: {
            "cargo-build-medium-cold": {
                description: "Cold full compile of perf/fixtures/medium",
                theoretical_ms: 60000
            },
            "cargo-build-medium-warm": {
                description: "Immediate repeat of warm build (cargo freshness fast-path)",
                theoretical_ms: 500
            },
            "cargo-build-medium-from-warm-zccache": {
                description: "cargo clean + rebuild from warm zccache; 100% hits expected",
                theoretical_ms: 10000
            },
            "cargo-check-medium-cross-verb": {
                description: "build -> check; pins #758 / zccache#776 cross-verb cache-key regression",
                theoretical_ms: 1500
            },
            "touch-no-change-medium-warm": {
                description: "Touch all source mtimes; content unchanged; 100% hits expected",
                theoretical_ms: 1500
            },
            "worktree-share-medium-warm": {
                description: "Cross-worktree path-remap reuse",
                theoretical_ms: 1500
            }
        },
        references: {
            perf_matrix_workflow:     "https://github.com/zackees/soldr/blob/main/.github/workflows/perf-matrix.yml",
            benchmark_stats_workflow: "https://github.com/zackees/soldr/blob/main/.github/workflows/benchmark-stats.yml",
            perf_matrix_doc:          "https://github.com/zackees/soldr/blob/main/PERF.md",
            meta_tracking_issue:      "https://github.com/zackees/soldr/issues/757"
        }
    }' >"${OUT_DIR}/manifest.json"

echo "assemble: wrote manifest.json" >&2

# --- Copy index.html + .nojekyll -------------------------------------

cp "${REPO_ROOT}/bench/index.html" "${OUT_DIR}/index.html"
: >"${OUT_DIR}/.nojekyll"

echo "assemble: publish dir ready at ${OUT_DIR}" >&2
ls -la "${OUT_DIR}" >&2
