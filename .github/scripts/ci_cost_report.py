#!/usr/bin/env python3
"""Report all GitHub Actions runner time for exact SHAs and a routine event.

Example: uv run --no-project python .github/scripts/ci_cost_report.py \
  --repo zackees/soldr --event push --baseline-sha SHA1 --baseline-sha SHA2 \
  --post-sha SHA3 --post-sha SHA4

For same-SHA PR label transitions, repeat each SHA with a distinct matching
``--baseline-anchor-run-id`` or ``--post-anchor-run-id``. Anchor IDs are paired
with SHA arguments in their respective order.

To compare a routine main push with a full-equivalent manual dispatch on the
same candidate SHA, use ``--baseline-event workflow_dispatch`` and
``--post-event push`` with explicit anchors for each wave. A dispatch anchor is
mandatory: GitHub's run ``head_sha`` names the workflow ref, which can differ
from the candidate checked out by CI. The dispatch must have the workflow's
``CI full <candidate_sha>`` run title. Cross-event controls are reported as
such, never disguised as pre/post samples of one event.

Only ``gh api`` read calls are made.
Chained ``workflow_run`` costs require the versioned ``ci-cost-parent-v1``
receipt in their run title; absent or conflicting attribution fails closed. A
matching SHA or timestamp alone never establishes a causal relationship. Supply anchor run IDs when a SHA was used
for multiple PR label transitions; the report refuses to merge event waves
more than two minutes apart without them. The output is JSON so the inputs,
included and excluded runs, attempts, jobs, and arithmetic remain inspectable. Wall time
is an estimate of runner time, not GitHub's billed-minute ledger. Optional
``--weights`` accepts a JSON object mapping a job runner label to a vCPU or
billing multiplier; unmapped jobs are excluded from weighted sums and listed.
"""

import argparse
import json
import re
import statistics
import subprocess
from datetime import datetime

PAGE_SIZE = 100
EVENT_WINDOW_SECONDS = 120
EVENTS = ("push", "pull_request", "workflow_dispatch", "merge_group")


class GhAPI:
    def get(self, endpoint, params=None):
        cmd = ["gh", "api", "--method", "GET", endpoint]
        for key, value in (params or {}).items():
            cmd += ["-f", f"{key}={value}"]
        return json.loads(subprocess.check_output(cmd, text=True))


def parse_parent_receipt(title, run_id):
    match = re.fullmatch(r"ci-cost-parent-v1=([1-9][0-9]*)", title or "")
    if not match:
        raise ValueError(f"run {run_id} has no unambiguous parent receipt")
    return int(match.group(1))


def pages(api, endpoint, key, params=None):
    """Fetch all pages, including the final page after a full page of 100."""
    page, observed = 1, 0
    expected = None
    identifiers = set()
    while True:
        data = api.get(
            endpoint, {**(params or {}), "per_page": PAGE_SIZE, "page": page}
        )
        count = data.get("total_count")
        if isinstance(count, bool) or not isinstance(count, int) or count < 0:
            raise ValueError(f"{endpoint}: missing or invalid total_count")
        if key == "workflow_runs" and count >= 1000:
            raise ValueError(
                f"{endpoint}: search reaches GitHub's 1000-run limit; narrow the time range"
            )
        if expected is not None and count != expected:
            raise ValueError(f"{endpoint}: inventory changed during pagination; retry")
        expected = count
        items = data[key]
        for item in items:
            if item["id"] in identifiers:
                raise ValueError(f"{endpoint}: duplicate paginated ID; retry")
            identifiers.add(item["id"])
        observed += len(items)
        yield from items
        if len(items) < PAGE_SIZE:
            if observed != expected:
                raise ValueError(
                    f"{endpoint}: incomplete inventory ({observed}/{expected})"
                )
            break
        page += 1


def parse_time(value):
    return datetime.fromisoformat(value.replace("Z", "+00:00"))


def seconds(start, end):
    if not start or not end:
        return None, 0

    elapsed = (parse_time(end) - parse_time(start)).total_seconds()
    if elapsed < 0:
        # GitHub's second-resolution job timestamps occasionally arrive one
        # second out of order. Retain the job and disclose the skew instead of
        # discarding an otherwise valid event-wide cost sample.
        if elapsed >= -2:
            return 0, -elapsed
        raise ValueError(f"negative job duration: {start} to {end}")
    return elapsed, 0


def validate_linked_receipt(receipt, repo, sha, anchor, linked):
    """Validate supplied capture evidence, not timestamps or a common SHA.

    Capture authenticity belongs to the trusted evidence supplier. This reader
    checks its contents against authoritative run associations; it is not a
    signature verifier for hostile, caller-invented webhook payloads.
    """
    if not isinstance(receipt, dict):
        raise ValueError("linked receipt must be an object")
    before = receipt.get("before_sha", "")
    after = receipt.get("after_sha")
    association = {
        "number": receipt.get("pull_request"),
        "head": {
            "ref": receipt.get("head_ref"),
            "sha": sha,
            "repo": {"id": receipt.get("head_repository_id")},
        },
    }
    if (
        receipt.get("schema") != "fleet-ci-linked-event/v1"
        or receipt.get("repository") != repo
        or receipt.get("relation") != "branch-push-to-pr-synchronize"
        or receipt.get("anchor_run_id") != anchor["id"]
        or type(receipt.get("anchor_run_id")) is not int
        or receipt.get("linked_run_id") != linked["id"]
        or anchor["event"] != "pull_request"
        or linked["event"] != "push"
        or after != sha
        or linked["head_sha"] != sha
        or not isinstance(before, str)
        or not re.fullmatch(r"[0-9a-f]{40}", before)
        or before == "0" * 40
        or before == after
        or type(receipt.get("pull_request")) is not int
        or receipt["pull_request"] <= 0
        or type(receipt.get("head_repository_id")) is not int
        or receipt["head_repository_id"] <= 0
        or not isinstance(receipt.get("head_ref"), str)
        or not receipt["head_ref"]
    ):
        raise ValueError("invalid synchronize linked-event receipt")
    for run in [anchor, linked]:
        if run.get("head_branch") != receipt["head_ref"] or not any(
            row.get("number") == association["number"]
            and row.get("head", {}).get("ref") == receipt["head_ref"]
            and row.get("head", {}).get("sha") == sha
            and row.get("head", {}).get("repo", {}).get("id")
            == receipt["head_repository_id"]
            for row in run.get("pull_requests", [])
        ):
            raise ValueError("linked run PR association mismatch")
    provenance = receipt.get("provenance")
    if provenance == "operator-attested":
        return False
    if provenance != "captured-event-payloads":
        raise ValueError("linked receipt has no explicit provenance")
    pr, push = receipt.get("pr_payload"), receipt.get("push_payload")
    if not isinstance(pr, dict) or not isinstance(push, dict):
        raise ValueError("captured original event payloads required")
    if (
        pr.get("action") != "synchronize"
        or pr.get("number") != receipt["pull_request"]
        or pr.get("repository", {}).get("full_name") != repo
        or pr.get("pull_request", {}).get("head", {}).get("ref") != receipt["head_ref"]
        or pr.get("pull_request", {}).get("head", {}).get("sha") != sha
        or pr.get("pull_request", {}).get("head", {}).get("repo", {}).get("id")
        != receipt["head_repository_id"]
        or push.get("repository", {}).get("id") != receipt["head_repository_id"]
        or push.get("ref") != "refs/heads/" + receipt["head_ref"]
        or push.get("created") is not False
        or push.get("deleted") is not False
        or any(
            payload.get("before") != before or payload.get("after") != after
            for payload in [pr, push]
        )
    ):
        raise ValueError("captured events do not prove synchronize relationship")
    return True


def collect_sha(
    api,
    repo,
    sha,
    event,
    *,
    weights=None,
    anchor_run_id=None,
    linked_event_receipts=None,
):
    if event not in EVENTS:
        raise ValueError(f"event must be one of {EVENTS}")
    if not sha:
        raise ValueError("empty SHA")
    if weights is not None and any(value <= 0 for value in weights.values()):
        raise ValueError("weights must be positive")
    if event == "workflow_dispatch":
        if anchor_run_id is None:
            raise ValueError("workflow_dispatch requires an anchor run ID")
        if not re.fullmatch(r"[0-9a-fA-F]{40}", sha):
            raise ValueError("dispatch candidate SHA must be 40 hex characters")
        run = api.get(f"repos/{repo}/actions/runs/{anchor_run_id}")
        if (
            run.get("event") != event
            or str(run.get("path", "")).split("@", 1)[0] != ".github/workflows/ci.yml"
            or run.get("display_title", "").lower() != f"CI full {sha}".lower()
        ):
            raise ValueError(
                f"anchor run {anchor_run_id} is not CI full for candidate {sha}"
            )
        source_sha = run["head_sha"]
        discovered = list(
            pages(
                api,
                f"repos/{repo}/actions/runs",
                "workflow_runs",
                {"head_sha": source_sha},
            )
        )
        runs = list(
            {
                item["id"]: item
                for item in [*discovered, run]
                if item["head_sha"] == source_sha
            }.values()
        )
        event_runs = [run]
    else:
        endpoint = f"repos/{repo}/actions/runs"
        runs = list(pages(api, endpoint, "workflow_runs", {"head_sha": sha}))
        source_sha = sha
        runs = [run for run in runs if run["head_sha"] == source_sha]
        event_runs = [run for run in runs if run["event"] == event]
    anchor_time = None
    if anchor_run_id is not None:
        anchors = [run for run in event_runs if run["id"] == anchor_run_id]
        if len(anchors) != 1:
            raise ValueError(
                f"anchor run {anchor_run_id} is not a {event} run at {sha}"
            )
        anchor_time = parse_time(anchors[0]["created_at"])
    elif len(event_runs) > 1:
        times = [parse_time(run["created_at"]) for run in event_runs]
        if (max(times) - min(times)).total_seconds() > EVENT_WINDOW_SECONDS:
            raise ValueError(
                f"multiple {event} event waves at {sha}; supply an anchor run ID"
            )
    roots = [
        run
        for run in event_runs
        if anchor_time is None
        or event == "workflow_dispatch"
        or abs((parse_time(run["created_at"]) - anchor_time).total_seconds())
        <= EVENT_WINDOW_SECONDS
    ]
    if not roots:
        raise ValueError(f"no {event} workflow runs for {sha}")
    receipt_results = []
    linked_ids = set()
    for receipt in linked_event_receipts or []:
        if event != "pull_request" or anchor_run_id is None:
            raise ValueError("linked events require an explicit PR anchor")
        if (
            not isinstance(receipt, dict)
            or type(receipt.get("linked_run_id")) is not int
        ):
            raise ValueError("invalid linked run ID")
        if receipt.get("anchor_run_id") != anchor_run_id:
            raise ValueError("linked receipt anchor mismatch")
        linked_id = receipt["linked_run_id"]
        if linked_id <= 0 or linked_id in linked_ids:
            raise ValueError("duplicate or invalid linked run ID")
        linked_ids.add(linked_id)
        linked = api.get(f"repos/{repo}/actions/runs/{linked_id}")
        verified = validate_linked_receipt(receipt, repo, sha, anchors[0], linked)
        receipt_results.append({"receipt": receipt, "acceptance_evidence": verified})
        roots.append(linked)
        runs.append(linked)
    selected = {run["id"] for run in roots}
    # A workflow_run executes the default branch workflow, so its head SHA
    # can differ from the triggering run. Search by event/time, not head_sha.
    descendants = list(
        pages(
            api,
            f"repos/{repo}/actions/runs",
            "workflow_runs",
            {
                "event": "workflow_run",
                "created": ">=" + min(run["created_at"] for run in roots),
            },
        )
    )
    earliest = min(parse_time(run["created_at"]) for run in roots)
    descendants = [
        run
        for run in descendants
        if run["event"] == "workflow_run" and parse_time(run["created_at"]) >= earliest
    ]
    runs = list({run["id"]: run for run in [*runs, *descendants]}.values())
    parents = {
        run["id"]: parse_parent_receipt(run.get("display_title"), run["id"])
        for run in runs
        if run["event"] == "workflow_run"
    }
    # Receipts provide causation, not just a shared SHA or timestamp. Include
    # transitive descendants even hours after the direct event's anchor.
    while True:
        children = {child for child, parent in parents.items() if parent in selected}
        expanded = selected | children
        if expanded == selected:
            break
        selected = expanded
    included, excluded, jobs_out = [], [], []
    total_seconds = weighted_seconds = 0
    event_seconds = {}
    unknown_weights = []
    for run in sorted(runs, key=lambda item: item["id"]):
        if (
            event != "workflow_dispatch"
            and run["event"] != "workflow_run"
            and run["head_sha"] != sha
        ):
            continue  # Defensive against an API filter regression.
        info = {
            key: run.get(key)
            for key in (
                "id",
                "workflow_id",
                "name",
                "display_title",
                "event",
                "head_sha",
                "status",
                "conclusion",
                "html_url",
                "created_at",
                "run_attempt",
            )
        }
        if run["id"] not in selected:
            reason = (
                f"parent-run={parents[run['id']]} outside-event-wave"
                if run["id"] in parents
                else (
                    "outside-anchor-window"
                    if run["event"] == event
                    else f"event={run['event']}"
                )
            )
            excluded.append({**info, "reason": reason})
            continue
        if run["id"] in parents:
            info["trigger_run_id"] = parents[run["id"]]
        if run["status"] != "completed":
            raise ValueError(
                f"run {run['id']} is not completed; rerun after it finishes"
            )
        included.append(info)
        for attempt in range(1, run["run_attempt"] + 1):
            job_endpoint = (
                f"repos/{repo}/actions/runs/{run['id']}/attempts/{attempt}/jobs"
            )
            for job in pages(api, job_endpoint, "jobs"):
                # Skipped jobs never allocate a runner. GitHub may still
                # attach synthetic timestamps; retain their coverage outcome
                # without charging that span or assigning a runner weight.
                if job.get("conclusion") == "skipped":
                    duration, timestamp_skew = 0, 0
                else:
                    duration, timestamp_skew = seconds(
                        job.get("started_at"), job.get("completed_at")
                    )
                if duration is None and job.get("started_at"):
                    raise ValueError(f"job {job['id']} has no completion time")
                duration = duration or 0
                total_seconds += duration
                event_seconds[run["event"]] = (
                    event_seconds.get(run["event"], 0) + duration
                )
                labels = job.get("labels") or []
                matched = [label for label in labels if weights and label in weights]
                if weights is not None:
                    if len(matched) == 1:
                        weighted_seconds += duration * weights[matched[0]]
                    elif duration:
                        unknown_weights.append(job["id"])
                jobs_out.append(
                    {
                        "run_id": run["id"],
                        "attempt": attempt,
                        "id": job["id"],
                        "name": job.get("name"),
                        "conclusion": job.get("conclusion"),
                        "labels": labels,
                        "seconds": duration,
                        "timestamp_skew_seconds": timestamp_skew,
                        "weight_label": matched[0] if len(matched) == 1 else None,
                    }
                )
    if not included:
        raise ValueError(f"no completed {event} workflow runs for {sha}")
    return {
        "sha": sha,
        "event": event,
        "anchor_run_id": anchor_run_id,
        "event_window_seconds": EVENT_WINDOW_SECONDS,
        "direct_root_attribution": (
            "explicit full dispatch"
            if event == "workflow_dispatch"
            else "120-second heuristic; not causal proof"
        ),
        "linked_event_receipts": receipt_results,
        "linked_events_acceptance_evidence": bool(receipt_results)
        and all(row["acceptance_evidence"] for row in receipt_results),
        "runner_minutes_by_event": {
            key: value / 60 for key, value in event_seconds.items()
        },
        "runner_minutes": total_seconds / 60,
        "weighted_runner_minutes_known": (
            weighted_seconds / 60 if weights is not None else None
        ),
        "unknown_weight_job_ids": unknown_weights,
        "included_runs": included,
        "excluded_runs": excluded,
        "jobs": jobs_out,
    }


def summarize(reports):
    values = [report["runner_minutes"] for report in reports]
    if not values:
        raise ValueError("a cohort needs at least one SHA")
    return {
        "sha_count": len(values),
        "median_runner_minutes": statistics.median(values),
        "range_runner_minutes": [min(values), max(values)],
        "samples": reports,
    }


def compare(baseline, post, allow_cross_event=False):
    identities = [
        (report["sha"], report.get("anchor_run_id"), report.get("event"))
        for report in baseline + post
    ]
    if len(set(identities)) != len(identities):
        raise ValueError("baseline and post samples must be unique and disjoint")
    if len(baseline) != len(post):
        raise ValueError("baseline and post cohorts need the same number of samples")
    events = {report.get("event") for report in baseline + post if report.get("event")}
    if len(events) > 1 and not allow_cross_event:
        raise ValueError("baseline and post cohorts must use the same event")
    before, after = summarize(baseline), summarize(post)
    if before["median_runner_minutes"] == 0:
        raise ValueError("baseline median is zero; ratio is undefined")
    ratio = after["median_runner_minutes"] / before["median_runner_minutes"]
    inventories = {
        f"{report['sha']}@{report.get('anchor_run_id') or 'all'}:{report.get('event') or 'unspecified'}": sorted(
            {run["workflow_id"] for run in report.get("included_runs", [])}
        )
        for report in baseline + post
    }
    linked_evidence = all(
        row["acceptance_evidence"]
        for report in baseline + post
        for row in report.get("linked_event_receipts", [])
    )
    return {
        "baseline": before,
        "post": after,
        "median_ratio": ratio,
        "cross_event_control": len(events) > 1,
        "workflow_ids_by_sample": inventories,
        "workflow_inventory_matched": len({tuple(ids) for ids in inventories.values()})
        <= 1,
        "linked_events_acceptance_evidence": linked_evidence,
        "meets_12_5_percent_target": ratio <= 0.125 and linked_evidence,
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True, help="OWNER/REPO")
    parser.add_argument("--event", choices=EVENTS)
    parser.add_argument("--baseline-event", choices=EVENTS)
    parser.add_argument("--post-event", choices=EVENTS)
    parser.add_argument("--baseline-sha", action="append", required=True)
    parser.add_argument("--post-sha", action="append", required=True)
    parser.add_argument("--baseline-anchor-run-id", type=int, action="append")
    parser.add_argument("--post-anchor-run-id", type=int, action="append")
    parser.add_argument("--weights", help="JSON label-to-positive-multiplier file")
    parser.add_argument(
        "--linked-event-receipts", help="JSON list of explicit linked event receipts"
    )
    args = parser.parse_args(argv)
    if args.event and (args.baseline_event or args.post_event):
        parser.error("use --event or separate baseline/post events, not both")
    baseline_event = args.event or args.baseline_event
    post_event = args.event or args.post_event
    if not baseline_event or not post_event:
        parser.error("supply --event or both --baseline-event and --post-event")
    weights = None
    if args.weights:
        with open(args.weights, encoding="utf-8") as stream:
            weights = json.load(stream)
        if not isinstance(weights, dict) or not all(
            isinstance(key, str) and isinstance(value, (int, float)) and value > 0
            for key, value in weights.items()
        ):
            parser.error("--weights must map labels to positive numeric multipliers")

    def cohort_samples(shas, anchors, label):
        if anchors and len(anchors) != len(shas):
            parser.error(f"{label} anchors must match {label} SHA count")
        return list(zip(shas, anchors or [None] * len(shas), strict=True))

    baseline_samples = cohort_samples(
        args.baseline_sha, args.baseline_anchor_run_id, "baseline"
    )
    post_samples = cohort_samples(args.post_sha, args.post_anchor_run_id, "post")
    linked_receipts = []
    if args.linked_event_receipts:
        with open(args.linked_event_receipts, encoding="utf-8") as stream:
            linked_receipts = json.load(stream)
        known_anchors = {
            anchor
            for _, anchor in baseline_samples + post_samples
            if anchor is not None
        }
        if not isinstance(linked_receipts, list) or any(
            not isinstance(row, dict) or row.get("anchor_run_id") not in known_anchors
            for row in linked_receipts
        ):
            parser.error(
                "linked receipts must be a list matching explicit sample anchors"
            )
    api = GhAPI()
    reports = {}
    for event, samples in (
        (baseline_event, baseline_samples),
        (post_event, post_samples),
    ):
        for sha, anchor in samples:
            key = (sha, anchor, event)
            if key not in reports:
                reports[key] = collect_sha(
                    api,
                    args.repo,
                    sha,
                    event,
                    weights=weights,
                    anchor_run_id=anchor,
                    linked_event_receipts=[
                        row for row in linked_receipts if row["anchor_run_id"] == anchor
                    ],
                )
    result = compare(
        [reports[sample[0], sample[1], baseline_event] for sample in baseline_samples],
        [reports[sample[0], sample[1], post_event] for sample in post_samples],
        allow_cross_event=baseline_event != post_event,
    )
    result.update(
        {
            "repo": args.repo,
            "baseline_event": baseline_event,
            "post_event": post_event,
            "weights": weights,
            "measurement": "sum of completed job wall seconds over every attempt; "
            "not GitHub billed minutes; missing runner labels leave weighted cost unknown",
        }
    )
    print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
