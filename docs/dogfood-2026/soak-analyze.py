#!/usr/bin/env python3
"""Summarise the soak: events from both sides, the journal audit, GitHub PR state."""
import json, subprocess, sys
from pathlib import Path
from collections import Counter


def p(values, q):
    v = sorted(values)
    return v[max(0, (q * len(v) + 99) // 100 - 1)] if v else None


def side(events, name):
    snaps = [e for e in events if e["kind"] == "snapshot"]
    vis = [e["seconds"] for e in events if e["kind"] == "visible"]
    seen = [v for v in vis if v is not None]
    lands = [e for e in events if e["kind"] == "land"]
    ok = [e for e in lands if e["code"] == 0 and e.get("url")]
    acked = Counter(i for e in events if e["kind"] in ("snapshot", "final_flush")
                    for i in e.get("published") or [])
    journal = {}
    for e in events:
        if e["kind"] == "journal":
            journal[e["fork"]] = e.get("ids") or []
    jcount = Counter(i for ids in journal.values() for i in ids)
    # Per fork: an id acknowledged for fork F must appear exactly once in F's own journal.
    acked_by = {}
    for e in events:
        if e["kind"] in ("snapshot", "final_flush"):
            for i in e.get("published") or []:
                acked_by.setdefault(i, set()).add(e["fork"])
    lost = [i for i, forks in acked_by.items()
            if any(Counter(journal.get(f, []))[i] != 1 for f in forks)]
    in_two_forks = [i for i in jcount if sum(i in ids for ids in journal.values()) > 1]
    dup_journal = [i for i, n in jcount.items() if n > 1]
    dup_ack = [i for i, n in acked.items() if n > 1]
    unacked = [i for i in jcount if i not in acked]
    return {
        "side": name,
        "agents": len({e["fork"] for e in snaps}),
        "snapshots": len(snaps),
        "snapshot_errors": sum(1 for e in snaps if e["code"] != 0),
        "snapshots_queued_offline": sum(1 for e in snaps if e.get("offline")),
        "p95_snapshot_seconds": p([e["seconds"] for e in snaps if e["code"] == 0], 95),
        "acknowledged_changesets": len(acked),
        "journal_changesets": sum(jcount.values()),
        "forks_audited": len(journal),
        "lost": len(lost), "lost_ids": lost[:10],
        "duplicates_in_journal": len(dup_journal), "acknowledged_twice": len(dup_ack),
        "ids_in_two_forks": len(in_two_forks),
        "in_journal_not_acknowledged": len(unacked),
        "visible_measured": len(vis), "visible_missed_60s": len(vis) - len(seen),
        "p50_snapshot_to_visible_seconds": p(seen, 50), "p95_snapshot_to_visible_seconds": p(seen, 95),
        "max_snapshot_to_visible_seconds": max(seen) if seen else None,
        "lands": len(lands), "lands_pr_opened": len(ok),
        "land_failures": [{k: e.get(k) for k in ("fork", "round", "code", "status", "reason", "err")}
                          for e in lands if not (e["code"] == 0 and e.get("url"))],
        "p50_land_to_pr_seconds": p([e["seconds"] for e in ok], 50),
        "p95_land_to_pr_seconds": p([e["seconds"] for e in ok], 95),
        "max_land_to_pr_seconds": max((e["seconds"] for e in ok), default=None),
        "crashes": [e for e in events if e["kind"] in ("agent_crash", "setup_error")],
        "start": next((e for e in events if e["kind"] == "start"), None),
        "end": next((e for e in events if e["kind"] == "end"), None),
    }


def main(mac_path, nuc_path, out_path):
    load = lambda f: [json.loads(l) for l in open(f) if l.strip()]
    mac, nuc = load(mac_path), load(nuc_path)
    chaos = [e for e in mac if e["kind"] in ("restart", "merge")]
    mac = [e for e in mac if e["kind"] not in ("restart", "merge")]
    # The test repo is deleted after the run: read the committed export when it is present.
    export = Path(__file__).with_name("soak-nuc-prs.json")
    if export.is_file():
        prs = json.loads(export.read_text())
    else:
        r = subprocess.run(["gh", "pr", "list", "-R", "broomva/cuenca-soak", "--state", "all", "--limit", "1000",
                            "--json", "number,state,mergeable,headRefName,createdAt"],
                           text=True, capture_output=True, timeout=120)
        if r.returncode != 0:
            sys.exit(f"gh pr list failed: {r.stderr[-300:]}")
        prs = json.loads(r.stdout)
    heads = Counter(x["headRefName"] for x in prs)
    res = {"mac": side(mac, "mac"), "nuc": side(nuc, "nuc"), "chaos": chaos,
           "github": {"prs": len(prs), "by_state": dict(Counter(x["state"] for x in prs)),
                      "open_by_mergeable": dict(Counter(x["mergeable"] for x in prs if x["state"] == "OPEN")),
                      "branches_with_more_than_one_pr": [h for h, n in heads.items() if n > 1]}}
    allv = [e["seconds"] for e in mac + nuc if e["kind"] == "visible" and e["seconds"] is not None]
    oks = [e["seconds"] for e in mac + nuc if e["kind"] == "land" and e["code"] == 0 and e.get("url")]
    res["total"] = {
        "acknowledged_changesets": res["mac"]["acknowledged_changesets"] + res["nuc"]["acknowledged_changesets"],
        "lost": res["mac"]["lost"] + res["nuc"]["lost"],
        "duplicates": res["mac"]["duplicates_in_journal"] + res["nuc"]["duplicates_in_journal"],
        "p95_snapshot_to_visible_seconds": p(allv, 95),
        "p95_land_to_pr_seconds": p(oks, 95),
        "lands_pr_opened": len(oks),
    }
    json.dump(res, open(out_path, "w"), indent=2)
    print(json.dumps({k: v for k, v in res.items()}, indent=1)[:6000])


if __name__ == "__main__":
    main(*sys.argv[1:4])
