//! Templated review-prompt rendering.
//!
//! Three resolution layers per slot/variable, picked first match:
//!   1. `prompt_templates(media_kind, slot)` / `prompt_variables(media_kind, name)` — per-kind override.
//!   2. `prompt_templates('default', slot)` / `prompt_variables('default', name)` — cross-kind default override.
//!   3. Rust constant baked below — the "factory default".
//!
//! The render context (`build_render_context`) feeds both system and user
//! slots plus every variable. Variables render before slots in a fixed order
//! so later variables can reference earlier ones via `vars.<name>`.

use std::collections::BTreeMap;

use koblem_client::types::{ReviewCandidate, ReviewRequest, WantContext};
use minijinja::{Environment, Value as MJValue, value::Serde};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::context::ReviewContext;
use crate::repositories::prompt_templates;

pub const MEDIA_KIND_DEFAULT: &str = "default";
pub const SLOT_SYSTEM: &str = "system";
pub const SLOT_USER: &str = "user";

/// Variables resolved before user-defined ones, in this exact order. Each one
/// is visible to later variables via `vars.<name>`.
pub const SEEDED_VARIABLES: &[&str] = &["format", "signal_descriptions", "watch_patterns"];

/// Map a `WantContext` to the snake_case key used in the DB and UI.
pub fn media_kind_for(want: &WantContext) -> &'static str {
    match want {
        WantContext::TvEpisode { .. } => "tv_episode",
        WantContext::TvSeason { .. } => "tv_season",
        WantContext::Movie { .. } => "movie",
        WantContext::SportEvent { .. } => "sport_event",
        WantContext::Other => "other",
    }
}

/// All media kinds the UI can override (excluding `'default'` which is the
/// fallback bucket every kind layers on top of).
pub fn all_media_kinds() -> &'static [&'static str] {
    &["tv_episode", "tv_season", "movie", "sport_event", "other"]
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("template render error in {slot}: {source}")]
    Template {
        slot: String,
        #[source]
        source: minijinja::Error,
    },
    #[error("variable render error in {name}: {source}")]
    Variable {
        name: String,
        #[source]
        source: minijinja::Error,
    },
    #[error("repository error: {0}")]
    Repo(#[from] sqlx::Error),
}

#[derive(Debug, Clone)]
pub struct MessagePair {
    pub system: String,
    pub user: String,
}

/// Resolve the template body for `(media_kind, slot)`, walking the three
/// fallback levels. Always returns something — the Rust default is the floor.
pub async fn resolve_template(
    pool: &PgPool,
    media_kind: &str,
    slot: &str,
) -> Result<String, EngineError> {
    if media_kind != MEDIA_KIND_DEFAULT
        && let Some(t) = prompt_templates::get_template(pool, media_kind, slot).await?
    {
        return Ok(t.body);
    }
    if let Some(t) = prompt_templates::get_template(pool, MEDIA_KIND_DEFAULT, slot).await? {
        return Ok(t.body);
    }
    Ok(default_template(slot).to_string())
}

/// Build the JSON render context fed to every template + variable.
pub fn build_render_context(
    ctx: &ReviewContext,
    request: &ReviewRequest,
    candidates: &[ReviewCandidate],
) -> Value {
    let candidates_with_signals: Vec<Value> = candidates
        .iter()
        .map(|c| {
            let signals = derive_signals(c);
            let mut v = serde_json::to_value(c).unwrap_or_else(|_| json!({}));
            if let Some(obj) = v.as_object_mut() {
                obj.insert(
                    "signals".to_string(),
                    Value::Array(signals.into_iter().map(Value::String).collect()),
                );
            }
            v
        })
        .collect();

    json!({
        "today": today_iso(),
        "wanted_title": request.wanted_title,
        "want": serde_json::to_value(&request.want).unwrap_or_else(|_| json!({})),
        "quality_profile": serde_json::to_value(&request.quality_profile).unwrap_or(Value::Null),
        "candidates": candidates_with_signals,
        "disambiguation": serde_json::to_value(&ctx.disambiguation).unwrap_or_else(|_| json!({"siblings": []})),
        "corrections": serde_json::to_value(&ctx.corrections).unwrap_or_else(|_| json!({"rules": [], "past_mistakes": []})),
        "vars": {},
    })
}

/// Per-call template overrides for `build_messages`. Used by the testbed
/// "try this prompt without saving it" flow and by the prompt-preview
/// endpoint. `None` for either slot falls back to the DB-resolved template.
#[derive(Debug, Default, Clone)]
pub struct TemplateOverrides<'a> {
    pub system: Option<&'a str>,
    pub user: Option<&'a str>,
    /// Per-variable overrides (variable name → draft body). Each entry
    /// supersedes the per-kind / default DB value for resolution.
    pub variables: Option<&'a std::collections::HashMap<String, String>>,
}

/// Build both rendered messages for a review request.
pub async fn build_messages(
    pool: &PgPool,
    ctx: &ReviewContext,
    request: &ReviewRequest,
    candidates: &[ReviewCandidate],
) -> Result<MessagePair, EngineError> {
    build_messages_with_overrides(
        pool,
        ctx,
        request,
        candidates,
        &TemplateOverrides::default(),
    )
    .await
}

pub async fn build_messages_with_overrides(
    pool: &PgPool,
    ctx: &ReviewContext,
    request: &ReviewRequest,
    candidates: &[ReviewCandidate],
    overrides: &TemplateOverrides<'_>,
) -> Result<MessagePair, EngineError> {
    let media_kind = media_kind_for(&request.want);
    let mut root = build_render_context(ctx, request, candidates);
    resolve_variables_with_overrides(pool, media_kind, &mut root, overrides.variables).await?;

    let env = make_environment();

    let system_body = match overrides.system {
        Some(s) => s.to_string(),
        None => resolve_template(pool, media_kind, SLOT_SYSTEM).await?,
    };
    let user_body = match overrides.user {
        Some(s) => s.to_string(),
        None => resolve_template(pool, media_kind, SLOT_USER).await?,
    };

    let system = env
        .render_str(&system_body, Serde(&root))
        .map_err(|source| EngineError::Template {
            slot: SLOT_SYSTEM.to_string(),
            source,
        })?;
    let user =
        env.render_str(&user_body, Serde(&root))
            .map_err(|source| EngineError::Template {
                slot: SLOT_USER.to_string(),
                source,
            })?;
    Ok(MessagePair { system, user })
}

async fn resolve_variables_with_overrides(
    pool: &PgPool,
    media_kind: &str,
    root_ctx: &mut Value,
    overrides: Option<&std::collections::HashMap<String, String>>,
) -> Result<(), EngineError> {
    // ONE SQL round-trip returns every variable's per-kind + default body in
    // a single result set — was previously N+1 across seeded + user-defined
    // names, which fired ~14 queries per review on the hot path.
    let resolved =
        prompt_templates::list_resolved_variables(pool, media_kind, SEEDED_VARIABLES).await?;

    // Collapse to (name → body) using the same fallback chain as the CRUD
    // endpoints: per-kind row → default row → Rust seeded constant.
    let mut bodies: BTreeMap<String, String> = BTreeMap::new();
    for v in resolved {
        let body = v
            .per_kind_body
            .or(v.default_body)
            .unwrap_or_else(|| default_variable(&v.name).to_string());
        bodies.insert(v.name, body);
    }

    // Per-call overrides supersede everything (testbed "try without saving"
    // and the preview endpoint).
    if let Some(o) = overrides {
        for (k, v) in o {
            bodies.insert(k.clone(), v.clone());
        }
    }

    // Render in fixed order: seeded first (so user-defined vars can
    // reference them via {{ vars.signal_descriptions }} etc.), then the
    // rest alphabetically.
    let mut order: Vec<String> = SEEDED_VARIABLES.iter().map(|s| s.to_string()).collect();
    let extras: Vec<String> = bodies
        .keys()
        .filter(|n| !SEEDED_VARIABLES.contains(&n.as_str()))
        .cloned()
        .collect();
    order.extend(extras);

    let env = make_environment();
    for name in order {
        let body = bodies
            .get(&name)
            .cloned()
            .unwrap_or_else(|| default_variable(&name).to_string());
        let rendered =
            env.render_str(&body, Serde(&*root_ctx))
                .map_err(|source| EngineError::Variable {
                    name: name.clone(),
                    source,
                })?;
        root_ctx
            .as_object_mut()
            .expect("render context must be a JSON object")
            .entry("vars")
            .or_insert_with(|| Value::Object(Default::default()))
            .as_object_mut()
            .expect("vars must be a JSON object")
            .insert(name, Value::String(rendered));
    }
    Ok(())
}

fn make_environment() -> Environment<'static> {
    let mut env = Environment::new();
    minijinja_contrib::add_to_environment(&mut env);
    register_filters(&mut env);
    env
}

fn register_filters(env: &mut Environment<'_>) {
    env.add_filter("render_candidate_list", filter_render_candidate_list);
    env.add_filter("render_siblings", filter_render_siblings);
    env.add_filter("regex_replace", filter_regex_replace);
    env.add_filter("flatten", filter_flatten);
    env.add_filter("pluck", filter_pluck);
}

// ── Filters ─────────────────────────────────────────────────────────

/// `{{ candidates | render_candidate_list }}` → "  1. NAME [signals]\n  2. ..."
fn filter_render_candidate_list(value: MJValue) -> Result<String, minijinja::Error> {
    let arr = mj_value_to_json(&value);
    let Value::Array(arr) = arr else {
        return Err(minijinja::Error::new(
            minijinja::ErrorKind::InvalidOperation,
            "render_candidate_list expects an array of candidates",
        ));
    };
    let mut out = String::new();
    for (i, c) in arr.iter().enumerate() {
        let name = c.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let signals: Vec<String> = c
            .get("signals")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let suffix = if signals.is_empty() {
            String::new()
        } else {
            format!(" [{}]", signals.join(" "))
        };
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&format!("  {}. {}{}", i + 1, name, suffix));
    }
    Ok(out)
}

/// `{{ disambiguation.siblings | render_siblings }}`
/// → "Also exists: A (2001) [Ended]; B (2021) [Returning Series]; ..."
fn filter_render_siblings(value: MJValue) -> Result<String, minijinja::Error> {
    let arr = mj_value_to_json(&value);
    let Value::Array(arr) = arr else {
        return Err(minijinja::Error::new(
            minijinja::ErrorKind::InvalidOperation,
            "render_siblings expects an array of siblings",
        ));
    };
    if arr.is_empty() {
        return Ok(String::new());
    }
    let parts: Vec<String> = arr
        .iter()
        .map(|s| {
            let title = s.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let year = s.get("year").and_then(|v| v.as_i64());
            let status = s.get("status").and_then(|v| v.as_str());
            let season = s.get("season").and_then(|v| v.as_str());
            let episode = s.get("episode").and_then(|v| v.as_str());
            let ep_title = s.get("episode_title").and_then(|v| v.as_str());

            let mut desc = title.to_string();
            if let Some(y) = year {
                desc = format!("{desc} ({y})");
            }
            if let Some(st) = status {
                desc = format!("{desc} [{st}]");
            }
            if let (Some(s), Some(e), Some(t)) = (season, episode, ep_title) {
                desc = format!("{desc} — S{s}E{e} = \"{t}\"");
            }
            desc
        })
        .collect();
    Ok(format!("Also exists: {}", parts.join("; ")))
}

/// `{{ s | regex_replace('pattern', 'replacement') }}`
fn filter_regex_replace(
    s: String,
    pattern: String,
    replacement: String,
) -> Result<String, minijinja::Error> {
    let re = regex::Regex::new(&pattern).map_err(|e| {
        minijinja::Error::new(
            minijinja::ErrorKind::InvalidOperation,
            format!("regex_replace: invalid pattern: {e}"),
        )
    })?;
    Ok(re.replace_all(&s, replacement.as_str()).into_owned())
}

/// `{{ nested | flatten }}` flattens one level: `[[1,2],[3]]` → `[1,2,3]`.
fn filter_flatten(value: MJValue) -> Result<MJValue, minijinja::Error> {
    let json = mj_value_to_json(&value);
    let Value::Array(arr) = json else {
        return Err(minijinja::Error::new(
            minijinja::ErrorKind::InvalidOperation,
            "flatten expects an array",
        ));
    };
    let mut out = Vec::new();
    for item in arr {
        if let Value::Array(inner) = item {
            out.extend(inner);
        } else {
            out.push(item);
        }
    }
    Ok(MJValue::from(Serde(Value::Array(out))))
}

/// `{{ candidates | pluck('name') }}` is sugar for `map(attribute='name') | list`.
fn filter_pluck(value: MJValue, attr: String) -> Result<MJValue, minijinja::Error> {
    let json = mj_value_to_json(&value);
    let Value::Array(arr) = json else {
        return Err(minijinja::Error::new(
            minijinja::ErrorKind::InvalidOperation,
            "pluck expects an array",
        ));
    };
    let mut out = Vec::new();
    for item in arr {
        if let Value::Object(obj) = &item
            && let Some(v) = obj.get(&attr)
        {
            out.push(v.clone());
        }
    }
    Ok(MJValue::from(Serde(Value::Array(out))))
}

fn mj_value_to_json(v: &MJValue) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

// ── Signal derivation ───────────────────────────────────────────────

/// Build the bracketed-tag list for one candidate. Mirrors the inline emission
/// the old `review_candidates_messages` did so the default user template's
/// `render_candidate_list` filter produces the same output.
pub fn derive_signals(c: &ReviewCandidate) -> Vec<String> {
    let mut tags = Vec::new();
    let pq = &c.parsed_quality;
    if pq.is_proper {
        tags.push("PROPER".into());
    }
    if pq.is_repack {
        tags.push("REPACK".into());
    }
    if pq.is_remux {
        tags.push("REMUX".into());
    }
    if pq.is_dolby_vision {
        tags.push("DV".into());
    } else if pq.is_hdr10plus {
        tags.push("HDR10+".into());
    } else if pq.is_hdr10 {
        tags.push("HDR10".into());
    } else if pq.is_hdr {
        tags.push("HDR".into());
    }
    if pq.is_3d {
        tags.push("3D".into());
    }
    if pq.is_hardcoded_subs {
        tags.push("HARDCODED-SUBS".into());
    }
    if let Some(d) = &c.publish_date {
        tags.push(format!("pub:{}", normalize_date(d)));
    }
    if let Some(s) = c.seeders {
        tags.push(format!("seed:{s}"));
    }
    if let Some(v) = c.votes
        && v != 0
    {
        tags.push(format!("votes:{v}"));
    }
    if let Some(g) = c.grabs {
        tags.push(format!("grabs:{g}"));
    }
    tags
}

fn today_iso() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}",
        now.year(),
        now.month() as u8,
        now.day()
    )
}

fn normalize_date(raw: &str) -> String {
    if let Ok(dt) = time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc2822)
    {
        return format!("{:04}-{:02}-{:02}", dt.year(), dt.month() as u8, dt.day());
    }
    if raw.len() >= 10 && raw.as_bytes().get(4) == Some(&b'-') {
        return raw[..10].to_string();
    }
    raw.to_string()
}

// ── Default templates and variables (Rust-baked level-3 fallback) ───

pub fn default_template(slot: &str) -> &'static str {
    match slot {
        SLOT_SYSTEM => DEFAULT_SYSTEM_TEMPLATE,
        SLOT_USER => DEFAULT_USER_TEMPLATE,
        _ => "",
    }
}

pub fn default_variable(name: &str) -> &'static str {
    match name {
        "signal_descriptions" => DEFAULT_SIGNAL_DESCRIPTIONS,
        "watch_patterns" => DEFAULT_WATCH_PATTERNS,
        "format" => DEFAULT_FORMAT,
        _ => "",
    }
}

const DEFAULT_SYSTEM_TEMPLATE: &str = include_str!("prompt_engine/default_system.tmpl");
const DEFAULT_USER_TEMPLATE: &str = include_str!("prompt_engine/default_user.tmpl");
const DEFAULT_SIGNAL_DESCRIPTIONS: &str =
    include_str!("prompt_engine/default_signal_descriptions.tmpl");
const DEFAULT_WATCH_PATTERNS: &str = include_str!("prompt_engine/default_watch_patterns.tmpl");
const DEFAULT_FORMAT: &str = include_str!("prompt_engine/default_format.tmpl");

#[derive(Debug, Serialize)]
pub struct ContextSchemaField {
    pub path: &'static str,
    pub kind: &'static str,
    pub note: &'static str,
}

/// Documentation of the render-context shape, used by the "Available context"
/// panel in the Prompts UI so users know what to reference.
pub fn context_schema() -> &'static [ContextSchemaField] {
    &[
        ContextSchemaField {
            path: "today",
            kind: "string",
            note: "ISO YYYY-MM-DD, server time",
        },
        ContextSchemaField {
            path: "wanted_title",
            kind: "string",
            note: "loose title from kobload",
        },
        ContextSchemaField {
            path: "want.kind",
            kind: "string",
            note: "tv_episode | tv_season | movie | sport_event | other",
        },
        ContextSchemaField {
            path: "want.season",
            kind: "string?",
            note: "TV episode/season",
        },
        ContextSchemaField {
            path: "want.episode",
            kind: "string?",
            note: "TV episode",
        },
        ContextSchemaField {
            path: "want.episode_title",
            kind: "string?",
            note: "TV episode",
        },
        ContextSchemaField {
            path: "want.air_date",
            kind: "string?",
            note: "TV episode",
        },
        ContextSchemaField {
            path: "want.year",
            kind: "int?",
            note: "TV episode/season, movie",
        },
        ContextSchemaField {
            path: "want.episodes_needed[]",
            kind: "list",
            note: "TV season; each {season, episode, sort_order, air_date, title, in_library, library_video_height}",
        },
        ContextSchemaField {
            path: "want.league_name",
            kind: "string",
            note: "sport_event",
        },
        ContextSchemaField {
            path: "want.round",
            kind: "int?",
            note: "sport_event",
        },
        ContextSchemaField {
            path: "want.location",
            kind: "string?",
            note: "sport_event",
        },
        ContextSchemaField {
            path: "want.session_type",
            kind: "string?",
            note: "sport_event (Race/Sprint/Qualifying/...)",
        },
        ContextSchemaField {
            path: "want.event_title",
            kind: "string?",
            note: "sport_event",
        },
        ContextSchemaField {
            path: "want.event_date",
            kind: "string?",
            note: "sport_event",
        },
        ContextSchemaField {
            path: "quality_profile",
            kind: "object?",
            note: "min/max_resolution, preferred_/rejected_ lists, reject_hardcoded_subs, reject_remux",
        },
        ContextSchemaField {
            path: "candidates[]",
            kind: "list",
            note: "name, kobload_score, parsed_quality, signals[], publish_date, seeders, votes, grabs, reject_reasons",
        },
        ContextSchemaField {
            path: "candidates[].signals",
            kind: "string[]",
            note: "PROPER, REPACK, REMUX, DV, HDR..., 3D, HARDCODED-SUBS, pub:DATE, seed:N, votes:N, grabs:N",
        },
        ContextSchemaField {
            path: "disambiguation.siblings[]",
            kind: "list",
            note: "{title, year, status, season?, episode?, episode_title?} — other shows that match the title loosely",
        },
        ContextSchemaField {
            path: "corrections.rules[]",
            kind: "list",
            note: "{rule, scope, entity_name?}",
        },
        ContextSchemaField {
            path: "corrections.past_mistakes[]",
            kind: "list",
            note: "{model_decision, release_name, correct_decision, reason}",
        },
        ContextSchemaField {
            path: "vars.<name>",
            kind: "string",
            note: "any resolved variable — signal_descriptions, watch_patterns, format, plus your own",
        },
    ]
}

#[cfg(test)]
mod render_snapshots {
    //! Snapshot tests pinning the default user template's output for every
    //! shipped fixture. When the template (or this test's synthetic context)
    //! intentionally changes, re-bless the goldens with:
    //!   `BLESS=1 cargo test --manifest-path api/Cargo.toml render_snapshots`
    //! Otherwise the test reads the golden and asserts byte-equality after
    //! normalising `CURRENT DATE: YYYY-MM-DD` → `CURRENT DATE: <today>` so
    //! snapshots stay stable across days.

    use super::*;
    use crate::context::{
        CorrectionRule, Corrections, Disambiguation, DisambiguationSibling, PastMistake,
    };

    fn render_fixture(fixture: &str, ctx: &ReviewContext) -> String {
        let raw = std::fs::read_to_string(format!("src/fixtures/{fixture}.json"))
            .expect("fixture must exist");
        let r: koblem_client::types::ReviewRequest =
            serde_json::from_str(&raw).expect("fixture must parse");
        let mut root = build_render_context(ctx, &r, &r.candidates);
        // Snapshot path doesn't exercise the variable resolver — variables
        // are an orthogonal concern and have their own DB tests.
        root.as_object_mut()
            .unwrap()
            .insert("vars".into(), serde_json::json!({}));
        let out = make_environment()
            .render_str(DEFAULT_USER_TEMPLATE, Serde(&root))
            .expect("default user template must render");
        normalise_today(&out)
    }

    fn normalise_today(s: &str) -> String {
        regex::Regex::new(r"CURRENT DATE: \d{4}-\d{2}-\d{2}")
            .unwrap()
            .replace(s, "CURRENT DATE: <today>")
            .into_owned()
    }

    fn assert_snapshot(name: &str, actual: &str) {
        let path = format!("src/fixtures/expected_user/{name}.txt");
        if std::env::var("BLESS").is_ok() {
            std::fs::write(&path, actual).expect("write golden");
            eprintln!("blessed {path}");
            return;
        }
        let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
            panic!(
                "missing golden {path}\n\nrun with BLESS=1 to create it. \
                 Actual rendered output:\n{actual}"
            )
        });
        assert_eq!(
            actual, expected,
            "\n\n--- {name} drift ---\nActual:\n{actual}\nExpected:\n{expected}\n\n\
             If the template change is intentional, re-bless with `BLESS=1 cargo test`."
        );
    }

    #[test]
    fn snapshot_tv_episode() {
        let out = render_fixture("tv_episode", &ReviewContext::default());
        assert_snapshot("tv_episode", &out);
    }

    #[test]
    fn snapshot_tv_season() {
        let out = render_fixture("tv_season", &ReviewContext::default());
        assert_snapshot("tv_season", &out);
    }

    #[test]
    fn snapshot_movie() {
        let out = render_fixture("movie", &ReviewContext::default());
        assert_snapshot("movie", &out);
    }

    #[test]
    fn snapshot_sport_event() {
        let out = render_fixture("sport_event", &ReviewContext::default());
        assert_snapshot("sport_event", &out);
    }

    #[test]
    fn snapshot_other() {
        let out = render_fixture("other", &ReviewContext::default());
        assert_snapshot("other", &out);
    }

    /// Locks in the rendering of `disambiguation.siblings` + `corrections`
    /// (rules + past mistakes) — these don't appear in any base fixture, so
    /// without this snapshot they'd silently regress.
    #[test]
    fn snapshot_tv_episode_with_disambiguation_and_corrections() {
        let ctx = ReviewContext {
            disambiguation: Disambiguation {
                siblings: vec![
                    DisambiguationSibling {
                        title: "Severance: Behind the Cigarette Brand".into(),
                        year: Some(2024),
                        status: Some("Documentary".into()),
                        ..Default::default()
                    },
                    DisambiguationSibling {
                        title: "Severance".into(),
                        year: Some(2006),
                        status: Some("Movie".into()),
                        season: Some("2".into()),
                        episode: Some("7".into()),
                        episode_title: Some("N/A — film".into()),
                    },
                ],
            },
            corrections: Corrections {
                rules: vec![
                    CorrectionRule {
                        rule: "prefer NTb releases for ATVP shows".into(),
                        scope: "release-classification".into(),
                        entity_name: None,
                    },
                    CorrectionRule {
                        rule: "skip the leaked work print".into(),
                        scope: "entity".into(),
                        entity_name: Some("Severance".into()),
                    },
                ],
                past_mistakes: vec![PastMistake {
                    model_decision: "accept".into(),
                    release_name: "Severance.S02E07.WORKPRINT.x264-LEAK".into(),
                    correct_decision: "reject".into(),
                    reason: "leaked workprint, not the final episode".into(),
                }],
            },
            ..ReviewContext::default()
        };
        let out = render_fixture("tv_episode", &ctx);
        assert_snapshot("tv_episode_with_extras", &out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koblem_client::types::{ReviewCandidate, ReviewRequest, WantContext};

    fn empty_ctx() -> ReviewContext {
        ReviewContext::default()
    }

    fn req() -> ReviewRequest {
        ReviewRequest {
            wanted_title: "Severance".into(),
            want: WantContext::TvEpisode {
                season: "2".into(),
                episode: "7".into(),
                episode_title: Some("Chikhai Bardo".into()),
                air_date: Some("2026-02-21".into()),
                year: Some(2022),
            },
            ..Default::default()
        }
    }

    #[test]
    fn render_candidate_list_emits_one_based_indices() {
        let env = make_environment();
        let cands = json!([
            { "name": "A", "signals": ["PROPER", "seed:10"] },
            { "name": "B", "signals": [] },
        ]);
        let out = env
            .render_str(
                "{{ x | render_candidate_list }}",
                Serde(json!({ "x": cands })),
            )
            .unwrap();
        assert_eq!(out, "  1. A [PROPER seed:10]\n  2. B");
    }

    #[test]
    fn render_siblings_produces_legacy_also_exists_line() {
        let env = make_environment();
        let siblings = json!([
            { "title": "Law & Order: Criminal Intent", "year": 2001, "status": "Ended" },
            { "title": "Law & Order: SVU", "year": 1999, "status": "Returning Series", "season": "27", "episode": "17", "episode_title": "Deep Under" },
        ]);
        let out = env
            .render_str("{{ x | render_siblings }}", Serde(json!({ "x": siblings })))
            .unwrap();
        assert_eq!(
            out,
            "Also exists: Law & Order: Criminal Intent (2001) [Ended]; Law & Order: SVU (1999) [Returning Series] — S27E17 = \"Deep Under\""
        );
    }

    #[test]
    fn empty_siblings_render_as_empty_string() {
        let env = make_environment();
        let out = env
            .render_str("{{ x | render_siblings }}", Serde(json!({ "x": [] })))
            .unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn pluck_extracts_attribute() {
        let env = make_environment();
        let out = env
            .render_str(
                "{{ items | pluck('id') | join(',') }}",
                Serde(json!({ "items": [{"id": 1}, {"id": 2}, {"id": 3}] })),
            )
            .unwrap();
        assert_eq!(out, "1,2,3");
    }

    #[test]
    fn flatten_flattens_one_level() {
        let env = make_environment();
        let out = env
            .render_str(
                "{{ x | flatten | join(',') }}",
                Serde(json!({ "x": [[1, 2], [3], [4, 5]] })),
            )
            .unwrap();
        assert_eq!(out, "1,2,3,4,5");
    }

    #[test]
    fn regex_replace_supports_simple_pattern() {
        let env = make_environment();
        let out = env
            .render_str(
                r"{{ s | regex_replace('\d+', '#') }}",
                Serde(json!({ "s": "version 1.2.3" })),
            )
            .unwrap();
        assert_eq!(out, "version #.#.#");
    }

    #[test]
    fn build_render_context_lifts_signals_into_each_candidate() {
        let r = req();
        let cands = vec![ReviewCandidate {
            name: "Severance.S02E07.1080p.WEB-DL.x264-NTb".into(),
            kobload_accepted: true,
            kobload_score: 8.0,
            seeders: Some(50),
            ..Default::default()
        }];
        let v = build_render_context(&empty_ctx(), &r, &cands);
        let signals = &v["candidates"][0]["signals"];
        assert!(
            signals
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s.as_str() == Some("seed:50"))
        );
    }

    #[test]
    fn media_kind_for_returns_snake_case() {
        assert_eq!(
            media_kind_for(&WantContext::TvSeason {
                season: "1".into(),
                year: None,
                episodes_needed: None
            }),
            "tv_season"
        );
        assert_eq!(media_kind_for(&WantContext::Other), "other");
    }

    #[test]
    fn default_user_template_renders_full_tv_season_against_fixture() {
        let raw = include_str!("fixtures/tv_season.json");
        let r: ReviewRequest = serde_json::from_str(raw).unwrap();
        let mut root = build_render_context(&empty_ctx(), &r, &r.candidates);
        // Inject the seeded variables manually so the template can reference
        // them — full DB resolution is exercised in the db_tests module.
        root.as_object_mut().unwrap().insert(
            "vars".into(),
            json!({
                "format": "<format>",
                "signal_descriptions": "<sigs>",
                "watch_patterns": "<watch>",
            }),
        );
        let out = make_environment()
            .render_str(DEFAULT_USER_TEMPLATE, Serde(&root))
            .unwrap();
        // Title + per-kind WANTED block.
        assert!(
            out.contains("WANTED: \"American Dad!\" (2005)"),
            "got: {out}"
        );
        assert!(
            out.contains("[episodes needed: S22E1, S22E2, S22E3, S22E4, S22E5, S22E6, S22E7]"),
            "episodes needed list missing or wrong: {out}"
        );
        // QUALITY WANT renders structured profile.
        assert!(out.contains("MAXIMUM RESOLUTION: 1080p"), "got: {out}");
        assert!(out.contains("PREFERRED CODECS: x265, x264"), "got: {out}");
        assert!(out.contains("REJECT HARDCODED SUBS"), "got: {out}");
        // Candidate list 1-based with signal tags from the engine.
        assert!(out.contains("CANDIDATES:"), "got: {out}");
        assert!(
            out.contains(
                "  1. American.Dad.2005.S12.1080p.DSNP.Webrip.AV1.10bit.EAC3.5.1.GokiTAoE"
            ),
            "got: {out}"
        );
    }

    #[test]
    fn default_user_template_renders_sport_event_with_labeled_block() {
        let raw = include_str!("fixtures/sport_event.json");
        let r: ReviewRequest = serde_json::from_str(raw).unwrap();
        let mut root = build_render_context(&empty_ctx(), &r, &r.candidates);
        root.as_object_mut()
            .unwrap()
            .insert("vars".into(), json!({}));
        let out = make_environment()
            .render_str(DEFAULT_USER_TEMPLATE, Serde(&root))
            .unwrap();
        assert!(out.contains("LEAGUE: MotoGP"), "got: {out}");
        assert!(out.contains("ROUND: 4"), "got: {out}");
        assert!(out.contains("LOCATION: Spain"), "got: {out}");
        assert!(out.contains("SESSION: Race"), "got: {out}");
        assert!(out.contains("EVENT: \"Spanish GP\""), "got: {out}");
    }

    #[test]
    fn user_template_propagates_disambiguation_siblings_via_filter() {
        let mut ctx = ReviewContext::default();
        ctx.disambiguation.siblings = vec![
            crate::context::DisambiguationSibling {
                title: "Law & Order: Criminal Intent".into(),
                year: Some(2001),
                status: Some("Ended".into()),
                ..Default::default()
            },
            crate::context::DisambiguationSibling {
                title: "Law & Order: Trial by Jury".into(),
                year: Some(2005),
                status: Some("Canceled".into()),
                ..Default::default()
            },
        ];
        let mut req = req();
        req.wanted_title = "Law & Order: SVU".into();
        let mut root = build_render_context(&ctx, &req, &[]);
        root.as_object_mut()
            .unwrap()
            .insert("vars".into(), json!({}));
        let out = make_environment()
            .render_str(DEFAULT_USER_TEMPLATE, Serde(&root))
            .unwrap();
        assert!(
            out.contains(
                "Also exists: Law & Order: Criminal Intent (2001) [Ended]; Law & Order: Trial by Jury (2005) [Canceled]"
            ),
            "got: {out}"
        );
    }
}

#[cfg(test)]
mod db_tests {
    use super::*;
    use sqlx::PgPool;
    use tokio::sync::MutexGuard as AsyncMutexGuard;

    use crate::repositories::test_pool as shared_pool;

    async fn test_pool() -> Option<(PgPool, AsyncMutexGuard<'static, ()>)> {
        shared_pool("prompt_templates, prompt_variables").await
    }

    #[tokio::test]
    async fn resolve_template_falls_through_to_rust_default_when_db_empty() {
        let Some((pool, _guard)) = test_pool().await else {
            return;
        };
        let body = resolve_template(&pool, "tv_episode", SLOT_USER)
            .await
            .unwrap();
        assert!(body.contains("CURRENT DATE: {{ today }}"), "got: {body}");
    }

    #[tokio::test]
    async fn resolve_template_default_db_row_overrides_rust_default() {
        let Some((pool, _guard)) = test_pool().await else {
            return;
        };
        crate::repositories::prompt_templates::upsert_template(
            &pool,
            MEDIA_KIND_DEFAULT,
            SLOT_SYSTEM,
            "OVERRIDDEN AT DEFAULT",
        )
        .await
        .unwrap();
        let body = resolve_template(&pool, "tv_season", SLOT_SYSTEM)
            .await
            .unwrap();
        assert_eq!(body, "OVERRIDDEN AT DEFAULT");
    }

    #[tokio::test]
    async fn resolve_template_per_kind_row_beats_default_row() {
        let Some((pool, _guard)) = test_pool().await else {
            return;
        };
        crate::repositories::prompt_templates::upsert_template(
            &pool,
            MEDIA_KIND_DEFAULT,
            SLOT_SYSTEM,
            "DEFAULT",
        )
        .await
        .unwrap();
        crate::repositories::prompt_templates::upsert_template(
            &pool,
            "sport_event",
            SLOT_SYSTEM,
            "SPORT-SPECIFIC",
        )
        .await
        .unwrap();
        let sport = resolve_template(&pool, "sport_event", SLOT_SYSTEM)
            .await
            .unwrap();
        let movie = resolve_template(&pool, "movie", SLOT_SYSTEM).await.unwrap();
        assert_eq!(sport, "SPORT-SPECIFIC");
        assert_eq!(movie, "DEFAULT");
    }

    /// Drive the variable resolution chain through the same single-query
    /// path the engine uses on the hot path. Locks in the fallback order:
    /// per-kind row → default row → Rust seeded constant.
    async fn resolved_body(pool: &PgPool, media_kind: &str, name: &str) -> String {
        let resolved =
            prompt_templates::list_resolved_variables(pool, media_kind, SEEDED_VARIABLES)
                .await
                .unwrap();
        let row = resolved.into_iter().find(|v| v.name == name);
        match row {
            Some(v) => v
                .per_kind_body
                .or(v.default_body)
                .unwrap_or_else(|| default_variable(name).to_string()),
            None => default_variable(name).to_string(),
        }
    }

    #[tokio::test]
    async fn resolve_variable_chain_matches_template_chain() {
        let Some((pool, _guard)) = test_pool().await else {
            return;
        };
        // No row → Rust seeded default.
        let body = resolved_body(&pool, "movie", "format").await;
        assert!(body.contains("STRICT RESPONSE FORMAT"), "got: {body}");
        // Default-row override.
        crate::repositories::prompt_templates::upsert_variable(
            &pool,
            MEDIA_KIND_DEFAULT,
            "format",
            "DEFAULT-FORMAT",
        )
        .await
        .unwrap();
        let body = resolved_body(&pool, "movie", "format").await;
        assert_eq!(body, "DEFAULT-FORMAT");
        // Per-kind override beats it.
        crate::repositories::prompt_templates::upsert_variable(
            &pool,
            "movie",
            "format",
            "MOVIE-FORMAT",
        )
        .await
        .unwrap();
        let body = resolved_body(&pool, "movie", "format").await;
        assert_eq!(body, "MOVIE-FORMAT");
    }

    #[tokio::test]
    async fn build_messages_renders_full_pair_for_default_kind() {
        let Some((pool, _guard)) = test_pool().await else {
            return;
        };
        let raw = include_str!("fixtures/tv_episode.json");
        let r: koblem_client::types::ReviewRequest = serde_json::from_str(raw).unwrap();
        let pair = build_messages(&pool, &ReviewContext::default(), &r, &r.candidates)
            .await
            .unwrap();
        assert!(
            pair.system.contains("FINAL PICK LIST"),
            "system: {}",
            pair.system
        );
        assert!(pair.user.contains("WANTED:"), "user: {}", pair.user);
        assert!(pair.user.contains("CANDIDATES:"), "user: {}", pair.user);
        // 1-based indexing into the candidate list.
        assert!(
            pair.user.contains("  1. Severance.S02E07"),
            "user: {}",
            pair.user
        );
    }

    #[tokio::test]
    async fn template_with_syntax_error_surfaces_friendly_error_with_slot_label() {
        let Some((pool, _guard)) = test_pool().await else {
            return;
        };
        // Unbalanced `{% if %}` is a render-time syntax error in minijinja.
        crate::repositories::prompt_templates::upsert_template(
            &pool,
            MEDIA_KIND_DEFAULT,
            SLOT_USER,
            "{% if true %}no endif",
        )
        .await
        .unwrap();
        let raw = include_str!("fixtures/movie.json");
        let r: koblem_client::types::ReviewRequest = serde_json::from_str(raw).unwrap();
        let err = build_messages(&pool, &ReviewContext::default(), &r, &r.candidates)
            .await
            .unwrap_err();
        let s = format!("{err}");
        // The user-slot label rides along so the UI can attribute the error.
        assert!(s.contains("user"), "expected slot label in error: {s}");
    }

    #[tokio::test]
    async fn variable_can_reference_earlier_seeded_variable() {
        let Some((pool, _guard)) = test_pool().await else {
            return;
        };
        // `bonus_var` is alphabetically after `format` so it should see
        // vars.format already populated when it renders.
        crate::repositories::prompt_templates::upsert_variable(
            &pool,
            MEDIA_KIND_DEFAULT,
            "bonus_var",
            "FORMAT WAS: {{ vars.format[:20] }}",
        )
        .await
        .unwrap();
        // Wire user template to reference bonus_var so we can prove it rendered.
        crate::repositories::prompt_templates::upsert_template(
            &pool,
            MEDIA_KIND_DEFAULT,
            SLOT_USER,
            "{{ vars.bonus_var }}",
        )
        .await
        .unwrap();
        let raw = include_str!("fixtures/other.json");
        let r: koblem_client::types::ReviewRequest = serde_json::from_str(raw).unwrap();
        let pair = build_messages(&pool, &ReviewContext::default(), &r, &r.candidates)
            .await
            .unwrap();
        assert_eq!(pair.user, "FORMAT WAS: STRICT RESPONSE FORM");
    }
}
