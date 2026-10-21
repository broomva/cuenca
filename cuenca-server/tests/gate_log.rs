//! The gate engine at publish, log-only (BRO-3313, spec section 4 and phase 2): a
//! landing request is judged under the policy of the main it lands onto, never the
//! candidate's; the verdict is journaled with the request; a dry run answers the same
//! verdict; nothing is refused because of it.

mod common;

use std::collections::BTreeSet;

use common::*;
use cuenca_core::gates::{AuthorKind, Evaluation};
use cuenca_core::{ChangesetId, Event, ForkName, LocationName};
use cuenca_server::PrincipalKind::{Agent, OwnerDevice};
use cuenca_tree::{Changeset, ContentHash, Entry, GitOid, Op, RepoPath, Tree};
use serde_json::{Value, json};

const NA: &str = "nAgentA11CNTRL";
const ND: &str = "nDevice11CNTRL";

/// The base policy: the workspace's own keys, trimmed.
const POLICY: &str = r#"version: "1.0"
gates:
  hard:
    - id: G1
      rule: "No force push to main/master"
      pattern: "git push.*--force.*(main|master)"
    - id: G4
      rule: "No committing secrets files"
      pattern: "\\.(env|env\\.local|credentials\\.json|service-account\\.json)$"
auto_merge:
  rules:
    - path_touched: .control/policy.yaml
      action: require_human
    - path_touched: /CLAUDE.md
      action: require_human
    - path_touched: docs/
      action: auto
  gates:
    governance:
      paths: [CLAUDE.md, AGENTS.md, .control/policy.yaml]
      l3_max_per_window: 1
      l3_window_seconds: 86400
cuenca:
  gates:
    max_file_bytes: 4096
"#;

/// Secrets the candidates add, one per built-in pattern family. Written here in pieces
/// so this file does not itself trip a scanner.
fn secrets() -> Vec<String> {
    vec![
        format!("aws = \"{}{}\"\n", "AKIA", "IOSFODNN7EXAMPLE"),
        format!(
            "token: {}{}\n",
            "ghp_", "0123456789abcdefghijABCDEFGHIJ012345"
        ),
        format!("{} RSA PRIVATE KEY-----\nMIIE\n", "-----BEGIN"),
        format!("SLACK={}{}\n", "xoxb-", "1234567890-abcdefghij"),
        format!(
            "ANTHROPIC={}{}\n",
            "sk-ant-", "api03-abcdefghijklmnopqrstuv"
        ),
        format!("stripe {}{}\n", "sk_live_", "abcdefghijklmnopqrstuvwx"),
        format!("g = {}{}\n", "AIza", "SyA-1234567890abcdefghijklmnopqrstu"),
        format!(
            "{}{}\n",
            "github_pat_",
            "11ABCDEFG0123456789_abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOP"
        ),
        format!(
            "CUENCA_KEY={}{}\n",
            "cuenca_0123456789abcdef_", "0123456789abcdef0123456789abcdef"
        ),
        format!(
            "OPENAI={}{}\n",
            "sk-proj-", "abcdefghijklmnopqrstuvwxyz0123456789"
        ),
    ]
}

struct Bed {
    s: Server,
    a1: String,
    d1: String,
    next: std::cell::Cell<u32>,
}

impl Bed {
    /// A server with location `l1` whose main holds [`POLICY`], an agent `a1` granted
    /// l1..l4 and an owner device `d1`.
    fn new() -> Self {
        let s = Server::start();
        let a1 = s.mint("a1", Agent, NA, grant(&["l1", "l2", "l3", "l4"], &[]));
        let d1 = s.mint("d1", OwnerDevice, ND, grant(&[], &[]));
        let bed = Self {
            s,
            a1,
            d1,
            next: std::cell::Cell::new(0),
        };
        bed.location(
            "l1",
            &[
                (".control/policy.yaml", POLICY.as_bytes()),
                (
                    ".cuenca/checks.toml",
                    b"[checks.test]\nargv = [\"make\", \"test\"]\n",
                ),
                ("README", b"v0\n"),
                ("AGENTS.md", b"agents\n"),
                ("old.txt", b"old\n"),
                ("docs/a.md", b"a\n"),
                ("docs/b.md", b"b\n"),
                // Over the size limit already: main may hold it, an edit may not grow it.
                ("legacy.bin", &[b'x'; 5000]),
            ],
        );
        bed
    }

    fn location(&self, name: &str, files: &[(&str, &[u8])]) -> Tree {
        let store = self.s.authority.store();
        let tree = Tree::from_entries(files.iter().map(|(p, c)| {
            let oid = GitOid::for_blob(c);
            store.ingest_blob(oid, c.len() as u64, *c).unwrap();
            (RepoPath::new(*p).unwrap(), Entry::File(oid))
        }))
        .unwrap();
        store
            .add_location(&LocationName::new(name).unwrap(), None, &tree)
            .unwrap();
        tree
    }

    fn main(&self, location: &str) -> Tree {
        self.s
            .authority
            .store()
            .main_tree(&LocationName::new(location).unwrap())
            .unwrap()
    }

    fn who(&self, token: &str) {
        self.s.whois.node(if token == self.d1 { ND } else { NA });
    }

    /// A new fork of `location` held by `token`.
    fn fork(&self, token: &str, location: &str) -> String {
        let n = self.next.get();
        self.next.set(n + 1);
        let name = format!("c{n}");
        self.who(token);
        let r = self.s.post(
            token,
            "/v1/forks",
            json!({ "fork": name, "location": location }),
        );
        assert_eq!(r.status(), 200, "{}", r.text().unwrap());
        name
    }

    /// Publish `ops` (puts as `(path, content, mode)`, deletes as paths) over `base`.
    fn publish(&self, token: &str, fork: &str, base: &Tree, ops: Vec<Op>) -> ChangesetId {
        self.who(token);
        let cs = Changeset::new(base, ops).unwrap();
        let r = self.s.publish(token, fork, 1, &cs);
        assert_eq!(r.status(), 200, "{}", r.text().unwrap());
        ChangesetId::of(&cs)
    }

    fn put(&self, token: &str, path: &str, content: &[u8]) -> Op {
        self.who(token);
        Op::put(
            RepoPath::new(path).unwrap(),
            Entry::File(self.s.put_blob(token, content)),
        )
    }

    fn request(
        &self,
        token: &str,
        fork: &str,
        id: ChangesetId,
        declarations: &Value,
        dry: bool,
    ) -> Value {
        self.who(token);
        let path = if dry {
            format!("/v1/forks/{fork}/landing/dry-run")
        } else {
            format!("/v1/forks/{fork}/landing")
        };
        let r = self.s.post(
            token,
            &path,
            json!({ "epoch": 1, "changeset": id.to_hex(), "declarations": declarations }),
        );
        assert_eq!(r.status(), 200, "{path}");
        let text = r.text().unwrap();
        for secret in secrets() {
            assert!(
                !text.contains(secret.trim()),
                "a verdict carries secret bytes"
            );
        }
        serde_json::from_str(&text).unwrap()
    }

    /// The landing intent journaled for `fork`, read back from the log.
    fn journaled(&self, fork: &str) -> Evaluation {
        let history = self
            .s
            .authority
            .store()
            .fork_history(&ForkName::new(fork).unwrap())
            .unwrap();
        history
            .into_iter()
            .rev()
            .find_map(|r| match r.event {
                Event::LandingIntent { gates, .. } => Some(*gates),
                _ => None,
            })
            .expect("a journaled intent")
    }

    /// Move `location`'s main by a fork of `token` writing `ops`.
    fn advance(&self, token: &str, location: &str, ops: Vec<Op>) {
        let fork = self.fork(token, location);
        let id = self.publish(token, &fork, &self.main(location), ops);
        self.request(token, &fork, id, &json!({}), false);
        self.s
            .authority
            .store()
            .advance_main(&ForkName::new(&fork).unwrap(), id)
            .unwrap();
    }
}

fn failing(gates: &Value) -> BTreeSet<String> {
    gates["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| !r["findings"].as_array().unwrap().is_empty())
        .map(|r| r["gate"].as_str().unwrap().to_string())
        .collect()
}

fn set(gates: &[&str]) -> BTreeSet<String> {
    gates.iter().map(|g| g.to_string()).collect()
}

fn sha(bytes: &[u8]) -> String {
    ContentHash::of(bytes).to_hex()
}

/// Ticket acceptance: a changeset that weakens a gate and adds a secret is evaluated
/// under the base policy and logged as failing, with the base policy's hash: 10 of 10
/// variants.
#[test]
fn a_change_that_weakens_a_gate_and_adds_a_secret_is_judged_by_the_base_policy_10_of_10() {
    let bed = Bed::new();
    let weakened: Vec<Option<String>> = vec![
        Some(POLICY.replace("    - id: G4\n", "    - id: G4-retired\n")),
        Some(POLICY.replace(
            r#"pattern: "\\.(env|env\\.local|credentials\\.json|service-account\\.json)$""#,
            r#"pattern: "x^""#,
        )),
        Some(POLICY.replace("max_file_bytes: 4096", "max_file_bytes: 1000000000")),
        Some(POLICY.replace(
            "    - path_touched: .control/policy.yaml\n      action: require_human\n",
            "",
        )),
        Some(POLICY.replace(
            "max_file_bytes: 4096",
            "max_file_bytes: 4096\n    declare_deletions: false",
        )),
        Some(POLICY.replace(
            "max_file_bytes: 4096",
            "max_file_bytes: 4096\n    declare_reverts: false",
        )),
        Some(POLICY.replace("      l3_max_per_window: 1\n", "")),
        Some(String::new()),
        None, // the policy file deleted
        Some("{ this is not yaml".into()),
    ];
    let base = bed.main("l1");
    let policy_path = RepoPath::new(".control/policy.yaml").unwrap();
    let mut failed = 0;
    for (i, (candidate, secret)) in weakened.iter().zip(secrets()).enumerate() {
        let fork = bed.fork(&bed.a1, "l1");
        let policy_op = match candidate {
            Some(text) => bed.put(&bed.a1, ".control/policy.yaml", text.as_bytes()),
            None => Op::delete(policy_path.clone()),
        };
        // Half the secrets go into a secret-named file, half into an ordinary one.
        let secret_path = if i % 2 == 0 {
            "config/.env"
        } else {
            "src/settings.py"
        };
        let ops = vec![policy_op, bed.put(&bed.a1, secret_path, secret.as_bytes())];
        let id = bed.publish(&bed.a1, &fork, &base, ops);
        let v = bed.request(&bed.a1, &fork, id, &json!({}), false);
        let gates = &v["gates"];
        assert_eq!(gates["passed"], false, "variant {i}: {gates:#}");
        assert_eq!(gates["mode"], "log_only");
        assert_eq!(
            gates["policy"]["sha256"],
            sha(POLICY.as_bytes()),
            "variant {i}: judged by a policy other than the base's"
        );
        if let Some(text) = candidate {
            assert_ne!(sha(text.as_bytes()), sha(POLICY.as_bytes()));
        }
        assert_eq!(gates["base"], base.git_tree_id().to_hex());
        let fails = failing(gates);
        assert!(fails.contains("protected_path"), "variant {i}: {fails:?}");
        assert!(fails.contains("secret_content"), "variant {i}: {fails:?}");
        if i % 2 == 0 {
            assert!(fails.contains("secret_path"), "variant {i}: {fails:?}");
        }
        if candidate.is_none() {
            assert!(fails.contains("deletion"), "variant {i}: {fails:?}");
        }
        // Journaled, as answered, and the intent is recorded: nothing was refused.
        let journaled = bed.journaled(&fork);
        assert_eq!(serde_json::to_value(&journaled).unwrap(), *gates);
        let state = bed
            .s
            .authority
            .store()
            .fork(&ForkName::new(&fork).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(state.landing_intent.map(|(c, _)| c), Some(id));
        failed += 1;
    }
    assert_eq!(failed, 10);
}

/// Ticket acceptance: the dry run returns the same results as publish, on a 20-case
/// suite covering every gate, passing and failing. Each case also has its expected
/// failing gates, so the suite pins what the gates decide, not only that two paths
/// agree.
#[test]
fn a_dry_run_answers_what_the_landing_request_journals_20_cases() {
    let bed = Bed::new();
    let (a1, d1) = (bed.a1.clone(), bed.d1.clone());
    // A fork cut before main moves: its changes are not fast-forwards.
    let stale = bed.fork(&a1, "l1");
    let stale_base = bed.main("l1");
    // Main moves twice: README v0 → v1, then a governance change by the owner's device.
    bed.advance(&a1, "l1", vec![bed.put(&a1, "README", b"v1\n")]);
    bed.advance(&d1, "l1", vec![bed.put(&d1, "AGENTS.md", b"agents v2\n")]);

    bed.location("l2", &[("README", b"no policy\n")]);
    bed.location(
        "l3",
        &[
            (".control/policy.yaml", b"gates: {hard: 3}\n"),
            ("README", b"x\n"),
        ],
    );
    // A landing window on a day other than today (UTC): closed now.
    let today = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        / 86_400
        + 3)
        % 7;
    let days = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    let closed = format!(
        "cuenca:\n  gates:\n    landing_windows:\n      - {{ days: [{}], from: \"00:00\", to: \"24:00\" }}\n",
        days[((today + 1) % 7) as usize]
    );
    bed.location(
        "l4",
        &[
            (".control/policy.yaml", closed.as_bytes()),
            ("README", b"x\n"),
        ],
    );

    let path = |p: &str| RepoPath::new(p).unwrap();
    let secret = secrets();
    let mut cases: Vec<Case> = Vec::new();
    let main = bed.main("l1");
    let mut case = |name: &'static str,
                    token: &str,
                    location: &'static str,
                    ops: Vec<Op>,
                    decl: Value,
                    expect: &[&str]| {
        let base = bed.main(location);
        cases.push((
            name,
            token.to_string(),
            location,
            base,
            ops,
            decl,
            set(expect),
        ));
    };
    case(
        "clean edit",
        &a1,
        "l1",
        vec![bed.put(&a1, "README", b"v2\n")],
        json!({}),
        &[],
    );
    case(
        "AWS key",
        &a1,
        "l1",
        vec![bed.put(&a1, "src/aws.py", secret[0].as_bytes())],
        json!({}),
        &["secret_content"],
    );
    case(
        ".env file",
        &a1,
        "l1",
        vec![bed.put(&a1, "app/.env", b"DEBUG=1\n")],
        json!({}),
        &["secret_path"],
    );
    case(
        "over the size limit",
        &a1,
        "l1",
        vec![bed.put(&a1, "big.bin", &[b'x'; 5000])],
        json!({}),
        &["file_size"],
    );
    case(
        "agent edits the policy",
        &a1,
        "l1",
        vec![bed.put(&a1, ".control/policy.yaml", b"version: 2\n")],
        json!({}),
        &["protected_path", "governance_rate"],
    );
    case(
        "owner device edits the policy",
        &d1,
        "l1",
        vec![bed.put(&d1, ".control/policy.yaml", b"version: 2\n")],
        json!({}),
        &["governance_rate"],
    );
    case(
        "agent writes the root CLAUDE.md",
        &a1,
        "l1",
        vec![bed.put(&a1, "CLAUDE.md", b"rules\n")],
        json!({}),
        &["protected_path", "governance_rate"],
    );
    case(
        "agent writes a nested CLAUDE.md",
        &a1,
        "l1",
        vec![bed.put(&a1, "docs/CLAUDE.md", b"rules\n")],
        json!({}),
        &["protected_path"],
    );
    case(
        "undeclared delete",
        &a1,
        "l1",
        vec![Op::delete(path("old.txt"))],
        json!({}),
        &["deletion"],
    );
    case(
        "declared delete",
        &a1,
        "l1",
        vec![Op::delete(path("old.txt"))],
        json!({ "deletes": ["old.txt"] }),
        &[],
    );
    case(
        "directory declared",
        &a1,
        "l1",
        vec![Op::delete(path("docs/a.md")), Op::delete(path("docs/b.md"))],
        json!({ "deletes": ["docs/"] }),
        &[],
    );
    // A mode-only change re-checks no content: the file is over the size limit, and
    // main held it before.
    let legacy = main.get("legacy.bin").unwrap().oid();
    case(
        "executable bit only",
        &a1,
        "l1",
        vec![Op::put(path("legacy.bin"), Entry::Executable(legacy))],
        json!({}),
        &[],
    );
    case(
        "symlink to a token",
        &a1,
        "l1",
        {
            bed.who(&a1);
            let token = format!("{}{}", "ghp_", "0123456789abcdefghijABCDEFGHIJ012345");
            let oid = bed.s.put_blob(&a1, token.as_bytes());
            vec![Op::put(path("link"), Entry::Symlink(oid))]
        },
        json!({}),
        &["secret_content"],
    );
    case(
        "undeclared revert",
        &a1,
        "l1",
        vec![bed.put(&a1, "README", b"v0\n")],
        json!({}),
        &["revert"],
    );
    case(
        "declared revert",
        &a1,
        "l1",
        vec![bed.put(&a1, "README", b"v0\n")],
        json!({ "reverts": ["README"] }),
        &[],
    );
    case(
        "governance path over the rate",
        &a1,
        "l1",
        vec![bed.put(&a1, "AGENTS.md", b"agents v3\n")],
        json!({}),
        &["governance_rate"],
    );
    case(
        "no policy: defaults",
        &a1,
        "l2",
        vec![bed.put(&a1, "prod.env", b"A=1\n")],
        json!({}),
        &["secret_path"],
    );
    case(
        "unreadable base policy",
        &a1,
        "l3",
        vec![bed.put(&a1, "README", b"y\n")],
        json!({}),
        &["policy"],
    );
    case(
        "outside the landing window",
        &a1,
        "l4",
        vec![bed.put(&a1, "README", b"y\n")],
        json!({}),
        &["landing_window"],
    );
    drop(case);

    // The stale fork: its base is main before both moves.
    let stale_id = bed.publish(
        &a1,
        &stale,
        &stale_base,
        vec![bed.put(&a1, "README", b"stale\n")],
    );
    let mut checked = 0;
    {
        let dry = bed.request(&a1, &stale, stale_id, &json!({}), true)["gates"].clone();
        let landed = bed.request(&a1, &stale, stale_id, &json!({}), false)["gates"].clone();
        assert_eq!(verdict(&dry), verdict(&landed), "stale fork");
        assert_eq!(failing(&landed), set(&["fast_forward"]), "stale fork");
        checked += 1;
    }
    for (name, token, location, base, ops, decl, expect) in cases {
        let fork = bed.fork(&token, location);
        let id = bed.publish(&token, &fork, &base, ops);
        let dry = bed.request(&token, &fork, id, &decl, true)["gates"].clone();
        let landed = bed.request(&token, &fork, id, &decl, false)["gates"].clone();
        assert_eq!(verdict(&dry), verdict(&landed), "{name}");
        assert_eq!(failing(&landed), expect, "{name}: {landed:#}");
        assert_eq!(landed["passed"], expect.is_empty(), "{name}");
        let author = if token == bed.d1 {
            AuthorKind::Human
        } else {
            AuthorKind::Agent
        };
        assert_eq!(
            landed["author"],
            serde_json::to_value(author).unwrap(),
            "{name}"
        );
        assert_eq!(
            serde_json::to_value(bed.journaled(&fork)).unwrap(),
            landed,
            "{name}"
        );
        checked += 1;
    }
    assert_eq!(checked, 20);
}

/// One case of the suite: name, author's token, location, base, ops, declarations and
/// the gates expected to fail.
type Case = (
    &'static str,
    String,
    &'static str,
    Tree,
    Vec<Op>,
    Value,
    BTreeSet<String>,
);

/// The parts of a verdict that do not depend on when it was computed.
fn verdict(gates: &Value) -> Value {
    json!({
        "base": gates["base"],
        "log_len": gates["log_len"],
        "policy": gates["policy"],
        "checks": gates["checks"],
        "author": gates["author"],
        "declarations": gates["declarations"],
        "results": gates["results"],
        "passed": gates["passed"],
    })
}

/// Ticket acceptance: nothing is blocked. A failing changeset's intent is recorded,
/// and main can still move to it.
#[test]
fn a_failing_verdict_refuses_nothing() {
    let bed = Bed::new();
    let fork = bed.fork(&bed.a1, "l1");
    let base = bed.main("l1");
    let secret = secrets();
    let ops = vec![
        bed.put(&bed.a1, ".control/policy.yaml", b""),
        bed.put(&bed.a1, "keys/.env", secret[2].as_bytes()),
        Op::delete(RepoPath::new("old.txt").unwrap()),
    ];
    let id = bed.publish(&bed.a1, &fork, &base, ops);
    let v = bed.request(&bed.a1, &fork, id, &json!({}), false);
    assert_eq!(v["gates"]["passed"], false);
    assert_eq!(v["fresh"], true);
    assert_eq!(
        failing(&v["gates"]),
        set(&[
            "deletion",
            "protected_path",
            "secret_content",
            "secret_path"
        ])
    );
    // A retried request appends nothing and answers the journaled verdict.
    let len = bed.s.authority.store().len().unwrap();
    let again = bed.request(&bed.a1, &fork, id, &json!({}), false);
    assert_eq!(again["fresh"], false);
    assert_eq!(again["offset"], v["offset"]);
    assert_eq!(again["gates"], v["gates"]);
    assert_eq!(bed.s.authority.store().len().unwrap(), len);
    // Main moves to it: log-only gates are not consulted.
    bed.s
        .authority
        .store()
        .advance_main(&ForkName::new(&fork).unwrap(), id)
        .unwrap();
    assert!(bed.main("l1").get("keys/.env").is_some());
}

/// A dry run and a landing request check who asks before any gate runs: a caller who
/// does not hold the fork learns nothing about its head.
#[test]
fn only_the_holder_gets_a_verdict() {
    let bed = Bed::new();
    let fork = bed.fork(&bed.a1, "l1");
    let id = bed.publish(
        &bed.a1,
        &fork,
        &bed.main("l1"),
        vec![bed.put(&bed.a1, "README", b"v9\n")],
    );
    // The owner device reads every fork but holds none of a1's.
    bed.who(&bed.d1);
    for path in ["landing", "landing/dry-run"] {
        let r = bed.s.post(
            &bed.d1,
            &format!("/v1/forks/{fork}/{path}"),
            json!({ "epoch": 1, "changeset": id.to_hex() }),
        );
        assert_eq!(r.status(), 403, "{path}");
        assert_eq!(r.json::<Value>().unwrap()["error"], "not_holder");
    }
    // A wrong changeset id from the holder: not the head, before any gate runs.
    bed.who(&bed.a1);
    let other = ChangesetId::of(&Changeset::new(&Tree::new(), vec![]).unwrap());
    let r = bed.s.post(
        &bed.a1,
        &format!("/v1/forks/{fork}/landing/dry-run"),
        json!({ "epoch": 1, "changeset": other.to_hex() }),
    );
    assert_eq!(r.status(), 409);
    // Unknown fields in the request are refused, not ignored.
    let r = bed.s.post(
        &bed.a1,
        &format!("/v1/forks/{fork}/landing"),
        json!({ "epoch": 1, "changeset": id.to_hex(), "bypass": true }),
    );
    assert_eq!(r.status(), 400);
}
