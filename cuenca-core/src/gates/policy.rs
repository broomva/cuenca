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
//! **No key is read as absent because it is misspelt.** Every mapping the engine reads,
//! the top level included, is read through one declared schema ([`SCHEMA`], read by
//! [`Section`]): a key the schema does not list is an error, a key it requires must be
//! there, and the engine can only read a key the schema declares. A misspelt setting,
//! or a misspelt section holding settings, is therefore an error at any depth, never a
//! dropped gate. Two mappings are open, because the rest of them belongs to the
//! workspace and differs between policies (`write_gate`, `permissions`, `governed`,
//! ...): the top level, where the engine reads `gates`, `auto_merge` and `cuenca`, and
//! `gates`, where it reads `hard`. There another key is allowed unless it is a near miss
//! of one the engine reads (`auto_marge`, `Auto-Merge`, `cuenca_gates`), which is an
//! error. YAML tags are refused before loading (yaml-rust2 drops a key whose tag does
//! not convert and shifts its mapping). A `gates.hard` entry whose id looks like `G4`
//! without being it (`g4`, `G4 `, `G4` with an invisible character) is an error; a `G4`
//! without a `pattern` adds none (the built-in one always runs). A governance glob that
//! is empty or does not compile is an error. A key the engine reads must hold its
//! type when it is present: only a missing key is absent. `key:` with no value is an
//! error, except `auto_merge.gates` and its `governance`, which p9 reads as its
//! defaults. So `auto_merge: ~` is an error here, where p9 reads it as auto-merge
//! switched off: here it would switch the gates off. Every `auto_merge.rules` item is
//! checked as p9 checks it (an action p9 knows, and exactly one of `branch_pattern` and
//! `path_touched`). A policy that cannot be compiled is reported as a failing `policy`
//! result and the built-in defaults run instead; it never makes the gates pass.
//!
//! The file must be UTF-8, at most [`MAX_POLICY_BYTES`], nest at most
//! [`MAX_POLICY_DEPTH`] levels, and must not use YAML aliases: the loader copies an
//! aliased node at every use, so a few lines of aliases could expand without bound, and
//! it recurses per level, so a deep document could overflow the stack.

use regex::Regex;
use regex::bytes::Regex as BytesRegex;
use yaml_rust2::Yaml;
use yaml_rust2::parser::{Event, Parser};
use yaml_rust2::scanner::TScalarStyle;

/// Where the base's policy is read from.
pub const POLICY_PATH: &str = ".control/policy.yaml";
/// Where the base's check manifest is read from (decision 4 of the build loop: checks
/// for verify come from the base tree). The engine only records its hash; `cuenca
/// verify` reads it ([`crate::checks`]).
pub const CHECKS_PATH: &str = crate::checks::MANIFEST_PATH;
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

/// One mapping of the policy the engine reads: where it is, the keys it may have and
/// the keys it must have. `[]` is an item of a list.
#[derive(Debug)]
pub(crate) struct Schema {
    pub(crate) path: &'static str,
    pub(crate) known: &'static [&'static str],
    pub(crate) required: &'static [&'static str],
    /// Other keys are allowed, except one that is a near miss of a known key
    /// ([`near_miss`]): the top level, whose other keys belong to the rest of the
    /// workspace and change without the engine.
    pub(crate) open: bool,
}

/// Every mapping the engine reads, and nothing else is read ([`Section`]).
pub(crate) const SCHEMA: &[Schema] = &[
    Schema {
        // The rest of the top level is the workspace's (`write_gate`, `permissions`,
        // `project`, ... differ between policies), so only near misses are refused.
        path: "",
        known: &["gates", "auto_merge", "cuenca"],
        required: &[],
        open: true,
    },
    Schema {
        // The workspace's own gate lists (`governed`, `G1_merge`, ...) vary between
        // policies; the engine reads only `hard`.
        path: "gates",
        known: &["hard"],
        required: &[],
        open: true,
    },
    Schema {
        path: "gates.hard[]",
        known: &["id", "measurement", "pattern", "rule"],
        required: &["id"],
        open: false,
    },
    Schema {
        // p9's keys (`_parse_auto_merge`).
        path: "auto_merge",
        known: &[
            "enabled",
            "require_no_requested_changes",
            "require_branch_up_to_date",
            "merge_method",
            "delete_branch",
            "rules",
            "default_action",
            "gates",
        ],
        required: &[],
        open: false,
    },
    Schema {
        // Exactly one of the two targets is checked where the rule is read.
        path: "auto_merge.rules[]",
        known: &["action", "branch_pattern", "path_touched"],
        required: &[],
        open: false,
    },
    Schema {
        // p9's keys (`_parse_gates`).
        path: "auto_merge.gates",
        known: &["review_bots", "aggregate_checks", "p20", "governance"],
        required: &[],
        open: false,
    },
    Schema {
        path: "auto_merge.gates.governance",
        known: &[
            "paths",
            "required_strata",
            "required_checks",
            "l3_max_per_window",
            "l3_window_seconds",
        ],
        required: &[],
        open: false,
    },
    Schema {
        path: "cuenca",
        known: &["gates"],
        required: &[],
        open: false,
    },
    Schema {
        path: "cuenca.gates",
        known: &[
            "max_file_bytes",
            "secret_paths",
            "secret_patterns",
            "landing_windows",
        ],
        required: &[],
        open: false,
    },
    Schema {
        path: "cuenca.gates.landing_windows[]",
        known: &["days", "from", "to", "utc_offset"],
        required: &["days", "from", "to"],
        open: false,
    },
];

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
        // PyYAML, which p9 loads the file with, also breaks lines at U+0085, U+2028 and
        // U+2029; yaml-rust2 does not, so rules p9 reads could be one string here (r9, B).
        // No YAML parser should see a character the other reads differently: these, and
        // every other control character but tab, LF and CR, are refused.
        if let Some((at, c)) = text.char_indices().find(|&(_, c)| {
            matches!(c, '\u{85}' | '\u{2028}' | '\u{2029}')
                || (c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
        }) {
            let line = text[..at].matches('\n').count() + 1;
            return Err(format!(
                "{POLICY_PATH} holds the character U+{:04X} (line {line}), which the gate engine refuses",
                u32::from(c)
            ));
        }
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

        let root = Section::read(Some(root), "", POLICY_PATH)?;
        let gates = Section::read(root.get("gates"), "gates", "gates")?;
        let auto_merge = Section::read(root.get("auto_merge"), "auto_merge", "auto_merge")?;
        let cuenca = Section::read(root.get("cuenca"), "cuenca", "cuenca")?;

        // gates.hard[id: G4].pattern
        if let Some(hard) = list(gates.get("hard"), "gates.hard")? {
            for (i, gate) in hard.iter().enumerate() {
                let key = format!("gates.hard[{i}]");
                let gate = Section::read(Some(gate), "gates.hard[]", &key)?;
                let id = string(gate.get("id"), &format!("{key}.id"))?
                    .expect("SCHEMA requires gates.hard[].id");
                if id != "G4" {
                    // An id that only looks like the engine's gate would drop it: `g4`,
                    // `G4 `, or `G4` with an invisible character (r7, A).
                    let folded: String = id
                        .chars()
                        .filter(|c| c.is_alphanumeric())
                        .flat_map(char::to_lowercase)
                        .collect();
                    if folded == "g4" {
                        return Err(format!(
                            "{key}.id is {id:?}; the secret-path gate is \"G4\""
                        ));
                    }
                    continue;
                }
                // A G4 without a pattern names another rule (the control kernel's
                // template uses G4 for its own); the built-in pattern runs regardless,
                // and a misspelt `pattern` is an unknown key.
                let Some(pattern) = string(gate.get("pattern"), &format!("{key}.pattern"))? else {
                    continue;
                };
                one_more_pattern(&policy)?;
                policy.secret_paths.push(
                    compile(&pattern).map_err(|e| not_compiled("gates.hard[G4].pattern", &e))?,
                );
            }
        }

        // auto_merge.rules[action: require_human].path_touched. Every rule is checked
        // as p9 checks it, so a misspelt action or target is an error, never a rule
        // that protects nothing (r4, C).
        if let Some(rules) = list(auto_merge.get("rules"), "auto_merge.rules")? {
            for (i, rule) in rules.iter().enumerate() {
                let key = format!("auto_merge.rules[{i}]");
                // A misspelt `action` would read as `notify` and protect nothing (r6, A
                // and B), so a rule's keys are pinned too.
                let rule = Section::read(Some(rule), "auto_merge.rules[]", &key)?;
                let action = string(rule.get("action"), &format!("{key}.action"))?;
                let action = action.as_deref().unwrap_or("notify");
                if !RULE_ACTIONS.contains(&action) {
                    return Err(format!("{key}.action is not one of {RULE_ACTIONS:?}"));
                }
                let branch = string(rule.get("branch_pattern"), &format!("{key}.branch_pattern"))?;
                let path = string(rule.get("path_touched"), &format!("{key}.path_touched"))?;
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

        policy.governance = governance(auto_merge.get("gates"))?;

        // cuenca.gates
        let section = Section::read(cuenca.get("gates"), "cuenca.gates", "cuenca.gates")?;
        if let Some(max) = int(section.get("max_file_bytes"), "cuenca.gates.max_file_bytes")? {
            if max > DEFAULT_MAX_FILE_BYTES {
                return Err(format!(
                    "cuenca.gates.max_file_bytes is {max}: a policy may lower the \
                     {DEFAULT_MAX_FILE_BYTES}-byte limit, not raise it"
                ));
            }
            policy.max_file_bytes = max;
        }
        if let Some(paths) = strings(section.get("secret_paths"), "cuenca.gates.secret_paths")? {
            for p in paths {
                one_more_pattern(&policy)?;
                policy.secret_paths.push(
                    compile(&p).map_err(|e| not_compiled("cuenca.gates.secret_paths[]", &e))?,
                );
            }
        }
        if let Some(patterns) = strings(
            section.get("secret_patterns"),
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
            section.get("landing_windows"),
            "cuenca.gates.landing_windows",
        )? {
            if windows.is_empty() {
                return Err(
                    "cuenca.gates.landing_windows is empty: landing would never be allowed".into(),
                );
            }
            for (i, w) in windows.iter().enumerate() {
                policy.landing_windows.push(window(w, i)?);
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
        Some(Yaml::Null) => None,
        gates => gates,
    };
    // A misspelt `governance` would drop the tier (r5).
    let gates = Section::read(gates, "auto_merge.gates", "auto_merge.gates")?;
    let gov = match gates.get("governance") {
        Some(Yaml::Null) => None,
        gov => gov,
    };
    let gov = Section::read(gov, KEY, KEY)?;
    let paths = match strings(gov.get("paths"), &format!("{KEY}.paths"))? {
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
            // A glob that does not compile would match nothing and drop its path
            // from the tier (r7, A).
            if paths
                .iter()
                .any(|p| p.is_empty() || super::builtin::try_glob(p).is_err())
            {
                return Err(format!(
                    "{KEY}.paths holds an empty glob or one that does not compile"
                ));
            }
            paths
        }
    };
    if let Some(max) = int(
        gov.get("l3_max_per_window"),
        &format!("{KEY}.l3_max_per_window"),
    )? && max != 1
    {
        return Err(format!("{KEY}.l3_max_per_window is {max}; it must be 1"));
    }
    let (lo, hi) = GOVERNANCE_WINDOW_SECONDS;
    let window = int(
        gov.get("l3_window_seconds"),
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
            // yaml-rust2 turns a key whose tag does not convert (`!!null x:`) into its
            // own "no key yet" marker, and the mapping shifts by one pair and drops its
            // last key before any schema sees it (r7, B and C). p9's loader reads the
            // file differently. So no tag is read at all.
            Ok((
                Event::Scalar(_, _, _, Some(_))
                | Event::SequenceStart(_, Some(_))
                | Event::MappingStart(_, Some(_)),
                mark,
            )) => {
                return Err(format!(
                    "{POLICY_PATH} uses a YAML tag (line {}), which the gate engine refuses",
                    mark.line()
                ));
            }
            // PyYAML, which p9 loads the file with, merges a mapping under a plain `<<`
            // key into its parent; yaml-rust2 keeps `<<` as an ordinary key, so the
            // gates would compile defaults where p9 sees rules (r8, B). Refused.
            Ok((Event::Scalar(value, TScalarStyle::Plain, _, None), mark)) if value == "<<" => {
                return Err(format!(
                    "{POLICY_PATH} uses a YAML merge key `<<` (line {}), which the gate engine refuses",
                    mark.line()
                ));
            }
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
/// defaults. Only [`Section::get`] calls it.
fn field<'y>(y: &'y Yaml, key: &str) -> Option<&'y Yaml> {
    y.as_hash()?.get(&Yaml::String(key.to_string()))
}

/// A mapping of the policy, checked against its [`Schema`]: the only way the engine
/// reads a key (r3 to r6 each found one more mapping whose misspelt key read as absent).
#[derive(Clone, Copy)]
struct Section<'y> {
    y: &'y Yaml,
    schema: &'static Schema,
}

impl<'y> Section<'y> {
    /// The mapping `y` (a missing one has no keys) read as the schema at `path`; `at`
    /// names it in errors. Refuses a key the schema does not list and a missing key it
    /// requires.
    fn read(y: Option<&'y Yaml>, path: &str, at: &str) -> Res<Self> {
        let schema = SCHEMA
            .iter()
            .find(|s| s.path == path)
            .unwrap_or_else(|| panic!("no schema for policy section {path:?}"));
        let y = mapping(y, at)?;
        for key in y.as_hash().into_iter().flat_map(|m| m.keys()) {
            match key.as_str() {
                Some(k) if schema.known.contains(&k) => {}
                Some(k) if schema.open => {
                    if let Some(known) = schema.known.iter().find(|known| near_miss(k, known)) {
                        return Err(format!("{at}: unknown key {k:?}, too close to {known:?}"));
                    }
                }
                _ => return Err(format!("{at}: unknown key {key:?}")),
            }
        }
        if y.as_hash().is_some() {
            for key in schema.required {
                if field(y, key).is_none() {
                    return Err(format!("{at}.{key} is missing"));
                }
            }
        }
        Ok(Self { y, schema })
    }

    /// The value of `key`, which the schema must declare.
    fn get(&self, key: &str) -> Option<&'y Yaml> {
        assert!(
            self.schema.known.contains(&key),
            "policy key {key:?} is not in the schema of {:?}",
            self.schema.path
        );
        field(self.y, key)
    }
}

/// Whether `key` reads as a misspelling of `known`: once case and everything but
/// letters and digits are dropped, it starts with `known` or is at most two edits from
/// it (`Auto-Merge`, `auto_marge`, `gate`, `cuenca_gates`).
fn near_miss(key: &str, known: &str) -> bool {
    let fold = |s: &str| -> Vec<char> {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect()
    };
    let (a, b) = (fold(key), fold(known));
    if a.starts_with(&b) {
        return true;
    }
    // Levenshtein distance, one row at a time.
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != cb)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()] <= 2
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

fn window(y: &Yaml, i: usize) -> Res<Window> {
    let key = format!("cuenca.gates.landing_windows[{i}]");
    let y = Section::read(Some(y), "cuenca.gates.landing_windows[]", &key)?;
    let days = strings(y.get("days"), &format!("{key}.days"))?
        .ok_or_else(|| format!("{key}.days is missing"))?
        .iter()
        .map(|d| {
            DAYS.iter()
                .position(|x| x.eq_ignore_ascii_case(d))
                .map(|i| i as u8)
                .ok_or_else(|| format!("{key}.days: {d:?} is not one of {DAYS:?}"))
        })
        .collect::<Res<Vec<_>>>()?;
    if days.is_empty() {
        return Err(format!("{key}.days is empty"));
    }
    let clock = |name: &str| -> Res<u16> {
        let s = string(y.get(name), &format!("{key}.{name}"))?
            .ok_or_else(|| format!("{key}.{name} is missing"))?;
        parse_clock(&s).ok_or_else(|| format!("{key}.{name}: {s:?} is not HH:MM"))
    };
    let (from, to) = (clock("from")?, clock("to")?);
    if from >= to {
        return Err(format!(
            "{key}: from must be before to (a window may not cross midnight; use two)"
        ));
    }
    let utc_offset = match string(y.get("utc_offset"), &format!("{key}.utc_offset"))? {
        None => 0,
        Some(s) => {
            parse_offset(&s).ok_or_else(|| format!("{key}.utc_offset: {s:?} is not ±HH:MM"))?
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
    fn a_line_break_only_pyyaml_sees_is_refused() {
        // PyYAML breaks lines at U+0085, U+2028 and U+2029; yaml-rust2 does not, so the
        // rules below were one string under `note:` and the engine protected nothing (r9, B).
        for sep in ["\u{2028}", "\u{2029}", "\u{85}", "\u{0b}", "\u{0c}"] {
            let text = format!(
                "note: x{sep}auto_merge:{sep}  rules:{sep}  - path_touched:{sep}      CLAUDE.md{sep}    action:{sep}      require_human\n"
            );
            let got = Policy::parse(text.as_bytes());
            assert!(
                got.is_err(),
                "{sep:?}: parsed Ok, protected {:?}",
                got.map(|p| p.protected.len())
            );
        }
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
            // Under a key the engine ignores, so only the alias refusal catches it.
            b"catalog:\n  base: &a [x, x]\n  more: [*a, *a]\n",
            b"auto_merge:\n  rules:\n    - path_touched: ''\n      action: require_human\n",
            b"\xff\xfe",
            b"a: 1\n---\nb: 2\n",
            // A merge key: PyYAML merges it, yaml-rust2 would keep `<<` as a key (r8, B).
            b"<<:\n  auto_merge:\n    rules:\n      - {path_touched: CLAUDE.md, action: require_human}\n",
            b"gates:\n  <<: {hard: [{id: G4, pattern: 'x'}]}\n",
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
            // ... and inside a rule (r6, A and B): `actoin` would read as `notify`.
            b"auto_merge:\n  rules:\n    - {path_touched: .control/policy.yaml, actoin: require_human}\n",
            b"auto_merge:\n  rules:\n    - path_touched: /CLAUDE.md\n      Action: require_human\n",
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
            // ... and at the top level (A10): `auto_marge` would drop every rule.
            b"auto_marge:\n  rules:\n    - {path_touched: CLAUDE.md, action: require_human}\n",
            b"cuenca_gates: {max_file_bytes: 1}\n",
            b"Auto-Merge: {}\n",
            b"gate: {hard: []}\n",
            // A tag is refused (r7, B and C): yaml-rust2 drops a key whose tag does not
            // convert and shifts the mapping, losing its last key.
            b"!!null x: version\nauto_merge:\n  rules:\n    - {path_touched: CLAUDE.md, action: require_human}\n  gates: {}\n",
            b"auto_merge:\n  !!bool yes: 1\n  rules:\n    - {path_touched: CLAUDE.md, action: require_human}\n",
            b"cuenca: !!map {gates: {}}\n",
            // A governance glob is refused if empty or if it does not compile, rather
            // than matching nothing (r7, A).
            b"auto_merge:\n  gates:\n    governance:\n      paths: [CLAUDE.md, AGENTS.md, METALAYER.md, .control/policy.yaml, .control/rcs-parameters.toml, '[z-ab]']\n",
            b"auto_merge:\n  gates:\n    governance:\n      paths: [CLAUDE.md, AGENTS.md, METALAYER.md, .control/policy.yaml, .control/rcs-parameters.toml, '']\n",
            "gates:\n  hard:\n    - {id: \"G4\u{200b}\", pattern: x}\n".as_bytes(),
        ] {
            assert!(Policy::parse(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
        assert!(Policy::parse(&vec![b' '; MAX_POLICY_BYTES + 1]).is_err());
    }

    /// A policy that has every section the engine reads and every key it reads.
    const FULL: &str = r#"
version: 1
gates:
  hard:
    - {id: G1, rule: r, measurement: m}
    - {id: G4, pattern: '\.pem$', rule: r, measurement: m}
  soft: []
auto_merge:
  enabled: true
  rules:
    - {path_touched: CLAUDE.md, action: require_human}
    - {branch_pattern: 'release/*', action: notify}
  gates:
    governance:
      paths: [CLAUDE.md, AGENTS.md, METALAYER.md, .control/policy.yaml, .control/rcs-parameters.toml]
      l3_max_per_window: 1
      l3_window_seconds: 86400
cuenca:
  gates:
    max_file_bytes: 100
    secret_paths: ['\.key$']
    secret_patterns: ['SECRET_[A-Z]+']
    landing_windows:
      - {days: [mon], from: '09:00', to: '17:00', utc_offset: '-05:00'}
"#;

    /// The mapping of `doc` at a [`Schema`] path (`[]`: the list's last item).
    fn at<'y>(doc: &'y mut Yaml, path: &str) -> &'y mut Yaml {
        at_labelled(doc, path).0
    }

    /// [`at`], and the mapping's name as an error gives it (`rules[1]`).
    fn at_labelled<'y>(doc: &'y mut Yaml, path: &str) -> (&'y mut Yaml, String) {
        let mut y = doc;
        let mut label = Vec::new();
        for seg in path.split('.').filter(|s| !s.is_empty()) {
            let (name, item) = match seg.strip_suffix("[]") {
                Some(name) => (name, true),
                None => (seg, false),
            };
            let Yaml::Hash(map) = y else {
                panic!("{path}: {name} is under no mapping")
            };
            y = map
                .get_mut(&Yaml::String(name.into()))
                .unwrap_or_else(|| panic!("FULL has no {path}"));
            if item {
                let Yaml::Array(items) = y else {
                    panic!("{path}: {name} is not a list")
                };
                label.push(format!("{name}[{}]", items.len() - 1));
                y = items.last_mut().expect("an item");
            } else {
                label.push(name.to_string());
            }
        }
        let label = if label.is_empty() {
            POLICY_PATH.to_string()
        } else {
            label.join(".")
        };
        (y, label)
    }

    fn emit(doc: &Yaml) -> String {
        let mut out = String::new();
        yaml_rust2::YamlEmitter::new(&mut out).dump(doc).unwrap();
        out
    }

    /// Every mapping the engine reads, in [`SCHEMA`]: an unknown key, any key it reads
    /// misspelt, and a required key left out are each an error naming the mapping,
    /// never a key read as absent (A10: rounds 3 to 6 each found one more mapping).
    #[test]
    fn every_mapping_refuses_an_unknown_misspelt_or_missing_key() {
        let full = yaml_rust2::YamlLoader::load_from_str(FULL)
            .unwrap()
            .remove(0);
        let p = Policy::parse(emit(&full).as_bytes()).unwrap();
        assert_eq!((p.secret_paths.len(), p.secret_patterns.len()), (3, 1));
        assert_eq!(p.protected, ["CLAUDE.md"]);
        assert!(p.governance.is_some() && p.max_file_bytes == 100);
        assert_eq!(p.landing_windows.len(), 1);

        for schema in SCHEMA {
            let label = at_labelled(&mut full.clone(), schema.path).1;
            let mut edits: Vec<(String, Yaml)> = Vec::new();
            let mut doc = full.clone();
            let Yaml::Hash(map) = at(&mut doc, schema.path) else {
                panic!("{label} is not a mapping in FULL")
            };
            // An open mapping takes any key that is not a near miss of its own.
            let unknown = if schema.open {
                format!("{}x", schema.known[0])
            } else {
                "zz_unknown".to_string()
            };
            map.insert(Yaml::String(unknown), Yaml::Integer(1));
            edits.push((format!("{label}: unknown key"), doc));

            let mut present = Vec::new();
            let mut doc = full.clone();
            if let Yaml::Hash(map) = at(&mut doc, schema.path) {
                present.extend(map.keys().filter_map(Yaml::as_str).map(str::to_string));
            }
            assert!(
                schema
                    .required
                    .iter()
                    .all(|r| present.iter().any(|p| p == r)),
                "FULL lacks a required key of {label}"
            );
            // A misspelt key the schema does not declare (the top level's `version`)
            // is the workspace's own, not the engine's.
            let declared = present
                .iter()
                .filter(|k| schema.known.contains(&k.as_str()));
            for key in declared {
                let mut doc = full.clone();
                let Yaml::Hash(map) = at(&mut doc, schema.path) else {
                    unreachable!()
                };
                let value = map.remove(&Yaml::String(key.clone())).unwrap();
                if schema.required.contains(&key.as_str()) {
                    let mut left_out = full.clone();
                    *at(&mut left_out, schema.path) = Yaml::Hash(map.clone());
                    edits.push((format!("{label}.{key} is missing"), left_out));
                }
                map.insert(Yaml::String(format!("{key}_")), value);
                edits.push((format!("{label}: unknown key"), doc));
            }
            for (want, doc) in edits {
                let text = emit(&doc);
                let err = Policy::parse(text.as_bytes()).expect_err(&text);
                assert!(err.contains(&want), "{want:?} not in {err:?} for\n{text}");
            }
        }
    }

    /// An entry that looks like the engine's G4 without being it is an error, not a
    /// gate that is never read.
    #[test]
    fn an_id_that_looks_like_g4_is_an_error() {
        for id in ["g4", "G4 ", " g4"] {
            let text = format!("gates:\n  hard:\n    - {{id: '{id}', pattern: '\\.pem$'}}\n");
            let err = Policy::parse(text.as_bytes()).unwrap_err();
            assert!(err.contains("gates.hard[0].id"), "{err}");
        }
        assert!(Policy::parse(b"gates:\n  hard:\n    - {id: G4b, pattern: x}\n").is_ok());
    }

    /// The rest of the top level is the workspace's: keys the engine does not read,
    /// whichever policy they come from, compile (r7, B: `write_gate`, `permissions`,
    /// `project`, `trust_tiers` refused four of the workspace's five p9-valid policies).
    #[test]
    fn other_top_level_keys_are_the_workspaces() {
        let p = Policy::parse(
            b"version: 1\nwrite_gate: {x: 1}\npermissions: []\nproject: p\ntrust_tiers: {}\nrcs: 1\nauto_merge:\n  rules:\n    - {path_touched: CLAUDE.md, action: require_human}\n",
        )
        .unwrap();
        assert_eq!(p.protected, ["CLAUDE.md"]);
        for (key, known) in [
            ("auto_marge", "auto_merge"),
            ("AutoMerge", "auto_merge"),
            ("gate", "gates"),
            ("cuenka", "cuenca"),
        ] {
            assert!(near_miss(key, known), "{key}");
        }
        for (key, known) in [
            ("write_gate", "gates"),
            ("plants", "gates"),
            ("catalog", "cuenca"),
            ("ci_heal", "cuenca"),
        ] {
            assert!(!near_miss(key, known), "{key}");
        }
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
