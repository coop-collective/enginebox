//! Repository automations (docs/PROJECT-CONFIG.md, "Automations"): agent
//! sessions a repository defines in `.enginebox/automations/*.yml` on its
//! default branch, which the controller runs on a UTC cron schedule, on pushes
//! that touch matching paths, and on "run now".
//!
//! This module is the pure part: the strict file schema, cron and path
//! matching, the run prompt, and the per-repository record of files and run
//! history. `lib.rs` reads the files from the repository, starts runs through
//! the ordinary pool run path and settles them.
//!
//! A file only declares. It never grants a credential, a connection or a pool:
//! runs use the repository's project pool, the controller's provider accounts
//! and the repository's own agent profile (with its approved mappings), as any
//! other run on the repository does.

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, Datelike, Duration, Timelike, Utc};
use enginebox_core::state;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Where automation files live, relative to the repository root.
pub const DIR: &str = ".enginebox/automations";
/// Most automation files one repository may define.
pub const MAX_FILES: usize = 32;
/// Largest automation file.
pub const MAX_FILE_BYTES: usize = 32 * 1024;
/// Longest prompt (the native request also carries the controller's preamble).
pub const MAX_PROMPT: usize = 8000;
/// The run kind of an automation run.
pub const RUN_KIND: &str = "automation";
/// Default and longest run time.
pub const DEFAULT_TIMEOUT_MINUTES: u64 = 30;
pub const MAX_TIMEOUT_MINUTES: u64 = 240;
/// Model tiers a file may name instead of a model.
pub const TIERS: [&str; 3] = ["basic", "standard", "hard"];
/// Runs kept per repository (oldest finished ones are dropped first).
const MAX_RUNS: usize = 200;
/// A missed schedule is caught up at most this far back (controller restarts).
const CATCH_UP_HOURS: i64 = 24;
/// Most changed paths kept on a push trigger record.
const MAX_RECORDED_PATHS: usize = 20;

const TOP_KEYS: [&str; 12] = [
    "name",
    "description",
    "prompt",
    "skill",
    "model",
    "profile",
    "runners",
    "on",
    "concurrency",
    "timeout-minutes",
    "push",
    "enabled",
];

// File schema ----------------------------------------------------------------

/// What runs the automation's agent: a tier the controller maps per provider,
/// or one provider's exact model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Model {
    Tier(String),
    Exact(String),
}

/// A push trigger: branches (names or globs) and optional path globs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushTrigger {
    /// Empty means the repository's default branch.
    pub branches: Vec<String>,
    /// Empty means any change.
    pub paths: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Concurrency {
    /// One firing waits while a run is active; a newer one replaces it.
    Queue,
    /// Firings while a run is active are recorded as skipped.
    Skip,
}

/// One validated automation file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Automation {
    /// The file name without its extension.
    pub id: String,
    pub path: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    pub model: Model,
    /// A repository agent profile from `.enginebox/config.json`; None for the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Providers in order of preference.
    pub runners: Vec<String>,
    pub schedule: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_trigger: Option<PushTrigger>,
    pub manual: bool,
    pub concurrency: Concurrency,
    pub timeout_minutes: u64,
    /// The only branches the run may push: `<prefix><date>`. None: read-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_prefix: Option<String>,
    pub enabled: bool,
}

/// Automation IDs: the file name, `^[a-z0-9][a-z0-9-]{0,47}$`.
pub fn valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    (1..=48).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

/// The automation ID of a file path, or None for a path that is not one.
pub fn id_of(path: &str) -> Option<String> {
    let name = path.strip_prefix(DIR)?.strip_prefix('/')?;
    let stem = name
        .strip_suffix(".yml")
        .or_else(|| name.strip_suffix(".yaml"))?;
    (!stem.contains('/')).then(|| stem.to_owned())
}

fn valid_skill(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=64).contains(&b.len())
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || b":_-".contains(c))
}

fn valid_model_id(m: &str) -> bool {
    let b = m.as_bytes();
    (1..=128).contains(&b.len())
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || b"_.:-".contains(c))
}

/// A branch glob: branch characters plus `*`.
fn valid_branch_glob(g: &str) -> bool {
    let plain = g.replace('*', "x");
    enginebox_agents::repository::valid_branch(&plain) || g == "*" || g == "**"
}

/// A path glob: relative, no `..`, no backslash or control characters.
fn valid_path_glob(g: &str) -> bool {
    (1..=200).contains(&g.len())
        && !g.starts_with('/')
        && !g.split('/').any(|p| p == ".." || p.is_empty())
        && !g.chars().any(|c| c.is_control() || c == '\\')
}

/// A push branch prefix: a branch path ending in `/`, never a ref namespace.
pub fn valid_branch_prefix(p: &str) -> bool {
    p.ends_with('/')
        && p.len() <= 100
        && enginebox_agents::repository::valid_branch(p.trim_end_matches('/'))
        && !p.starts_with("refs/")
        && !p.eq_ignore_ascii_case("head/")
}

/// Whether `prefix` could name the default branch (or a branch git cannot
/// create beside it).
pub fn prefix_covers_default(prefix: &str, default_branch: &str) -> bool {
    let bare = prefix.trim_end_matches('/');
    default_branch == bare
        || default_branch.starts_with(prefix)
        || bare.starts_with(&format!("{default_branch}/"))
}

/// Convert a YAML document to JSON, refusing anchors and aliases (an alias
/// could expand a small file into a huge document) and non-string keys.
fn yaml_to_json(text: &str) -> Result<Value> {
    use yaml_rust2::parser::{Event, Parser};
    let mut parser = Parser::new_from_str(text);
    let mut documents = 0;
    loop {
        let (event, _) = parser
            .next_token()
            .map_err(|e| anyhow::anyhow!("The file is not valid YAML: {e}"))?;
        match event {
            Event::StreamEnd => break,
            Event::Alias(_) => bail!("YAML anchors and aliases are not supported."),
            Event::Scalar(_, _, anchor, _)
            | Event::SequenceStart(anchor, _)
            | Event::MappingStart(anchor, _)
                if anchor > 0 =>
            {
                bail!("YAML anchors and aliases are not supported.")
            }
            Event::DocumentStart => documents += 1,
            _ => {}
        }
    }
    ensure!(
        documents == 1,
        "An automation file holds one YAML document."
    );
    let docs = yaml_rust2::YamlLoader::load_from_str(text)
        .map_err(|e| anyhow::anyhow!("The file is not valid YAML: {e}"))?;
    fn convert(y: &yaml_rust2::Yaml) -> Result<Value> {
        use yaml_rust2::Yaml;
        Ok(match y {
            Yaml::Null => Value::Null,
            Yaml::Boolean(b) => Value::Bool(*b),
            Yaml::Integer(i) => json!(i),
            Yaml::Real(r) => json!(r.parse::<f64>().context("Invalid number")?),
            Yaml::String(s) => Value::String(s.clone()),
            Yaml::Array(a) => Value::Array(a.iter().map(convert).collect::<Result<_>>()?),
            Yaml::Hash(h) => {
                let mut m = Map::new();
                for (k, v) in h {
                    let Yaml::String(k) = k else {
                        bail!("Keys must be strings.");
                    };
                    ensure!(
                        m.insert(k.clone(), convert(v)?).is_none(),
                        "Duplicate key {k}."
                    );
                }
                Value::Object(m)
            }
            Yaml::Alias(_) | Yaml::BadValue => bail!("Unsupported YAML value."),
        })
    }
    convert(docs.first().context("The file is empty.")?)
}

fn only_keys(object: &Map<String, Value>, allowed: &[&str], what: &str) -> Result<()> {
    if let Some(unknown) = object.keys().find(|k| !allowed.contains(&k.as_str())) {
        bail!(
            "{what}: unknown key {unknown} (known: {}).",
            allowed.join(", ")
        );
    }
    Ok(())
}

fn text(v: &Value, key: &str, max: usize) -> Result<Option<String>> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => {
            ensure!(
                !s.trim().is_empty() && s.len() <= max && !s.contains('\0'),
                "{key} must be 1-{max} bytes of text."
            );
            Ok(Some(s.clone()))
        }
        Some(_) => bail!("{key} must be text."),
    }
}

fn strings(v: &Value, what: &str, max: usize) -> Result<Vec<String>> {
    let list = match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a
            .iter()
            .map(|i| i.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .with_context(|| format!("{what} must list text values."))?,
        _ => bail!("{what} must be a list."),
    };
    ensure!(list.len() <= max, "{what} lists at most {max} values.");
    Ok(list)
}

impl Automation {
    /// Parse and strictly validate one file. Unknown keys are refused.
    pub fn parse(path: &str, source: &str) -> Result<Self> {
        let id = id_of(path).context("Automation files are .enginebox/automations/<id>.yml.")?;
        ensure!(
            valid_id(&id),
            "Automation file names match ^[a-z0-9][a-z0-9-]{{0,47}}$ (then .yml or .yaml)."
        );
        ensure!(
            source.len() <= MAX_FILE_BYTES,
            "Automation files are at most {MAX_FILE_BYTES} bytes."
        );
        let value = yaml_to_json(source)?;
        let object = value
            .as_object()
            .context("An automation file is a mapping of keys.")?;
        only_keys(object, &TOP_KEYS, "automation")?;
        let name = text(&value, "name", 200)?.unwrap_or_else(|| id.clone());
        let description = text(&value, "description", 1000)?;
        let prompt = text(&value, "prompt", MAX_PROMPT)?;
        let skill = text(&value, "skill", 64)?;
        if let Some(s) = &skill {
            ensure!(
                valid_skill(s),
                "skill names a skill (letters, digits, :, _ and -)."
            );
        }
        ensure!(
            prompt.is_some() || skill.is_some(),
            "An automation needs a prompt, a skill or both."
        );
        let runners = match value.get("runners") {
            None | Some(Value::Null) => {
                vec!["claude".to_owned(), "codex".to_owned(), "cursor".to_owned()]
            }
            Some(v) => {
                let list = strings(v, "runners", 3)?;
                ensure!(!list.is_empty(), "runners names at least one provider.");
                for (i, r) in list.iter().enumerate() {
                    ensure!(
                        enginebox_agents::PROVIDERS.contains(&r.as_str()),
                        "runners lists providers: {}.",
                        enginebox_agents::PROVIDERS.join(", ")
                    );
                    ensure!(!list[..i].contains(r), "runners lists each provider once.");
                }
                list
            }
        };
        let model = match text(&value, "model", 128)? {
            None => Model::Tier("standard".into()),
            Some(m) if TIERS.contains(&m.as_str()) => Model::Tier(m),
            Some(m) => {
                ensure!(
                    valid_model_id(&m),
                    "model is a tier (basic, standard, hard) or a model identifier."
                );
                ensure!(
                    runners.len() == 1,
                    "An exact model belongs to one provider: list exactly one runner, or use a tier."
                );
                Model::Exact(m)
            }
        };
        let profile = text(&value, "profile", 32)?;
        if let Some(p) = &profile {
            ensure!(
                crate::regex_free_profile_name(p),
                "profile names an agent profile from .enginebox/config.json."
            );
        }
        let concurrency = match text(&value, "concurrency", 16)?.as_deref() {
            None | Some("queue") => Concurrency::Queue,
            Some("skip") => Concurrency::Skip,
            Some(_) => bail!("concurrency is queue or skip (one run at a time either way)."),
        };
        let timeout_minutes = match value.get("timeout-minutes") {
            None | Some(Value::Null) => DEFAULT_TIMEOUT_MINUTES,
            Some(v) => v
                .as_u64()
                .filter(|m| (1..=MAX_TIMEOUT_MINUTES).contains(m))
                .with_context(|| format!("timeout-minutes is 1-{MAX_TIMEOUT_MINUTES}."))?,
        };
        let enabled = match value.get("enabled") {
            None | Some(Value::Null) => true,
            Some(Value::Bool(b)) => *b,
            Some(_) => bail!("enabled is true or false."),
        };
        let branch_prefix = match value.get("push") {
            None | Some(Value::Null) => None,
            Some(Value::Object(push)) => {
                only_keys(push, &["branch-prefix"], "push")?;
                let prefix = push
                    .get("branch-prefix")
                    .and_then(Value::as_str)
                    .context("push names a branch-prefix, such as automation/<id>/.")?;
                ensure!(
                    valid_branch_prefix(prefix),
                    "push.branch-prefix is a branch path ending in /, such as automation/{id}/."
                );
                Some(prefix.to_owned())
            }
            Some(_) => bail!("push is a mapping with branch-prefix."),
        };
        let on = value
            .get("on")
            .and_then(Value::as_object)
            .context("on names at least one trigger: schedule, push or manual.")?;
        only_keys(on, &["schedule", "push", "manual"], "on")?;
        let mut schedule = Vec::new();
        if let Some(s) = on.get("schedule") {
            let list = s
                .as_array()
                .context("on.schedule is a list of {cron: \"...\"}.")?;
            ensure!(
                (1..=10).contains(&list.len()),
                "on.schedule lists 1-10 crons."
            );
            for entry in list {
                let entry = entry
                    .as_object()
                    .context("on.schedule is a list of {cron: \"...\"}.")?;
                only_keys(entry, &["cron"], "on.schedule")?;
                let cron = entry
                    .get("cron")
                    .and_then(Value::as_str)
                    .context("on.schedule entries have cron.")?;
                Cron::parse(cron)
                    .with_context(|| format!("on.schedule: {cron} is not 5-field UTC cron"))?;
                schedule.push(cron.to_owned());
            }
        }
        let push_trigger = match on.get("push") {
            None => None,
            Some(Value::Null) => Some(PushTrigger {
                branches: vec![],
                paths: vec![],
            }),
            Some(Value::Object(p)) => {
                only_keys(p, &["branches", "paths"], "on.push")?;
                let branches = match p.get("branches") {
                    None | Some(Value::Null) => vec![],
                    Some(v) => strings(v, "on.push.branches", 20)?,
                };
                ensure!(
                    branches.iter().all(|b| valid_branch_glob(b)),
                    "on.push.branches lists branch names or globs."
                );
                let paths = match p.get("paths") {
                    None | Some(Value::Null) => vec![],
                    Some(v) => strings(v, "on.push.paths", 50)?,
                };
                ensure!(
                    paths.iter().all(|g| valid_path_glob(g)),
                    "on.push.paths lists relative path globs such as docs/**."
                );
                Some(PushTrigger { branches, paths })
            }
            Some(_) => bail!("on.push is a mapping with branches and paths."),
        };
        let manual = match on.get("manual") {
            None | Some(Value::Null) => true,
            Some(Value::Bool(b)) => *b,
            Some(_) => bail!("on.manual is true or false."),
        };
        ensure!(
            !schedule.is_empty() || push_trigger.is_some() || on.contains_key("manual"),
            "on names at least one trigger: schedule, push or manual."
        );
        Ok(Self {
            id,
            path: path.to_owned(),
            name,
            description,
            prompt,
            skill,
            model,
            profile,
            runners,
            schedule,
            push_trigger,
            manual,
            concurrency,
            timeout_minutes,
            branch_prefix,
            enabled,
        })
    }

    /// Whether a push to `branch` with `paths` changed fires this automation.
    /// `paths` is None when the changed paths are unknown (then any push to a
    /// matching branch fires). Pushes to the automation's own branches never do.
    pub fn push_fires(&self, default_branch: &str, branch: &str, paths: Option<&[String]>) -> bool {
        let Some(trigger) = &self.push_trigger else {
            return false;
        };
        if self
            .branch_prefix
            .as_deref()
            .is_some_and(|p| branch.starts_with(p))
        {
            return false;
        }
        let branch_ok = if trigger.branches.is_empty() {
            branch == default_branch
        } else {
            trigger.branches.iter().any(|g| glob_match(g, branch))
        };
        branch_ok
            && (trigger.paths.is_empty()
                || paths.is_none_or(|changed| {
                    changed
                        .iter()
                        .any(|p| trigger.paths.iter().any(|g| glob_match(g, p)))
                }))
    }

    /// The model for one provider: an exact model as written, a tier through
    /// the deployment's map (`tiers`) or the built-in Claude aliases; None
    /// leaves the provider's default.
    pub fn model_for(&self, provider: &str, tiers: &Value) -> Option<String> {
        match &self.model {
            Model::Exact(m) => Some(m.clone()),
            Model::Tier(t) => tiers[provider][t]
                .as_str()
                .filter(|m| valid_model_id(m))
                .map(str::to_owned)
                .or_else(|| match (provider, t.as_str()) {
                    ("claude", "basic") => Some("haiku".into()),
                    ("claude", "standard") => Some("sonnet".into()),
                    ("claude", "hard") => Some("opus".into()),
                    _ => None,
                }),
        }
    }

    /// The public view the API returns.
    pub fn public(&self, now: DateTime<Utc>) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or(Value::Null);
        v["model"] = match &self.model {
            Model::Tier(t) => json!({"tier": t}),
            Model::Exact(m) => json!({"model": m}),
        };
        v["next_scheduled_at"] = json!(
            self.schedule
                .iter()
                .filter_map(|c| Cron::parse(c).ok()?.next_after(now))
                .min()
                .map(|t| t.to_rfc3339())
        );
        v
    }
}

// Cron -----------------------------------------------------------------------

/// A 5-field UTC cron (minute hour day-of-month month day-of-week) with `*`,
/// `a`, `a-b`, `*/n`, `a-b/n` and lists. Day of week is 0-7 (0 and 7 are
/// Sunday). When both day fields are restricted, either may match (as cron).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cron {
    minutes: u64,
    hours: u64,
    days: u64,
    months: u64,
    weekdays: u64,
    days_any: bool,
    weekdays_any: bool,
}

fn cron_field(field: &str, min: u32, max: u32) -> Result<(u64, bool)> {
    let mut bits = 0u64;
    let any = field == "*";
    for part in field.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().ok().filter(|s| *s > 0).context("step")?),
            None => (part, 1),
        };
        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (a.parse::<u32>()?, b.parse::<u32>()?)
        } else {
            let a = range.parse::<u32>()?;
            (a, if part.contains('/') { max } else { a })
        };
        ensure!(min <= lo && lo <= hi && hi <= max, "out of range");
        let mut v = lo;
        while v <= hi {
            bits |= 1 << v;
            v += step;
        }
    }
    Ok((bits, any))
}

impl Cron {
    pub fn parse(expression: &str) -> Result<Self> {
        let fields: Vec<&str> = expression.split_whitespace().collect();
        ensure!(fields.len() == 5, "cron has five fields");
        let (minutes, _) = cron_field(fields[0], 0, 59)?;
        let (hours, _) = cron_field(fields[1], 0, 23)?;
        let (days, days_any) = cron_field(fields[2], 1, 31)?;
        let (months, _) = cron_field(fields[3], 1, 12)?;
        let (mut weekdays, weekdays_any) = cron_field(fields[4], 0, 7)?;
        if weekdays & (1 << 7) != 0 {
            weekdays |= 1;
        }
        Ok(Self {
            minutes,
            hours,
            days,
            months,
            weekdays,
            days_any,
            weekdays_any,
        })
    }
    pub fn matches(&self, t: DateTime<Utc>) -> bool {
        let bit = |set: u64, v: u32| set & (1 << v) != 0;
        let day = bit(self.days, t.day());
        let weekday = bit(self.weekdays, t.weekday().num_days_from_sunday());
        let day_ok = match (self.days_any, self.weekdays_any) {
            (true, true) => true,
            (true, false) => weekday,
            (false, true) => day,
            (false, false) => day || weekday,
        };
        bit(self.minutes, t.minute())
            && bit(self.hours, t.hour())
            && bit(self.months, t.month())
            && day_ok
    }
    /// The latest matching minute in `(after, until]`, looking back at most
    /// [`CATCH_UP_HOURS`].
    pub fn latest_due(&self, after: DateTime<Utc>, until: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let floor = |t: DateTime<Utc>| t.with_second(0)?.with_nanosecond(0);
        let mut t = floor(until)?;
        let limit = after.max(until - Duration::hours(CATCH_UP_HOURS));
        while t > limit {
            if self.matches(t) {
                return Some(t);
            }
            t -= Duration::minutes(1);
        }
        None
    }
    /// The next matching minute after `now`, within about a year.
    pub fn next_after(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        let mut t = now.with_second(0)?.with_nanosecond(0)? + Duration::minutes(1);
        let end = now + Duration::days(366);
        // Skip whole hours and days that cannot match, so this stays cheap.
        while t < end {
            if self.months & (1 << t.month()) == 0 || !self.day_matches(t) {
                t = (t + Duration::days(1)).with_hour(0)?.with_minute(0)?;
                continue;
            }
            if self.hours & (1 << t.hour()) == 0 {
                t = (t + Duration::hours(1)).with_minute(0)?;
                continue;
            }
            if self.matches(t) {
                return Some(t);
            }
            t += Duration::minutes(1);
        }
        None
    }
    fn day_matches(&self, t: DateTime<Utc>) -> bool {
        let day = self.days & (1 << t.day()) != 0;
        let weekday = self.weekdays & (1 << t.weekday().num_days_from_sunday()) != 0;
        match (self.days_any, self.weekdays_any) {
            (true, true) => true,
            (true, false) => weekday,
            (false, true) => day,
            (false, false) => day || weekday,
        }
    }
}

// Globs ----------------------------------------------------------------------

/// Match `path` against `pattern`: `**` crosses directories (and `**/` may
/// match nothing), `*` and `?` stay within one path segment.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    fn go(p: &[u8], s: &[u8]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(b'*') if p.get(1) == Some(&b'*') => {
                let rest = &p[2..];
                if let Some(after) = rest.strip_prefix(b"/")
                    && go(after, s)
                {
                    return true;
                }
                (0..=s.len()).any(|i| go(rest, &s[i..]))
            }
            Some(b'*') => {
                let rest = &p[1..];
                let mut i = 0;
                loop {
                    if go(rest, &s[i..]) {
                        return true;
                    }
                    if i == s.len() || s[i] == b'/' {
                        return false;
                    }
                    i += 1;
                }
            }
            Some(b'?') => !s.is_empty() && s[0] != b'/' && go(&p[1..], &s[1..]),
            Some(c) => s.first() == Some(c) && go(&p[1..], &s[1..]),
        }
    }
    go(pattern.as_bytes(), path.as_bytes())
}

// Firings and runs -----------------------------------------------------------

/// Why a run started.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trigger {
    Schedule {
        cron: String,
        at: String,
    },
    Push {
        branch: String,
        commit: String,
        /// The first changed paths; empty with `paths_known: false`.
        paths: Vec<String>,
        paths_known: bool,
    },
    Manual {
        by: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
}
impl Trigger {
    pub fn push(branch: &str, commit: &str, paths: Option<&[String]>) -> Self {
        Self::Push {
            branch: branch.to_owned(),
            commit: commit.to_owned(),
            paths: paths
                .unwrap_or_default()
                .iter()
                .take(MAX_RECORDED_PATHS)
                .cloned()
                .collect(),
            paths_known: paths.is_some(),
        }
    }
    /// What makes two firings the same firing.
    fn identity(&self) -> String {
        match self {
            Self::Schedule { cron, at } => format!("schedule\n{cron}\n{at}"),
            Self::Push { branch, commit, .. } => format!("push\n{branch}\n{commit}"),
            Self::Manual { key: Some(k), .. } => format!("manual\n{k}"),
            Self::Manual { key: None, .. } => {
                format!("manual\n{}", uuid::Uuid::new_v4().simple())
            }
        }
    }
    fn summary(&self) -> String {
        match self {
            Self::Schedule { cron, at } => format!("the schedule \"{cron}\" (UTC) at {at}"),
            Self::Push {
                branch,
                commit,
                paths,
                paths_known,
            } => {
                let changed = if *paths_known && !paths.is_empty() {
                    format!("; changed paths include {}", paths.join(", "))
                } else {
                    String::new()
                };
                format!("a push to {branch} (commit {commit}){changed}")
            }
            Self::Manual { by, .. } => format!("a manual run by {by}"),
        }
    }
}

/// The idempotency key of one firing: the same repository, automation and
/// trigger always give the same key, so a repeated tick, webhook or retry
/// never starts a second run.
pub fn firing_key(repository: &str, automation: &str, trigger: &Trigger) -> String {
    let digest = Sha256::digest(format!(
        "{repository}\n{automation}\n{}",
        trigger.identity()
    ));
    digest[..12].iter().map(|b| format!("{b:02x}")).collect()
}

/// The native run's idempotency key for one provider attempt of a firing.
pub fn run_idempotency_key(automation: &str, firing: &str, provider: &str) -> String {
    format!("automation.{automation}.{firing}.{provider}")
}

/// One firing and the run it started.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Run {
    /// The firing key.
    pub id: String,
    pub automation: String,
    pub trigger: Trigger,
    /// `pending` (waiting for the automation's active run), `starting`,
    /// `queued`, `running`, `succeeded`, `failed`, `cancelled`, `skipped` or
    /// `refused` (could not start).
    pub status: String,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The commit of the automation file the run used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The branch the run may push.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Whether that branch exists after the run (checked when it ends).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_pushed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// Token usage as the run reported it (subscription runs have no price).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
impl Run {
    pub fn new(id: String, automation: &str, trigger: Trigger, now: DateTime<Utc>) -> Self {
        Self {
            id,
            automation: automation.to_owned(),
            trigger,
            status: "starting".into(),
            created_at: now.to_rfc3339(),
            run_id: None,
            pool: None,
            provider: None,
            model: None,
            commit: None,
            branch: None,
            branch_pushed: None,
            started_at: None,
            finished_at: None,
            usage: None,
            elapsed_seconds: None,
            error: None,
        }
    }
    pub fn active(&self) -> bool {
        matches!(
            self.status.as_str(),
            "pending" | "starting" | "queued" | "running"
        )
    }
    /// Settle from the native run record: returns whether it changed.
    pub fn settle(&mut self, record: &Value) -> bool {
        let before = self.clone();
        let status = record["status"].as_str().unwrap_or("");
        self.status = match status {
            "queued" | "running" | "succeeded" | "failed" | "cancelled" => status.to_owned(),
            "" => self.status.clone(),
            "starting" => "queued".into(),
            _ => "failed".into(),
        };
        if let Some(s) = record["started_at"].as_str() {
            self.started_at = Some(s.to_owned());
        }
        if let Some(s) = record["finished_at"].as_str() {
            self.finished_at = Some(s.to_owned());
        }
        if !record["usage"].is_null() {
            self.usage = Some(record["usage"].clone());
        }
        if let Some(e) = record["elapsed_seconds"].as_f64() {
            self.elapsed_seconds = Some(e);
        }
        if matches!(self.status.as_str(), "failed" | "cancelled") && self.error.is_none() {
            self.error = record["failure"]["reason"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| {
                    record["result"]["exit_code"]
                        .as_i64()
                        .map(|c| format!("exit code {c}"))
                });
        }
        *self != before
    }
}

/// The branch a firing may push: `<prefix><YYYY-MM-DD>`, with `-2`, `-3`...
/// when an earlier run in `taken` already used that name.
pub fn branch_for(prefix: &str, now: DateTime<Utc>, taken: &[&str]) -> String {
    let base = format!("{prefix}{}", now.format("%Y-%m-%d"));
    if !taken.contains(&base.as_str()) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|b| !taken.contains(&b.as_str()))
        .unwrap_or(base)
}

/// The native prompt: the controller's fixed rules, then the file's task.
pub fn prompt(
    a: &Automation,
    repository: &str,
    default_branch: &str,
    commit: Option<&str>,
    trigger: &Trigger,
    branch: Option<&str>,
) -> String {
    let checkout = match commit {
        Some(c) => format!("git checkout {c} (the commit of {default_branch} this run is for)"),
        None => format!("git checkout {default_branch}"),
    };
    let push = match branch {
        Some(b) => format!(
            "Pushing: you may push only the new branch {b}: git checkout -b {b}, commit, then git push origin {b}. Never push {default_branch} or any other branch, never force-push (no --force, --force-with-lease or + refspecs), and never delete a branch or tag. If nothing is worth changing, push nothing and say so."
        ),
        None => "Pushing: this automation is read-only. Do not commit or push anything.".into(),
    };
    let mut out = format!(
        "You are running the Engine Box automation \"{name}\" ({path}) on the repository {repository}, started by {trigger}.\n\nSetup: git clone \"$ENGINEBOX_REPO_REMOTE\" \"$ENGINEBOX_RUN_DIR\" (the directory is empty), cd into it, and {checkout}.\n{push}\nFinish with a short summary of what you did and the branch you pushed, if any.\n\nTask:\n",
        name = a.name,
        path = a.path,
        trigger = trigger.summary(),
    );
    if let Some(skill) = &a.skill {
        out.push_str(&format!(
            "Use the {skill} skill for this task (load it before you start).\n"
        ));
    }
    if let Some(p) = &a.prompt {
        out.push_str(p.trim());
        out.push('\n');
    }
    out
}

// Records --------------------------------------------------------------------

/// One automation file as last read: valid, or its error.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation: Option<Automation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Everything the controller keeps for one repository's automations.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub repository: String,
    pub org_id: String,
    /// The default branch commit the files were read at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_error: Option<String>,
    #[serde(default)]
    pub files: Vec<FileEntry>,
    /// Per automation: the time its schedule was last evaluated up to.
    #[serde(default)]
    pub schedule_checked: std::collections::BTreeMap<String, String>,
    /// Newest last.
    #[serde(default)]
    pub runs: Vec<Run>,
}
impl Record {
    pub fn automations(&self) -> impl Iterator<Item = &Automation> {
        self.files.iter().filter_map(|f| f.automation.as_ref())
    }
    pub fn automation(&self, id: &str) -> Option<&Automation> {
        self.automations().find(|a| a.id == id)
    }
    /// Replace the files with those read at `commit`. Schedules of new
    /// automations start now, so adding a file never backfills old slots.
    pub fn set_files(&mut self, commit: &str, files: Vec<(String, String)>, now: DateTime<Utc>) {
        let mut entries: Vec<FileEntry> = files
            .into_iter()
            .map(|(path, source)| match Automation::parse(&path, &source) {
                Ok(a) => FileEntry {
                    path,
                    automation: Some(a),
                    error: None,
                },
                Err(e) => FileEntry {
                    path,
                    automation: None,
                    error: Some(format!("{e:#}").chars().take(300).collect()),
                },
            })
            .collect();
        // Two files with the same ID (x.yml and x.yaml): both are refused.
        let ids: Vec<String> = entries
            .iter()
            .filter_map(|e| e.automation.as_ref().map(|a| a.id.clone()))
            .collect();
        for e in &mut entries {
            if let Some(a) = &e.automation
                && ids.iter().filter(|i| **i == a.id).count() > 1
            {
                e.error = Some(format!(
                    "Another file also defines the automation {}.",
                    a.id
                ));
                e.automation = None;
            }
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        self.files = entries;
        self.commit = Some(commit.to_owned());
        self.read_at = Some(now.to_rfc3339());
        self.read_error = None;
        let ids: Vec<String> = self.automations().map(|a| a.id.clone()).collect();
        self.schedule_checked.retain(|k, _| ids.contains(k));
        for id in ids {
            self.schedule_checked
                .entry(id)
                .or_insert_with(|| now.to_rfc3339());
        }
    }
    /// Schedule firings due by `now`: at most one per automation (the latest
    /// due slot), and the evaluation point moves to `now`.
    pub fn due(&mut self, now: DateTime<Utc>) -> Vec<(String, Trigger)> {
        let mut due = Vec::new();
        let automations: Vec<Automation> = self.automations().cloned().collect();
        for a in automations
            .iter()
            .filter(|a| a.enabled && !a.schedule.is_empty())
        {
            let after = self
                .schedule_checked
                .get(&a.id)
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .map(|t| t.with_timezone(&Utc))
                .unwrap_or(now);
            let latest = a
                .schedule
                .iter()
                .filter_map(|c| Some((c, Cron::parse(c).ok()?.latest_due(after, now)?)))
                .max_by_key(|(_, t)| *t);
            if let Some((cron, at)) = latest {
                due.push((
                    a.id.clone(),
                    Trigger::Schedule {
                        cron: cron.clone(),
                        at: at.to_rfc3339(),
                    },
                ));
            }
            self.schedule_checked.insert(a.id.clone(), now.to_rfc3339());
        }
        due
    }
    /// Keep the newest runs, dropping finished ones first.
    pub fn prune(&mut self) {
        while self.runs.len() > MAX_RUNS {
            match self.runs.iter().position(|r| !r.active()) {
                Some(i) => {
                    self.runs.remove(i);
                }
                None => {
                    self.runs.remove(0);
                }
            }
        }
    }
    pub fn public(&self, now: DateTime<Utc>) -> Value {
        json!({
            "repository": self.repository,
            "commit": self.commit,
            "read_at": self.read_at,
            "read_error": self.read_error,
            "automations": self.files.iter().map(|f| match &f.automation {
                Some(a) => {
                    let mut v = a.public(now);
                    v["valid"] = json!(true);
                    v["last_run"] = json!(self.runs.iter().rev().find(|r| r.automation == a.id));
                    v
                }
                None => json!({"path": f.path, "id": id_of(&f.path), "valid": false, "error": f.error}),
            }).collect::<Vec<_>>(),
        })
    }
}

/// Records under `<root>/automations/`, one file per repository.
#[derive(Clone)]
pub struct Store {
    root: PathBuf,
}
impl Store {
    pub fn open(root: &Path) -> Self {
        Self {
            root: root.join("automations"),
        }
    }
    fn path(&self, repository: &str) -> PathBuf {
        self.root
            .join(format!("{}.json", crate::repo_config::file_key(repository)))
    }
    pub fn get(&self, repository: &str) -> Option<Record> {
        state::read_json(&self.path(repository))
            .ok()
            .and_then(|v| serde_json::from_value(v).ok())
    }
    pub fn put(&self, record: &Record) -> Result<()> {
        state::write_json(
            &self.path(&record.repository),
            &serde_json::to_value(record)?,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const DOCS_STEWARD: &str = include_str!("../../../examples/automations/docs-steward.yml");

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn the_docs_steward_example_parses() {
        let a = Automation::parse(".enginebox/automations/docs-steward.yml", DOCS_STEWARD).unwrap();
        assert_eq!(a.id, "docs-steward");
        assert_eq!(a.model, Model::Tier("basic".into()));
        assert_eq!(a.schedule, vec!["0 6 * * 1"]);
        assert_eq!(
            a.push_trigger,
            Some(PushTrigger {
                branches: vec!["main".into()],
                paths: vec!["docs/**".into()]
            })
        );
        assert_eq!(a.branch_prefix.as_deref(), Some("automation/docs-steward/"));
        assert!(a.manual && a.enabled);
        assert_eq!(
            a.model_for("claude", &Value::Null).as_deref(),
            Some("haiku")
        );
        assert_eq!(a.model_for("codex", &Value::Null), None);
        assert_eq!(
            a.model_for("codex", &json!({"codex":{"basic":"gpt-mini"}}))
                .as_deref(),
            Some("gpt-mini")
        );
    }

    #[test]
    fn files_are_validated_strictly() {
        let p = ".enginebox/automations/x.yml";
        let ok = "prompt: tidy\non:\n  manual: true\n";
        assert!(Automation::parse(p, ok).is_ok());
        for (bad, why) in [
            (
                "prompt: x\non: {manual: true}\nextra: 1\n",
                "unknown key extra",
            ),
            (
                "prompt: x\non: {manual: true, cron: x}\n",
                "on: unknown key cron",
            ),
            ("on: {manual: true}\n", "prompt, a skill"),
            ("prompt: x\n", "on names"),
            (
                "prompt: x\non: {schedule: [{cron: '61 * * * *'}]}\n",
                "not 5-field",
            ),
            (
                "prompt: x\non: {schedule: [{cron: '* * * *'}]}\n",
                "not 5-field",
            ),
            ("prompt: x\nmodel: gpt 5\non: {manual: true}\n", "tier"),
            (
                "prompt: x\nmodel: claude-opus-4\non: {manual: true}\n",
                "exactly one runner",
            ),
            (
                "prompt: x\nrunners: [claude, claude]\non: {manual: true}\n",
                "once",
            ),
            (
                "prompt: x\nrunners: [gemini]\non: {manual: true}\n",
                "providers",
            ),
            (
                "prompt: x\ntimeout-minutes: 0\non: {manual: true}\n",
                "timeout-minutes",
            ),
            (
                "prompt: x\npush: {branch-prefix: main}\non: {manual: true}\n",
                "ending in /",
            ),
            (
                "prompt: x\npush: {branch-prefix: refs/heads/}\non: {manual: true}\n",
                "ending in /",
            ),
            (
                "prompt: x\npush: {branch: a/}\non: {manual: true}\n",
                "unknown key branch",
            ),
            (
                "prompt: x\non: {push: {paths: ['../x']}}\n",
                "relative path globs",
            ),
            ("prompt: x\non: {push: {tags: [v1]}}\n", "unknown key tags"),
            (
                "prompt: x\nconcurrency: parallel\non: {manual: true}\n",
                "queue or skip",
            ),
            ("a: &x 1\nprompt: *x\non: {manual: true}\n", "anchors"),
            (
                "prompt: x\nprompt: y\non: {manual: true}\n",
                "duplicated key",
            ),
            ("- 1\n", "mapping"),
        ] {
            let error = format!("{:#}", Automation::parse(p, bad).unwrap_err());
            assert!(error.contains(why), "{bad:?}: {error}");
        }
        assert!(Automation::parse(".enginebox/automations/Bad.yml", ok).is_err());
        assert!(Automation::parse(".enginebox/automations/a/b.yml", ok).is_err());
        assert!(Automation::parse(".enginebox/x.yml", ok).is_err());
        let long = format!(
            "prompt: '{}'\non: {{manual: true}}\n",
            "x".repeat(MAX_PROMPT + 1)
        );
        assert!(Automation::parse(p, &long).is_err());
    }

    #[test]
    fn cron_matches_utc_minutes() {
        let monday_six = Cron::parse("0 6 * * 1").unwrap();
        assert!(monday_six.matches(at("2026-11-09T06:00:00Z")));
        assert!(!monday_six.matches(at("2026-11-10T06:00:00Z")));
        assert_eq!(
            monday_six.next_after(at("2026-11-06T12:00:00Z")),
            Some(at("2026-11-09T06:00:00Z"))
        );
        let every_15 = Cron::parse("*/15 9-17 * * 1-5").unwrap();
        assert!(every_15.matches(at("2026-11-06T09:45:00Z")));
        assert!(!every_15.matches(at("2026-11-07T09:45:00Z")));
        let sunday = Cron::parse("0 0 * * 7").unwrap();
        assert!(sunday.matches(at("2026-11-08T00:00:00Z")));
        // Both day fields restricted: either matches.
        let either = Cron::parse("0 0 1 * 1").unwrap();
        assert!(either.matches(at("2026-11-09T00:00:00Z")));
        assert!(either.matches(at("2026-12-01T00:00:00Z")));
        // Only the latest due slot fires; nothing older than a day is caught up.
        let hourly = Cron::parse("0 * * * *").unwrap();
        assert_eq!(
            hourly.latest_due(at("2026-11-06T09:30:00Z"), at("2026-11-06T12:10:00Z")),
            Some(at("2026-11-06T12:00:00Z"))
        );
        assert_eq!(
            hourly.latest_due(at("2026-11-06T12:00:00Z"), at("2026-11-06T12:59:59Z")),
            None
        );
        assert_eq!(
            monday_six.latest_due(at("2026-11-01T00:00:00Z"), at("2026-11-12T00:00:00Z")),
            None
        );
    }

    #[test]
    fn path_globs() {
        assert!(glob_match("docs/**", "docs/a.md"));
        assert!(glob_match("docs/**", "docs/x/y/z.md"));
        assert!(!glob_match("docs/**", "src/docs/a.md"));
        assert!(glob_match("**/*.md", "README.md"));
        assert!(glob_match("**/*.md", "a/b/c.md"));
        assert!(glob_match("docs/*.md", "docs/a.md"));
        assert!(!glob_match("docs/*.md", "docs/x/a.md"));
        assert!(glob_match("src/?.rs", "src/a.rs"));
        assert!(glob_match("main", "main"));
        assert!(glob_match("release/*", "release/1.0"));
        assert!(!glob_match("release/*", "release/1.0/x"));
    }

    #[test]
    fn push_triggers_filter_branches_and_paths_and_ignore_their_own_branches() {
        let a = Automation::parse(".enginebox/automations/docs-steward.yml", DOCS_STEWARD).unwrap();
        let docs = vec!["docs/a.md".to_owned()];
        let code = vec!["src/a.rs".to_owned()];
        assert!(a.push_fires("main", "main", Some(&docs)));
        assert!(!a.push_fires("main", "main", Some(&code)));
        assert!(a.push_fires("main", "main", None));
        assert!(!a.push_fires("main", "feature", Some(&docs)));
        let any = Automation::parse(
            ".enginebox/automations/x.yml",
            "prompt: x\npush: {branch-prefix: automation/x/}\non: {push: {branches: ['**']}}\n",
        )
        .unwrap();
        assert!(any.push_fires("main", "feature/y", Some(&code)));
        assert!(!any.push_fires("main", "automation/x/2026-11-06", Some(&code)));
        let default_only =
            Automation::parse(".enginebox/automations/x.yml", "prompt: x\non: {push: }\n").unwrap();
        assert!(default_only.push_fires("trunk", "trunk", Some(&code)));
        assert!(!default_only.push_fires("trunk", "main", Some(&code)));
    }

    #[test]
    fn firing_keys_are_stable_per_trigger() {
        let s = Trigger::Schedule {
            cron: "0 6 * * 1".into(),
            at: "2026-11-09T06:00:00+00:00".into(),
        };
        assert_eq!(firing_key("a/b", "x", &s), firing_key("a/b", "x", &s));
        assert_ne!(firing_key("a/b", "x", &s), firing_key("a/b", "y", &s));
        let p1 = Trigger::push("main", "abc", Some(&["docs/a".to_owned()]));
        let p2 = Trigger::push("main", "abc", None);
        assert_eq!(firing_key("a/b", "x", &p1), firing_key("a/b", "x", &p2));
        let m = Trigger::Manual {
            by: "owner".into(),
            key: None,
        };
        assert_ne!(firing_key("a/b", "x", &m), firing_key("a/b", "x", &m));
        let key = run_idempotency_key("docs-steward", &firing_key("a/b", "x", &s), "claude");
        assert!(
            enginebox_agents::service::valid_idempotency_key(&key),
            "{key}"
        );
    }

    #[test]
    fn schedules_fire_once_per_slot_and_new_files_do_not_backfill() {
        let mut r = Record::default();
        r.set_files(
            "c1",
            vec![(
                ".enginebox/automations/docs-steward.yml".into(),
                DOCS_STEWARD.into(),
            )],
            at("2026-11-09T05:59:00Z"),
        );
        assert!(r.due(at("2026-11-09T05:59:30Z")).is_empty());
        let due = r.due(at("2026-11-09T06:00:20Z"));
        assert_eq!(due.len(), 1);
        assert!(r.due(at("2026-11-09T06:00:50Z")).is_empty());
        // A file added after the slot does not fire for it.
        let mut late = Record::default();
        late.set_files(
            "c1",
            vec![(
                ".enginebox/automations/docs-steward.yml".into(),
                DOCS_STEWARD.into(),
            )],
            at("2026-11-09T06:05:00Z"),
        );
        assert!(late.due(at("2026-11-09T06:06:00Z")).is_empty());
    }

    #[test]
    fn invalid_and_duplicate_files_are_reported_not_run() {
        let mut r = Record::default();
        r.set_files(
            "c1",
            vec![
                (
                    ".enginebox/automations/a.yml".into(),
                    "prompt: x\non: {manual: true}\n".into(),
                ),
                (
                    ".enginebox/automations/a.yaml".into(),
                    "prompt: x\non: {manual: true}\n".into(),
                ),
                (".enginebox/automations/b.yml".into(), "nope: 1\n".into()),
            ],
            at("2026-11-09T06:05:00Z"),
        );
        assert_eq!(r.automations().count(), 0);
        let public = r.public(at("2026-11-09T06:05:00Z"));
        assert_eq!(public["automations"].as_array().unwrap().len(), 3);
        assert!(
            public["automations"][2]["error"]
                .as_str()
                .unwrap()
                .contains("unknown key nope")
        );
    }

    #[test]
    fn the_prompt_carries_the_rules_and_the_task() {
        let a = Automation::parse(".enginebox/automations/docs-steward.yml", DOCS_STEWARD).unwrap();
        let t = Trigger::push("main", "abc123", Some(&["docs/x.md".to_owned()]));
        let branch = branch_for("automation/docs-steward/", at("2026-11-09T06:00:00Z"), &[]);
        assert_eq!(branch, "automation/docs-steward/2026-11-09");
        assert_eq!(
            branch_for(
                "automation/docs-steward/",
                at("2026-11-09T06:00:00Z"),
                &[&branch]
            ),
            "automation/docs-steward/2026-11-09-2"
        );
        let text = prompt(&a, "alpha/site", "main", Some("abc123"), &t, Some(&branch));
        assert!(text.contains("git checkout abc123"));
        assert!(text.contains("git push origin automation/docs-steward/2026-11-09"));
        assert!(text.contains("never force-push"));
        assert!(text.contains("docs/decisions/"));
        assert!(text.len() < 12000);
        let read_only = prompt(&a, "alpha/site", "main", None, &t, None);
        assert!(read_only.contains("read-only"));
        assert!(prefix_covers_default("main/", "main"));
        assert!(prefix_covers_default("release/", "release/1"));
        assert!(!prefix_covers_default("automation/x/", "main"));
    }

    #[test]
    fn runs_settle_from_the_native_record() {
        let mut r = Run::new(
            "k".into(),
            "a",
            Trigger::Manual {
                by: "o".into(),
                key: None,
            },
            Utc::now(),
        );
        assert!(r.settle(&json!({"status":"running","started_at":"2026-11-09T06:00:00Z"})));
        assert!(r.active());
        assert!(r.settle(&json!({"status":"failed","failure":{"reason":"timed_out"},"usage":{"input_tokens":3},"elapsed_seconds":4.5})));
        assert_eq!(
            (r.status.as_str(), r.error.as_deref(), r.elapsed_seconds),
            ("failed", Some("timed_out"), Some(4.5))
        );
        assert!(!r.active());
    }
}
