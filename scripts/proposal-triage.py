#!/usr/bin/env python3
"""Triage weave-generated, human-held proposals by the user-approved rules.

Rules (RULE_VERSION below; bead subgov-91eebc6b):
  1. Near-duplicates inside a workspace collapse onto the richest bead.
  2. Documentation proposals are rejected unless they cite a live hazard.
  3. Test/coverage proposals are rejected unless tied to a dated defect.
  4. Security proposals citing a concrete vulnerability are admitted.
  5. Other proposals are admitted when the evidence check passes: a dated
     failure or live observation, a healthy gate, a working verifier.
Everything else is left held. Admission removes `proposal-pending` and `human`.
Every decision is appended to a JSONL receipt so mature outcomes can be joined
to the rule version later (NEEDLE self-learning lifecycle).

Default is a dry run. Pass --apply to mutate bead stores.
"""
import argparse
import hashlib
import json
import re
import subprocess
import sys
import time
from collections import Counter, defaultdict
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone
from pathlib import Path

RULE_VERSION = "proposal-triage-2026-10-23.1"
HOLD_PREFIXES = ("deferred", "retry-cooldown", "over-budget", "failure-count",
                 "cycling", "umbrella", "split-child", "parent-")
DOC_TITLE = re.compile(r"\b(document|documentation|docs?|readme|runbook|reference|"
                       r"provenance banner|markdown|guide|changelog|adr)\b", re.I)
TEST_TITLE = re.compile(r"\b(tests?|testing|coverage|regression|contract-test|pin|pins|"
                        r"fixture|mechanical enforcement)\b", re.I)
HAZARD = re.compile(r"prohibited|retired path|forbidden|data loss|outage|incident|exposed|"
                    r"unauthenticated|credential|secret|vulnerab|critical", re.I)
DEFECT = re.compile(r"\b(regression|bug|fixed in|failed|failing|broke|crash|incident|outage)\b", re.I)
DOC_EXCLUDE = re.compile(r"\b(ci|gate|enforce|beads?|alerts?|probe|deploy|release|version)\b", re.I)
DATED = re.compile(r"2026-(09|10)-\d\d")
LIVE = re.compile(r"observed|verified|live|evidence|failed|failing", re.I)
SEC_TITLE = re.compile(r"npm audit|vulnerab|\bcve\b|unauthenticated|authentication|authorization|"
                       r"ownership check|access.control", re.I)
SEC_EVIDENCE = re.compile(r"\d+\s+(critical|high)|unauthenticated|missing auth|ownership|"
                          r"exposed|cve-\d", re.I)
VERIFIER_MARKERS = ("Cargo.toml", "go.mod", "package.json", "pyproject.toml", "justfile",
                    "Justfile", "Makefile")


def run(cmd, cwd, timeout=120):
    return subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=timeout)


def tokens(title):
    return frozenset(x for x in re.findall(r"[a-z0-9_]+", title.lower()) if len(x) > 3)


def held_only(issue):
    labels = issue.get("labels", [])
    if "proposal-pending" not in labels or "weave-generated" not in labels:
        return False
    return not any(l.startswith(HOLD_PREFIXES) for l in labels)


def richness(issue):
    d = issue.get("description", "")
    return (bool(DATED.search(d)) + bool(LIVE.search(d)), len(d),
            -len(issue.get("created_at", "")), issue["id"])


def gate_degraded(workspace, gate_dir):
    key = hashlib.sha256(str(workspace.resolve()).encode()).hexdigest()[:12]
    try:
        return json.loads((gate_dir / f"{key}.json").read_text()).get("degraded", False) is not False
    except (OSError, ValueError):
        return False


def has_verifier(workspace):
    if any((workspace / m).exists() for m in VERIFIER_MARKERS):
        return True
    cfg = workspace / ".needle.yaml"
    try:
        text = cfg.read_text()
    except OSError:
        return False
    return bool(re.search(r"^\s*(gates|verification):", text, re.M))


def decide(issue, survivor_of, healthy):
    """Return (action, reason) for a non-duplicate proposal."""
    title, desc = issue["title"], issue.get("description", "")
    dated_evidence = bool(DATED.search(desc) and LIVE.search(desc))
    if (DOC_TITLE.search(title) and not TEST_TITLE.search(title) and not DOC_EXCLUDE.search(title)
            and not HAZARD.search(desc)):
        return "reject-doc", "documentation proposal without a cited live hazard"
    if TEST_TITLE.search(title) and not (DEFECT.search(desc) and DATED.search(desc)):
        return "reject-test", "test/coverage proposal not tied to a dated defect"
    if SEC_TITLE.search(title) and SEC_EVIDENCE.search(desc):
        return ("admit-security", "security proposal citing a concrete vulnerability") if healthy \
            else ("leave", "security proposal in a workspace without a healthy gate or verifier")
    if dated_evidence:
        return ("admit-evidence", "dated failure or live observation; healthy gate and verifier") \
            if healthy else ("leave", "evidence present but workspace gate unhealthy or no verifier")
    return "leave", "no dated evidence"


def plan_workspace(workspace, gate_dir):
    r = run(["bead", "list", "--status", "open", "--json", "--limit", "20000"], workspace, 180)
    try:
        data = json.loads(r.stdout)
    except ValueError:
        return []
    issues = data if isinstance(data, list) else data.get("issues", [])
    props = [i for i in issues if held_only(i)]
    healthy = not gate_degraded(workspace, gate_dir) and has_verifier(workspace)
    # 1. near-duplicate clusters (Jaccard >= 0.7), keep the richest.
    props.sort(key=richness, reverse=True)
    toks = [tokens(i["title"]) for i in props]
    by_id_all = {i["id"]: i for i in props}
    taken, plan = set(), []
    for a in range(len(props)):
        if a in taken:
            continue
        cluster = []
        for b in range(a + 1, len(props)):
            if b in taken:
                continue
            ta, tb = toks[a], toks[b]
            if ta and tb and len(ta & tb) / len(ta | tb) >= 0.7:
                cluster.append(b)
                taken.add(b)
        for b in cluster:
            plan.append({"workspace": str(workspace), "id": props[b]["id"], "action": "reject-dup",
                         "reason": f"duplicate of {props[a]['id']}", "survivor": props[a]["id"]})
    for a, issue in enumerate(props):
        if a in taken:
            continue
        action, reason = decide(issue, None, healthy)
        plan.append({"workspace": str(workspace), "id": issue["id"], "action": action,
                     "reason": reason, "title": issue["title"][:100]})
    # 2a. Security paraphrases rarely share words: keep one bead per topic.
    topics = (("deps", re.compile(r"npm audit|vulnerabilit", re.I)),
              ("ownership", re.compile(r"ownership", re.I)),
              ("authz", re.compile(r"authenticat|authoriz|unprotected|endpoint", re.I)))
    seen_topic = {}
    for item in sorted((x for x in plan if x["action"] == "admit-security"),
                       key=lambda x: richness(by_id_all[x["id"]]), reverse=True):
        topic = next((n for n, rx in topics if rx.search(item["title"])), item["id"])
        if topic in seen_topic:
            item["action"], item["reason"] = "leave", f"same security topic as admitted {seen_topic[topic]}"
        else:
            seen_topic[topic] = item["id"]
    # 2b. Loose duplicate pass over admissions only: never admit two look-alikes.
    by_id = {i["id"]: (i, tokens(i["title"])) for i in props}
    kept = []
    for item in sorted((x for x in plan if x["action"].startswith("admit")),
                       key=lambda x: richness(by_id[x["id"]][0]), reverse=True):
        ta = by_id[item["id"]][1]
        twin = next((k for k in kept if ta and by_id[k][1]
                     and len(ta & by_id[k][1]) / len(ta | by_id[k][1]) >= 0.4), None)
        if twin:
            item["action"], item["reason"] = "leave", f"possible duplicate of admitted {twin}"
        else:
            kept.append(item["id"])
    return plan


def apply_workspace(workspace, items, receipts_path):
    ws = Path(workspace)
    stamp = datetime.now(timezone.utc).isoformat()
    note = f"Proposal triage {RULE_VERSION} (user-approved 2026-10-23, bead subgov-91eebc6b)"
    done = 0
    for it in items:
        act = it["action"]
        try:
            if act.startswith("reject"):
                r = run(["bead", "close", it["id"], "--reason",
                         f"Rejected by proposal triage ({it['reason']}); rule {RULE_VERSION}. Reopen to reconsider.",
                         "--no-auto-flush"], ws)
            elif act.startswith("admit"):
                ok = True
                for label in ("proposal-pending", "human"):
                    ok &= run(["bead", "label", "remove", it["id"], "--label", label,
                               "--no-auto-flush"], ws).returncode == 0
                run(["bead", "update", it["id"], "--append-notes",
                     f"Admitted: {it['reason']}. {note}.", "--no-auto-flush"], ws)
                r = type("R", (), {"returncode": 0 if ok else 1})()
            else:
                continue
            it["ok"] = r.returncode == 0
        except Exception as exc:  # keep going; record the failure
            it["ok"] = False
            it["error"] = str(exc)[:200]
        it["at"] = stamp
        it["rule"] = RULE_VERSION
        with receipts_path.open("a") as f:
            f.write(json.dumps(it) + "\n")
        done += 1
    run(["bead", "sync", "flush-only"], ws, 300)
    return done


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--demand", default=str(Path.home() / ".local/state/subgov/zai-ready-demand.json"))
    p.add_argument("--gate-dir", default=str(Path.home() / ".needle/state/gate-health"))
    p.add_argument("--receipts", default=str(Path.home() / ".local/state/subgov/proposal-triage-receipts.jsonl"))
    p.add_argument("--plan-out")
    p.add_argument("--apply", action="store_true")
    p.add_argument("--only", action="append", help="limit to workspace basename(s)")
    p.add_argument("--workers", type=int, default=4)
    args = p.parse_args()
    demand = json.loads(Path(args.demand).read_text())
    spaces = sorted({b["workspace"] for b in demand["blocked"] if b.get("id")})
    spaces = [Path(s) for s in spaces if not args.only or Path(s).name in args.only]
    gate_dir = Path(args.gate_dir)
    with ThreadPoolExecutor(max_workers=args.workers) as pool:
        plans = list(pool.map(lambda w: plan_workspace(w, gate_dir), spaces))
    plan = [x for part in plans for x in part]
    print(json.dumps(Counter(x["action"] for x in plan), indent=1))
    if args.plan_out:
        Path(args.plan_out).write_text(json.dumps(plan))
    if not args.apply:
        return 0
    grouped = defaultdict(list)
    for x in plan:
        if x["action"] != "leave":
            grouped[x["workspace"]].append(x)
    receipts = Path(args.receipts)
    receipts.parent.mkdir(parents=True, exist_ok=True)
    with ThreadPoolExecutor(max_workers=args.workers) as pool:
        total = sum(pool.map(lambda kv: apply_workspace(kv[0], kv[1], receipts), grouped.items()))
    print(f"applied {total} decisions; receipts: {receipts}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
