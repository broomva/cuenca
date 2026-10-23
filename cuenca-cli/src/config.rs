//! Where the CLI keeps the authority's URL and the caller's key, and its per-checkout
//! state.
//!
//! The config file is `$CUENCA_CONFIG`, else `$XDG_CONFIG_HOME/cuenca/config.json`, else
//! `~/.config/cuenca/config.json`, written with mode 0600. `CUENCA_URL` and
//! `CUENCA_TOKEN` override what it holds. The token is never printed.

use std::fmt;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct Config {
    /// The authority's agent API, e.g. `http://100.82.195.109:7419`.
    pub url: String,
    /// The bearer key minted for this principal.
    pub token: String,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("url", &self.url)
            .field("token", &"<redacted>")
            .finish()
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn home() -> anyhow::Result<PathBuf> {
    env("HOME").map(PathBuf::from).context("HOME is not set")
}

/// The config file's path.
pub fn config_path() -> anyhow::Result<PathBuf> {
    if let Some(p) = env("CUENCA_CONFIG") {
        return Ok(p.into());
    }
    let base = match env("XDG_CONFIG_HOME") {
        Some(d) => PathBuf::from(d),
        None => home()?.join(".config"),
    };
    Ok(base.join("cuenca").join("config.json"))
}

/// The directory holding per-checkout state: `$CUENCA_STATE_HOME`, else
/// `$XDG_STATE_HOME/cuenca`, else `~/.local/state/cuenca`.
pub fn state_home() -> anyhow::Result<PathBuf> {
    if let Some(p) = env("CUENCA_STATE_HOME") {
        return Ok(p.into());
    }
    Ok(match env("XDG_STATE_HOME") {
        Some(d) => PathBuf::from(d).join("cuenca"),
        None => home()?.join(".local").join("state").join("cuenca"),
    })
}

impl Config {
    /// The config, with `CUENCA_URL` and `CUENCA_TOKEN` applied over the file.
    pub fn load() -> anyhow::Result<Self> {
        let path = config_path()?;
        let file = match std::fs::read(&path) {
            Ok(bytes) => Some(
                serde_json::from_slice::<Self>(&bytes)
                    .with_context(|| format!("{} is not a cuenca config", path.display()))?,
            ),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let url = env("CUENCA_URL").or_else(|| file.as_ref().map(|c| c.url.clone()));
        let token = env("CUENCA_TOKEN").or_else(|| file.as_ref().map(|c| c.token.clone()));
        match (url, token) {
            (Some(url), Some(token)) => Ok(Self { url, token }),
            _ => bail!(
                "not logged in: run `cuenca login --url <authority> --token-stdin`, \
                 or set CUENCA_URL and CUENCA_TOKEN"
            ),
        }
    }

    /// Write the config file atomically, mode 0600.
    pub fn save(&self) -> anyhow::Result<PathBuf> {
        let path = config_path()?;
        let dir = path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        tmp.write_all(&serde_json::to_vec_pretty(self)?)?;
        tmp.as_file().sync_all()?;
        tmp.persist(&path)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }
}
