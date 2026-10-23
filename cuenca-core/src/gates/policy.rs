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
//! | `cuenca.gates.max_file_bytes` | `file_size` (may only lower the built-in limit) |
//! | `cuenca.gates.secret_paths`, `cuenca.gates.secret_patterns` | `secret_path`, `secret_content` (added to the built-ins) |
//! | `cuenca.gates.landing_windows` | `landing_window` |
//!
//! `deletion`, `revert` and `fast_forward` always run: the Deletion Gate has no off
//! switch.
//!
//! **Floors, not defaults.** A base policy can tighten a gate but never loosen one past
//! what the rest of the workspace enforces. The governance tier follows p9 exactly
//! (`p9.py`, `GovernanceTier`): once a policy has `auto_merge.gates`, the tier applies,
//! with p9's five L3 paths as a floor (a policy may add paths, never drop one; compared
//! case-folded), `l3_max_per_window` exactly 1, and `l3_window_seconds` between one
//! day and 30 days. A value of these keys that p9 would refuse is a compile error here
//! too, never a looser gate. p9's other keys (`required_strata`, `required_checks`,
//! `p20`, `review_bots`) feed no gate here; they are p9's to validate. A location whose policy has no `auto_merge.gates` (any directory tree, not a
//! workspace) has no governance tier, as under p9.
//!
//! Top-level keys other than `gates`, `auto_merge` and `cuenca` are ignored, so the
//! rest of the policy can change freely. Under `auto_merge` and `auto_merge.gates` an
//! unknown key is an error, as under p9, and so it is under `cuenca`, `cuenca.gates` and
//! `auto_merge.gates.governance`, so a misspelt setting, or a misspelt section holding
//! settings, is not silently dropped. A key the engine reads must hold its type
//! when it is present: only a missing key is absent. `key:` with no value is an error,
//! except `auto_merge.gates` and its `governance`, which p9 reads as its defaults. So
//! `auto_merge: ~` is an error here, where p9 reads it as auto-merge switched off:
//! here it would switch the gates off. Every `auto_merge.rules` item is checked as p9
//! checks it (an action p9 knows, and exactly one of `branch_pattern` and
//! `path_touched`), so a misspelt rule is an error rather than a rule that protects
//! nothing. A policy that cannot be compiled is
//! reported as a failing `policy` result and the built-in defaults run instead; it
//! never makes the gates pass.
//!
//! The file must be UTF-8, at most [`MAX_POLICY_BYTES`], nest at most
//! [`MAX_POLICY_DEPTH`] levels, and must not use YAML aliases: the loader copies an
//! aliased node at every use, so a few lines of aliases could expand without bound, and
//! it recurses per level, so a deep document could overflow the stack.

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
/// The built-in secret-file pattern: the workspace's G4, case-folded (on the Mac's
/// disk `.ENV` and `.env` name one file).
pub const DEFAULT_SECRET_PATH: &str =
    r"(?i)\.(env|env\.local|credentials\.json|service-account\.json)$";
/// p9's L3 governance paths: a policy may add to them, never drop one.
pub const GOVERNANCE_PATHS: [&str; 5] = [
    "CLAUDE.md",
    "AGENTS.md",
    "METALAYER.md",
    ".control/policy.yaml",
    ".control/rcs-parameters.toml",
];
/// p9's bounds on the governance window, in seconds: one day to 30 days.
pub const GOVERNANCE_WINDOW_SECONDS: (u64, u64) = (86_400, 30 * 86_400);
/// At most this many secret patterns, paths and content together, the built-ins (one
/// path pattern, ten content patterns) included: each runs on every path or every new
/// blob.
pub const MAX_POLICY_PATTERNS: usize = 64;
/// At most this many protected rules and governance paths together: each is matched
/// against every path a changeset touches. The workspace's policy has 38.
pub const MAX_POLICY_RULES: usize = 256;
/// A policy may nest at most this deep; the workspace's nests 5 levels.
pub const MAX_POLICY_DEPTH: usize = 32;
/// p9's rule actions; a rule without one is `notify`.
const RULE_ACTIONS: [&str; 3] = ["auto", "require_human", "notify"];
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

/// p9's governance tier as the gates read it: one change to `paths` per window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Governance {
    pub paths: Vec<String>,
    pub window_ms: u64,
}

/// The settings compiled from one policy file.
#[derive(Debug, Clone)]
pub struct Policy {
    pub secret_paths: Vec<Regex>,
    /// Extra content patterns, by name, on top of the built-ins.
    pub secret_patterns: Vec<(String, BytesRegex)>,
    pub max_file_bytes: u64,
    pub protected: Vec<String>,
    /// The governance tier: at most one change to these paths per window. `None` when
    /// the policy has no `auto_merge.gates`.
    pub governance: Option<Governance>,
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
            governance: None,
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
        // YAML allows a byte-order mark at the start of the stream, and p9's loader
        // skips it. The YAML loader here would make it part of the first key, so
        // `\u{feff}auto_merge` would read as no `auto_merge` (r4, B).
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        prescan(text)?;
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

        let gates = mapping(field(root, "gates"), "gates")?;
        let auto_merge = mapping(field(root, "auto_merge"), "auto_merge")?;
        let cuenca = mapping(field(root, "cuenca"), "cuenca")?;
        // A misspelt key one level up from the settings would drop them all as absent
        // (r5, A, B and C): `auto_merge` has p9's keys (`_parse_auto_merge`), and
        // `cuenca` is the engine's own.
        only_keys(
            auto_merge,
            &[
                "enabled",
                "require_no_requested_changes",
                "require_branch_up_to_date",
                "merge_method",
                "delete_branch",
                "rules",
                "default_action",
                "gates",
            ],
            "auto_merge",
        )?;
        only_keys(cuenca, &["gates"], "cuenca")?;

        // gates.hard[id: G4].pattern
        if let Some(hard) = list(field(gates, "hard"), "gates.hard")? {
            for gate in hard {
                let gate = mapping(Some(gate), "gates.hard[]")?;
                if field(gate, "id").and_then(Yaml::as_str) == Some("G4") {
                    let pattern = string(field(gate, "pattern"), "gates.hard[G4].pattern")?
                        .ok_or("gates.hard[G4].pattern is missing")?;
                    one_more_pattern(&policy)?;
                    policy.secret_paths.push(
                        compile(&pattern)
                            .map_err(|e| not_compiled("gates.hard[G4].pattern", &e))?,
                    );
                }
            }
        }

        // auto_merge.rules[action: require_human].path_touched. Every rule is checked
        // as p9 checks it, so a misspelt action or target is an error, never a rule
        // that protects nothing (r4, C).
        if let Some(rules) = list(field(auto_merge, "rules"), "auto_merge.rules")? {
            for (i, rule) in rules.iter().enumerate() {
                let key = format!("auto_merge.rules[{i}]");
                let rule = mapping(Some(rule), &key)?;
                let action = string(field(rule, "action"), &format!("{key}.action"))?;
                let action = action.as_deref().unwrap_or("notify");
                if !RULE_ACTIONS.contains(&action) {
                    return Err(format!("{key}.action is not one of {RULE_ACTIONS:?}"));
                }
                let branch = string(
                    field(rule, "branch_pattern"),
                    &format!("{key}.branch_pattern"),
                )?;
                let path = string(field(rule, "path_touched"), &format!("{key}.path_touched"))?;
                let path = match (
                    branch.filter(|b| !b.is_empty()),
                    path.filter(|p| !p.is_empty()),
                ) {
                    (Some(_), Some(_)) => {
                        return Err(format!("{key} sets both branch_pattern and path_touched"));
                    }
                    (None, None) => {
                        return Err(format!(
                            "{key} sets neither branch_pattern nor path_touched"
                        ));
                    }
                    (_, path) => path,
                };
                if action == "require_human"
                    && let Some(path) = path
                {
                    policy.protected.push(path);
                }
            }
        }

        policy.governance = governance(field(auto_merge, "gates"))?;

        // cuenca.gates
        let section = mapping(field(cuenca, "gates"), "cuenca.gates")?;
        only_keys(
            section,
            &[
                "max_file_bytes",
                "secret_paths",
                "secret_patterns",
                "landing_windows",
            ],
            "cuenca.gates",
        )?;
        if let Some(max) = int(
            field(section, "max_file_bytes"),
            "cuenca.gates.max_file_bytes",
        )? {
            if max > DEFAULT_MAX_FILE_BYTES {
                return Err(format!(
                    "cuenca.gates.max_file_bytes is {max}: a policy may lower the \
                     {DEFAULT_MAX_FILE_BYTES}-byte limit, not raise it"
                ));
            }
            policy.max_file_bytes = max;
        }
        if let Some(paths) = strings(field(section, "secret_paths"), "cuenca.gates.secret_paths")? {
            for p in paths {
                one_more_pattern(&policy)?;
                policy.secret_paths.push(
                    compile(&p).map_err(|e| not_compiled("cuenca.gates.secret_paths[]", &e))?,
                );
            }
        }
        if let Some(patterns) = strings(
            field(section, "secret_patterns"),
            "cuenca.gates.secret_patterns",
        )? {
            for (i, p) in patterns.into_iter().enumerate() {
                one_more_pattern(&policy)?;
                let re = compile_bytes(&p)
                    .map_err(|e| not_compiled("cuenca.gates.secret_patterns[]", &e))?;
                policy
                    .secret_patterns
                    .push((format!("policy pattern {}", i + 1), re));
            }
        }
        if let Some(windows) = list(
            field(section, "landing_windows"),
            "cuenca.gates.landing_windows",
        )? {
            if windows.is_empty() {
                return Err(
                    "cuenca.gates.landing_windows is empty: landing would never be allowed".into(),
                );
            }
            for w in windows {
                policy.landing_windows.push(window(w)?);
            }
        }
        let rules =
            policy.protected.len() + policy.governance.as_ref().map_or(0, |g| g.paths.len());
        if rules > MAX_POLICY_RULES {
            return Err(format!(
                "{rules} protected and governance paths, over the limit of {MAX_POLICY_RULES}"
            ));
        }
        Ok(policy)
    }
}

/// The governance tier from `auto_merge.gates`, with p9's defaults and floors.
///
/// `gates:` and `governance:` with no value are the defaults, as in p9 (it reads them
/// as gated mode with its defaults), so a null there is the strict reading.
fn governance(gates: Option<&Yaml>) -> Res<Option<Governance>> {
    const KEY: &str = "auto_merge.gates.governance";
    let gates = match gates {
        None => return Ok(None),
        Some(Yaml::Null) => &NULL,
        Some(gates) => mapping(Some(gates), "auto_merge.gates")?,
    };
    // p9's keys (`_parse_gates`): a misspelt `governance` would drop the tier (r5).
    only_keys(
        gates,
        &["review_bots", "aggregate_checks", "p20", "governance"],
        "auto_merge.gates",
    )?;
    let gov = match field(gates, "governance") {
        None | Some(Yaml::Null) => &NULL,
        gov => mapping(gov, KEY)?,
    };
    only_keys(
        gov,
        &[
            "paths",
            "required_strata",
            "required_checks",
            "l3_max_per_window",
            "l3_window_seconds",
        ],
        KEY,
    )?;
    let paths = match strings(field(gov, "paths"), &format!("{KEY}.paths"))? {
        None => GOVERNANCE_PATHS.iter().map(|p| p.to_string()).collect(),
        Some(paths) => {
            let have: Vec<String> = paths.iter().map(|p| p.to_lowercase()).collect();
            let missing: Vec<&str> = GOVERNANCE_PATHS
                .iter()
                .copied()
                .filter(|p| !have.contains(&p.to_lowercase()))
                .collect();
            if !missing.is_empty() {
                return Err(format!(
                    "{KEY}.paths may add paths but not drop the L3 set; missing {missing:?}"
                ));
            }
            paths
        }
    };
    if let Some(max) = int(
        field(gov, "l3_max_per_window"),
        &format!("{KEY}.l3_max_per_window"),
    )? && max != 1
    {
        return Err(format!("{KEY}.l3_max_per_window is {max}; it must be 1"));
    }
    let (lo, hi) = GOVERNANCE_WINDOW_SECONDS;
    let window = int(
        field(gov, "l3_window_seconds"),
        &format!("{KEY}.l3_window_seconds"),
    )?
    .unwrap_or(lo);
    if !(lo..=hi).contains(&window) {
        return Err(format!(
            "{KEY}.l3_window_seconds is {window}; it must be between {lo} and {hi}"
        ));
    }
    Ok(Some(Governance {
        paths,
        window_ms: window * 1000,
    }))
}

/// Read the policy's event stream once, iteratively, before the loader builds it:
/// refuse aliases (the loader copies an aliased node at every use) and nesting deeper
/// than [`MAX_POLICY_DEPTH`] (the loader and its values recurse per level, and a
/// deep enough document overflows the evaluating thread's stack).
fn prescan(text: &str) -> Res<()> {
    let mut parser = Parser::new_from_str(text);
    let mut depth = 0usize;
    loop {
        match parser.next_token() {
            Ok((Event::StreamEnd, _)) => return Ok(()),
            Ok((Event::SequenceStart(..) | Event::MappingStart(..), mark)) => {
                depth += 1;
                if depth > MAX_POLICY_DEPTH {
                    return Err(format!(
                        "{POLICY_PATH} nests deeper than {MAX_POLICY_DEPTH} levels (line {})",
                        mark.line()
                    ));
                }
            }
            Ok((Event::SequenceEnd | Event::MappingEnd, _)) => depth = depth.saturating_sub(1),
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

/// A pattern that does not compile, named by its key and never quoted: a pattern can
/// be a secret's own prefix, and errors are journaled where fork readers see them.
fn not_compiled(key: &str, e: &regex::Error) -> String {
    match e {
        regex::Error::CompiledTooBig(limit) => {
            format!("{key}: a pattern compiles over the {limit}-byte limit")
        }
        _ => format!("{key}: a pattern is not a valid regular expression"),
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

/// What a missing section reads as: no keys.
static NULL: Yaml = Yaml::Null;

/// The value of `key` in the mapping `y`; `None` only when the key is missing (or `y`
/// is not a mapping, which only a missing parent section is by then). Indexing
/// (`y[key]`) answers alike for a missing key, a key of a scalar and a tag that failed
/// to convert, and that is how a malformed section kept reading as absent and dropping
/// its gates (r3 and r4). Here a key that is present must hold its type: `key:` with
/// no value and a failed tag are errors, except where [`governance`] reads p9's
/// defaults.
fn field<'y>(y: &'y Yaml, key: &str) -> Option<&'y Yaml> {
    y.as_hash()?.get(&Yaml::String(key.to_string()))
}

/// Refuses a key of mapping `y` (at `at`) that is not in `known`.
fn only_keys(y: &Yaml, known: &[&str], at: &str) -> Res<()> {
    for key in y.as_hash().into_iter().flat_map(|m| m.keys()) {
        match key.as_str() {
            Some(k) if known.contains(&k) => {}
            _ => return Err(format!("{at}: unknown key {key:?}")),
        }
    }
    Ok(())
}

/// Refuses a pattern before it is compiled once the cap (built-ins included) is
/// reached, so a policy listing thousands costs at most the cap in compiles (r3, B).
fn one_more_pattern(policy: &Policy) -> Res<()> {
    let patterns = policy.secret_paths.len()
        + policy.secret_patterns.len()
        + super::builtin::SECRET_PATTERNS.len();
    if patterns >= MAX_POLICY_PATTERNS {
        return Err(format!(
            "more than {MAX_POLICY_PATTERNS} secret patterns, built-ins included"
        ));
    }
    Ok(())
}

/// A section that must be a mapping when present; a missing one has no keys.
fn mapping<'y>(y: Option<&'y Yaml>, key: &str) -> Res<&'y Yaml> {
    match y {
        None => Ok(&NULL),
        Some(y) if y.as_hash().is_some() => Ok(y),
        Some(_) => Err(format!("{key} is not a mapping")),
    }
}

fn list<'y>(y: Option<&'y Yaml>, key: &str) -> Res<Option<&'y Vec<Yaml>>> {
    let Some(y) = y else {
        return Ok(None);
    };
    y.as_vec()
        .map(Some)
        .ok_or_else(|| format!("{key} is not a list"))
}

fn string(y: Option<&Yaml>, key: &str) -> Res<Option<String>> {
    let Some(y) = y else {
        return Ok(None);
    };
    y.as_str()
        .map(|s| Some(s.to_string()))
        .ok_or_else(|| format!("{key} is not a string"))
}

fn strings(y: Option<&Yaml>, key: &str) -> Res<Option<Vec<String>>> {
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

fn int(y: Option<&Yaml>, key: &str) -> Res<Option<u64>> {
    let Some(y) = y else {
        return Ok(None);
    };
    match y.as_i64() {
        Some(n) if n >= 0 => Ok(Some(n as u64)),
        _ => Err(format!("{key} is not a non-negative integer")),
    }
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
    let days = strings(field(y, "days"), &format!("{KEY}.days"))?
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
    let clock = |name: &str| -> Res<u16> {
        let s = string(field(y, name), &format!("{KEY}.{name}"))?
            .ok_or_else(|| format!("{KEY}.{name} is missing"))?;
        parse_clock(&s).ok_or_else(|| format!("{KEY}.{name}: {s:?} is not HH:MM"))
    };
    let (from, to) = (clock("from")?, clock("to")?);
    if from >= to {
        return Err(format!(
            "{KEY}: from must be before to (a window may not cross midnight; use two)"
        ));
    }
    let utc_offset = match string(field(y, "utc_offset"), &format!("{KEY}.utc_offset"))? {
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
        assert!(p.secret_paths[0].is_match("app/.ENV"));
        assert!(p.protected.is_empty() && p.governance.is_none());
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
      paths: [claude.md, AGENTS.md, METALAYER.md, .control/policy.yaml, .control/rcs-parameters.toml, docs/ADR*]
      l3_max_per_window: 1
      l3_window_seconds: 172800
cuenca:
  gates:
    max_file_bytes: 100
    landing_windows:
      - { days: [mon, fri], from: "09:00", to: "17:30", utc_offset: "-05:00" }
"#,
        )
        .unwrap();
        // G1's lookahead is not compiled: only G4 is read.
        assert_eq!(p.secret_paths.len(), 2);
        assert!(p.secret_paths[1].is_match("key.pem"));
        assert_eq!(p.protected, [".control/policy.yaml"]);
        let gov = p.governance.unwrap();
        assert_eq!(gov.paths.len(), 6);
        assert_eq!(gov.window_ms, 172_800_000);
        assert_eq!(p.max_file_bytes, 100);
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
            // p9's floors: no looser governance tier, no larger file limit, no
            // switching the Deletion Gate off.
            b"auto_merge:\n  gates:\n    governance:\n      paths: [CLAUDE.md]\n",
            b"auto_merge:\n  gates:\n    governance:\n      l3_max_per_window: 2\n",
            b"auto_merge:\n  gates:\n    governance:\n      l3_max_per_window: 0\n",
            b"auto_merge:\n  gates:\n    governance:\n      l3_window_seconds: 86399\n",
            b"auto_merge:\n  gates:\n    governance:\n      l3_window_seconds: 2592001\n",
            b"auto_merge:\n  gates:\n    governance:\n      l3_max: 1\n",
            b"auto_merge:\n  gates: [1]\n",
            b"cuenca:\n  gates:\n    max_file_bytes: 10485761\n",
            b"cuenca:\n  gates:\n    declare_deletions: false\n",
            // A section of the wrong type is an error, never "absent" (r3, A):
            // otherwise `auto_merge: disabled` silently drops the protected paths
            // and the governance tier.
            b"auto_merge: disabled\n",
            b"auto_merge: [rules]\n",
            b"gates: off\n",
            b"cuenca: 1\n",
            b"cuenca:\n  gates: on\n",
            b"auto_merge:\n  rules: [require_human]\n",
            b"gates:\n  hard: [G4]\n",
            // A present key with no value, or with a tag that failed to convert, is
            // not a missing key (r4, A and C): `auto_merge: ~` would drop the same
            // gates as `auto_merge: disabled`.
            b"auto_merge:\n",
            b"auto_merge: ~\n",
            b"gates: null\n",
            b"cuenca:\n",
            // A misspelt section one level up reads as absent too, unless the parent's
            // keys are pinned (r5, A, B and C).
            b"auto_merge:\n  rule: [{path_touched: CLAUDE.md, action: require_human}]\n",
            b"auto_merge:\n  gate:\n    governance: {}\n",
            b"auto_merge:\n  gates:\n    governanse: {}\n",
            b"cuenca:\n  gate: {max_file_bytes: 1}\n",
            b"cuenca:\n  gates:\n",
            b"auto_merge:\n  rules:\n",
            b"auto_merge: !!int nope\n",
            b"cuenca:\n  gates:\n    max_file_bytes: !!int 99999999999999999999999\n",
            b"auto_merge:\n  gates:\n    governance:\n      paths: ~\n",
            b"auto_merge:\n  gates:\n    governance:\n      l3_max_per_window: ~\n",
            // A rule is checked as p9 checks it (r4, C): a misspelt action or a
            // missing target protects nothing, silently.
            b"auto_merge:\n  rules:\n    - {path_touched: CLAUDE.md, action: require-human}\n",
            b"auto_merge:\n  rules:\n    - {path_touched: CLAUDE.md, action: [require_human]}\n",
            b"auto_merge:\n  rules:\n    - {path_touchd: CLAUDE.md, action: require_human}\n",
            b"auto_merge:\n  rules:\n    - {action: require_human}\n",
            b"auto_merge:\n  rules:\n    - {path_touched: CLAUDE.md, branch_pattern: x, action: notify}\n",
            b"gates:\n  hard:\n    - id: G4\n      patern: '\\.pem$'\n",
        ] {
            assert!(Policy::parse(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
        assert!(Policy::parse(&vec![b' '; MAX_POLICY_BYTES + 1]).is_err());
    }

    /// A byte-order mark is skipped, as p9's loader skips it, and never turns the first
    /// key into an unknown one (r4, B: `\u{feff}auto_merge` read as no `auto_merge`).
    #[test]
    fn a_byte_order_mark_is_not_part_of_the_first_key() {
        let p = Policy::parse(
            b"\xef\xbb\xbfauto_merge:\n  rules:\n    - {path_touched: /CLAUDE.md, action: require_human}\n  gates: {}\n",
        )
        .unwrap();
        assert_eq!(p.protected, ["/CLAUDE.md"]);
        assert!(p.governance.is_some());
    }

    /// p9's rule defaults: a rule without an action is `notify`, and only
    /// `require_human` rules protect a path.
    #[test]
    fn rules_follow_p9() {
        let p = Policy::parse(
            b"auto_merge:\n  rules:\n    - {path_touched: a/}\n    - {branch_pattern: 'release/*', action: require_human}\n    - {path_touched: b/, action: auto}\n    - {path_touched: c/, action: require_human}\n",
        )
        .unwrap();
        assert_eq!(p.protected, ["c/"]);
    }

    /// Once a policy has `auto_merge.gates`, the governance tier applies with p9's
    /// defaults; without it, there is none. The limits at p9's edges compile.
    #[test]
    fn the_governance_tier_has_p9s_defaults_and_edges() {
        for text in [
            &b"auto_merge:\n  gates: {}\n"[..],
            b"auto_merge:\n  gates:\n    governance:\n",
            b"auto_merge:\n  gates:\n",
            b"auto_merge:\n  gates: ~\n",
        ] {
            let gov = Policy::parse(text).unwrap().governance.unwrap();
            assert_eq!(gov.paths, GOVERNANCE_PATHS);
            assert_eq!(gov.window_ms, 86_400_000);
        }
        assert!(
            Policy::parse(b"auto_merge:\n  rules: []\n")
                .unwrap()
                .governance
                .is_none()
        );
        let max = Policy::parse(b"auto_merge:\n  gates:\n    governance:\n      l3_window_seconds: 2592000\n      l3_max_per_window: 1\n")
            .unwrap();
        assert_eq!(max.governance.unwrap().window_ms, 2_592_000_000);
        let at = Policy::parse(b"cuenca:\n  gates:\n    max_file_bytes: 10485760\n").unwrap();
        assert_eq!(at.max_file_bytes, DEFAULT_MAX_FILE_BYTES);
    }

    /// A pattern is never quoted back, and the number of patterns is capped.
    #[test]
    fn patterns_are_capped_and_never_quoted() {
        let secretish = "PRODUCTION_SECRET_VALUE(";
        for key in ["secret_paths", "secret_patterns"] {
            let text = format!("cuenca:\n  gates:\n    {key}: [\"{secretish}\"]\n");
            let e = Policy::parse(text.as_bytes()).unwrap_err();
            assert!(!e.contains("PRODUCTION"), "{e}");
        }
        let g4 = format!("gates:\n  hard:\n    - id: G4\n      pattern: \"{secretish}\"\n");
        assert!(
            !Policy::parse(g4.as_bytes())
                .unwrap_err()
                .contains("PRODUCTION")
        );
        let many = |n: usize| {
            let list: Vec<String> = (0..n).map(|i| format!("\"p{i}\"")).collect();
            format!(
                "cuenca:\n  gates:\n    secret_patterns: [{}]\n",
                list.join(", ")
            )
        };
        let rules = |n: usize| {
            let list: Vec<String> = (0..n).map(|i| format!("\"r{i}\"")).collect();
            format!(
                "auto_merge:\n  gates:\n    governance:\n      paths: [CLAUDE.md, AGENTS.md, METALAYER.md, .control/policy.yaml, .control/rcs-parameters.toml, {}]\n",
                list.join(", ")
            )
        };
        assert!(Policy::parse(rules(MAX_POLICY_RULES - 5).as_bytes()).is_ok());
        assert!(Policy::parse(rules(MAX_POLICY_RULES - 4).as_bytes()).is_err());
        // The eleven built-ins count: 53 more fit, 54 do not.
        let room = MAX_POLICY_PATTERNS - 1 - super::super::builtin::SECRET_PATTERNS.len();
        assert_eq!(room, 53);
        assert!(Policy::parse(many(room).as_bytes()).is_ok());
        assert!(Policy::parse(many(room + 1).as_bytes()).is_err());
        // The cap is checked before compiling, so a long list fails fast (r3, B:
        // 10,000 patterns took 32 s when every one was compiled first).
        let started = std::time::Instant::now();
        assert!(Policy::parse(many(10_000).as_bytes()).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// Deep nesting is refused by the pre-scan's depth cap, quickly and without
    /// exhausting the stack (an evaluation thread must never abort on a base policy).
    #[test]
    fn deep_nesting_is_refused_not_a_crash() {
        let depth = 400_000;
        let text = format!("x: {}1{}\n", "[".repeat(depth), "]".repeat(depth));
        assert!(text.len() < MAX_POLICY_BYTES);
        let started = std::time::Instant::now();
        assert!(Policy::parse(text.as_bytes()).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        let block: String = (0..1200)
            .map(|i| format!("{}k{i}:\n", " ".repeat(i)))
            .collect();
        assert!(Policy::parse(block.as_bytes()).is_err());
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
