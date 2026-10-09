//! Chat-completion request builders for the koblem domain endpoints.
//!
//! `review_candidates_messages` is the only one that's templated end-to-end —
//! it goes through `prompt_engine` and resolves both the system + user
//! messages from the `prompt_templates` / `prompt_variables` tables (with
//! Rust-baked fallbacks). Filter / validate / rank are static prompts.

use koblem_client::types::{
    GrammarMode, REVIEW_GRAMMAR, ReviewCandidate, ReviewRequest, SamplingConfig,
};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::context::ReviewContext;
use crate::prompt_engine;
use crate::repositories::grammars;

const QUALITY_DEFINITIONS: &str = "\
Quality rules for media releases:
- WEB-DL = direct rip from streaming service (Amazon, Netflix, Hulu, Disney+) = BEST quality
- WEB = web source but not direct rip = good quality
- HDTV = TV broadcast capture = acceptable quality
- CAM = filmed with camera in cinema = TERRIBLE quality = always REJECT
- TS/TELESYNC = similar to CAM = always REJECT
- DVDRip = old DVD source = outdated = REJECT for modern shows
- XviD = ancient codec = REJECT
- H.264/H264/x264 = good modern codec
- H.265/HEVC/x265 = efficient but may have playback issues
- 1080p = full HD = preferred
- 720p = acceptable
- 480p = too low = REJECT
- WEB-DL is BETTER than WEB because WEB-DL is a direct lossless rip from the streaming service, \
  while WEB may be a re-encode or screen capture";

/// Build a chat completion request for the RSS prefilter pass.
pub fn filter_releases_messages(
    wanted_title: &str,
    wanted_context: &str,
    releases: &[String],
    sampling: &SamplingConfig,
) -> Value {
    let release_list = releases
        .iter()
        .enumerate()
        .map(|(i, r)| format!("  {}. {}", i + 1, r))
        .collect::<Vec<_>>()
        .join("\n");

    let system = "You are a semantic release matcher. You decide which release filenames plausibly \
         belong to a specific wanted item (see WANTED in the user message). The candidates have \
         already passed a cheap fuzzy name match — your job is to catch and discard the false positives.\n\
         \n\
         RULES:\n\
         - Accept naming variance freely: abbreviations (R01 vs Round01), venue names vs country names, broadcaster tags, release-group nicknames, alt episode titles. The point of using an LLM is to handle this.\n\
         - Reject when the release is clearly a different item: wrong year, wrong season/round, wrong show, wrong franchise, wrong session (Sprint/Qualifying ≠ Race), foreign-language fansub when English was wanted.\n\
         - DO NOT make a quality judgment here (CAM/TS/720p/2160p are all fine at this stage). A separate downstream pass handles quality. Your job is solely identity matching.\n\
         - DO NOT reject for absent details (no episode title in filename = normal, not a reason to reject).\n\
         \n\
         OUTPUT — return ONLY this JSON object. \"notes\" comes first, \"accept\" comes second.\n\
         \n\
         {\n\
           \"notes\": \"<one short overall observation, or null>\",\n\
           \"accept\": [<integer>, ...]\n\
         }\n\
         \n\
         - \"notes\" is a brief one-sentence commit-summary, not a scratchpad. Do not inventory the candidate list in it.\n\
         - \"accept\" is a flat array of 1-based integer indices into the CANDIDATES list. No objects, no reasons, no names.";

    let mut user = format!(
        "CURRENT DATE: {today}\nWANTED: \"{wanted_title}\"\n",
        today = today_iso()
    );
    let ctx_trimmed = wanted_context.trim();
    if !ctx_trimmed.is_empty() {
        user.push_str("CONTEXT: ");
        user.push_str(ctx_trimmed);
        user.push('\n');
    }
    user.push_str("\nCANDIDATES:\n");
    user.push_str(&release_list);

    let mut body = json!({
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user}
        ],
        "cache_prompt": true,
    });

    sampling.apply_to(&mut body);
    body
}

/// Build a chat completion request for single release validation.
pub fn validate_release_messages(
    release_name: &str,
    expected_show: &str,
    expected_season_episode: &str,
    expected_title: &str,
    sampling: &SamplingConfig,
) -> Value {
    let system = format!(
        "{QUALITY_DEFINITIONS}\n\n\
         You validate whether a release matches expectations. Check:\n\
         1. Does the release name match the expected show? (ignore apostrophes/punctuation differences)\n\
         2. Does it match the expected season/episode?\n\
         3. Is the quality acceptable based on the rules above?\n\n\
         Respond with a JSON object: {{\"valid\": true/false, \"issues\": [\"issue1\", ...]}}\n\
         If valid, issues should be an empty array."
    );
    let mut body = json!({
        "messages": [
            {"role": "system", "content": system},
            {
                "role": "user",
                "content": format!(
                    "Release: {release_name}\n\
                     Expected show: {expected_show}\n\
                     Expected episode: {expected_season_episode}\n\
                     Expected title: {expected_title}"
                )
            }
        ],
        "max_tokens": 300,
        "cache_prompt": true,
    });
    sampling.apply_to(&mut body);
    body
}

/// Build a chat completion request for ranking release candidates.
pub fn rank_releases_messages(
    wanted: &Value,
    quality_preferences: &Value,
    releases: &[Value],
    sampling: &SamplingConfig,
) -> Value {
    let release_list = releases
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let name = r
                .as_str()
                .unwrap_or_else(|| r.get("name").and_then(|n| n.as_str()).unwrap_or("unknown"));
            format!("{}. {}", i + 1, name)
        })
        .collect::<Vec<_>>()
        .join("\n");

    let count = releases.len();
    let system = format!(
        "{QUALITY_DEFINITIONS}\n\n\
         You rank media releases from best to worst quality. For each release, assign a \
         score from 1-10 and explain why. Prefer:\n\
         1. WEB-DL over WEB over HDTV\n\
         2. 1080p over 720p\n\
         3. x264/x265 over older codecs\n\
         4. Known release groups over unknown\n\n\
         Respond with a JSON object: {{\"ranked\": [{{\"release\": \"name\", \"score\": N, \"reason\": \"...\"}}]}}\n\
         Order from highest score to lowest. Include every release provided in the user message."
    );
    let mut body = json!({
        "messages": [
            {"role": "system", "content": system},
            {
                "role": "user",
                "content": format!(
                    "Wanted: {}\nQuality preferences: {}\nRank these {count} releases:\n{release_list}",
                    serde_json::to_string(wanted).unwrap_or_default(),
                    serde_json::to_string(quality_preferences).unwrap_or_default()
                )
            }
        ],
        "cache_prompt": true,
    });
    sampling.apply_to(&mut body);
    body
}

/// Build a chat completion request for reviewing release candidates.
///
/// Both system and user messages come from `prompt_engine` — per-media-kind
/// templates with default fallbacks, all template-driven.
pub async fn review_candidates_messages(
    pool: &PgPool,
    ctx: &ReviewContext,
    request: &ReviewRequest,
    candidates: &[ReviewCandidate],
    sampling: &SamplingConfig,
) -> Result<Value, prompt_engine::EngineError> {
    let pair = prompt_engine::build_messages(pool, ctx, request, candidates).await?;

    let mut body = json!({
        "messages": [
            {"role": "system", "content": pair.system},
            {"role": "user", "content": pair.user}
        ],
        "cache_prompt": true,
    });

    sampling.apply_to(&mut body);
    Ok(body)
}

/// Resolve a [`SamplingConfig`]'s grammar mode to the concrete GBNF string
/// to send to llama-server, or `None` when no grammar should be applied.
pub async fn resolve_grammar_mode(pool: &PgPool, mode: &GrammarMode) -> Option<String> {
    match mode {
        GrammarMode::Off => None,
        GrammarMode::Review => Some(REVIEW_GRAMMAR.to_string()),
        GrammarMode::Named(name) => match grammars::get_grammar(pool, name).await {
            Ok(Some(g)) => Some(g.gbnf),
            Ok(None) => {
                tracing::warn!(
                    grammar = %name,
                    "grammar mode references unknown name; treating as off"
                );
                None
            }
            Err(e) => {
                tracing::error!(error = %e, grammar = %name, "grammar lookup failed");
                None
            }
        },
    }
}

/// Apply the resolved grammar mode from `sampling` to `body`.
pub async fn apply_grammar_to_body(pool: &PgPool, body: &mut Value, sampling: &SamplingConfig) {
    let resolved = sampling.resolve();
    let Some(mode) = resolved.grammar.as_ref() else {
        return;
    };
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    match resolve_grammar_mode(pool, mode).await {
        Some(gbnf) => {
            obj.insert("grammar".to_string(), Value::String(gbnf));
        }
        None => {
            obj.remove("grammar");
        }
    }
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

/// Load a persisted `SamplingConfig` blob from the settings table.
pub async fn load_sampling_config(pool: &sqlx::PgPool, key: &str) -> SamplingConfig {
    use crate::repositories::settings::get_setting;

    let Ok(Some(raw)) = get_setting(pool, key).await else {
        return SamplingConfig::default();
    };
    serde_json::from_value::<SamplingConfig>(raw).unwrap_or_default()
}

/// Stable-partition candidates so low-relevance items sink to the end without
/// disturbing the relative order of everything else. The hybrid DeltaNet
/// architecture loses attention on deeply-positioned items in long lists
/// (~300+), and kobload dumps often front-load irrelevant results (movies
/// sharing a keyword, other seasons). A full relevance sort disrupts the
/// model's natural reading pattern and can regress edge cases like venue-only
/// naming variance. Instead, we partition: candidates matching ≥2 tokens from
/// the wanted metadata keep their original relative order and appear first;
/// candidates matching <2 tokens are appended afterward (also in original
/// order).
pub fn sort_candidates_by_relevance(candidates: &mut Vec<ReviewCandidate>, token_bag: &str) {
    if candidates.len() < 50 {
        return;
    }

    let tokens: Vec<String> = token_bag
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3)
        .map(|w| w.to_lowercase())
        .collect();
    if tokens.is_empty() {
        return;
    }

    let lowered: Vec<String> = candidates.iter().map(|c| c.name.to_lowercase()).collect();
    let n = candidates.len() as f64;

    let token_weights: Vec<f64> = tokens
        .iter()
        .map(|t| {
            let freq = lowered.iter().filter(|l| l.contains(t.as_str())).count() as f64;
            if freq == 0.0 {
                0.0
            } else {
                (n / freq).ln().max(0.1)
            }
        })
        .collect();

    let idf_score = |lower: &str| -> f64 {
        tokens
            .iter()
            .zip(token_weights.iter())
            .filter(|(t, _)| lower.contains(t.as_str()))
            .map(|(_, w)| w)
            .sum()
    };

    let scores: Vec<f64> = lowered.iter().map(|l| idf_score(l)).collect();
    let max_score = scores.iter().cloned().fold(0.0_f64, f64::max);
    if max_score < 0.1 {
        return;
    }

    let threshold = max_score * 0.5;

    let mut relevant = Vec::new();
    let mut junk = Vec::new();
    for (c, &s) in candidates.drain(..).zip(scores.iter()) {
        if s >= threshold {
            relevant.push(c);
        } else {
            junk.push(c);
        }
    }

    if junk.is_empty() || relevant.is_empty() {
        candidates.extend(relevant);
        candidates.extend(junk);
        return;
    }

    candidates.extend(relevant);
    candidates.extend(junk);
}

/// Concatenate the request fields the IDF tokeniser cares about: title plus
/// every per-variant identity hint (year, season/episode, sport league /
/// location / round / session). Keeps the sort signal as rich as the old
/// `wanted_summary`-driven version.
pub fn build_sort_token_bag(request: &ReviewRequest) -> String {
    use koblem_client::types::WantContext;
    let mut s = request.wanted_title.clone();
    match &request.want {
        WantContext::TvEpisode {
            season,
            episode,
            episode_title,
            year,
            ..
        } => {
            if let Some(y) = year {
                s.push_str(&format!(" {y}"));
            }
            s.push_str(&format!(" S{season}E{episode}"));
            if let Some(t) = episode_title {
                s.push(' ');
                s.push_str(t);
            }
        }
        WantContext::TvSeason {
            season,
            year,
            episodes_needed,
        } => {
            if let Some(y) = year {
                s.push_str(&format!(" {y}"));
            }
            s.push_str(&format!(" S{season}"));
            if let Some(eps) = episodes_needed {
                for e in eps {
                    s.push_str(&format!(" S{season}E{}", e.episode));
                    if let Some(t) = &e.title {
                        s.push(' ');
                        s.push_str(t);
                    }
                }
            }
        }
        WantContext::Movie { year } => {
            if let Some(y) = year {
                s.push_str(&format!(" {y}"));
            }
        }
        WantContext::SportEvent {
            league_name,
            season,
            round,
            location,
            session_type,
            event_title,
            ..
        } => {
            s.push_str(&format!(" {league_name} {season}"));
            if let Some(r) = round {
                s.push_str(&format!(" R{r:02} Round{r:02} Round.{r:02}"));
            }
            if let Some(loc) = location {
                s.push(' ');
                s.push_str(loc);
            }
            if let Some(st) = session_type {
                s.push(' ');
                s.push_str(st);
            }
            if let Some(t) = event_title {
                s.push(' ');
                s.push_str(t);
            }
        }
        WantContext::Other => {}
    }
    s
}
