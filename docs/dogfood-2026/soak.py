#!/usr/bin/env python3
"""Cuenca 1 h soak against the live authority (BRO-3259 R41).

agent:  N agent threads on this host, each its own key, fork and checkout.
        Every round: edit, snapshot, time until the edit is readable in the
        volume (Mac: the Finder SMB mount; NUC: a watcher running as cuenca),
        and every --land-every rounds verify + land (opens a GitHub PR).
watch:  NUC only, run as the cuenca user: answers visibility requests from the
        local materialised volume, on the same clock as the agents.
chaos:  Mac only: SIGKILL the authority once mid-run, and merge conflict PRs.
"""
import argparse, hashlib, json, os, random, subprocess, sys, threading, time, uuid
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

BIN = os.environ.get("CUENCA_BIN", "cuenca")
lock = threading.Lock()


def now():
    return time.time()


def utc(t=None):
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(t or now()))


class Out:
    def __init__(self, d):
        self.d = Path(d); self.d.mkdir(parents=True, exist_ok=True)
        self.f = open(self.d / "events.jsonl", "a")

    def ev(self, **kw):
        kw.setdefault("t", now())
        with lock:
            self.f.write(json.dumps(kw) + "\n"); self.f.flush()


def run(argv, env, timeout=300):
    t = now()
    try:
        r = subprocess.run([BIN, "--json", *argv], env=env, text=True,
                           capture_output=True, timeout=timeout)
        out = None
        try:
            out = json.loads(r.stdout.strip().splitlines()[-1]) if r.stdout.strip() else None
        except (ValueError, IndexError):
            pass
        return r.returncode, out, (r.stderr or "")[-400:], now() - t
    except subprocess.TimeoutExpired:
        return 124, None, "timeout", now() - t


class NucVisibility:
    """Posts requests for the watcher and tails its results."""
    def __init__(self, req, res):
        self.req, self.res = Path(req), Path(res)
        self.seen = {}
        threading.Thread(target=self.tail, daemon=True).start()

    def tail(self):
        pos = 0
        while True:
            if self.res.exists():
                with open(self.res) as f:
                    f.seek(pos)
                    for line in f:
                        if line.endswith("\n"):
                            r = json.loads(line); self.seen[r["key"]] = r["seen"]
                            pos += len(line.encode())
            time.sleep(0.2)

    def wait(self, fork, rel, content, t0, limit=60):
        key = uuid.uuid4().hex
        with lock, open(self.req, "a") as f:
            f.write(json.dumps({"key": key, "fork": fork, "rel": rel,
                                "sha": hashlib.sha256(content).hexdigest(), "t0": t0}) + "\n")
        while now() - t0 < limit:
            if key in self.seen:
                return self.seen[key] - t0
            time.sleep(0.2)
        return None


def mac_visible(fork, rel, content, t0, limit=60):
    p = Path("/Volumes/cuenca/forks") / fork / rel
    while now() - t0 < limit:
        try:
            if p.read_bytes() == content:
                return now() - t0
        except OSError:
            pass
        time.sleep(0.25)
    return None


def agent(a, name, idx, out, deadline, visible):
    home = Path(a.root) / name
    env = dict(os.environ, CUENCA_CONFIG=str(home / "config.json"),
               CUENCA_STATE_HOME=str(home / "state"))
    co = home / "co"
    if not co.exists():
        code, o, err, _ = run(["fork", "soak", "--name", name], env)
        if code != 0 and "exists" not in err:
            out.ev(kind="setup_error", fork=name, step="fork", code=code, err=err)
        code, o, err, _ = run(["checkout", name, str(co)], env)
        if code != 0:
            out.ev(kind="setup_error", fork=name, step="checkout", code=code, err=err); return
    rnd, rng = 0, random.Random(idx)
    time.sleep(rng.uniform(0, 15))
    while now() < deadline:
        rel = "conflict.txt" if idx % 2 == 0 else f"agents/{name}.txt"
        content = f"{name} round {rnd} {uuid.uuid4().hex}\n".encode()
        (co / rel).parent.mkdir(parents=True, exist_ok=True)
        (co / rel).write_bytes(content)
        if rnd % 10 == 3:  # a wider edit now and then
            for j in rng.sample(range(200), 20):
                (co / f"data/f{j:03d}.txt").write_text(f"{name} r{rnd} f{j}\n")
        t0 = now()
        code, o, err, sec = run(["snapshot", "--dir", str(co)], env)
        flush = (o or {}).get("flush") or {}
        pub = [p["id"] if isinstance(p, dict) else p for p in flush.get("published") or []]
        out.ev(kind="snapshot", fork=name, round=rnd, code=code, seconds=sec, published=pub,
               pending=flush.get("pending"), offline=flush.get("offline"),
               err=(flush.get("error") or err) if code or flush.get("error") else None)
        vis = None
        if code == 0 and pub and not flush.get("pending"):
            vis = visible(name, rel, content, t0)
            out.ev(kind="visible", fork=name, round=rnd, seconds=vis)
        if rnd % a.land_every == idx % a.land_every and code == 0:
            vc, vo, verr, vsec = run(["verify", "--dir", str(co)], env)
            out.ev(kind="verify", fork=name, round=rnd, code=vc, seconds=vsec,
                   passed=(vo or {}).get("passed"), err=verr if vc else None)
            if vc == 0:
                lc, lo, lerr, lsec = run(["land", "--dir", str(co), "--wait", "180"], env, timeout=400)
                lo = lo or {}
                out.ev(kind="land", fork=name, round=rnd, code=lc, seconds=lsec,
                       status=lo.get("status"), pr=lo.get("pr"), url=lo.get("url"),
                       changeset=lo.get("changeset"), reason=lo.get("reason"),
                       retrying=lo.get("retrying_after"), err=lerr if lc else None)
        rnd += 1
        time.sleep(rng.uniform(a.pause_min, a.pause_max))
    # final flush of anything queued, then audit the journal
    code, o, err, _ = run(["snapshot", "--dir", str(co)], env)
    flush = (o or {}).get("flush") or {}
    pub = [p["id"] if isinstance(p, dict) else p for p in flush.get("published") or []]
    out.ev(kind="final_flush", fork=name, code=code, published=pub, pending=flush.get("pending"))
    code, log, err, _ = run(["log", name], env)
    ids = [r["event"]["id"] for r in (log or {}).get("records", [])
           if r["event"]["event"] == "draft_published"]
    out.ev(kind="journal", fork=name, code=code, ids=ids)


def cmd_agent(a):
    out = Out(a.out)
    names = [l.split("\t")[0] for l in Path(a.names).read_text().splitlines() if l.strip()]
    visible = NucVisibility(a.req, a.res).wait if a.side == "nuc" else mac_visible
    deadline = now() + a.seconds
    out.ev(kind="start", side=a.side, agents=len(names), seconds=a.seconds, utc=utc())
    with ThreadPoolExecutor(max_workers=len(names)) as pool:
        futs = [pool.submit(agent, a, n, int(n.rsplit("-", 1)[1]), out, deadline, visible)
                for n in names]
        for f in futs:
            try:
                f.result()
            except Exception as e:  # noqa: BLE001 - recorded, never swallowed
                out.ev(kind="agent_crash", err=repr(e))
    out.ev(kind="end", utc=utc())


def cmd_watch(a):
    req, res = Path(a.req), Path(a.res)
    pending, pos = {}, 0
    res_f = open(res, "a"); os.chmod(res, 0o644)
    end = now() + a.seconds
    while now() < end:
        if req.exists():
            with open(req) as f:
                f.seek(pos)
                for line in f:
                    if line.endswith("\n"):
                        r = json.loads(line); pending[r["key"]] = r; pos += len(line.encode())
        for key, r in list(pending.items()):
            p = Path(a.volume) / "forks" / r["fork"] / r["rel"]
            try:
                if hashlib.sha256(p.read_bytes()).hexdigest() == r["sha"]:
                    res_f.write(json.dumps({"key": key, "seen": now()}) + "\n"); res_f.flush()
                    del pending[key]; continue
            except OSError:
                pass
            if now() - r["t0"] > 90:
                del pending[key]
        time.sleep(0.1)


def cmd_chaos(a):
    out = Out(a.out)
    start = now()
    merges = [int(x) for x in a.merge_at.split(",")]
    done = set()

    def gh(*args):
        r = subprocess.run(["gh", *args], text=True, capture_output=True, timeout=120)
        return r.returncode, r.stdout, r.stderr[-300:]

    restarted = False
    while now() - start < a.seconds:
        el = now() - start
        for m in merges:
            if el >= m and m not in done:
                done.add(m)
                code, o, err = gh("pr", "list", "-R", a.repo, "--state", "open", "--limit", "200",
                                  "--json", "number,files,mergeable")
                cands = [p for p in json.loads(o or "[]")
                         if any(f["path"] == "conflict.txt" for f in p["files"])
                         and p["mergeable"] == "MERGEABLE"]
                if cands:
                    n = min(p["number"] for p in cands)
                    c, _, e = gh("pr", "merge", str(n), "-R", a.repo, "--squash")
                    out.ev(kind="merge", at=m, pr=n, code=c, err=e if c else None)
                else:
                    out.ev(kind="merge", at=m, pr=None, err="no mergeable conflict.txt PR")
        if not restarted and el >= a.restart_at:
            restarted = True
            t = now()
            r = subprocess.run(["ssh", "-o", "ConnectTimeout=10", "nuc-gpu",
                                "wsl.exe -d Ubuntu-24.04 -u ops -- bash /opt/soak/kill-authority.sh"],
                               text=True, capture_output=True, timeout=60)
            old = r.stdout.strip()
            back = None
            for _ in range(600):
                try:
                    import urllib.request
                    urllib.request.urlopen(a.url + "/v1/forks", timeout=1)
                    back = now() - t; break
                except Exception as e:  # noqa: BLE001
                    if getattr(e, "code", None) in (401, 403, 404):
                        back = now() - t; break
                time.sleep(0.1)
            out.ev(kind="restart", signal="SIGKILL", old_pid=old, killed_utc=utc(t),
                   seconds_until_answering=back, err=r.stderr[-300:] or None)
        time.sleep(1)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    sub = p.add_subparsers(dest="cmd", required=True)
    ag = sub.add_parser("agent")
    ag.add_argument("--side", choices=["mac", "nuc"], required=True)
    ag.add_argument("--names", required=True); ag.add_argument("--root", required=True)
    ag.add_argument("--out", required=True); ag.add_argument("--seconds", type=int, default=3600)
    ag.add_argument("--land-every", type=int, default=30)
    ag.add_argument("--pause-min", type=float, default=8); ag.add_argument("--pause-max", type=float, default=20)
    ag.add_argument("--req"); ag.add_argument("--res")
    w = sub.add_parser("watch")
    w.add_argument("--req", required=True); w.add_argument("--res", required=True)
    w.add_argument("--volume", default="/srv/cuenca/volume"); w.add_argument("--seconds", type=int, default=4200)
    c = sub.add_parser("chaos")
    c.add_argument("--out", required=True); c.add_argument("--seconds", type=int, default=3600)
    c.add_argument("--restart-at", type=int, default=1800); c.add_argument("--merge-at", default="900,2100,2700")
    c.add_argument("--repo", default="broomva/cuenca-soak"); c.add_argument("--url", default="http://100.112.149.32:7419")
    a = p.parse_args()
    {"agent": cmd_agent, "watch": cmd_watch, "chaos": cmd_chaos}[a.cmd](a)


if __name__ == "__main__":
    main()
