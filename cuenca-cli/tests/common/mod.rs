//! The harness `cli.rs` and `mcp.rs` share: a real `cuenca serve` and per-principal
//! clients.
#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

pub const NODE: &str = "nMac11CNTRL";

pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_cuenca")
}

/// `cuenca`, run with its soft open-file limit at `fd_limit` if one is given.
pub fn cuenca(fd_limit: Option<u32>) -> Command {
    match fd_limit {
        None => Command::new(bin()),
        Some(n) => {
            let mut c = Command::new("/bin/sh");
            c.args([
                "-c",
                &format!("ulimit -Sn {n} && exec \"$0\" \"$@\""),
                bin(),
            ]);
            c
        }
    }
}

/// A `cuenca serve` on a port of its own, restartable on the same state and port.
pub struct Authority {
    pub dir: tempfile::TempDir,
    pub port: u16,
    pub child: Option<Child>,
    pub fd_limit: Option<u32>,
    /// The upstream its lander lands to, if any.
    pub upstream: Option<(PathBuf, PathBuf)>,
}

impl Authority {
    pub fn start() -> Self {
        Self::start_with(None)
    }

    /// An authority whose lander lands to `up`: its `gh`, and its GitHub URL pointed at
    /// the local remote through git's environment configuration.
    pub fn start_landing(up: &Upstream) -> Self {
        Self::start_inner(None, Some((up.gh.clone(), up.remote.clone())))
    }

    pub fn start_with(fd_limit: Option<u32>) -> Self {
        Self::start_inner(fd_limit, None)
    }

    fn start_inner(fd_limit: Option<u32>, upstream: Option<(PathBuf, PathBuf)>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let script = dir.path().join("tailscale");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n  whois) echo '{{\"Node\":{{\"StableID\":\"{NODE}\"}}}}' ;;\n  status) echo '{{\"Self\":{{\"ID\":\"{NODE}\"}}}}' ;;\n  *) exit 1 ;;\nesac\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut a = Self {
            dir,
            port: 0,
            child: None,
            fd_limit,
            upstream,
        };
        // A port found free can be taken by a parallel test before the server binds it:
        // then the server exits, and another port is tried.
        for _ in 0..5 {
            a.port = free_port();
            if a.try_up().is_ok() {
                return a;
            }
        }
        panic!("cuenca serve did not come up: {}", a.log());
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("serve.log")).unwrap_or_default()
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn socket(&self) -> PathBuf {
        self.dir.path().join("admin.sock")
    }

    pub fn up(&mut self) {
        if let Err(e) = self.try_up() {
            panic!("cuenca serve did not come up: {e}: {}", self.log());
        }
    }

    pub fn try_up(&mut self) -> Result<(), String> {
        let _ = std::fs::remove_file(self.socket());
        let log = std::fs::File::options()
            .create(true)
            .append(true)
            .open(self.dir.path().join("serve.log"))
            .unwrap();
        let mut serve = cuenca(self.fd_limit);
        serve.arg("serve");
        if let Some((gh, remote)) = &self.upstream {
            serve
                .arg("--gh")
                .arg(gh)
                .env("GIT_CONFIG_COUNT", "1")
                .env(
                    "GIT_CONFIG_KEY_0",
                    format!("url.file://{}.insteadOf", remote.display()),
                )
                .env("GIT_CONFIG_VALUE_0", UPSTREAM_URL);
        }
        let mut child = serve
            .args(["--unsafe-bind", "--bind"])
            .arg(format!("127.0.0.1:{}", self.port))
            .arg("--state")
            .arg(self.dir.path().join("state"))
            .arg("--admin-socket")
            .arg(self.socket())
            .arg("--tailscale")
            .arg(self.dir.path().join("tailscale"))
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while std::net::TcpStream::connect(("127.0.0.1", self.port)).is_err()
            || !self.socket().exists()
        {
            if let Some(status) = child.try_wait().unwrap() {
                return Err(format!("it exited ({status})"));
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err("not up after 60 s".into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.child = Some(child);
        Ok(())
    }

    /// Kill the authority without warning.
    pub fn down(&mut self) {
        if let Some(mut c) = self.child.take() {
            c.kill().unwrap();
            c.wait().unwrap();
        }
    }

    pub fn admin(&self, args: &[&str]) -> Output {
        let out = Command::new(bin())
            .arg("admin")
            .arg("--socket")
            .arg(self.socket())
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "admin {args:?}: {out:?}");
        out
    }

    /// Mint an agent key and return its token.
    pub fn mint(&self, principal: &str, extra: &[&str]) -> String {
        let mut args = vec!["key", "mint", principal, "--kind", "agent", "--node", NODE];
        args.extend_from_slice(extra);
        let out = self.admin(&args);
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }
}

impl Drop for Authority {
    fn drop(&mut self) {
        self.down();
    }
}

/// One principal's `cuenca`, with its own HOME, config and state.
pub struct Agent {
    pub home: tempfile::TempDir,
    pub fd_limit: Option<u32>,
}

impl Agent {
    pub fn login(a: &Authority, token: &str) -> Self {
        let me = Self {
            home: tempfile::tempdir().unwrap(),
            fd_limit: None,
        };
        let mut child = me
            .cmd(&["login", "--url", &a.url(), "--token-stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write as _;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(token.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "login: {out:?}");
        let mode = std::fs::metadata(me.home.path().join("config.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        me
    }

    pub fn cmd(&self, args: &[&str]) -> Command {
        let mut c = cuenca(self.fd_limit);
        c.args(args)
            .env("HOME", self.home.path())
            .env("CUENCA_CONFIG", self.home.path().join("config.json"))
            .env("CUENCA_STATE_HOME", self.home.path().join("state"))
            .env_remove("CUENCA_URL")
            .env_remove("CUENCA_TOKEN");
        c
    }

    /// Run a verb with `--json`: its exit status and its JSON.
    pub fn run(&self, args: &[&str]) -> (i32, Value) {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let out = self.cmd(&all).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let json = serde_json::from_str(text.trim())
            .unwrap_or_else(|e| panic!("{args:?} printed {text:?}: {e}; stderr {:?}", out.stderr));
        (out.status.code().unwrap(), json)
    }

    pub fn ok(&self, args: &[&str]) -> Value {
        let (code, v) = self.run(args);
        assert_eq!(code, 0, "{args:?}: {v}");
        v
    }

    /// The event names of a fork's log records, in order.
    pub fn records(&self, fork: &str) -> Vec<String> {
        self.ok(&["log", fork])["records"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["event"]["event"].as_str().unwrap().to_string())
            .collect()
    }

    /// A location's main as the authority has it.
    pub fn head_of_location(&self, location: &str) -> String {
        let v = self.ok(&["forks"]);
        v["locations"]
            .as_array()
            .unwrap()
            .iter()
            .find(|l| l["name"] == location)
            .unwrap_or_else(|| panic!("{location} not in {v}"))["main"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// The fork's head as the authority has it.
    pub fn head(&self, fork: &str) -> String {
        let v = self.ok(&["forks"]);
        v["forks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["name"] == fork)
            .unwrap_or_else(|| panic!("{fork} not in {v}"))["head"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

pub fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// A plain directory (not a git repository) registered as location `site`.
pub fn site(a: &Authority) -> tempfile::TempDir {
    let site = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(site.path().join("docs")).unwrap();
    std::fs::write(site.path().join("docs/index.md"), "# site\n").unwrap();
    std::fs::write(site.path().join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
    std::fs::set_permissions(
        site.path().join("run.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::write(site.path().join(".DS_Store"), "finder").unwrap();
    assert!(!site.path().join(".git").exists());
    a.admin(&["location", "add", "site", "--dir", s(site.path())]);
    site
}

pub const UPSTREAM_URL: &str = "https://github.com/cuenca-test/land";

/// Run git in `dir`; its stdout.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// A stand-in for a GitHub repository (as in cuenca-server's tests): `work`, a git
/// repository with one commit of `files`; `remote.git`, a bare clone of it; and a fake
/// `gh` that keeps pull requests as files.
pub struct Upstream {
    pub dir: tempfile::TempDir,
    pub work: PathBuf,
    pub remote: PathBuf,
    pub gh: PathBuf,
}

impl Upstream {
    pub fn new(files: &[(&str, &str)]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        for (path, content) in files {
            let p = work.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "init"]);
        git(
            dir.path(),
            &["clone", "-q", "--bare", s(&work), "remote.git"],
        );
        let remote = dir.path().join("remote.git");
        let gh = dir.path().join("gh");
        std::fs::write(
            &gh,
            format!(
                r#"#!/bin/sh
DIR='{dir}'
REMOTE='{remote}'
cmd="$1 $2"
shift 2
repo= head= base=
while [ $# -gt 0 ]; do
  case "$1" in
    --repo) repo=$2; shift ;;
    --head) head=$2; shift ;;
    --base) base=$2; shift ;;
  esac
  shift
done
key=$(printf %s "$head" | tr / _)
mkdir -p "$DIR/prs"
case "$cmd" in
  "pr list")
    if [ -f "$DIR/prs/$key" ]; then
      n=$(cat "$DIR/prs/$key")
      oid=$(git --git-dir="$REMOTE" rev-parse "refs/heads/$head")
      printf '[{{"number":%s,"url":"https://github.com/%s/pull/%s","headRefOid":"%s","headRefName":"%s"}}]\n' "$n" "$repo" "$n" "$oid" "$head"
    else
      echo '[]'
    fi ;;
  "pr create")
    cat > "$DIR/body.$key"
    n=$(( $(ls "$DIR/prs" | wc -l) + 1 ))
    echo "$n" > "$DIR/prs/$key"
    echo "$head $base $repo" >> "$DIR/creates.log"
    echo "https://github.com/$repo/pull/$n" ;;
  *) echo "fake gh: no $cmd" >&2; exit 1 ;;
esac
"#,
                dir = dir.path().display(),
                remote = remote.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            dir,
            work,
            remote,
            gh,
        }
    }

    /// The pull requests `gh pr create` opened: `<head> <base> <repo>` each.
    pub fn creates(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("creates.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    pub fn body(&self, branch: &str) -> String {
        std::fs::read_to_string(
            self.dir
                .path()
                .join(format!("body.{}", branch.replace('/', "_"))),
        )
        .unwrap()
    }
}
