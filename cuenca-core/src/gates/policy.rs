//! Compiling `.control/policy.yaml` into the settings the built-in gates read.
//!
//! The policy file stays the source (spec section 4). The engine reads the parts of it
//! that already govern landing, as p9 does, plus an optional `cuenca.gates` section:
//!
//! | Policy key | Gate |
//! |---|---|
//! | `gates.hard[id: G4].pattern` | `secret_path` (added to the built-in pattern) |
//! | `auto_merge.rules[action: require_human].path_touched` | `protected_path` |
//! | `auto_merge.gates.governance.{paths, l3_max_per_window, l3_window_seconds}` | `governance_rate` |
//! | `cuenca.gates.max_file_bytes` | `file_size` |
//! | `cuenca.gates.secret_paths`, `cuenca.gates.secret_patterns` | `secret_path`, `secret_content` (added to the built-ins) |
//! | `cuenca.gates.declare_deletions`, `cuenca.gates.declare_reverts` | `deletion`, `revert` (both on by default) |
//! | `cuenca.gates.landing_windows` | `landing_window` |
//!
//! Every other key is ignored, so the rest of the policy can change freely. Under
//! `cuenca.gates` an unknown key is an error, so a misspelt setting is not silently
//! dropped. A policy that cannot be compiled is reported as a failing `policy` result
//! and the built-in defaults run instead; it never makes the gates pass.
//!
//! The file must be UTF-8, at most [`MAX_POLICY_BYTES`], and must not use YAML aliases:
//! the loader copies an aliased node at every use, so a few lines of aliases could
//! expand without bound.

use regex::Regex;
use regex::bytes::Regex as BytesRegex;
use yaml_rust2::Yaml;
use yaml_rust2::parser::{Event, Parser};

/// Where the base's policy is read from.
pub const POLICY_PATH: &str = ".control/policy.yaml";
/// Where the base's check manifest is read from (decision 4 of the build loop: checks
/// for verify come from the base tree). The engine only records its hash.
pub const CHECKS_PATH: &str = ".cuenca/checks.toml";
/// A larger policy file is refused.
pub const MAX_POLICY_BYTES: usize = 1 << 20;
/// Files larger than this fail `file_size` unless the policy says otherwise. GitHub
/// warns at 50 MiB; a source tree rarely needs more than this.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 10 << 20;
/// The built-in secret-file pattern: the workspace's G4.
pub const DEFAULT_SECRET_PATH: &str =
    r"\.(env|env\.local|credentials\.json|service-account\.json)$";
/// A compiled regex may take at most this much memory.
const REGEX_SIZE_LIMIT: usize = 1 << 20;

/// One window in which landing is allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// Days of the week, 0 = Monday.
    pub days: Vec<u8>,
    /// Minutes since local midnight, `from` inclusive, `to` exclusive.
    pub from: u16,
    pub to: u16,
    /// The local time zone as an offset from UTC, in minutes.
    pub utc_offset: i16,
}

/// The settings compiled from one policy file.
#[derive(Debug, Clone)]
pub struct Policy {
    pub secret_paths: Vec<Regex>,
    /// Extra content patterns, by name, on top of the built-ins.
    pub secret_patterns: Vec<(String, BytesRegex)>,
    pub max_file_bytes: u64,
    pub protected: Vec<String>,
    pub governance_paths: Vec<String>,
    /// At most this many changes to governance paths per window; `None`: no limit.
    pub governance_max: Option<u64>,
    pub governance_window_ms: u64,
    pub declare_deletions: bool,
    pub declare_reverts: bool,
    /// Landing is allowed only inside one of these; empty: always.
    pub landing_windows: Vec<Window>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            secret_paths: vec![compile(DEFAULT_SECRET_PATH).expect("built-in pattern")],
            secret_patterns: Vec::new(),
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            protected: Vec::new(),
            governance_paths: Vec::new(),
            governance_max: None,
            governance_window_ms: 86_400_000,
            declare_deletions: true,
            declare_reverts: true,
            landing_windows: Vec::new(),
        }
    }
}

type Res<T> = std::result::Result<T, String>;

impl Policy {
    /// Compile a policy file's bytes.
    pub fn parse(bytes: &[u8]) -> Res<Self> {
        if bytes.len() > MAX_POLICY_BYTES {
            return Err(format!(
                "{POLICY_PATH} is {} bytes, over the {MAX_POLICY_BYTES}-byte limit",
                bytes.len()
            ));
        }
        let text = std::str::from_utf8(bytes).map_err(|_| format!("{POLICY_PATH} is not UTF-8"))?;
        refuse_aliases(text)?;
        let docs = yaml_rust2::YamlLoader::load_from_str(text)
            .map_err(|e| format!("{POLICY_PATH} is not valid YAML: {e}"))?;
        let mut policy = Self::default();
        let root = match docs.as_slice() {
            [] => return Ok(policy),
            [one] => one,
            _ => return Err(format!("{POLICY_PATH} holds more than one YAML document")),
        };
        if root.is_null() {
            return Ok(policy);
        }
        if root.as_hash().is_none() {
            return Err(format!("{POLICY_PATH} is not a mapping"));
        }

        // gates.hard[id: G4].pattern
        if let Some(hard) = list(&root["gates"]["hard"], "gates.hard")? {
            for gate in hard {
                if gate["id"].as_str() == Some("G4")
                    && let Some(pattern) = string(&gate["pattern"], "gates.hard[G4].pattern")?
                {
                    policy.secret_paths.push(
                        compile(&pattern).map_err(|e| format!("gates.hard[G4].pattern: {e}"))?,
                    );
                }
            }
        }

        // auto_merge.rules[action: require_human].path_touched
        if let Some(rules) = list(&root["auto_merge"]["rules"], "auto_merge.rules")? {
            for rule in rules {
                if rule["action"].as_str() == Some("require_human")
                    && let Some(path) =
                        string(&rule["path_touched"], "auto_merge.rules[].path_touched")?
                {
                    if path.is_empty() {
                        return Err("auto_merge.rules[].path_touched is empty".into());
                    }
                    policy.protected.push(path);
                }
            }
        }

        // auto_merge.gates.governance
        let gov = &root["auto_merge"]["gates"]["governance"];
        if let Some(paths) = strings(&gov["paths"], "auto_merge.gates.governance.paths")? {
            policy.governance_paths = paths;
        }
        if let Some(max) = int(
            &gov["l3_max_per_window"],
            "auto_merge.gates.governance.l3_max_per_window",
        )? {
            policy.governance_max = Some(max);
        }
        if let Some(window) = int(
            &gov["l3_window_seconds"],
            "auto_merge.gates.governance.l3_window_seconds",
        )? {
            if window == 0 {
                return Err("auto_merge.gates.governance.l3_window_seconds is 0".into());
            }
            policy.governance_window_ms = window.saturating_mul(1000);
        }

        // cuenca.gates
        let section = &root["cuenca"]["gates"];
        if let Some(map) = section.as_hash() {
            for key in map.keys() {
                let known = [
                    "max_file_bytes",
                    "secret_paths",
                    "secret_patterns",
                    "declare_deletions",
                    "declare_reverts",
                    "landing_windows",
                ];
                match key.as_str() {
                    Some(k) if known.contains(&k) => {}
                    _ => return Err(format!("cuenca.gates: unknown key {key:?}")),
                }
            }
        } else if !section.is_badvalue() && !section.is_null() {
            return Err("cuenca.gates is not a mapping".into());
        }
        if let Some(max) = int(&section["max_file_bytes"], "cuenca.gates.max_file_bytes")? {
            policy.max_file_bytes = max;
        }
        if let Some(paths) = strings(&section["secret_paths"], "cuenca.gates.secret_paths")? {
            for p in paths {
                policy
                    .secret_paths
                    .push(compile(&p).map_err(|e| format!("cuenca.gates.secret_paths: {e}"))?);
            }
        }
        if let Some(patterns) =
            strings(&section["secret_patterns"], "cuenca.gates.secret_patterns")?
        {
            for (i, p) in patterns.into_iter().enumerate() {
                let re =
                    compile_bytes(&p).map_err(|e| format!("cuenca.gates.secret_patterns: {e}"))?;
                policy
                    .secret_patterns
                    .push((format!("policy pattern {}", i + 1), re));
            }
        }
        if let Some(on) = boolean(
            &section["declare_deletions"],
            "cuenca.gates.declare_deletions",
        )? {
            policy.declare_deletions = on;
        }
        if let Some(on) = boolean(&section["declare_reverts"], "cuenca.gates.declare_reverts")? {
            policy.declare_reverts = on;
        }
        if let Some(windows) = list(&section["landing_windows"], "cuenca.gates.landing_windows")? {
            if windows.is_empty() {
                return Err(
                    "cuenca.gates.landing_windows is empty: landing would never be allowed".into(),
                );
            }
            for w in windows {
                policy.landing_windows.push(window(w)?);
            }
        }
        Ok(policy)
    }
}

fn refuse_aliases(text: &str) -> Res<()> {
    let mut parser = Parser::new_from_str(text);
    loop {
        match parser.next_token() {
            Ok((Event::StreamEnd, _)) => return Ok(()),
            Ok((Event::Alias(_), mark)) => {
                return Err(format!(
                    "{POLICY_PATH} uses a YAML alias (line {}), which the gate engine refuses",
                    mark.line()
                ));
            }
            Ok(_) => {}
            Err(e) => return Err(format!("{POLICY_PATH} is not valid YAML: {e}")),
        }
    }
}

fn compile(pattern: &str) -> std::result::Result<Regex, regex::Error> {
    regex::RegexBuilder::new(pattern)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
}

pub(super) fn compile_bytes(pattern: &str) -> std::result::Result<BytesRegex, regex::Error> {
    regex::bytes::RegexBuilder::new(pattern)
        .size_limit(REGEX_SIZE_LIMIT)
        .build()
}

fn absent(y: &Yaml) -> bool {
    y.is_badvalue() || y.is_null()
}

fn list<'y>(y: &'y Yaml, key: &str) -> Res<Option<&'y Vec<Yaml>>> {
    if absent(y) {
        return Ok(None);
    }
    y.as_vec()
        .map(Some)
        .ok_or_else(|| format!("{key} is not a list"))
}

fn string(y: &Yaml, key: &str) -> Res<Option<String>> {
    if absent(y) {
        return Ok(None);
    }
    y.as_str()
        .map(|s| Some(s.to_string()))
        .ok_or_else(|| format!("{key} is not a string"))
}

fn strings(y: &Yaml, key: &str) -> Res<Option<Vec<String>>> {
    let Some(items) = list(y, key)? else {
        return Ok(None);
    };
    items
        .iter()
        .map(|i| {
            i.as_str()
                .map(str::to_string)
                .ok_or_else(|| format!("{key} holds a non-string"))
        })
        .collect::<Res<Vec<_>>>()
        .map(Some)
}

fn int(y: &Yaml, key: &str) -> Res<Option<u64>> {
    if absent(y) {
        return Ok(None);
    }
    match y.as_i64() {
        Some(n) if n >= 0 => Ok(Some(n as u64)),
        _ => Err(format!("{key} is not a non-negative integer")),
    }
}

fn boolean(y: &Yaml, key: &str) -> Res<Option<bool>> {
    if absent(y) {
        return Ok(None);
    }
    y.as_bool()
        .map(Some)
        .ok_or_else(|| format!("{key} is not true or false"))
}

const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

fn window(y: &Yaml) -> Res<Window> {
    const KEY: &str = "cuenca.gates.landing_windows[]";
    let Some(map) = y.as_hash() else {
        return Err(format!("{KEY} is not a mapping"));
    };
    for key in map.keys() {
        if !matches!(key.as_str(), Some("days" | "from" | "to" | "utc_offset")) {
            return Err(format!("{KEY}: unknown key {key:?}"));
        }
    }
    let days = strings(&y["days"], &format!("{KEY}.days"))?
        .ok_or_else(|| format!("{KEY}.days is missing"))?
        .iter()
        .map(|d| {
            DAYS.iter()
                .position(|x| x.eq_ignore_ascii_case(d))
                .map(|i| i as u8)
                .ok_or_else(|| format!("{KEY}.days: {d:?} is not one of {DAYS:?}"))
        })
        .collect::<Res<Vec<_>>>()?;
    if days.is_empty() {
        return Err(format!("{KEY}.days is empty"));
    }
    let clock = |field: &str| -> Res<u16> {
        let s = string(&y[field], &format!("{KEY}.{field}"))?
            .ok_or_else(|| format!("{KEY}.{field} is missing"))?;
        parse_clock(&s).ok_or_else(|| format!("{KEY}.{field}: {s:?} is not HH:MM"))
    };
    let (from, to) = (clock("from")?, clock("to")?);
    if from >= to {
        return Err(format!(
            "{KEY}: from must be before to (a window may not cross midnight; use two)"
        ));
    }
    let utc_offset = match string(&y["utc_offset"], &format!("{KEY}.utc_offset"))? {
        None => 0,
        Some(s) => {
            parse_offset(&s).ok_or_else(|| format!("{KEY}.utc_offset: {s:?} is not ±HH:MM"))?
        }
    };
    Ok(Window {
        days,
        from,
        to,
        utc_offset,
    })
}

/// `HH:MM`, 00:00 to 24:00, as minutes.
fn parse_clock(s: &str) -> Option<u16> {
    let (h, m) = s.split_once(':')?;
    if h.len() != 2 || m.len() != 2 {
        return None;
    }
    let (h, m): (u16, u16) = (h.parse().ok()?, m.parse().ok()?);
    let minutes = h * 60 + m;
    (m < 60 && minutes <= 24 * 60).then_some(minutes)
}

/// `+HH:MM` or `-HH:MM`, at most 14 hours, as minutes.
fn parse_offset(s: &str) -> Option<i16> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let minutes = parse_clock(rest)?;
    (minutes <= 14 * 60).then_some(sign * minutes as i16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_absent_or_empty_policy_is_the_defaults() {
        let p = Policy::parse(b"").unwrap();
        assert_eq!(p.secret_paths.len(), 1);
        assert!(p.protected.is_empty() && p.declare_deletions && p.declare_reverts);
        assert_eq!(p.max_file_bytes, DEFAULT_MAX_FILE_BYTES);
    }

    #[test]
    fn the_workspace_keys_compile() {
        let p = Policy::parse(
            br#"
gates:
  hard:
    - id: G1
      pattern: "git push.*(?!x)"
    - id: G4
      pattern: "\\.pem$"
auto_merge:
  rules:
    - path_touched: .control/policy.yaml
      action: require_human
    - path_touched: docs/
      action: auto
  gates:
    governance:
      paths: [CLAUDE.md, AGENTS.md]
      l3_max_per_window: 1
      l3_window_seconds: 3600
cuenca:
  gates:
    max_file_bytes: 100
    declare_reverts: false
    landing_windows:
      - { days: [mon, fri], from: "09:00", to: "17:30", utc_offset: "-05:00" }
"#,
        )
        .unwrap();
        // G1's lookahead is not compiled: only G4 is read.
        assert_eq!(p.secret_paths.len(), 2);
        assert!(p.secret_paths[1].is_match("key.pem"));
        assert_eq!(p.protected, [".control/policy.yaml"]);
        assert_eq!(p.governance_paths, ["CLAUDE.md", "AGENTS.md"]);
        assert_eq!(p.governance_max, Some(1));
        assert_eq!(p.governance_window_ms, 3_600_000);
        assert_eq!(p.max_file_bytes, 100);
        assert!(p.declare_deletions && !p.declare_reverts);
        assert_eq!(
            p.landing_windows,
            [Window {
                days: vec![0, 4],
                from: 540,
                to: 1050,
                utc_offset: -300
            }]
        );
    }

    #[test]
    fn a_bad_policy_is_an_error_not_defaults() {
        for bad in [
            &b"a: [1"[..],
            b"- just a list",
            b"cuenca:\n  gates:\n    max_file_byte: 1\n",
            b"cuenca:\n  gates:\n    max_file_bytes: -1\n",
            b"cuenca:\n  gates:\n    landing_windows: []\n",
            b"cuenca:\n  gates:\n    landing_windows: [{days: [mon], from: '10:00', to: '09:00'}]\n",
            b"gates:\n  hard:\n    - id: G4\n      pattern: '('\n",
            b"base: &a [x, x]\nmore: [*a, *a]\n",
            b"auto_merge:\n  rules:\n    - path_touched: ''\n      action: require_human\n",
            b"\xff\xfe",
            b"a: 1\n---\nb: 2\n",
        ] {
            assert!(Policy::parse(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
        assert!(Policy::parse(&vec![b' '; MAX_POLICY_BYTES + 1]).is_err());
    }

    #[test]
    fn clocks_and_offsets() {
        assert_eq!(parse_clock("00:00"), Some(0));
        assert_eq!(parse_clock("24:00"), Some(1440));
        assert_eq!(parse_clock("24:01"), None);
        assert_eq!(parse_clock("9:00"), None);
        assert_eq!(parse_clock("09:60"), None);
        assert_eq!(parse_offset("+05:30"), Some(330));
        assert_eq!(parse_offset("-14:00"), Some(-840));
        assert_eq!(parse_offset("-14:01"), None);
        assert_eq!(parse_offset("05:00"), None);
    }
}
