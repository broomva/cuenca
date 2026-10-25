//! `cuenca`: the agent verbs over the authority's HTTP API, `cuenca admin` owner verbs
//! over its local socket, and `cuenca serve`, the authority itself.

mod admin;

use std::io::Read as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;
use clap::{Parser, Subcommand};
use cuenca_cli::verbs::{self, SnapshotFlags, Watch};
use cuenca_cli::{Client, Config, Report};

/// Cuenca, the agent-native change plane: fork any directory, work in a local checkout,
/// publish snapshots as drafts, and ask to land them.
#[derive(Parser)]
#[command(name = "cuenca", version, propagate_version = true)]
struct Cli {
    /// Print each result as one JSON object (for agents).
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    verb: Verb,
}

#[derive(Subcommand)]
enum Verb {
    /// Store the authority's URL and this principal's key in the config file (mode 0600).
    Login {
        /// The authority's agent API, e.g. http://100.82.195.109:7419.
        #[arg(long)]
        url: String,
        /// Read the key from standard input (so it stays out of the shell history).
        #[arg(long)]
        token_stdin: bool,
    },
    /// List the locations and forks this key may read.
    Forks,
    /// Fork a location: a new fork of its main, held by you. With `--from`, fork a
    /// fork instead: a new fork of its head (an owner fork is snapshotted first).
    Fork {
        #[arg(required_unless_present = "from", conflicts_with = "from")]
        location: Option<String>,
        /// The fork to fork, at its head, instead of a location's main.
        #[arg(long)]
        from: Option<String>,
        /// The fork's name (lowercase ASCII, digits, `.`, `_`, `-`); generated if absent.
        #[arg(long)]
        name: Option<String>,
        /// Materialise the new fork as a git worktree for a harness.
        #[arg(long)]
        checkout: Option<PathBuf>,
    },
    /// Materialise a fork's head in a directory (missing or empty) and make it a checkout.
    Checkout { fork: String, dir: PathBuf },
    /// Materialise a fork as a detached git worktree for an agent harness.
    Worktree { fork: String, dir: PathBuf },
    /// Show the checkout's fork, its changes since the last snapshot, and what is queued.
    Status {
        /// A path inside the checkout.
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
    /// Snapshot the checkout and publish it as a draft (queued if the authority is
    /// unreachable, and published with the next snapshot).
    Snapshot {
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        /// Accept that paths `.forkignore` now names are deleted from the fork.
        #[arg(long)]
        drop_ignored: bool,
        /// Keep running: snapshot 2 s after changes stop, at least every 10 s while they
        /// continue.
        #[arg(long)]
        watch: bool,
        /// With --watch: seconds unchanged before a snapshot.
        #[arg(long, default_value_t = 2.0, requires = "watch")]
        settle: f64,
        /// With --watch: most seconds between snapshots while changes continue.
        #[arg(long, default_value_t = 10.0, requires = "watch")]
        max_wait: f64,
    },
    /// Show what changed: in the checkout since its last snapshot, or from tree A to tree B
    /// (each a fork name or `location:<name>`).
    Diff {
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        #[arg(requires = "b")]
        a: Option<String>,
        b: Option<String>,
    },
    /// Show the log records naming a fork (the checkout's by default).
    Log {
        fork: Option<String>,
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
    /// Run the checks the location's main declares (`.cuenca/checks.toml`, never the
    /// candidate's) on the fork's head, and record the result; exits 1 if any failed, 3 if
    /// the authority did not take the record (the results are still printed).
    Verify {
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
    /// Ask to land the checkout's fork at its head, verified first (publishes anything
    /// queued first). On a location with a git remote, waits for the pull request.
    Land {
        /// Seconds to wait for the pull request.
        #[arg(long, default_value_t = 60)]
        wait: u64,
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
    /// Serve the agent verbs as MCP tools on standard input and output, as the principal
    /// whose key the config holds (`claude mcp add cuenca -- cuenca mcp`). No owner verb is
    /// a tool.
    Mcp,
    /// Owner acts, on the authority's local admin socket. No agent verb, HTTP route or MCP
    /// tool reaches these.
    Admin(admin::Admin),
    /// Run the authority: the agent API on the tailnet, owner acts on a local socket.
    Serve {
        /// State directory: the log, blobs and keys file.
        #[arg(long, env = "CUENCA_STATE")]
        state: PathBuf,
        /// Address for the agent API: this host's tailnet address and a port.
        #[arg(long, env = "CUENCA_BIND")]
        bind: SocketAddr,
        /// Path of the admin socket.
        #[arg(long, env = "CUENCA_ADMIN_SOCKET")]
        admin_socket: PathBuf,
        /// Bind an address outside the tailnet ranges.
        #[arg(long)]
        unsafe_bind: bool,
        /// The tailscale binary.
        #[arg(long, default_value = "tailscale")]
        tailscale: PathBuf,
        /// The gh binary, logged in, that landing opens pull requests with.
        #[arg(long, default_value = "gh")]
        gh: PathBuf,
        /// The volume to keep every location's main and fork's head in, for Samba to serve.
        #[arg(long, env = "CUENCA_VOLUME")]
        volume: Option<PathBuf>,
    },
}

fn client() -> anyhow::Result<Client> {
    Ok(Client::new(&Config::load()?)?)
}

fn print(json: bool, report: &Report) {
    if json {
        println!("{}", report.json);
    } else {
        print!("{}", report.text);
    }
}

fn run(cli: Cli) -> anyhow::Result<i32> {
    let report = match cli.verb {
        Verb::Login { url, token_stdin } => {
            anyhow::ensure!(
                token_stdin,
                "pass --token-stdin and give the key on standard input"
            );
            let mut token = String::new();
            std::io::stdin().read_to_string(&mut token)?;
            let config = Config {
                url,
                token: token.trim().to_string(),
            };
            let me = Client::new(&config)?
                .whoami()
                .context("the authority did not accept the key")?;
            let path = config.save()?;
            Report {
                json: serde_json::json!({ "principal": me.principal, "kind": me.kind, "node": me.node, "config": path }),
                text: format!(
                    "logged in as {} ({}) on node {}; saved {}\n",
                    me.principal,
                    me.kind,
                    me.node,
                    path.display()
                ),
                exit: 0,
            }
        }
        Verb::Forks => verbs::forks(&client()?)?,
        Verb::Fork {
            location,
            from,
            name,
            checkout,
        } => {
            let of = match (&location, &from) {
                (_, Some(f)) => verbs::ForkOf::Fork(f),
                (Some(l), None) => verbs::ForkOf::Location(l),
                (None, None) => unreachable!("clap requires a location or --from"),
            };
            let client = client()?;
            let created = verbs::fork(&client, of, name.as_deref())?;
            match checkout {
                None => created,
                Some(dir) => {
                    let fork = created.json["fork"]["name"]
                        .as_str()
                        .context("created fork name")?;
                    cuenca_cli::worktree::checkout(&client, fork, &dir).with_context(|| {
                        format!(
                            "fork {fork} was created; retry with `cuenca worktree {fork} <dir>`"
                        )
                    })?
                }
            }
        }
        Verb::Checkout { fork, dir } => verbs::checkout(&client()?, &fork, &dir)?,
        Verb::Worktree { fork, dir } => cuenca_cli::worktree::checkout(&client()?, &fork, &dir)?,
        Verb::Status { dir } => verbs::status(client().ok().as_ref(), &dir)?,
        Verb::Snapshot {
            dir,
            drop_ignored,
            watch,
            settle,
            max_wait,
        } => {
            let flags = SnapshotFlags { drop_ignored };
            let client = client()?;
            if watch {
                for (flag, v) in [("--settle", settle), ("--max-wait", max_wait)] {
                    anyhow::ensure!(
                        v.is_finite() && (0.0..=86_400.0).contains(&v),
                        "{flag} is {v}: give seconds between 0 and 86400"
                    );
                }
                let pace = Watch {
                    settle: Duration::from_secs_f64(settle),
                    max_wait: Duration::from_secs_f64(max_wait),
                    ..Watch::default()
                };
                verbs::watch(&client, &dir, flags, pace, |r| print(cli.json, &r))?;
                return Ok(0);
            }
            verbs::snapshot(&client, &dir, flags)?
        }
        Verb::Diff { dir, a, b } => {
            let between = a.as_deref().zip(b.as_deref());
            verbs::diff(&client()?, &dir, between)?
        }
        Verb::Log { fork, dir } => verbs::log(&client()?, &dir, fork.as_deref())?,
        Verb::Verify { dir } => verbs::verify(&client()?, &dir)?,
        Verb::Land { dir, wait } => {
            verbs::land(&client()?, &dir, std::time::Duration::from_secs(wait))?
        }
        Verb::Mcp => {
            // Standard output is the MCP channel: a startup failure goes to stderr even
            // under --json.
            let config = match Config::load() {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("cuenca mcp: {e:#}");
                    return Ok(1);
                }
            };
            tokio::runtime::Runtime::new()?.block_on(cuenca_cli::mcp::serve(config))?;
            return Ok(0);
        }
        Verb::Admin(a) => admin::run(a)?,
        Verb::Serve {
            state,
            bind,
            admin_socket,
            unsafe_bind,
            tailscale,
            gh,
            volume,
        } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "info".into()),
                )
                .init();
            tokio::runtime::Runtime::new()?.block_on(cuenca_server::run(
                cuenca_server::Config {
                    state,
                    bind,
                    admin_socket,
                    unsafe_bind,
                    tailscale,
                    gh,
                    volume,
                },
            ))?;
            return Ok(0);
        }
    };
    print(cli.json, &report);
    Ok(report.exit)
}

fn main() {
    let cli = Cli::parse();
    let json = cli.json;
    let code = match run(cli) {
        Ok(code) => code,
        Err(e) => {
            if json {
                println!("{}", serde_json::json!({ "error": format!("{e:#}") }));
            } else {
                eprintln!("cuenca: {e:#}");
            }
            1
        }
    };
    std::process::exit(code);
}
