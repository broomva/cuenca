#![allow(dead_code)]

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cuenca_core::NodeId;
use cuenca_server::admin::{self, AdminRequest};
use cuenca_server::{Authority, Grant, Peer, PrincipalKind, Whois, WhoisFuture};
use cuenca_tree::{Changeset, Entry, GitOid, Op, RepoPath, Tree};
use reqwest::blocking::{Client, Response};
use serde_json::{Value, json};

/// A [`Whois`] whose answer the test sets: every source is named as `answer`.
pub struct Switch(Mutex<Result<Option<Peer>, String>>);

impl Switch {
    pub fn set(&self, answer: Result<Option<Peer>, String>) {
        *self.0.lock().unwrap() = answer;
    }

    /// Name every source as `node`, serving no route.
    pub fn node(&self, node: &str) {
        self.set(Ok(Some(peer(node, false))));
    }
}

impl Whois for Switch {
    fn whois(&self, _ip: IpAddr) -> WhoisFuture<'_> {
        let answer = self.0.lock().unwrap().clone();
        Box::pin(async move { answer })
    }
}

pub fn peer(node: &str, serves_routes: bool) -> Peer {
    Peer {
        node: NodeId::new(node).unwrap(),
        serves_routes,
    }
}

/// An authority on 127.0.0.1 (an unsafe bind, for tests), on a runtime of its own.
pub struct Server {
    pub url: String,
    pub admin: PathBuf,
    pub whois: Arc<Switch>,
    pub authority: Arc<Authority>,
    pub client: Client,
    rt: tokio::runtime::Runtime,
    _dir: Option<tempfile::TempDir>,
}

impl Server {
    pub fn start() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let mut s = Self::start_in(&path);
        s._dir = Some(dir);
        s
    }

    /// Start on the state in `dir` (kept by the caller).
    pub fn start_in(dir: &Path) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let whois = Arc::new(Switch(Mutex::new(Ok(None))));
        let authority =
            Arc::new(Authority::open(&dir.join("state"), whois.clone() as Arc<dyn Whois>).unwrap());
        // The admin socket's directory must be its owner's alone, whatever the umask.
        std::fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
        let admin_path = dir.join("admin.sock");
        let (listener, admin_listener) = rt.block_on(async {
            (
                tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
                admin::bind(&admin_path).unwrap(),
            )
        });
        let addr: SocketAddr = listener.local_addr().unwrap();
        rt.spawn(cuenca_server::serve_http(listener, authority.clone()));
        rt.spawn(admin::serve(admin_listener, authority.clone()));
        Self {
            url: format!("http://{addr}"),
            admin: admin_path,
            whois,
            authority,
            client: Client::new(),
            rt,
            _dir: None,
        }
    }

    pub fn admin(&self, req: AdminRequest) -> Value {
        admin::call(&self.admin, &req).unwrap()
    }

    /// Mint a key and return its token.
    pub fn mint(&self, principal: &str, kind: PrincipalKind, node: &str, grant: Grant) -> String {
        let v = self.admin(AdminRequest::KeyMint {
            principal: principal.parse_name(),
            kind,
            node: NodeId::new(node).unwrap(),
            grant,
        });
        assert_eq!(v["ok"], true, "{v}");
        v["token"].as_str().unwrap().to_string()
    }

    pub fn add_location(&self, name: &str) {
        let v = self.admin(AdminRequest::LocationAdd {
            name: cuenca_core::LocationName::new(name).unwrap(),
            remote: None,
            git: None,
            dir: None,
        });
        assert_eq!(v["ok"], true, "{v}");
    }

    pub fn get(&self, token: &str, path: &str) -> Response {
        self.client
            .get(format!("{}{path}", self.url))
            .bearer_auth(token)
            .send()
            .unwrap()
    }

    pub fn post(&self, token: &str, path: &str, body: Value) -> Response {
        self.client
            .post(format!("{}{path}", self.url))
            .bearer_auth(token)
            .json(&body)
            .send()
            .unwrap()
    }

    /// Upload `content` and return its git id.
    pub fn put_blob(&self, token: &str, content: &[u8]) -> GitOid {
        let oid = GitOid::for_blob(content);
        let r = self
            .client
            .put(format!("{}/v1/blobs/{}", self.url, oid.to_hex()))
            .bearer_auth(token)
            .body(content.to_vec())
            .send()
            .unwrap();
        assert_eq!(r.status(), 200, "{}", r.text().unwrap());
        oid
    }

    pub fn fork_head(&self, token: &str, fork: &str) -> Tree {
        let v: Value = self
            .get(token, &format!("/v1/forks/{fork}/tree"))
            .json()
            .unwrap();
        serde_json::from_value(v["tree"].clone()).unwrap()
    }

    /// The changeset writing `files` over `base`, uploading their content.
    pub fn write(&self, token: &str, base: &Tree, files: &[(&str, &[u8])]) -> Changeset {
        let ops = files
            .iter()
            .map(|(p, c)| {
                Op::put(
                    RepoPath::new(*p).unwrap(),
                    Entry::File(self.put_blob(token, c)),
                )
            })
            .collect();
        Changeset::new(base, ops).unwrap()
    }

    pub fn publish(&self, token: &str, fork: &str, epoch: u64, cs: &Changeset) -> Response {
        self.post(
            token,
            &format!("/v1/forks/{fork}/drafts"),
            json!({ "epoch": epoch, "changeset": cs }),
        )
    }

    /// Drop the server without waiting for in-flight requests.
    pub fn stop(self) {
        self.rt.shutdown_background();
    }
}

pub trait ParseName {
    fn parse_name(&self) -> cuenca_core::PrincipalName;
}

impl ParseName for str {
    fn parse_name(&self) -> cuenca_core::PrincipalName {
        cuenca_core::PrincipalName::new(self).unwrap()
    }
}

pub fn grant(locations: &[&str], forks: &[&str]) -> Grant {
    Grant {
        locations: locations
            .iter()
            .map(|l| cuenca_core::LocationName::new(*l).unwrap())
            .collect(),
        forks: forks
            .iter()
            .map(|f| cuenca_core::ForkName::new(*f).unwrap())
            .collect(),
    }
}

/// Run git in `dir`, panicking with its stderr if it fails; its stdout.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
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

/// A stand-in for a GitHub repository: `remote.git`, a bare repository with one commit
/// on `main`, cloned from `work`; and a fake `gh` that keeps pull requests as files.
pub struct Upstream {
    pub dir: PathBuf,
    /// The GitHub URL the location names; `git_config` points it at `remote.git`.
    pub url: String,
    pub remote: PathBuf,
    pub work: PathBuf,
    pub gh: PathBuf,
    pub git_config: Vec<String>,
}

pub const UPSTREAM_URL: &str = "https://github.com/cuenca-test/land";

impl Upstream {
    pub fn new(dir: &Path, files: &[(&str, &str)]) -> Self {
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "-b", "main"]);
        for (path, content) in files {
            let p = work.join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        git(&work, &["add", "-A"]);
        git(&work, &["commit", "-q", "-m", "init"]);
        let remote = dir.join("remote.git");
        git(
            dir,
            &[
                "clone",
                "-q",
                "--bare",
                work.to_str().unwrap(),
                "remote.git",
            ],
        );
        let gh = dir.join("gh");
        std::fs::write(
            &gh,
            format!(
                r#"#!/bin/sh
# A fake gh: one file per pull request under prs/, named by its head branch.
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
      oid=$(cat "$DIR/prs/$key.oid" 2>/dev/null || git --git-dir="$REMOTE" rev-parse "refs/heads/$head")
      printf '[{{"number":%s,"url":"https://github.com/%s/pull/%s","headRefOid":"%s","headRefName":"%s"}}]\n' "$n" "$repo" "$n" "$oid" "$head"
    else
      echo '[]'
    fi ;;
  "pr create")
    cat > "$DIR/body.$key"
    n=$(( $(ls "$DIR/prs" | grep -cv '\.oid$') + 1 ))
    echo "$n" > "$DIR/prs/$key"
    echo "$head $base $repo" >> "$DIR/creates.log"
    echo "https://github.com/$repo/pull/$n" ;;
  *) echo "fake gh: no $cmd" >&2; exit 1 ;;
esac
"#,
                dir = dir.display(),
                remote = remote.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let git_config = vec![format!(
            "url.file://{}.insteadOf={UPSTREAM_URL}",
            remote.display()
        )];
        Self {
            dir: dir.to_path_buf(),
            url: UPSTREAM_URL.to_string(),
            remote,
            work,
            gh,
            git_config,
        }
    }

    /// The pull requests `gh pr create` opened: `<head> <base> <repo>` each.
    pub fn creates(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.join("creates.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// A pull request's body.
    pub fn body(&self, branch: &str) -> String {
        std::fs::read_to_string(self.dir.join(format!("body.{}", branch.replace('/', "_"))))
            .unwrap()
    }

    /// Run git on the remote.
    pub fn git(&self, args: &[&str]) -> String {
        git(&self.remote, args)
    }

    /// A lander configuration for this upstream.
    pub fn land_config(&self, repos: PathBuf) -> cuenca_server::land::LandConfig {
        let mut c = cuenca_server::land::LandConfig::new(repos, self.gh.clone());
        c.git_config = self.git_config.clone();
        c.poll = std::time::Duration::from_millis(100);
        c
    }
}
