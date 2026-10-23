//! `cuenca mcp`: the agent verbs as MCP tools over stdio (spec rev 2.2 section 0, ticket
//! BRO-3253).
//!
//! One tool per agent verb, each calling the same function in [`crate::verbs`] the binary
//! calls, so a tool and its CLI verb cannot drift. The tool list is [`TOOLS`] and nothing
//! else: there is no owner verb (`cuenca admin`) here, so an agent asking this server to
//! mint a key, register a location, take a fork over or land a location without a remote
//! gets "unknown tool" for each, because no such tool exists.
//!
//! The server holds no authority of its own. It acts as the principal whose key the
//! config holds (`cuenca login`, or `CUENCA_URL` and `CUENCA_TOKEN`), read once at start;
//! the authority checks that key on every request, exactly as for the CLI.
//!
//! A tool's result is the verb's JSON as `structuredContent` (it validates against the
//! tool's `outputSchema`) and its text as a text block. A verb that refuses or fails (the
//! authority denied it, the checkout is not one, a draft stayed queued) is a tool result
//! with `isError: true` and a structured `error`; malformed arguments and unknown tools are
//! JSON-RPC errors. Nothing a client sends panics the server.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListToolsResult, PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
    ToolAnnotations,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, ServiceExt as _};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::client::{Client, ClientError};
use crate::config::Config;
use crate::verbs::{self, Report, SnapshotFlags};

/// The tools, in the order `tools/list` gives them: the agent verbs of spec rev 2.2
/// section 0, and only those.
pub const TOOLS: [&str; 9] = [
    "forks", "fork", "checkout", "status", "snapshot", "diff", "log", "verify", "land",
];

/// How long the `land` tool waits for the pull request.
const LAND_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Serve the tools on standard input and output until the client closes them.
pub async fn serve(config: Config) -> anyhow::Result<()> {
    let running = Server { config }
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|e| anyhow::anyhow!("starting the MCP session: {e}"))?;
    running.waiting().await?;
    Ok(())
}

/// The MCP server: the caller's config, and the agent verbs.
#[derive(Clone)]
pub struct Server {
    config: Config,
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("cuenca", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Cuenca: fork any directory, work in a local checkout, publish snapshots as \
                 drafts and ask to land them. `fork` a location, `checkout` the fork into an \
                 empty directory, edit files there, `snapshot` to publish, `diff` and `log` to \
                 inspect, `land` to ask for it to land. Paths are absolute.",
            )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(tools()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tools().into_iter().find(|t| t.name == name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let call = Call::parse(&request.name, request.arguments.unwrap_or_default())?;
        let config = self.config.clone();
        // The verbs block (files, and the authority over blocking HTTP), so they run off
        // the async workers; a panic in one comes back as a join error, not a dead server.
        let done = tokio::task::spawn_blocking(move || call.run(&config))
            .await
            .map_err(|e| ErrorData::internal_error(format!("the tool failed: {e}"), None))?;
        Ok(result(done).into())
    }
}

/// A tool call whose arguments parsed.
enum Call {
    Forks,
    Fork(ForkArgs),
    Checkout(CheckoutArgs),
    Status(DirArgs),
    Snapshot(SnapshotArgs),
    Diff(DiffArgs),
    Log(LogArgs),
    Verify(DirArgs),
    Land(DirArgs),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ForkArgs {
    location: String,
    name: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckoutArgs {
    fork: String,
    dir: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DirArgs {
    dir: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotArgs {
    dir: PathBuf,
    #[serde(default)]
    drop_ignored: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DiffArgs {
    dir: Option<PathBuf>,
    a: Option<String>,
    b: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LogArgs {
    dir: Option<PathBuf>,
    fork: Option<String>,
}

fn args<T: DeserializeOwned>(tool: &str, arguments: JsonObject) -> Result<T, ErrorData> {
    serde_json::from_value(Value::Object(arguments))
        .map_err(|e| ErrorData::invalid_params(format!("{tool}: {e}"), None))
}

/// A directory argument: absolute, because the server's working directory is not the
/// agent's.
fn absolute(tool: &str, dir: &Path) -> Result<(), ErrorData> {
    if dir.is_absolute() {
        Ok(())
    } else {
        Err(ErrorData::invalid_params(
            format!("{tool}: dir {} is not an absolute path", dir.display()),
            None,
        ))
    }
}

impl Call {
    fn parse(tool: &str, a: JsonObject) -> Result<Self, ErrorData> {
        let call = match tool {
            "forks" => {
                let NoArgs {} = args(tool, a)?;
                Self::Forks
            }
            "fork" => Self::Fork(args(tool, a)?),
            "checkout" => Self::Checkout(args(tool, a)?),
            "status" => Self::Status(args(tool, a)?),
            "snapshot" => Self::Snapshot(args(tool, a)?),
            "diff" => Self::Diff(args(tool, a)?),
            "log" => Self::Log(args(tool, a)?),
            "verify" => Self::Verify(args(tool, a)?),
            "land" => Self::Land(args(tool, a)?),
            _ => {
                return Err(ErrorData::invalid_params(
                    format!("unknown tool {tool:?}: the tools are {}", TOOLS.join(", ")),
                    None,
                ));
            }
        };
        match &call {
            Self::Checkout(CheckoutArgs { dir, .. })
            | Self::Status(DirArgs { dir })
            | Self::Snapshot(SnapshotArgs { dir, .. })
            | Self::Verify(DirArgs { dir })
            | Self::Land(DirArgs { dir }) => absolute(tool, dir)?,
            Self::Diff(d) => {
                match (&d.a, &d.b, &d.dir) {
                    (Some(_), Some(_), None) | (None, None, Some(_)) => {}
                    _ => {
                        return Err(ErrorData::invalid_params(
                            "diff: give dir (the checkout against its last snapshot), or a \
                             and b (two trees), not both",
                            None,
                        ));
                    }
                }
                if let Some(dir) = &d.dir {
                    absolute(tool, dir)?;
                }
            }
            Self::Log(l) => {
                if l.dir.is_none() && l.fork.is_none() {
                    return Err(ErrorData::invalid_params(
                        "log: give fork, or dir (a checkout, for its fork)",
                        None,
                    ));
                }
                if let Some(dir) = &l.dir {
                    absolute(tool, dir)?;
                }
            }
            Self::Forks | Self::Fork(_) => {}
        }
        Ok(call)
    }

    fn run(self, config: &Config) -> anyhow::Result<Report> {
        let client = Client::new(config)?;
        match self {
            Self::Forks => verbs::forks(&client),
            Self::Fork(a) => verbs::fork(&client, &a.location, a.name.as_deref()),
            Self::Checkout(a) => verbs::checkout(&client, &a.fork, &a.dir),
            Self::Status(a) => verbs::status(Some(&client), &a.dir),
            Self::Snapshot(a) => verbs::snapshot(
                &client,
                &a.dir,
                SnapshotFlags {
                    drop_ignored: a.drop_ignored,
                },
            ),
            Self::Diff(a) => {
                let between = a.a.as_deref().zip(a.b.as_deref());
                // With a and b the checkout is not read; the verb still takes a path.
                let dir = a.dir.unwrap_or_else(|| PathBuf::from("/"));
                verbs::diff(&client, &dir, between)
            }
            Self::Log(a) => {
                let dir = a.dir.unwrap_or_else(|| PathBuf::from("/"));
                verbs::log(&client, &dir, a.fork.as_deref())
            }
            Self::Verify(a) => verbs::verify(&client, &a.dir),
            Self::Land(a) => verbs::land(&client, &a.dir, LAND_WAIT),
        }
    }
}

/// A verb's outcome as a tool result: its report, or a structured error.
fn result(done: anyhow::Result<Report>) -> CallToolResult {
    match done {
        Ok(report) => {
            let mut r = if report.exit == 0 {
                CallToolResult::structured(report.json)
            } else {
                CallToolResult::structured_error(report.json)
            };
            r.content = vec![ContentBlock::text(report.text)];
            r
        }
        Err(e) => {
            let message = format!("{e:#}");
            let mut error = json!({ "message": message });
            if let Some(c) = e.chain().find_map(|c| c.downcast_ref::<ClientError>()) {
                let mut shaped = verbs::client_error(c);
                shaped["message"] = json!(message);
                error = shaped;
            }
            let mut r = CallToolResult::structured_error(json!({ "error": error }));
            r.content = vec![ContentBlock::text(message)];
            r
        }
    }
}

fn object(v: Value) -> Arc<JsonObject> {
    match v {
        Value::Object(o) => Arc::new(o),
        _ => unreachable!("every schema here is an object literal"),
    }
}

/// An input schema: an object with exactly these properties. Which of diff's and log's
/// forms is given is checked in [`Call::parse`] and said in the tool's description, not
/// with a top-level `oneOf`/`anyOf`: some hosts refuse a tool list holding one.
fn input(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

/// An output schema: the verb's own object, or `{ "error": ... }` when it failed.
fn output(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "oneOf": [
            { "properties": properties, "required": required },
            {
                "properties": { "error": {
                    "type": "object",
                    "properties": {
                        "message": { "type": "string" },
                        "code": { "type": "string", "description": "the authority's error code" },
                        "status": { "type": "integer", "description": "the HTTP status" },
                        "offline": { "type": "boolean", "description": "the authority was unreachable" },
                    },
                    "required": ["message"],
                } },
                "required": ["error"],
            },
        ],
    })
}

fn dir(what: &str) -> Value {
    json!({
        "type": "string",
        "pattern": "^/",
        "description": format!("Absolute path of {what}."),
    })
}

/// The changes item: one path a changeset changes.
fn changes() -> Value {
    json!({
        "type": "array",
        "items": {
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "change": { "enum": ["added", "modified", "mode", "deleted", "other"] },
                "kind": { "enum": ["file", "executable", "symlink", "other"] },
            },
            "required": ["path", "change"],
        },
    })
}

fn strings() -> Value {
    json!({ "type": "array", "items": { "type": "string" } })
}

/// Every tool, with its input and output schemas.
pub fn tools() -> Vec<Tool> {
    let s = json!({ "type": "string" });
    let n = json!({ "type": "integer", "minimum": 0 });
    let b = json!({ "type": "boolean" });
    let o = json!({ "type": "object" });
    let a = json!({ "type": "array" });
    let fork = json!({
        "type": "object",
        "properties": { "name": s, "location": s, "head": s },
        "required": ["name", "location", "head"],
    });
    let read = ToolAnnotations::new().read_only(true);
    let write = ToolAnnotations::new().read_only(false).destructive(false);
    let spec = [
        (
            "forks",
            "List the locations and forks this key may read.",
            input(json!({}), &[]),
            output(
                json!({ "locations": { "type": "array", "items": o }, "forks": { "type": "array", "items": fork } }),
                &["locations", "forks"],
            ),
            read.clone(),
        ),
        (
            "fork",
            "Fork a location: a new fork of its main, held by you under epoch 1.",
            input(
                json!({
                    "location": { "type": "string", "description": "The location to fork." },
                    "name": { "type": "string", "description": "The fork's name (lowercase ASCII, digits, `.`, `_`, `-`); generated if absent." },
                }),
                &["location"],
            ),
            output(json!({ "fork": fork }), &["fork"]),
            write.clone(),
        ),
        (
            "checkout",
            "Materialise a fork's head in a directory (missing or empty) and make it a checkout.",
            input(
                json!({ "fork": { "type": "string", "description": "The fork to check out." }, "dir": dir("the directory to create or fill") }),
                &["fork", "dir"],
            ),
            output(
                json!({
                    "fork": s, "location": s, "epoch": n, "head": s, "root": s, "files": n,
                    "cloned": n, "copied": n, "clean": b, "ignored": strings(),
                }),
                &[
                    "fork", "location", "epoch", "head", "root", "files", "clean", "ignored",
                ],
            ),
            write.clone(),
        ),
        (
            "status",
            "Show the checkout's fork, its changes since the last snapshot, what is queued, and the fork as the authority has it.",
            input(
                json!({ "dir": dir("a directory inside the checkout") }),
                &["dir"],
            ),
            output(
                json!({
                    "root": s, "fork": s, "location": s, "epoch": n, "acknowledged": s,
                    "queued": n, "changes": changes(), "ignored": strings(), "skipped": a,
                    "authority": o,
                }),
                &[
                    "root",
                    "fork",
                    "location",
                    "epoch",
                    "queued",
                    "changes",
                    "authority",
                ],
            ),
            read.clone(),
        ),
        (
            "snapshot",
            "Snapshot the checkout and publish it as a draft. If the authority is unreachable it stays queued and goes out with the next snapshot.",
            input(
                json!({
                    "dir": dir("a directory inside the checkout"),
                    "drop_ignored": { "type": "boolean", "default": false, "description": "Accept that paths `.forkignore` now names are deleted from the fork." },
                }),
                &["dir"],
            ),
            output(
                json!({
                    "changes": changes(),
                    "flush": {
                        "type": "object",
                        "properties": {
                            "published": a, "pending": n, "offline": b,
                            "error": { "type": ["string", "null"] },
                        },
                        "required": ["published", "pending", "offline"],
                    },
                }),
                &["changes", "flush"],
            ),
            write.clone(),
        ),
        (
            "diff",
            "Show what changed: in the checkout since its last snapshot (give dir only), or from tree a to tree b (give a and b only, each a fork name or `location:<name>`).",
            input(
                json!({
                    "dir": dir("a directory inside the checkout"),
                    "a": { "type": "string", "description": "The tree to compare from." },
                    "b": { "type": "string", "description": "The tree to compare to." },
                }),
                &[],
            ),
            output(
                json!({ "from": s, "to": s, "changes": changes(), "ignored": strings() }),
                &["from", "to", "changes", "ignored"],
            ),
            read.clone(),
        ),
        (
            "log",
            "Show the log records naming a fork (give fork, or dir for the checkout's fork; at least one).",
            input(
                json!({ "fork": { "type": "string", "description": "The fork." }, "dir": dir("a directory inside a checkout") }),
                &[],
            ),
            output(
                json!({ "fork": s, "records": { "type": "array", "items": o } }),
                &["fork", "records"],
            ),
            read.clone(),
        ),
        (
            "verify",
            "Run the checks the location's main declares in .cuenca/checks.toml (never the candidate's own) on the fork's head, in a temporary checkout, and record how they ended. Publishes anything queued first; the head must be the checkout's last snapshot. Advisory: the record blocks nothing. isError when a check failed, or when the authority did not take the record (recorded false, offset null, record_error says why; the check results are still reported).",
            input(
                json!({ "dir": dir("a directory inside the checkout") }),
                &["dir"],
            ),
            output(
                json!({
                    "fork": s, "changeset": s, "candidate": s, "base": s,
                    "manifest": { "type": ["object", "null"] },
                    "checks": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": s, "argv": strings(), "timeout_secs": n,
                                "status": { "enum": ["passed", "failed", "timed_out", "not_run"] },
                                "exit_code": { "type": ["integer", "null"] },
                                "duration_ms": n, "output": s,
                            },
                            "required": ["name", "argv", "status", "exit_code", "duration_ms"],
                        },
                    },
                    "passed": b, "recorded": b,
                    "offset": { "type": ["integer", "null"] },
                    "record_error": { "type": ["object", "null"] },
                }),
                &[
                    "fork",
                    "changeset",
                    "candidate",
                    "base",
                    "manifest",
                    "checks",
                    "passed",
                    "recorded",
                    "offset",
                    "record_error",
                ],
            ),
            write.clone(),
        ),
        (
            "land",
            "Ask to land the checkout's fork at its head, which `verify` must have checked \
             against the location's main (publishes anything queued first). On a location \
             with a git remote the authority opens a pull request, which GitHub's required \
             checks gate; this waits up to a minute for it. On one without, the request \
             waits for the owner. `status`: requested, exporting, exported, reviewing, \
             in_review, landed or failed.",
            input(
                json!({ "dir": dir("a directory inside the checkout") }),
                &["dir"],
            ),
            output(
                json!({
                    "fork": s, "changeset": s, "offset": n, "status": s,
                    "waiting_for": { "type": ["string", "null"], "enum": ["owner", "authority", null] },
                    "pr": { "type": ["integer", "null"] },
                    "url": { "type": ["string", "null"] },
                    "reason": { "type": ["string", "null"] },
                    "retrying_after": { "type": ["string", "null"] },
                }),
                &["fork", "changeset", "offset", "status", "waiting_for"],
            ),
            write,
        ),
    ];
    spec.into_iter()
        .map(|(name, description, input, output, annotations)| {
            Tool::new(name, description, object(input))
                .with_raw_output_schema(object(output))
                .with_annotations(annotations)
        })
        .collect()
}
