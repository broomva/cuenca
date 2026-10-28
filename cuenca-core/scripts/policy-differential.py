#!/usr/bin/env python3
"""Differential test: the gate engine's policy reader against p9's (owner ask A15, BRO-3313).

p9 loads `.control/policy.yaml` with PyYAML and decides merges from it; the gate engine
reads the same file with yaml-rust2 (`cuenca_core::gates::Policy::parse`). Wherever the
two parsers read one file differently, the gates could pass a change p9 would hold. This
script generates policies (seeds, then mutations covering YAML's odd corners), feeds each
to both readers and fails on any disagreement.

What is compared, for each input:

- `protected`: the `path_touched` of every `auto_merge.rules` item whose action is
  `require_human`, in order. p9: `_parse_auto_merge`.
- `governance`: `null` when there is no `auto_merge.gates`, else the tier's paths and
  window in seconds. p9: `_parse_gates`.
- `secret_paths`: the patterns of `gates.hard[id: G4]` and `cuenca.gates.secret_paths`.
  p9 does not read these; the reference is a plain PyYAML `safe_load` projection
  ([`pyyaml_secret_paths`]), so the comparison is still against PyYAML's reading.
- whether the file is refused at all ("the merge rule": p9 refuses to merge on a policy
  it cannot parse).

Agreement:

- both accept and every projection is equal; or
- the engine refuses. A policy the engine cannot compile is a failing `policy` gate
  result and never makes the gates pass (`gates/mod.rs`, `policy_error`), so an engine
  refusal is fail-closed whatever p9 says. These are counted and reported.
- p9 refusing a policy the engine accepts is a disagreement: p9 would merge nothing on
  it while the gates would run on what the engine read.

p9's own blocks (`ci_watch`, `ci_heal`, the top level being a mapping) feed no gate and
are not compared: an empty file is p9's error and the engine's defaults, the same
defaults as a location without a policy.

Usage: policy-differential.py --engine BINARY [--p9 P9_PY] [--cases N] [--seed S]
       [--policy FILE ...] [--keep DIR]
Without --p9, the pinned p9 is downloaded from broomva/skills and its hash checked (CI).
Exit 0 when every input agrees, 1 on a disagreement (each is printed), 2 on a usage or
setup error.
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
import random
import subprocess
import sys
import tempfile
import time
from pathlib import Path

# The p9 the comparison is pinned to (broomva/skills): CI downloads this commit's file
# and checks the hash; a local run against another p9 says so.
P9_COMMIT = "ab15e996c4a608cf01ffb096eb4d4645865db83d"
P9_SHA256 = "3b0f80a53e66d4f3a4b64a03453adcdc2845e163eca00260806870f667e9d873"

# ─────────────────────────────────────────────────────────────────────────────
# The two readers
# ─────────────────────────────────────────────────────────────────────────────


def fetch_p9(dest: Path) -> Path:
    import urllib.request

    url = (f"https://raw.githubusercontent.com/broomva/skills/{P9_COMMIT}"
           "/skills/orchestration/p9/scripts/p9.py")
    for attempt in range(3):
        try:
            with urllib.request.urlopen(url, timeout=60) as r:
                body = r.read()
            break
        except OSError:
            if attempt == 2:
                raise
            time.sleep(5 * (attempt + 1))
    if hashlib.sha256(body).hexdigest() != P9_SHA256:
        raise SystemExit(f"{url} does not hash to the pinned {P9_SHA256}")
    dest.write_bytes(body)
    return dest


def load_p9(path: Path):
    spec = importlib.util.spec_from_file_location("p9_under_test", path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module  # its dataclasses look their module up
    spec.loader.exec_module(module)
    import yaml  # noqa: F401  (p9 falls back to a subset parser without PyYAML)

    if module._yaml_loader() is not yaml.safe_load:
        raise SystemExit("p9 is not loading with PyYAML's safe_load")
    return module


SAMPLE_PATHS = (
    "CLAUDE.md", "claude.md", "x/CLAUDE.md", "AGENTS.md", "METALAYER.md",
    ".control/policy.yaml", ".control/rcs-parameters.toml", "docs/charter.md",
    "docs/ADR-001.md", "docs/a.md", "docs/ab.md", "docs/İ.md", "docs/ſ.md",
    "0", "-", "a", "b", "z", "[", "]", "İ", "ſ",
)


def non_string_key(node) -> str | None:
    seen: set[int] = set()
    stack = [node]
    while stack:
        cur = stack.pop()
        if id(cur) in seen:
            continue
        seen.add(id(cur))
        if isinstance(cur, dict):
            for k, v in cur.items():
                if not isinstance(k, str):
                    return f"a non-string mapping key {k!r}"
                stack.append(v)
        elif isinstance(cur, list):
            stack.extend(cur)
    return None


def pyyaml_secret_paths(data) -> list[str] | str:
    """`gates.hard[id: G4].pattern` and `cuenca.gates.secret_paths` as PyYAML reads
    them, or a string saying why the shape is not one the engine may accept."""
    bad_key = non_string_key(data)
    if bad_key is not None:
        return bad_key
    out: list[str] = []
    gates = data.get("gates")
    if gates is not None:
        if not isinstance(gates, dict):
            return "gates is not a mapping"
        hard = gates.get("hard")
        if hard is not None:
            if not isinstance(hard, list):
                return "gates.hard is not a list"
            for g in hard:
                if not isinstance(g, dict):
                    return "a gates.hard item is not a mapping"
                if g.get("id") == "G4" and g.get("pattern") is not None:
                    if not isinstance(g["pattern"], str):
                        return "a G4 pattern is not a string"
                    out.append(g["pattern"])
    cuenca = data.get("cuenca")
    if cuenca is not None:
        if not isinstance(cuenca, dict):
            return "cuenca is not a mapping"
        cg = cuenca.get("gates")
        if cg is not None:
            if not isinstance(cg, dict):
                return "cuenca.gates is not a mapping"
            sp = cg.get("secret_paths")
            if sp is not None:
                if not isinstance(sp, list) or not all(isinstance(s, str) for s in sp):
                    return "cuenca.gates.secret_paths is not a list of strings"
                out.extend(sp)
    return out


def p9_projection(p9, text: str) -> dict:
    import yaml

    try:
        data = yaml.safe_load(text)
    except Exception as e:  # p9: "policy.yaml malformed"
        return {"ok": False, "error": f"PyYAML: {type(e).__name__}"}
    if data is None:
        data = {}
    if not isinstance(data, dict):
        return {"ok": False, "error": "p9: not a mapping"}
    try:
        am = p9._parse_auto_merge(data.get("auto_merge"))
    except p9.PolicyError as e:
        return {"ok": False, "error": f"p9: {e}"}
    except Exception as e:  # a crash in p9 is a refusal too
        return {"ok": False, "error": f"p9 crashed: {type(e).__name__}: {e}"}
    gov = None
    if am.gates is not None:
        g = am.gates.governance
        gov = {
            "paths": list(g.paths),
            "matches": [s for s in SAMPLE_PATHS if p9._path_matches(s, g.paths)],
            "window_seconds": g.l3_window_seconds,
        }
    return {
        "ok": True,
        "protected": [r.path_touched for r in am.rules
                      if r.action == "require_human" and r.path_touched],
        "governance": gov,
        "secret_paths": pyyaml_secret_paths(data),
    }


def engine_projections(binary: Path, files: list[Path]) -> list[dict]:
    out: list[dict] = []
    for i in range(0, len(files), 500):
        chunk = files[i:i + 500]
        run = subprocess.run([str(binary), *map(str, chunk)], capture_output=True,
                             text=True, timeout=600)
        if run.returncode != 0:
            raise SystemExit(f"engine exited {run.returncode}: {run.stderr[-2000:]}")
        lines = run.stdout.split("\n")[:-1]  # splitlines() also breaks at U+2028
        if len(lines) != len(chunk):
            raise SystemExit(f"engine printed {len(lines)} lines for {len(chunk)} files")
        out.extend(json.loads(line) for line in lines)
    return out


def compare(engine: dict, p9: dict) -> str | None:
    """None when the two agree, else why not."""
    if not engine["ok"]:
        return None  # fail-closed (module docstring)
    if not p9["ok"]:
        return f"engine accepts, p9 refuses ({p9['error']})"
    if isinstance(p9["secret_paths"], str):
        return f"engine accepts, PyYAML reads {p9['secret_paths']}"
    for key in ("protected", "governance", "secret_paths"):
        if engine[key] != p9[key]:
            return f"{key}: engine {engine[key]!r} != p9 {p9[key]!r}"
    return None


# ─────────────────────────────────────────────────────────────────────────────
# Inputs
# ─────────────────────────────────────────────────────────────────────────────

SEEDS = [
    # Gated mode, the shape of the workspace's policy.
    """version: "1.0"
ci_watch:
  enabled: true
ci_heal:
  enabled: true
gates:
  hard:
    - id: G1
      rule: no force push
    - id: G4
      pattern: '\\.(pem|key)$'
      rule: no secrets
auto_merge:
  enabled: true
  merge_method: squash
  delete_branch: true
  default_action: notify
  rules:
    - path_touched: .control/policy.yaml
      action: require_human
    - path_touched: /CLAUDE.md
      action: require_human
    - branch_pattern: "docs/*"
      action: notify
    - path_touched: scripts/hook.sh
      action: require_human
  gates:
    review_bots: [coderabbitai]
    p20:
      pass_score: 7
      max_loc: 200
    governance:
      paths: [CLAUDE.md, AGENTS.md, METALAYER.md, .control/policy.yaml, .control/rcs-parameters.toml, docs/charter.md]
      l3_window_seconds: 172800
cuenca:
  gates:
    secret_paths: ['(?i)id_rsa$']
""",
    # Legacy mode: no gates, an auto rule.
    """auto_merge:
  enabled: false
  rules:
    - branch_pattern: "chore/*"
      action: auto
    - path_touched: Cargo.lock
      action: require_human
  default_action: notify
""",
    # Gates with their defaults.
    """auto_merge:
  enabled: true
  rules:
    - path_touched: deploy/
      action: require_human
  gates:
    governance:
""",
    """auto_merge:
  gates:
""",
    # Only the cuenca section.
    """cuenca:
  gates:
    max_file_bytes: 1048576
    secret_paths: ['\\.p12$', 'secrets/']
""",
    # A byte-order mark, which p9 reads past.
    "\ufeffauto_merge:\n  rules:\n    - path_touched: CLAUDE.md\n      action: require_human\n",
    # Flow style throughout.
    """{auto_merge: {enabled: true, rules: [{path_touched: a/b, action: require_human}, {branch_pattern: x, action: notify}], gates: {governance: {l3_window_seconds: 86400}}}, gates: {hard: [{id: G4, pattern: 'x$'}]}}
""",
]

# Hand-picked inputs, each one a YAML corner a reader has been seen to differ on.
FIXED = [
    "\ufeffauto_merge:\n  rules:\n    - path_touched: CLAUDE.md\n      action: require_human\n",
    "auto_merge:\n  rules:\n    - path_touched: a\n      action: require_human\u2028    - path_touched: CLAUDE.md\n      action: require_human\n",
    "auto_merge:\n  rules:\n    - path_touched: a\n      action: require_human\u0085    - path_touched: b\n",
    "auto_merge:\n  rules:\n    - path_touched: a\r      action: require_human\r",
    "auto_merge:\r\n  rules:\r\n    - path_touched: a\r\n      action: require_human\r\n",
    "base: &b {path_touched: CLAUDE.md, action: require_human}\nauto_merge:\n  rules: [*b]\n",
    "auto_merge:\n  <<: {rules: [{path_touched: CLAUDE.md, action: require_human}]}\n",
    "auto_merge:\n  rules:\n    - <<: {path_touched: CLAUDE.md}\n      action: require_human\n",
    "auto_merge:\n  !!merge <<: {rules: [{path_touched: x, action: require_human}]}\n",
    # A merge key in a mapping whose other keys the engine allows (the top level, `gates`).
    "<<: {auto_merge: {rules: [{path_touched: CLAUDE.md, action: require_human}]}}\n",
    "<<:\n  auto_merge:\n    gates:\n",
    "gates:\n  <<: {hard: [{id: G4, pattern: 'x$'}]}\n",
    "base: &b {auto_merge: {rules: [{path_touched: CLAUDE.md, action: require_human}]}}\n<<: *b\n",
    "<<: [{cuenca: {gates: {secret_paths: ['k$']}}}]\n",
    "auto_merge:\n  rules:\n    - path_touched: !!str 123\n      action: require_human\n",
    "auto_merge:\n  rules:\n    - path_touched: on\n      action: require_human\n",
    "auto_merge:\n  rules:\n    - path_touched: 0o17\n      action: require_human\n",
    "auto_merge:\n  rules:\n    - path_touched: 2026-10-24\n      action: require_human\n",
    "auto_merge:\n  rules:\n    - path_touched: 1_000\n      action: require_human\n",
    "auto_merge:\n  rules:\n    - path_touched: 12:30\n      action: require_human\n",
    "auto_merge:\n  rules:\n    - path_touched: .inf\n      action: require_human\n",
    "auto_merge:\n  rules:\n    - path_touched: ~\n      action: require_human\n",
    "auto_merge:\n  rules:\n    - path_touched: a\n      path_touched: b\n      action: require_human\n",
    "auto_merge:\n  rules: []\nauto_merge:\n  rules:\n    - path_touched: b\n      action: require_human\n",
    "auto_merge:\n  enabled: yes\n",
    "auto_merge:\n  enabled: true\n  default_action: auto\n  gates:\n",
    "auto_merge:\n  rules:\n    - branch_pattern: x\n      action: auto\n  gates:\n",
    "auto_merge:\n  merge_method: fast\n",
    "auto_merge:\n  gates:\n    p20:\n      pass_score: 3\n",
    "auto_merge:\n  gates:\n    governance:\n      required_strata: [A]\n",
    "auto_merge:\n  gates:\n    governance:\n      l3_window_seconds: 86400.0\n",
    "auto_merge:\n  gates:\n    governance:\n      l3_window_seconds: 0x15180\n",
    "auto_merge:\n  gates:\n    governance:\n      l3_window_seconds: 24:00:00\n",
    "auto_merge:\n  gates:\n    governance:\n      l3_window_seconds: true\n",
    "auto_merge:\n  rules:\n    - path_touched: |\n        CLAUDE.md\n      action: require_human\n",
    "auto_merge:\n  rules:\n    - path_touched: >-\n        CLAUDE.md\n      action: require_human\n",
    "auto_merge:\n  rules:\n    - path_touched: \"CLAUDE\\u2028md\"\n      action: require_human\n",
    "auto_merge:\n  rules:\n    - path_touched: \"a\\tb\"\n      action: require_human\n",
    "auto_merge:\n  rules:\n    - ? path_touched\n      : CLAUDE.md\n      action: require_human\n",
    "\"auto_merge\":\n  'rules':\n    - \"path_touched\": CLAUDE.md\n      action: require_human\n",
    "auto_merge:\n\trules:\n",
    "auto_merge:\n  rules:\n  - path_touched: a\n    action: require_human\n",
    "---\nauto_merge:\n  rules:\n    - path_touched: a\n      action: require_human\n...\n",
    "---\n---\nauto_merge: {}\n",
    "%YAML 1.1\n---\nauto_merge:\n  enabled: true\n",
    "%TAG !e! tag:example.com,2000:\n---\nauto_merge: !e!x {}\n",
    "auto_merge: !!map {rules: [{path_touched: x, action: require_human}]}\n",
    "auto_merge:\n  rules: !!seq\n    - path_touched: x\n      action: require_human\n",
    "!!null auto_merge:\n  rules: []\n",
    "auto_merge: ~\n",
    "auto_merge:\n",
    "auto_merge: []\n",
    "gates:\n  hard:\n    - id: G4\n      pattern: on\n",
    "gates:\n  hard:\n    - {id: G4, pattern: 'a$'}\n    - {id: G4, pattern: 'b$'}\n",
    "gates:\n  hard:\n    - id: \"G4\"\n      pattern: 'a$'\n    - id: !!str G4\n      pattern: 'c$'\n",
    "cuenca:\n  gates:\n    secret_paths: [yes, '\\.x$']\n",
    "cuenca:\n  gates:\n    secret_paths: '\\.x$'\n",
    "",
    "# only a comment\n",
    "auto_merge: {rules: [{path_touched: a#b, action: require_human}]}\n",
    "auto_merge:\n  rules:\n    - path_touched: a #b\n      action: require_human\n",
    "auto_merge:\n  rules:\n    - path_touched: a\u00a0\n      action: require_human\n",
    "auto_merge:\n  rules:\n    - path_touched: 'it''s'\n      action: require_human\n",
    "auto_merge:\n  rules:\n    - path_touched: a\n      action: require_human\n\x00",
    "auto_merge:\n  rules:\n    - path_touched: a\x7f\n      action: require_human\n",
    "auto_merge:\n  rules:\n    - path_touched: a\x9b\n      action: require_human\n",
    "auto_merge:\n  rules:\n    - path_touched: a\ufeff\n      action: require_human\n",
    "auto_merge:\n  rules:\n    - path_touched: a\n      action: require_human\n\ufeffcuenca: {}\n",
    "auto_merge:\n  rules:\n    - path_touched: \"\\x41\"\n      action: require_human\n",
    "auto_merge:\n  rules:\n    - path_touched: \"\\N\"\n      action: require_human\n",
    # Round 9, B: U+2028/U+2029 as the only line breaks of a rule.
    *(f"note: x{sep}auto_merge:{sep}  rules:{sep}  - path_touched:{sep}      CLAUDE.md{sep}    action:{sep}"
      f"      require_human\n" for sep in ("\u2028", "\u2029")),
    "auto_merge:\n  rules: []\n# \ufffe\n",
    "version: a\uffffb\n",
    # Empty or missing names p9 refuses.
    "auto_merge:\n  gates:\n    governance:\n      required_checks: [ci]\n",
    "auto_merge:\n  gates:\n    review_bots: ['']\n",
    "auto_merge:\n  gates:\n    aggregate_checks: ['']\n",
    "auto_merge:\n  gates:\n    p20: {public_api_paths: ['']}\n",
    # Round 10, A, B and C: flow-context plain scalars PyYAML ends or refuses.
    "auto_merge:\n  gates:\n    governance:\n      paths: [CLAUDE.md, docs/ADR-00?.md]\n",
    "cuenca:\n  gates:\n    secret_paths: ['x', \\.pem?$]\n",
    "gates:\n  hard: [{id: G4, pattern: k?y$}]\n",
    "auto_merge:\n  rules: [{path_touched: a-b?c, action: require_human}]\n",
    "auto_merge:\n  rules: [{path_touched: |\n CLAUDE.md, action: require_human}]\n",
    "auto_merge:\n  rules: [{path_touched: >x, action: require_human}]\n",
    "auto_merge:\n  rules: [{path_touched: :x, action: require_human}]\n",
    "auto_merge:\n  rules: [{path_touched\n : CLAUDE.md, action: require_human}]\n",
    "permissions: {k\n: v}\nauto_merge: {rules: [{path_touched: x, action: require_human}]}\n",
    "auto_merge:\n  rules: [{path_touched: x, action: require_human}, {path_touched\n: y, action: notify}]\n",
    "auto_merge:\n  rules: [{path_touched: CLAUDE\n   .md, action: require_human}]\n",
    "auto_merge:\n  rules: [{path_touched: a b, action: require_human}]\n",
    "auto_merge:\n  rules: [{path_touched: -x, action: require_human}]\n",
    "auto_merge:\n  rules:\n    - path_touched: a?b\n      action: require_human\n",
    # Round 10, A: a non-scalar key, %YAML other than 1.x, a repeated %TAG, odd anchor names.
    "permissions: {[a]: b}\nauto_merge: {rules: [{path_touched: x, action: require_human}]}\n",
    "permissions:\n  ? {a: b}\n  : c\nauto_merge: {rules: [{path_touched: x, action: require_human}]}\n",
    "%YAML 2.0\n---\nauto_merge: {rules: [{path_touched: x, action: require_human}]}\n",
    "%YAML 0.9\n---\nauto_merge: {rules: [{path_touched: x, action: require_human}]}\n",
    "%TAG !e! tag:a,2000:\n%TAG !e! tag:b,2000:\n---\nauto_merge: {rules: [{path_touched: x, action: require_human}]}\n",
    "auto_merge: &a.b {rules: [{path_touched: x, action: require_human}]}\n",
    "auto_merge: &a:b {rules: [{path_touched: x, action: require_human}]}\n",
    "auto_merge: &a/b {rules: [{path_touched: x, action: require_human}]}\n",
    "auto_merge: &a%b {rules: [{path_touched: x, action: require_human}]}\n",
    "auto_merge: &a {rules: [{path_touched: x, action: require_human}]}\n",
    # Round 10, A: the shape two mutants together let through as engine [] vs p9 ['x'].
    "# c auto_merge: {rules: [{path_touched: x, action: require_human}]}\n",
    "note: x\u2028# c auto_merge: {rules: [{path_touched: x, action: require_human}]}\n",
    # Round 11, A: non-string mapping keys PyYAML folds to the same value in open mappings,
    # and character-class globs with a leading `-`, trailing `-`, or chained ranges.
    "permissions:\n  1: first\n  true: second\nauto_merge: {rules: [{path_touched: x, action: require_human}]}\n",
    "permissions:\n  0: first\n  false: second\nauto_merge: {rules: [{path_touched: x, action: require_human}]}\n",
    "permissions:\n  1.0: first\n  1: second\nauto_merge: {rules: [{path_touched: x, action: require_human}]}\n",
    "permissions:\n  ? \n  : first\n  ~: second\nauto_merge: {rules: [{path_touched: x, action: require_human}]}\n",
    "auto_merge:\n  gates:\n    governance:\n      paths: [CLAUDE.md, AGENTS.md, METALAYER.md, .control/policy.yaml, .control/rcs-parameters.toml, '[--a]', '[-a]', '[a-]', '[a-c-e]']\n",
    "auto_merge:\n  gates:\n    governance:\n      paths: [CLAUDE.md, AGENTS.md, METALAYER.md, .control/policy.yaml, .control/rcs-parameters.toml, '[Z-a]']\n",
    # Round 12: Unicode case-folding parity vs Python .lower()
    "auto_merge:\n  gates:\n    governance:\n      paths: [CLAUDE.md, AGENTS.md, METALAYER.md, .control/policy.yaml, .control/rcs-parameters.toml, 'doc[!ſ]/*']\n",
    "auto_merge:\n  gates:\n    governance:\n      paths: [CLAUDE.md, AGENTS.md, METALAYER.md, .control/policy.yaml, .control/rcs-parameters.toml, 'docs/??.md']\n",
    "\ufeffauto_merge:\n  \"rules\":\n    - path_touched: |\n",
]

ODD_SCALARS = [
    "yes", "no", "on", "off", "y", "n", "Y", "N", "True", "TRUE", "false", "~", "null",
    "Null", "NULL", "''", '""', "0", "017", "0o17", "0x1F", "0b101", "1_000", "86_400",
    "24:00:00", "1:30", "1e3", "1.5e3", "1.0", ".5", "+1", "-1", ".inf", "-.Inf", ".nan",
    "2026-10-24", "2026-10-24 10:00:00", "=", "<<", "!", "&x", "*x", "@x", "`x`",
    "'quoted'", '"dq"', "a: b", "[a]", "{a: b}", "- a", "#x", "a #b", "a#b", "|", ">",
    "%x", "?", "? x", ":x", "x:", "\\u2028", '"\\u2028"', '"\\x85"', '"\\L"', '"\\P"',
    '"\\N"', '"\\_"', '"\\e"', '"a\\\nb"', "CLAUDE.md", "/CLAUDE.md", "86400", "172800",
    "2592000", "2592001", "86399", "1", "require_human", "notify", "auto",
    "!!str x", "!!int 86400", "!!bool yes", "!!float 1", "!!binary aGk=", "!!timestamp 2026-10-24",
    "!custom x", "!!python/str x", "!<tag:yaml.org,2002:str> x",
]
ODD_BREAKS = ["\u2028", "\u2029", "\u0085", "\r", "\r\n", "\v", "\f", "\x1c", "\x1d", "\x1e"]
ODD_CHARS = ["\x00", "\x01", "\x07", "\x08", "\x1b", "\x7f", "\x80", "\x9f", "\u00a0",
             "\u200b", "\u200e", "\ufeff", "\ufffe", "\t", " "]
ODD_TAGS = ["!!str ", "!!merge ", "!!null ", "!!map ", "!!seq ", "!!set ", "!!omap ",
            "!x ", "! ", "!!python/object ", "!<tag:yaml.org,2002:str> "]


def mutate(text: str, rng: random.Random) -> str:
    lines = text.split("\n")
    kv = [i for i, l in enumerate(lines) if ":" in l and not l.lstrip().startswith("#")]
    op = min(rng.randrange(17), 14)  # the flow op (14) three times as often: r10 found its class
    if op == 0 and kv:  # an odd scalar as a value
        i = rng.choice(kv)
        key, _, _ = lines[i].partition(":")
        lines[i] = f"{key}: {rng.choice(ODD_SCALARS)}"
    elif op == 1 and kv:  # a tag before a value or a key
        i = rng.choice(kv)
        key, _, value = lines[i].partition(":")
        tag = rng.choice(ODD_TAGS)
        if rng.random() < 0.5:
            lines[i] = f"{key}: {tag}{value.strip()}"
        else:
            indent = len(key) - len(key.lstrip(" -"))
            lines[i] = f"{key[:indent]}{tag}{key[indent:]}:{value}"
    elif op == 2:  # an odd line break
        i = rng.randrange(len(lines))
        if i < len(lines) - 1:
            lines[i] = lines[i] + rng.choice(ODD_BREAKS) + lines.pop(i + 1)
    elif op == 3:  # an odd character anywhere
        i = rng.randrange(len(lines))
        at = rng.randint(0, len(lines[i]))
        lines[i] = lines[i][:at] + rng.choice(ODD_CHARS) + lines[i][at:]
    elif op == 4 and kv:  # a duplicated key, with another value
        i = rng.choice(kv)
        key, _, _ = lines[i].partition(":")
        dup = key.replace("- ", "  ", 1)
        lines.insert(i + 1, f"{dup}: {rng.choice(ODD_SCALARS)}")
    elif op == 5 and kv:  # an anchor and an alias
        i, j = rng.choice(kv), rng.choice(kv)
        key, _, value = lines[i].partition(":")
        if value.strip():
            lines[i] = f"{key}: &a {value.strip()}"
            k2, _, _ = lines[j].partition(":")
            lines[j] = f"{k2}: *a"
    elif op == 6 and kv:  # a merge key
        i = rng.choice(kv)
        indent = len(lines[i]) - len(lines[i].lstrip(" "))
        lines.insert(i, " " * indent + rng.choice([
            "<<: {path_touched: CLAUDE.md}", "<<: {action: require_human}",
            "<<: {rules: [{path_touched: CLAUDE.md, action: require_human}]}",
            "<<: {}", "<<: []", "? <<\n" + " " * indent + ": {}", "'<<': {}",
        ]))
    elif op == 7:  # indentation: a tab, or one space more or less
        i = rng.randrange(len(lines))
        if lines[i].startswith("  "):
            lines[i] = rng.choice(["\t" + lines[i][2:], " " + lines[i], lines[i][1:]])
    elif op == 8:  # line endings
        sep = rng.choice(["\r\n", "\r", "\n\n", " \n", "\t\n"])
        return sep.join(lines)
    elif op == 9:  # a byte-order mark
        if rng.random() < 0.5:
            return "\ufeff" + text
        i = rng.randrange(len(lines))
        lines[i] = "\ufeff" + lines[i]
    elif op == 10 and kv:  # a quoted key or value
        i = rng.choice(kv)
        key, _, value = lines[i].partition(":")
        indent = len(key) - len(key.lstrip(" -"))
        q = rng.choice(["'", '"'])
        if rng.random() < 0.5:
            lines[i] = f"{key[:indent]}{q}{key[indent:]}{q}:{value}"
        elif value.strip():
            lines[i] = f"{key}: {q}{value.strip()}{q}"
    elif op == 11:  # documents
        return rng.choice(["---\n" + text, text + "\n...\n", text + "\n---\n",
                           "%YAML 1.1\n---\n" + text, "--- |\n  x\n" + text])
    elif op == 12 and kv:  # a block scalar
        i = rng.choice(kv)
        key, _, value = lines[i].partition(":")
        indent = len(key) - len(key.lstrip(" -")) + 2
        if value.strip():
            lines[i] = f"{key}: {rng.choice(['|', '>', '|-', '>-', '|+', '|2'])}"
            lines.insert(i + 1, " " * indent + value.strip())
    elif op == 13:  # a comment
        i = rng.randrange(len(lines))
        lines[i] = lines[i] + rng.choice([" # x", "#x", " #", " # : y"])
    elif op == 14 and kv:  # a flow collection, with an odd plain scalar or a split key
        i = rng.choice(kv)
        key, _, value = lines[i].partition(":")
        word = value.strip() or "x"
        odd = rng.choice(FLOW_ODD).replace("W", word)
        lines[i] = f"{key}: " + rng.choice([f"[{odd}]", f"[a, {odd}]", f"{{k: {odd}}}", f"[{{k: {odd}}}]"])
    return "\n".join(lines)


# How a plain scalar in a flow collection can read differently: W is the original value.
FLOW_ODD = ["W?", "?W", "W?x", "|W", ">W", ":W", "W:x", "-W", "W x", "W\n  x", "W\n  : y",
            "W #c", "W#c", "!W", "%W", "@W", "`W`", "&a W", "*W", "W,", "W]", "W}", "'W'",
            '"W"', "W\u2028x", "? W"]


def generate(cases: int, seed: int, extra: list[str]) -> list[str]:
    rng = random.Random(seed)
    seeds = SEEDS + extra
    out = list(FIXED) + seeds
    while len(out) < cases:
        text = rng.choice(seeds)
        for _ in range(rng.randint(1, 3)):
            text = mutate(text, rng)
        out.append(text)
    return out


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--p9", type=Path, help="default: the pinned p9, downloaded")
    ap.add_argument("--engine", type=Path, required=True)
    ap.add_argument("--cases", type=int, default=5000)
    ap.add_argument("--seed", type=int, default=20261024)
    ap.add_argument("--policy", type=Path, action="append", default=[],
                    help="another seed policy (e.g. a workspace's own)")
    ap.add_argument("--show", type=int, default=40, help="print at most this many")
    ap.add_argument("--keep", type=Path, help="write the inputs here and keep them")
    args = ap.parse_args()

    engine_samples = tuple(
        json.loads(subprocess.check_output([str(args.engine), "--sample-paths"]))
    )
    if SAMPLE_PATHS != engine_samples:
        raise SystemExit(
            f"SAMPLE_PATHS divergence: Python has {SAMPLE_PATHS}, engine has {engine_samples}"
        )

    if args.p9 is None:
        args.p9 = fetch_p9(Path(tempfile.mkdtemp()) / "p9.py")
    digest = hashlib.sha256(args.p9.read_bytes()).hexdigest()
    pinned = digest == P9_SHA256
    p9 = load_p9(args.p9)
    import yaml

    extra = [p.read_text(encoding="utf-8") for p in args.policy]
    inputs = generate(args.cases, args.seed, extra)

    with tempfile.TemporaryDirectory() as tmp:
        root = args.keep or Path(tmp)
        root.mkdir(parents=True, exist_ok=True)
        files = []
        for n, text in enumerate(inputs):
            f = root / f"case-{n:05}.yaml"
            f.write_bytes(text.encode("utf-8", "surrogatepass"))
            files.append(f)
        engine = engine_projections(args.engine, files)

    # A floor, so an engine that refuses everything cannot pass: every seed policy and
    # every --policy file must be read alike by both, not merely refused by the engine.
    floor = {text for text in SEEDS + extra}
    disagreements = 0
    counts = {"both accept": 0, "engine refuses, p9 accepts": 0, "both refuse": 0}
    for n, (text, e) in enumerate(zip(inputs, engine)):
        p = p9_projection(p9, text)
        why = compare(e, p)
        if why is None and text in floor and not (e["ok"] and p["ok"]):
            why = f"a seed policy is refused (engine: {e.get('error')}; p9: {p.get('error')})"
        if why is not None:
            disagreements += 1
            if disagreements <= args.show:
                print(f"DISAGREE case {n}: {why}\n  input: {text!r}\n  engine: {e}\n")
        elif e["ok"]:
            counts["both accept"] += 1
        else:
            counts["both refuse" if not p["ok"] else "engine refuses, p9 accepts"] += 1

    print(f"p9 {'pinned ' + P9_COMMIT[:12] if pinned else 'NOT the pinned p9: sha256 ' + digest}"
          f" · PyYAML {yaml.__version__} · {len(inputs)} inputs (seed {args.seed})")
    print(" · ".join(f"{k} {v}" for k, v in counts.items()) + f" · disagree {disagreements}")
    return 1 if disagreements else 0


if __name__ == "__main__":
    sys.exit(main())
