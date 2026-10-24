//! `cuenca mcp` end to end: a real `cuenca serve`, a real `cuenca mcp` child process per
//! principal, and an MCP client (rmcp) driving it over stdio (BRO-3253; spec rows R20,
//! R21). Every structured result is checked against its tool's output schema.

mod common;

use std::collections::BTreeMap;

use common::*;
use rmcp::ServiceExt as _;
use rmcp::model::{CallToolRequestParams, CallToolResult, Tool};
use rmcp::service::{RoleClient, RunningService};
use rmcp::transport::TokioChildProcess;
use serde_json::{Value, json};

/// The agent verbs of spec rev 2.2 section 0: exactly the tools.
const AGENT_VERBS: [&str; 9] = [
    "forks", "fork", "checkout", "status", "snapshot", "diff", "log", "verify", "land",
];

/// An MCP session with `cuenca mcp`, run as the agent's principal.
struct Session {
    client: RunningService<RoleClient, ()>,
    tools: BTreeMap<String, Tool>,
}

impl Session {
    async fn start(agent: &Agent) -> Self {
        let cmd = tokio::process::Command::from(agent.cmd(&["mcp"]));
        let client = ().serve(TokioChildProcess::new(cmd).unwrap()).await.expect("initialize");
        let info = client.peer_info().expect("server info");
        assert_eq!(info.server_info.as_ref().unwrap().name, "cuenca");
        assert!(info.capabilities.tools.is_some());
        let tools = client
            .list_all_tools()
            .await
            .unwrap()
            .into_iter()
            .map(|t| (t.name.to_string(), t))
            .collect();
        Self { client, tools }
    }

    /// Call a tool; a JSON-RPC error is `Err` with its message.
    async fn try_call(&self, tool: &str, args: Value) -> Result<CallToolResult, String> {
        let Value::Object(args) = args else {
            panic!("arguments are an object")
        };
        self.client
            .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(args))
            .await
            .map_err(|e| e.to_string())
    }

    /// Call a tool and return its structured result, checked against the tool's output
    /// schema, with whether it is an error.
    async fn call(&self, tool: &str, args: Value) -> (bool, Value) {
        let r = self
            .try_call(tool, args.clone())
            .await
            .unwrap_or_else(|e| panic!("{tool} {args}: {e}"));
        let v = r
            .structured_content
            .clone()
            .unwrap_or_else(|| panic!("{tool}: no structured content in {r:?}"));
        let input = Value::Object((*self.tools[tool].input_schema).clone());
        assert!(
            jsonschema::is_valid(&input, &args),
            "{tool}: {args} fails its input schema"
        );
        let schema = Value::Object((*self.tools[tool].output_schema.clone().unwrap()).clone());
        let validator = jsonschema::validator_for(&schema).unwrap();
        let errors: Vec<String> = validator.iter_errors(&v).map(|e| e.to_string()).collect();
        assert!(
            errors.is_empty(),
            "{tool}: {v} breaks its schema: {errors:?}"
        );
        assert!(!r.content.is_empty(), "{tool}: a text block for people too");
        (r.is_error == Some(true), v)
    }

    async fn ok(&self, tool: &str, args: Value) -> Value {
        let (error, v) = self.call(tool, args.clone()).await;
        assert!(!error, "{tool} {args}: {v}");
        v
    }
}

/// R20/R21: `tools/list` is the agent verbs, each with valid input and output schemas,
/// and an MCP client round trip (initialize, list, call) forks, checks out, snapshots,
/// diffs and lands through them.
#[tokio::test(flavor = "multi_thread")]
async fn an_mcp_client_lists_the_agent_verbs_and_forks_snapshots_and_diffs() {
    let a = Authority::start();
    let site = site(&a);
    let a1 = Agent::login(&a, &a.mint("a1", &["--location", "site"]));
    let mcp = Session::start(&a1).await;

    let names: Vec<&str> = mcp.tools.keys().map(String::as_str).collect();
    let mut verbs = AGENT_VERBS.to_vec();
    verbs.sort_unstable();
    assert_eq!(names, verbs);
    for (name, tool) in &mcp.tools {
        let input = Value::Object((*tool.input_schema).clone());
        let output = Value::Object((*tool.output_schema.clone().unwrap()).clone());
        for schema in [&input, &output] {
            assert!(jsonschema::meta::is_valid(schema), "{name}: {schema}");
            assert_eq!(schema["type"], "object", "{name}");
        }
        assert_eq!(input["additionalProperties"], false, "{name}");
        for combinator in ["oneOf", "anyOf", "allOf"] {
            assert!(
                input.get(combinator).is_none(),
                "{name}: top-level {combinator}"
            );
        }
        assert!(tool.description.is_some(), "{name}");
    }

    let v = mcp.ok("forks", json!({})).await;
    assert_eq!(v["locations"][0]["name"], "site", "{v}");
    let v = mcp
        .ok("fork", json!({ "location": "site", "name": "m1" }))
        .await;
    assert_eq!(v["fork"]["name"], "m1");
    assert_eq!(v["fork"]["location"], "site");
    let v = mcp
        .ok("fork", json!({ "from": "m1", "name": "m1-copy" }))
        .await;
    assert_eq!(v["fork"]["from"], "m1", "{v}");

    let work = tempfile::tempdir().unwrap();
    let co = work.path().join("m1");
    let dir = s(&co);
    let v = mcp
        .ok("checkout", json!({ "fork": "m1", "dir": dir }))
        .await;
    assert_eq!(v["files"], 2, "{v}");
    assert_eq!(v["clean"], true);
    assert_eq!(
        std::fs::read_to_string(co.join("docs/index.md")).unwrap(),
        "# site\n"
    );

    std::fs::write(co.join("docs/index.md"), "# site, by mcp\n").unwrap();
    std::fs::write(co.join("mcp.txt"), "from an agent\n").unwrap();
    let v = mcp.ok("status", json!({ "dir": dir })).await;
    assert_eq!(v["changes"].as_array().unwrap().len(), 2, "{v}");
    assert_eq!(v["authority"]["epoch"], 1);
    let v = mcp.ok("diff", json!({ "dir": dir })).await;
    assert_eq!(v["to"], "working directory");
    assert_eq!(v["changes"].as_array().unwrap().len(), 2, "{v}");

    let v = mcp.ok("snapshot", json!({ "dir": dir })).await;
    assert_eq!(v["flush"]["pending"], 0, "{v}");
    assert_eq!(v["flush"]["published"].as_array().unwrap().len(), 1, "{v}");
    let head = a1.head("m1");
    assert_ne!(head, a1.head_of_location("site"));

    let v = mcp
        .ok("diff", json!({ "a": "location:site", "b": "m1" }))
        .await;
    assert_eq!(v["to"], head);
    let mut changed: Vec<(String, String)> = v["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["path"].as_str().unwrap().into(),
                c["change"].as_str().unwrap().into(),
            )
        })
        .collect();
    changed.sort();
    assert_eq!(
        changed,
        [
            ("docs/index.md".to_string(), "modified".to_string()),
            ("mcp.txt".to_string(), "added".to_string())
        ]
    );
    let v = mcp.ok("diff", json!({ "dir": dir })).await;
    assert_eq!(v["changes"].as_array().unwrap().len(), 0, "{v}");

    let v = mcp.ok("log", json!({ "fork": "m1" })).await;
    let events: Vec<&str> = v["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["event"]["event"].as_str().unwrap_or("?"))
        .collect();
    assert_eq!(events, ["fork_created", "draft_published"], "{v}");
    let by_dir = mcp.ok("log", json!({ "dir": dir })).await;
    assert_eq!(by_dir, v);

    // The site declares no checks: verify passes with zero.
    let v = mcp.ok("verify", json!({ "dir": dir })).await;
    assert_eq!(v["passed"], true, "{v}");
    assert_eq!(v["checks"], json!([]));

    let v = mcp.ok("land", json!({ "dir": dir })).await;
    assert_eq!(v["fork"], "m1");
    assert_eq!(v["status"], "requested", "{v}");

    // The location itself is untouched: the authority holds the change in the fork.
    assert_eq!(
        std::fs::read_to_string(site.path().join("docs/index.md")).unwrap(),
        "# site\n"
    );
    mcp.client.cancel().await.unwrap();
}

/// R21: an owner act is no tool, so an agent's call for one is refused, every time; a
/// denied verb and malformed arguments come back as structured errors, and the server
/// keeps serving.
#[tokio::test(flavor = "multi_thread")]
async fn owner_acts_are_no_tools_and_denials_are_structured_errors() {
    let a = Authority::start();
    let _site = site(&a);
    let other = tempfile::tempdir().unwrap();
    std::fs::write(other.path().join("secret.txt"), "not yours\n").unwrap();
    a.admin(&["location", "add", "other", "--dir", s(other.path())]);
    let a1 = Agent::login(&a, &a.mint("a1", &["--location", "site"]));
    let a2 = Agent::login(&a, &a.mint("a2", &["--location", "site", "--fork", "m1"]));
    let mcp1 = Session::start(&a1).await;
    let mcp2 = Session::start(&a2).await;

    // Owner verbs, under every name an agent might try: no such tool.
    for name in [
        "admin",
        "mint",
        "key_mint",
        "mint_key",
        "key",
        "revoke",
        "location_add",
        "register_location",
        "location",
        "take_over",
        "take-over",
        "takeover",
        "land_location",
        "serve",
        "login",
        "watch",
        "FORKS",
    ] {
        assert!(!mcp1.tools.contains_key(name), "{name} is a tool");
        let e = mcp1
            .try_call(name, json!({ "location": "site", "fork": "m1" }))
            .await
            .expect_err(name);
        assert!(e.contains("unknown tool"), "{name}: {e}");
    }

    // The authority refuses a fork of a location this key may not read (as not found,
    // so the refusal does not say the location exists).
    let (error, v) = mcp1
        .call("fork", json!({ "location": "other", "name": "theirs" }))
        .await;
    assert!(error, "{v}");
    assert_eq!(v["error"]["status"], 404, "{v}");
    assert_eq!(v["error"]["code"], "not_found", "{v}");

    // A non-holder's snapshot is refused and stays queued: a structured refusal.
    mcp1.ok("fork", json!({ "location": "site", "name": "m1" }))
        .await;
    let work = tempfile::tempdir().unwrap();
    let co = work.path().join("m1-by-a2");
    mcp2.ok("checkout", json!({ "fork": "m1", "dir": s(&co) }))
        .await;
    std::fs::write(co.join("intruder.txt"), "x\n").unwrap();
    let (error, v) = mcp2.call("snapshot", json!({ "dir": s(&co) })).await;
    assert!(error, "{v}");
    assert!(
        v["flush"]["error"].as_str().unwrap().contains("not_holder"),
        "{v}"
    );
    assert_eq!(v["flush"]["pending"], 1, "{v}");
    assert_eq!(a1.records("m1"), ["fork_created"]);

    // Not a checkout: a structured error, not a crash.
    let (error, v) = mcp1.call("status", json!({ "dir": s(work.path()) })).await;
    assert!(error, "{v}");
    assert!(v["error"]["message"].is_string(), "{v}");

    // Arguments the schemas refuse are refused by the server too.
    for (tool, args) in [
        ("fork", json!({ "location": "site", "nmae": "typo" })),
        ("fork", json!({})),
        ("fork", json!({ "location": "site", "from": "m1" })),
        ("forks", json!({ "all": true })),
        ("status", json!({ "dir": "relative/path" })),
        ("status", json!({ "dir": 7 })),
        ("diff", json!({ "a": "m1" })),
        ("diff", json!({ "a": "m1", "b": "m1", "dir": s(&co) })),
        ("diff", json!({})),
        ("log", json!({})),
        ("snapshot", json!({ "dir": s(&co), "watch": true })),
        ("checkout", json!({ "fork": "m1" })),
    ] {
        let schema = Value::Object((*mcp1.tools[tool].input_schema).clone());
        let e = mcp1.try_call(tool, args.clone()).await.expect_err(tool);
        assert!(e.contains(tool), "{tool} {args}: {e}");
        // The schema refuses it too, except fork's, diff's and log's forms, which only
        // the server checks (no top-level oneOf/anyOf in a tool's input schema).
        let forms = matches!(tool, "fork" | "diff" | "log")
            && args.get("dir").is_none_or(|d| d.is_string())
            && args.get("nmae").is_none();
        assert!(
            !jsonschema::is_valid(&schema, &args) || forms,
            "{tool} {args}"
        );
    }

    // The session is still up.
    let v = mcp1.ok("forks", json!({})).await;
    assert_eq!(v["forks"][0]["name"], "m1", "{v}");
    mcp1.client.cancel().await.unwrap();
    mcp2.client.cancel().await.unwrap();
}

/// With the authority down, a verb's result says so (`offline`), still within its output
/// schema, and status reports the authority's error in its own report; a `cuenca mcp`
/// that cannot start says why on stderr and keeps stdout clean.
#[tokio::test(flavor = "multi_thread")]
async fn an_unreachable_authority_is_a_structured_offline_error() {
    let mut a = Authority::start();
    let _site = site(&a);
    let a1 = Agent::login(&a, &a.mint("a1", &["--location", "site"]));
    let mcp = Session::start(&a1).await;
    mcp.ok("fork", json!({ "location": "site", "name": "m1" }))
        .await;
    let work = tempfile::tempdir().unwrap();
    let co = work.path().join("m1");
    mcp.ok("checkout", json!({ "fork": "m1", "dir": s(&co) }))
        .await;

    a.down();
    let (error, v) = mcp.call("forks", json!({})).await;
    assert!(error, "{v}");
    assert_eq!(v["error"]["offline"], true, "{v}");
    let v = mcp.ok("status", json!({ "dir": s(&co) })).await;
    assert_eq!(v["authority"]["offline"], true, "{v}");
    std::fs::write(co.join("offline.txt"), "queued\n").unwrap();
    let v = mcp.ok("snapshot", json!({ "dir": s(&co) })).await;
    assert_eq!(v["flush"]["offline"], true, "{v}");
    assert_eq!(v["flush"]["pending"], 1, "{v}");
    mcp.client.cancel().await.unwrap();

    let out = a1
        .cmd(&["--json", "mcp"])
        .env("CUENCA_CONFIG", work.path().join("none.json"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        out.stdout.is_empty(),
        "{:?}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("not logged in"));
}
