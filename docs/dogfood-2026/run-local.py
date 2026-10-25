#!/usr/bin/env python3
"""BRO-3259 bounded local receipt driver. Never merges or reads private locations.

Run from the Cuenca worktree with --scratch on the SSD. This intentionally uses
the tests' loopback identity fixture, not real Tailscale authentication. The
optional --github flag creates ONE real docs PR via Cuenca's existing lander.
"""
import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import selectors
import shutil
import socket
import subprocess
import threading
import time


def utc():
    return subprocess.check_output(["date", "-u", "+%Y-%m-%dT%H:%M:%SZ"], text=True).strip()


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


class Trial:
    def __init__(self, args):
        self.args = args
        self.root = args.scratch.resolve()
        self.root.mkdir(parents=True, exist_ok=False)
        self.root.chmod(0o700)
        self.out = args.output.resolve()
        self.out.mkdir(parents=True, exist_ok=True)
        self.repo = Path.cwd().resolve()
        self.bin = str(args.binary.resolve())
        self.lock = threading.Lock()
        self.commands = open(self.out / "commands.jsonl", "w")
        self.rpc = open(self.out / "mcp.jsonl", "w")
        self.events = []
        self.child = None
        self.mcp = None
        self.metrics = {"start_utc": utc(), "source_sha": self.git("rev-parse", "origin/main"),
                        "identity": "loopback test fixture; no real tailnet authentication",
                        "github_enabled": args.github, "host": self.cmd(["uname", "-sm"])[1].strip()}
        self.env = os.environ.copy()
        self.env.update(CUENCA_CONFIG=str(self.root / "config.json"),
                        CUENCA_STATE_HOME=str(self.root / "client-state"),
                        TMPDIR=str(self.root / "tmp"))
        (self.root / "tmp").mkdir()
        self.fixture = self.root / "tailscale-fixture"
        self.fixture.write_text('#!/bin/sh\ncase "$1" in\n'
                                'whois) echo \'{"Node":{"StableID":"dogfood-local-node"}}\' ;;\n'
                                'status) echo \'{"Self":{"ID":"dogfood-local-node"}}\' ;;\n'
                                '*) exit 1 ;;\nesac\n')
        self.fixture.chmod(0o700)
        with socket.socket() as s:
            s.bind(("127.0.0.1", 0))
            self.port = s.getsockname()[1]
        self.url = f"http://127.0.0.1:{self.port}"
        self.socket = self.root / "admin.sock"

    def cmd(self, argv, env=None, private=False, timeout=120):
        start = time.monotonic()
        r = subprocess.run(list(map(str, argv)), env=env, text=True,
                           capture_output=True, timeout=timeout)
        entry = {"utc": utc(), "argv": list(map(str, argv)), "exit": r.returncode,
                 "seconds": time.monotonic() - start,
                 "stdout": "<redacted local token>" if private else r.stdout,
                 "stderr": r.stderr}
        with self.lock:
            self.commands.write(json.dumps(entry) + "\n")
            self.commands.flush()
        return r.returncode, r.stdout, entry["seconds"]

    def git(self, *args):
        return subprocess.check_output(["git", *args], text=True).strip()

    def cli(self, *args, env=None):
        code, raw, elapsed = self.cmd([self.bin, "--json", *args], env=env or self.env)
        assert code == 0, (args, code, raw)
        return json.loads(raw), elapsed

    def admin(self, *args, private=False):
        code, raw, _ = self.cmd([self.bin, "--json", "admin", "--socket", self.socket, *args],
                                env=self.env, private=private)
        assert code == 0, (args, raw)
        return json.loads(raw)

    def start(self):
        if self.socket.exists():
            self.socket.unlink()
        argv = [self.bin, "serve", "--state", str(self.root / "authority"),
                "--bind", f"127.0.0.1:{self.port}", "--unsafe-bind",
                "--admin-socket", str(self.socket), "--tailscale", str(self.fixture),
                "--volume", str(self.root / "volume")]
        self.child = subprocess.Popen(argv, env=self.env, stdout=subprocess.DEVNULL,
                                      stderr=open(self.out / "serve.log", "a"))
        for _ in range(300):
            assert self.child.poll() is None, "authority exited; inspect serve.log"
            try:
                with socket.create_connection(("127.0.0.1", self.port), timeout=.1):
                    if self.socket.exists():
                        return
            except OSError:
                pass
            time.sleep(.1)
        raise RuntimeError("authority readiness timeout")

    def login(self):
        key = self.admin("key", "mint", "dogfood-mac", "--kind", "agent", "--node",
                         "dogfood-local-node", "--location", "scratch", "--location", "cuenca", private=True)
        config = self.root / "config.json"
        config.write_text(json.dumps({"url": self.url, "token": key["token"]}))
        config.chmod(0o600)

    def send(self, event):
        self.events.append(event)
        self.rpc.write(json.dumps(event) + "\n")
        self.rpc.flush()
        self.mcp.stdin.write(json.dumps(event) + "\n")
        self.mcp.stdin.flush()

    def request(self, method, params):
        ident = len(self.events) + 1
        self.send({"jsonrpc": "2.0", "id": ident, "method": method, "params": params})
        with selectors.DefaultSelector() as selector:
            selector.register(self.mcp.stdout, selectors.EVENT_READ)
            assert selector.select(180), f"MCP timeout: {method}"
        raw = self.mcp.stdout.readline()
        reply = json.loads(raw)
        self.events.append(reply)
        self.rpc.write(raw)
        self.rpc.flush()
        assert reply.get("id") == ident and "result" in reply, reply
        return reply["result"]

    def call(self, name, **args):
        r = self.request("tools/call", {"name": name, "arguments": args})
        assert not r.get("isError"), r
        return r["structuredContent"]

    def workflow(self):
        seed = self.root / "seed"
        (seed / ".cuenca").mkdir(parents=True)
        (seed / "README.md").write_text("# Public scratch for BRO-3259\n")
        (seed / ".cuenca/checks.toml").write_text(
            '[[check]]\nname = "scratch-readme"\nargv = ["/usr/bin/test", "-s", "README.md"]\n')
        self.admin("location", "add", "scratch", "--dir", str(seed))
        remote = ["--remote", "https://github.com/broomva/cuenca.git"] if self.args.github else []
        self.admin("location", "add", "cuenca", "--git", str(self.repo),
                   "--rev", self.metrics["source_sha"], *remote)
        self.login()
        self.mcp = subprocess.Popen([self.bin, "mcp"], env=self.env, stdin=subprocess.PIPE,
                                    stdout=subprocess.PIPE, stderr=open(self.out / "mcp.stderr", "w"), text=True)
        self.request("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                   "clientInfo": {"name": "bro-3259-receipt", "version": "1"}})
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        tools = self.request("tools/list", {})
        assert sorted(t["name"] for t in tools["tools"]) == sorted(
            ["forks", "fork", "checkout", "status", "snapshot", "diff", "log", "verify", "land"])
        self.call("forks")
        self.call("fork", location="cuenca", name="bro-3259-docs")
        co = self.root / "checkout"
        checkout = self.call("checkout", fork="bro-3259-docs", dir=str(co))
        self.metrics["checkout"] = checkout
        # Every content edit in the primary workflow is inside this returned checkout.
        doc = co / "docs/dogfood-local-workflow.md"
        doc.parent.mkdir(exist_ok=True)
        doc.write_text("# Local dogfood evidence boundary\n\n"
                       "A disposable SSD authority can exercise fork, checkout, status, snapshot,\n"
                       "diff, log, verify and land through `cuenca mcp`. The test suite's loopback\n"
                       "identity fixture is useful for this local workflow; it does not establish\n"
                       "real Tailscale authentication or the production permission boundary.\n\n"
                       "A location without a Git remote records a landing request and waits for\n"
                       "the owner (`cuenca land --wait 0` reports `waiting_for: owner`). A GitHub\n"
                       "location exports a branch and opens a PR through the existing lander.\n"
                       "Neither an open PR nor advisory verify is evidence of a gated merge.\n\n"
                       "Keep raw JSON-RPC receipts and the candidate tree id. After an authorized\n"
                       "merge, compare that id with the merged Git tree and retain required-check\n"
                       "results. Production Finder/Files and load acceptance wait on BRO-3461.\n")
        self.call("status", dir=str(co))
        self.call("diff", dir=str(co))
        snap = self.call("snapshot", dir=str(co))
        self.metrics["primary_snapshot"] = snap
        self.call("diff", a="location:cuenca", b="bro-3259-docs")
        self.call("log", dir=str(co))
        self.metrics["primary_verify"] = self.call("verify", dir=str(co))
        self.metrics["primary_land_started_utc"] = utc()
        start = time.monotonic()
        self.metrics["primary_land"] = self.call("land", dir=str(co))
        if self.args.github:
            # An open PR is the finish of this builder's authority, never a merge.
            self.metrics["primary_land"], _ = self.cli("land", "--dir", str(co), "--wait", "120")
        self.metrics["request_to_pr_or_wait_seconds"] = time.monotonic() - start
        self.metrics["primary_log"] = self.call("log", dir=str(co))
        self.call("fork", location="scratch", name="bro-3259-owner-wait")
        waitco = self.root / "owner-wait-checkout"
        self.call("checkout", fork="bro-3259-owner-wait", dir=str(waitco))
        (waitco / "owner-wait.txt").write_text("Harmless unlanded scratch draft.\n")
        self.call("snapshot", dir=str(waitco))
        verify = self.call("verify", dir=str(waitco))
        assert verify["passed"] and len(verify["checks"]) == 1, verify
        self.metrics["scratch_verify"] = verify
        self.metrics["owner_wait"] = self.call("land", dir=str(waitco))
        assert self.metrics["owner_wait"]["waiting_for"] == "owner"
        save(self.out / "workflow.json", self.metrics)

    def visible(self, fork, relative, content, started):
        path = self.root / "volume/forks" / fork / relative
        for _ in range(600):
            try:
                if path.read_bytes() == content:
                    return time.monotonic() - started
            except OSError:
                pass
            time.sleep(.05)
        raise RuntimeError(f"projection timeout: {fork}/{relative}")

    def load(self):
        n = self.args.agents
        def setup(i):
            name = f"load-{i:02d}"
            env = self.env.copy()
            state = self.root / name
            state.mkdir()
            key = self.admin("key", "mint", name, "--kind", "agent", "--node",
                             "dogfood-local-node", "--location", "scratch", private=True)
            config = state / "config.json"
            config.write_text(json.dumps({"url": self.url, "token": key["token"]}))
            config.chmod(0o600)
            env.update(CUENCA_CONFIG=str(config), CUENCA_STATE_HOME=str(state / "client-state"))
            self.cli("fork", "scratch", "--name", name, env=env)
            co = state / "checkout"
            self.cli("checkout", name, str(co), env=env)
            return name, co, env
        started_utc = utc()
        with ThreadPoolExecutor(max_workers=n) as pool:
            agents = list(pool.map(setup, range(n)))
        acknowledged = []
        snapshots, visible, requests = [], [], []
        before_bytes = (self.root / "authority/log.redb").stat().st_size
        free_before = shutil.disk_usage(self.root).free
        start = time.monotonic()
        restarts = []
        rounds = 0
        while time.monotonic() - start < self.args.seconds:
            def publish(agent):
                name, co, env = agent
                relative = "conflict.txt" if int(name[-2:]) % 2 == 0 else f"{name}.txt"
                content = f"{name} round {rounds}\n".encode()
                (co / relative).write_bytes(content)
                t = time.monotonic()
                snap, elapsed = self.cli("snapshot", "--dir", str(co), env=env)
                assert snap["flush"]["pending"] == 0 and len(snap["flush"]["published"]) == 1, snap
                projection = self.visible(name, relative, content, t)
                landed, request_seconds = self.cli("land", "--dir", str(co), "--wait", "0", env=env)
                assert landed["waiting_for"] == "owner", landed
                return {"fork": name, "round": rounds, "published": snap["flush"]["published"],
                        "snapshot_seconds": elapsed, "snapshot_to_local_volume_seconds": projection,
                        "request_to_owner_wait_seconds": request_seconds,
                        "relative": relative, "content_sha256": hashlib.sha256(content).hexdigest()}
            with ThreadPoolExecutor(max_workers=n) as pool:
                batch = list(pool.map(publish, agents))
            acknowledged.extend(batch)
            snapshots.extend(x["snapshot_seconds"] for x in batch)
            visible.extend(x["snapshot_to_local_volume_seconds"] for x in batch)
            requests.extend(x["request_to_owner_wait_seconds"] for x in batch)
            rounds += 1
            save(self.out / "acknowledged.json", acknowledged)
            if not restarts and time.monotonic() - start >= self.args.seconds / 2:
                t = time.monotonic()
                killed_pid = self.child.pid
                self.child.kill()
                self.child.wait(timeout=20)
                self.start()
                restarts.append({"utc": utc(), "signal": "SIGKILL", "old_pid": killed_pid,
                                 "new_pid": self.child.pid, "seconds": time.monotonic() - t,
                                 "boundary": "between completed/acknowledged batches"})
            time.sleep(.2)
        duration = time.monotonic() - start
        audit = []
        for name, co, env in agents:
            log, _ = self.cli("log", name, env=env)
            save(self.out / f"log-{name}.json", log)
            records = [r for r in log["records"] if r["event"]["event"] == "draft_published"]
            # Retain raw ids/offsets; compare ids, not just totals.
            ids = [r["event"]["id"] for r in records]
            expected = [p for a in acknowledged if a["fork"] == name for p in a["published"]]
            expected_ids = [p if isinstance(p, str) else p["id"] for p in expected]
            counts = Counter(ids)
            audit.append({"fork": name, "acknowledged_ids": expected_ids,
                          "journal_ids": ids, "lost": sum(counts[x] == 0 for x in expected_ids),
                          "duplicates": sum(max(0, counts[x] - 1) for x in expected_ids),
                          "unexpected": list((Counter(ids) - Counter(expected_ids)).elements())})
        save(self.out / "journal-audit.json", audit)
        after_bytes = (self.root / "authority/log.redb").stat().st_size
        # Logical event bytes are measured from the actual returned journal objects;
        # redb allocation is separately measured and must not be called logical growth.
        logical_bytes = sum(len(json.dumps(r, separators=(",", ":")).encode()) + 1
                            for name, _, _ in agents
                            for r in json.loads((self.out / f"log-{name}.json").read_text())["records"])
        def p95(values):
            return sorted(values)[max(0, (95 * len(values) + 99) // 100 - 1)]
        rate = max(0, after_bytes - before_bytes) * 3600 / duration
        result = {"start_utc": started_utc, "end_utc": utc(), "agents": n, "rounds": rounds,
                  "requested_seconds": self.args.seconds, "measured_seconds": duration,
                  "scaled": "short local trial, not the required 1h production run",
                  "acknowledged_changesets": len(acknowledged),
                  "lost": sum(a["lost"] for a in audit),
                  "duplicates": sum(a["duplicates"] for a in audit),
                  "restarts": restarts,
                  "p95_snapshot_seconds": p95(snapshots),
                  "p95_snapshot_to_local_volume_seconds": p95(visible),
                  "p95_request_to_owner_wait_seconds": p95(requests),
                  "p95_request_to_pr_open_seconds": None,
                  "pr_open_latency_reason": "load location has no remote; no GitHub PR flood",
                  "log_redb_bytes_before": before_bytes, "log_redb_bytes_after": after_bytes,
                  "redb_allocated_growth_bytes_per_hour_projection": rate,
                  "logical_journal_json_bytes": logical_bytes,
                  "logical_journal_json_bytes_per_hour_projection": logical_bytes * 3600 / duration,
                  "free_bytes_before": free_before, "free_bytes_after": shutil.disk_usage(self.root).free,
                  "projected_days_at_redb_growth": (shutil.disk_usage(self.root).free / rate / 24) if rate else None}
        save(self.out / "load.json", result)
        assert result["lost"] == result["duplicates"] == 0 and len(restarts) == 1, result

    def close(self):
        if self.mcp:
            self.mcp.terminate()
            self.mcp.wait(timeout=10)
        if self.child:
            self.child.terminate()
            self.child.wait(timeout=20)
        self.metrics["end_utc"] = utc()
        save(self.out / "workflow.json", self.metrics)
        self.rpc.close()
        self.commands.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scratch", type=Path, required=True, help="fresh, nonexistent SSD directory")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--github", action="store_true", help="create one real docs PR, never merge")
    parser.add_argument("--agents", type=int, default=30)
    parser.add_argument("--seconds", type=int, default=120)
    args = parser.parse_args()
    assert 1 <= args.agents <= 30 and 10 <= args.seconds <= 900
    t = Trial(args)
    try:
        t.start()
        t.workflow()
        t.load()
    finally:
        t.close()


if __name__ == "__main__":
    main()
