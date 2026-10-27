//! `.forkignore`: the host-local paths a snapshot never publishes.

use crate::error::{FsError, Result};
use cuenca_tree::{Entry, GitOid, Tree};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use rustix::fs::{CWD, Mode, OFlags, openat};
use std::fs::File;
use std::io::Read as _;
use std::os::fd::OwnedFd;
use std::path::Path;

/// The file name, at a location's root.
pub const FORKIGNORE: &str = ".forkignore";

/// What a location without a `.forkignore` ignores, and what registering a location
/// writes (spec section 2): Finder and iOS metadata, installed dependencies, build
/// output and local secrets.
///
/// A default that names a path a location's main tracks fails every snapshot of that
/// location, so a build or cache directory is a default only when no repository in the
/// workspace tracks one (measured over 102 repositories on 2026-10-27, BRO-3528).
/// `build`, `dist`, `out`, `*.log` and `.gradle` are tracked somewhere and stay out; a
/// location that wants them adds them to its own `.forkignore`, which replaces these
/// rules, so it restates the ones it keeps.
pub const DEFAULT_FORKIGNORE: &str = "\
# Paths a Cuenca snapshot never publishes (gitignore syntax). The location owns this file.
# Finder and iOS metadata
.DS_Store
._*
.Spotlight-V100
.Trashes
.fseventsd
# Local secrets: every .env file except the committed examples
.env
.env.*
!.env.example
!.env.*.example
!.env.sample
!.env.template
# Installed dependencies and environments
node_modules
.venv
Pods
.dart_tool
.tox
*.egg-info
.direnv
# Build output and caches
target
.next
.nuxt
.svelte-kit
.angular
.expo
.output
.vite
.turbo
.parcel-cache
.cache
__pycache__
.pytest_cache
.mypy_cache
.ruff_cache
coverage
DerivedData
.wrangler
.terraform
";

/// A parsed `.forkignore`, in gitignore syntax. Only the file at the root counts; a
/// directory it names is not descended into, so, as in git, a later `!` rule cannot
/// bring back a path under it.
#[derive(Debug)]
pub struct ForkIgnore {
    matcher: Gitignore,
}

impl ForkIgnore {
    /// Parse rules in gitignore syntax, refusing a pattern that does not compile.
    pub fn parse(text: &str) -> Result<Self> {
        let mut builder = GitignoreBuilder::new("");
        for line in text.lines() {
            builder
                .add_line(None, line)
                .map_err(|e| FsError::Ignore(e.to_string()))?;
        }
        let matcher = builder
            .build()
            .map_err(|e| FsError::Ignore(e.to_string()))?;
        Ok(Self { matcher })
    }

    /// [`DEFAULT_FORKIGNORE`].
    pub fn defaults() -> Self {
        Self::parse(DEFAULT_FORKIGNORE).expect("the default rules compile")
    }

    /// The rules of `root/.forkignore`, or the defaults when there is no such file. A
    /// file that exists is the whole rule set: the location may drop a default. It must be
    /// a regular file; a symlink is refused rather than followed.
    pub fn for_root(root: &Path) -> Result<Self> {
        let dir = open_dir(root)?;
        Ok(Self::load(&dir, root)?.0)
    }

    /// [`ForkIgnore::for_root`] through an open root directory, with the bytes the rules
    /// were parsed from (`None` when the defaults apply).
    pub(crate) fn load(dir: &OwnedFd, root: &Path) -> Result<(Self, Option<Vec<u8>>)> {
        let path = root.join(FORKIGNORE);
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let fd = match openat(dir, FORKIGNORE, flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok((Self::defaults(), None)),
            Err(rustix::io::Errno::LOOP) => return Err(not_a_file()),
            Err(e) => return Err(FsError::io(path)(e.into())),
        };
        let mut file = File::from(fd);
        if !file.metadata().map_err(FsError::io(&path))?.is_file() {
            return Err(not_a_file());
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(FsError::io(&path))?;
        Ok((Self::from_bytes(&bytes)?, Some(bytes)))
    }

    /// Rules from the bytes of a `.forkignore`, which must be UTF-8.
    fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| FsError::Ignore("the file is not UTF-8".into()))?;
        Self::parse(text)
    }

    /// The rules a snapshot of a directory holding `tree` follows, read the way a scan
    /// reads them from disk: the `.forkignore` at its root, which must be a file (an
    /// executable counts; a symlink or a folder does not) of UTF-8 text, or the
    /// defaults when there is none. `read` gives a blob's bytes by its id.
    pub fn for_tree(
        tree: &Tree,
        read: impl FnOnce(GitOid) -> std::result::Result<Vec<u8>, String>,
    ) -> Result<Self> {
        match tree.get(FORKIGNORE) {
            // A folder: on disk the scan finds `.forkignore` and refuses it.
            None if tree.entries().any(|(path, _)| {
                path.as_str()
                    .strip_prefix(FORKIGNORE)
                    .is_some_and(|r| r.starts_with('/'))
            }) =>
            {
                Err(not_a_file())
            }
            None => Ok(Self::defaults()),
            Some(Entry::File(oid) | Entry::Executable(oid)) => {
                Self::from_bytes(&read(*oid).map_err(FsError::Ignore)?)
            }
            Some(_) => Err(not_a_file()),
        }
    }

    /// Whether `path` (relative to the root) is ignored.
    pub fn is_ignored(&self, path: impl AsRef<Path>, is_dir: bool) -> bool {
        self.matcher.matched(path, is_dir).is_ignore()
    }

    /// Whether the rules name the file at `path`: the file is ignored, or one of its
    /// directories is (a later `!` rule cannot bring back a path under an ignored
    /// directory, as in git). This is what the walk leaves out, since it does not enter
    /// an ignored directory.
    pub fn excludes(&self, path: &str) -> bool {
        let mut end = 0;
        while let Some(i) = path[end..].find('/') {
            end += i;
            if self.is_ignored(&path[..end], true) {
                return true;
            }
            end += 1;
        }
        self.is_ignored(path, false)
    }
}

fn not_a_file() -> FsError {
    FsError::Ignore(format!("{FORKIGNORE} must be a regular file"))
}

/// Open a directory for an fd-relative walk or checkout. `root` itself may be reached
/// through a symlink; what is under it is never followed.
pub(crate) fn open_dir(root: &Path) -> Result<OwnedFd> {
    openat(
        CWD,
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| FsError::io(root)(e.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cuenca_tree::RepoPath;

    /// A stored tree's rules are read as a scan reads them from disk: a file or an
    /// executable of UTF-8 text, or the defaults when there is none; a symlink or text
    /// that is not UTF-8 is refused, never read loosely.
    #[test]
    fn rules_from_a_tree_are_read_as_strictly_as_from_disk() {
        let rules = b"secret\n".to_vec();
        let at = |entry: Entry| {
            Tree::from_entries([(RepoPath::new(FORKIGNORE).unwrap(), entry)]).unwrap()
        };
        let oid = GitOid::for_blob(&rules);
        let read = |bytes: Vec<u8>| move |_| Ok::<_, String>(bytes);
        for entry in [Entry::File(oid), Entry::Executable(oid)] {
            let got = ForkIgnore::for_tree(&at(entry), read(rules.clone())).unwrap();
            assert!(got.excludes("secret") && !got.excludes(".env"));
        }
        let none = ForkIgnore::for_tree(&Tree::new(), read(vec![])).unwrap();
        assert!(none.excludes(".env"));
        assert!(ForkIgnore::for_tree(&at(Entry::Symlink(oid)), read(rules.clone())).is_err());
        assert!(ForkIgnore::for_tree(&at(Entry::File(oid)), read(vec![b's', 0xff])).is_err());
        assert!(ForkIgnore::for_tree(&at(Entry::File(oid)), |_| Err("gone".into())).is_err());
        // A folder named `.forkignore` is refused, as a scan of it on disk is.
        let folder = Tree::from_entries([(
            RepoPath::new(format!("{FORKIGNORE}/x")).unwrap(),
            Entry::File(oid),
        )])
        .unwrap();
        assert!(ForkIgnore::for_tree(&folder, read(rules.clone())).is_err());
        let sibling = Tree::from_entries([(
            RepoPath::new(format!("{FORKIGNORE}.d/x")).unwrap(),
            Entry::File(oid),
        )])
        .unwrap();
        assert!(
            ForkIgnore::for_tree(&sibling, read(vec![]))
                .unwrap()
                .excludes(".env")
        );
    }

    #[test]
    fn the_defaults_cover_the_spec_list_at_any_depth() {
        let rules = ForkIgnore::defaults();
        for (path, dir) in [
            (".DS_Store", false),
            ("a/b/.DS_Store", false),
            ("._x.txt", false),
            ("docs/._notes.md", false),
            (".Spotlight-V100", true),
            (".Trashes", true),
            (".fseventsd", true),
            ("node_modules", true),
            ("web/node_modules", true),
            ("target", true),
            ("crates/x/target", true),
            (".venv", true),
            (".env", false),
            ("app/.env", false),
        ] {
            assert!(rules.is_ignored(path, dir), "{path} should be ignored");
        }
        for path in ["src/main.rs", ".envrc", "env", "targets.md", "README.md"] {
            assert!(!rules.is_ignored(path, false), "{path} should be kept");
        }
    }

    #[test]
    fn every_env_file_is_a_secret_except_the_committed_examples() {
        let rules = ForkIgnore::defaults();
        for path in [
            ".env.local",
            ".env.production",
            ".env.development.local",
            "web/.env.local",
            "apps/api/.env.test",
            ".env.dev",
        ] {
            assert!(rules.is_ignored(path, false), "{path} should be ignored");
        }
        for path in [
            ".env.example",
            "web/.env.example",
            ".env.production.example",
            ".env.aws.example",
            ".env.sample",
            ".env.template",
            ".envrc",
            "src/env.rs",
        ] {
            assert!(!rules.is_ignored(path, false), "{path} should be kept");
        }
    }

    #[test]
    fn build_output_and_caches_stay_out_at_any_depth() {
        let rules = ForkIgnore::defaults();
        for name in [
            "Pods",
            ".dart_tool",
            ".tox",
            "pkg.egg-info",
            ".direnv",
            ".next",
            ".nuxt",
            ".svelte-kit",
            ".angular",
            ".expo",
            ".output",
            ".vite",
            ".turbo",
            ".parcel-cache",
            ".cache",
            "__pycache__",
            ".pytest_cache",
            ".mypy_cache",
            ".ruff_cache",
            "coverage",
            "DerivedData",
            ".wrangler",
            ".terraform",
        ] {
            for path in [name.to_string(), format!("apps/web/{name}")] {
                assert!(rules.is_ignored(&path, true), "{path} should be ignored");
            }
        }
    }

    #[test]
    fn paths_some_workspace_repository_tracks_are_kept() {
        // Tracked in at least one workspace repository on 2026-10-27 (BRO-3528): a
        // default naming them would fail every snapshot of that location.
        let rules = ForkIgnore::defaults();
        for (path, dir) in [
            ("build", true),
            ("dist", true),
            ("out", true),
            ("run.log", false),
            (".gradle", true),
            (".vercel", true),
            (".vscode", true),
            ("tmp", true),
            ("build.rs", false),
            ("coverage.md", false),
        ] {
            assert!(!rules.is_ignored(path, dir), "{path} should be kept");
        }
    }

    #[test]
    fn a_bad_pattern_is_refused() {
        assert!(matches!(
            ForkIgnore::parse("[z-a]"),
            Err(FsError::Ignore(_))
        ));
    }

    #[test]
    fn negation_and_anchoring_follow_gitignore() {
        let rules = ForkIgnore::parse("*.log\n!keep.log\n/build\n").unwrap();
        assert!(rules.is_ignored("x.log", false));
        assert!(!rules.is_ignored("keep.log", false));
        assert!(rules.is_ignored("build", true));
        assert!(!rules.is_ignored("src/build", true));
    }

    #[test]
    fn excludes_matches_the_walk_under_an_ignored_directory() {
        let rules = ForkIgnore::parse("node_modules\n!node_modules/keep\n").unwrap();
        assert!(
            rules.excludes("node_modules/keep"),
            "git cannot re-include under an ignored dir"
        );
        assert!(rules.excludes("a/node_modules/x/y.js"));
        assert!(!rules.excludes("src/node_modules.rs"));
    }

    #[test]
    fn a_forkignore_that_is_a_symlink_or_a_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("rules"), b".env\n").unwrap();
        std::os::unix::fs::symlink("rules", dir.path().join(FORKIGNORE)).unwrap();
        assert!(matches!(
            ForkIgnore::for_root(dir.path()),
            Err(FsError::Ignore(_))
        ));
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(FORKIGNORE)).unwrap();
        assert!(matches!(
            ForkIgnore::for_root(dir.path()),
            Err(FsError::Ignore(_))
        ));
    }
}
