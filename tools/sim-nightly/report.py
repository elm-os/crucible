#!/usr/bin/env python3
"""Gathers the shard logs of the nightly simulation lanes of .github/workflows/sim.yml.

summary  Reads the logs <lane>-<shard>.jsonl that `cargo xtask sim run --out` wrote and prints the job summary:
         the runs of each lane, then every failed run with its invariant, tier, replay command and corpus line,
         under one heading per tier. A failure's tier is its run's, not its lane's: the stress lane also runs
         the realistic lines of the corpus. Writes every failure to a JSON file for the issue step, and
         `failures=<n>` and `gate=pass|fail` to $GITHUB_OUTPUT. The gate fails on a realistic-tier failure, on
         a shard without a log and on a lane that ran fewer fresh runs than its budget; stress-tier failures
         never fail it.
issue    Prints the issue text that lists the failures of that JSON file which the existing issue text (empty
         when there is no issue yet) does not hold, or nothing when it holds them all.

    python3 tools/sim-nightly/report.py summary --runs DIR --failures FILE --lane realistic:16:10200 ...
    python3 tools/sim-nightly/report.py issue --failures FILE --existing FILE

--lane is <lane>:<shards>:<fresh runs of its budget>. The run and its commit come from the GITHUB_*
variables of the job.
"""

import argparse
import json
import os
from collections import Counter
from pathlib import Path

TIERS = ("realistic", "stress")
NO_LOG = "no run log"
# A step summary holds at most 1 MiB, and an issue body or comment at most 65,536 characters.
SUMMARY_BUDGET = 900_000
ISSUE_BUDGET = 60_000


def lane_spec(text):
    try:
        name, shards, budget = text.split(":")
        spec = (name, int(shards), int(budget))
    except ValueError:
        raise argparse.ArgumentTypeError(f"`{text}` is not <lane>:<shards>:<fresh runs>")
    if name not in TIERS:
        raise argparse.ArgumentTypeError(f"`{name}` is not one of {', '.join(TIERS)}")
    return spec


def run_line():
    env = os.environ.get
    url = f"{env('GITHUB_SERVER_URL', 'https://github.com')}/{env('GITHUB_REPOSITORY', '')}"
    run = env("GITHUB_RUN_ID", "")
    return f"run [{run}]({url}/actions/runs/{run}) on `{env('GITHUB_REF_NAME', '')}` at `{env('GITHUB_SHA', '')}`"


def plural(count, word):
    return f"{count:,} {word}" + ("" if count == 1 else "s")


def failed_run(record, source):
    # The replay command and the corpus line as `cargo xtask sim run` prints them. The key is the replay
    # command, which no other run's command contains.
    invariant = record["invariant"] or "?"
    replay = f"cargo xtask sim replay --seed {record['seed']} --config {record['config']} --tier {record['tier']}"
    line = json.dumps(dict(record, note="failed"), separators=(",", ":"), ensure_ascii=False)
    return {
        "key": replay,
        "tier": record["tier"],
        "invariant": invariant,
        "replay": replay,
        "corpus_line": line,
        "text": f"- **{invariant}** in the {record['tier']} tier, a {record['note']} run of {source}\n"
        f"  ```sh\n  {replay}\n  ```\n  ```json\n  {line}\n  ```\n",
    }


def missing_log(lane, shard, shards):
    key = f"{lane} shard {shard}/{shards} of run {os.environ.get('GITHUB_RUN_ID', '')} wrote {NO_LOG}"
    return {
        "key": key,
        "tier": lane,
        "invariant": NO_LOG,
        "text": f"- {key}: a run did not finish before the job timeout, or the job failed before its runs "
        "(see its job log)\n",
    }


def sections(items, budget, empty, rest):
    """The failures under one heading per tier, as many as fit in `budget` characters."""
    out = []
    for tier in TIERS:
        mine = [item for item in items if item["tier"] == tier]
        out += ["", f"### {tier.capitalize()} tier", ""]
        if not mine:
            out.append(empty)
            continue
        counts = sorted(Counter(item["invariant"] for item in mine).items())
        out += [f"{plural(len(mine), 'failure')}: " + ", ".join(f"{k} ({v})" for k, v in counts) + ".", ""]
        shown = 0
        for item in mine:
            if len(item["text"]) > budget:
                break
            out.append(item["text"].rstrip("\n"))
            budget -= len(item["text"])
            shown += 1
        if shown < len(mine):
            out += ["", f"{plural(len(mine) - shown, 'more failure')}: see {rest}."]
    return out


def summary(args):
    failures = {}
    rows = []
    short = []
    for lane, shards, budget in args.lane:
        counts = Counter()
        logs = 0
        for shard in range(shards):
            path = args.runs / f"{lane}-{shard}.jsonl"
            if not path.exists():
                item = missing_log(lane, shard, shards)
                failures[item["key"]] = item
                continue
            logs += 1
            for text in path.read_text(encoding="utf-8").splitlines():
                if not text.strip():
                    continue
                record = json.loads(text)
                counts.update(["runs", record["note"], record["outcome"]])
                if record["outcome"] == "fail":
                    item = failed_run(record, f"{lane} shard {shard}/{shards}")
                    failures.setdefault(item["key"], item)
        rows.append(
            f"| {lane} | {logs} of {shards} | {counts['runs']:,} | {counts['fresh']:,} | {counts['pass']:,} "
            f"| {counts['fail']:,} | {counts['loss-expected']:,} |"
        )
        if counts["fresh"] < budget:
            short.append(f"the {lane} lane ran {counts['fresh']:,} fresh runs of its {budget:,}")
    items = list(failures.values())
    blocking = sum(1 for item in items if item["tier"] != "stress" and item["invariant"] != NO_LOG)
    missing = sum(1 for item in items if item["invariant"] == NO_LOG)
    gate = [plural(blocking, "realistic-tier failure")] if blocking else []
    gate += [plural(missing, "shard") + " without a run log"] if missing else []
    gate += short

    args.failures.write_text(json.dumps(items, indent=2) + "\n", encoding="utf-8")
    if os.environ.get("GITHUB_OUTPUT"):
        with open(os.environ["GITHUB_OUTPUT"], "a", encoding="utf-8") as output:
            output.write(f"failures={len(items)}\ngate={'fail' if gate else 'pass'}\n")

    out = ["## Nightly simulation", "", f"The {run_line()}.", ""]
    out += [
        "| Lane | Shard logs | Runs | Fresh runs | Passed | Failed | Losses expected |",
        "|---|---|---|---|---|---|---|",
    ]
    out += rows + [""]
    if gate:
        out.append(f"**Fail**: {'; '.join(gate)}.")
    else:
        out.append("**Pass**: no realistic-tier run failed, every shard wrote its log and every lane ran its budget.")
    if any(item["tier"] == "stress" for item in items):
        out += ["", "Stress-tier failures are filed but do not fail the workflow."]
    out += sections(items, SUMMARY_BUDGET, "No failures.", "the `sim-failures` artifact of the run")
    print("\n".join(out))


def issue(args):
    items = json.loads(args.failures.read_text(encoding="utf-8"))
    existing = args.existing.read_text(encoding="utf-8")
    new = [item for item in items if item["key"] not in existing]
    if not new:
        return
    head = "New failures of the" if existing.strip() else "Failures of the"
    out = [f"{head} {run_line()}. Triage: docs/testing/system-simulation.md, Triaging a nightly failure."]
    out += sections(new, ISSUE_BUDGET, "None.", "the run's summary and its `sim-failures` artifact")
    print("\n".join(out))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    gather = commands.add_parser("summary")
    gather.add_argument("--runs", type=Path, required=True)
    gather.add_argument("--failures", type=Path, required=True)
    gather.add_argument("--lane", type=lane_spec, action="append", required=True)
    gather.set_defaults(run=summary)
    file = commands.add_parser("issue")
    file.add_argument("--failures", type=Path, required=True)
    file.add_argument("--existing", type=Path, required=True)
    file.set_defaults(run=issue)
    args = parser.parse_args()
    args.run(args)


if __name__ == "__main__":
    main()
