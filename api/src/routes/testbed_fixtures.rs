//! Fixture loader for big stress-test scenarios.
//!
//! Anonymised kobload `/api/v1/search-and-download` dumps (titles, scores and
//! parsed quality only — indexer, URL and ID fields stripped) live in `api/scenarios/*.json`
//! and are embedded into the binary via `include_str!`. At scenario-build time
//! they are parsed into `Vec<ReviewCandidate>` so the testbed gets hundreds of
//! realistic candidates (with all the messy naming, fakes, samples, foreign dubs
//! etc. you'd see on a real release day) without anyone having to type them out.
//!
//! kobload's response shape doesn't include a per-result `accepted` bool, so we
//! derive `kobload_accepted` from a score threshold (`KOBLOAD_ACCEPT_THRESHOLD`,
//! tunable below). The threshold is loose by design — kobload is the dumb
//! upstream filter and the LLM's job is to disagree with it where appropriate.

use koblem_client::types::ReviewCandidate;
use serde::Deserialize;

/// Score threshold above which a kobload result is considered "accepted by
/// kobload" for the purposes of the diff. Real kobload runs use a more
/// nuanced threshold; for fixture testing we pick a value that produces a
/// roughly even split of accepted/rejected on the included dumps.
const KOBLOAD_ACCEPT_THRESHOLD: f64 = 1.1;

/// Top-level shape of `api/v1/search-and-download` responses.
#[derive(Deserialize)]
struct KobloadDump {
    results: Vec<KobloadResult>,
}

/// One row from a kobload search-and-download response.
///
/// Subset of the actual kobload schema — only the fields we map to
/// [`ReviewCandidate`]. Unknown fields are silently ignored thanks to the
/// default serde behaviour.
#[derive(Deserialize)]
struct KobloadResult {
    title: String,
    #[serde(default)]
    publish_date: Option<String>,
    #[serde(default)]
    total_score: f64,
    #[serde(default)]
    quality: KobloadQuality,
    // Note: search-and-download doesn't currently surface seeders/votes/grabs
    // in its response. If/when it does, we can map them through here.
}

#[derive(Deserialize, Default)]
struct KobloadQuality {
    #[serde(default)]
    resolution: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    codec: Option<String>,
    #[serde(default)]
    release_group: Option<String>,
    #[serde(default)]
    streaming_service: Option<String>,
}

/// Parse a kobload dump JSON blob and convert each result into a
/// [`ReviewCandidate`]. Deduplicates by exact title string so the Severance
/// fixture (351 raw results, 104 unique) doesn't drown the LLM in repeats —
/// at scale the model anchors on the first ~40 candidates and stops properly
/// reading the rest, and Severance in particular has a 2160p block of dupes
/// at the top followed by a 1080p block of dupes that the model never saw.
/// Keeping only unique titles preserves the stress-test coverage (every
/// distinct release is still present) while shrinking the prompt by ~70%.
///
/// Fails the binary if the fixture is malformed — these are baked in at
/// compile time and either parse or they don't.
pub fn load_dump(json: &str) -> Vec<ReviewCandidate> {
    use std::collections::HashSet;
    let dump: KobloadDump = serde_json::from_str(json).expect("malformed kobload fixture JSON");
    let mut seen: HashSet<String> = HashSet::new();
    dump.results
        .into_iter()
        .filter(|r| seen.insert(r.title.clone()))
        .map(|r| ReviewCandidate {
            name: r.title,
            kobload_accepted: r.total_score >= KOBLOAD_ACCEPT_THRESHOLD,
            kobload_score: r.total_score,
            parsed_quality: nats_jobs::ParsedQuality {
                resolution: r.quality.resolution,
                source: r.quality.source,
                codec: r.quality.codec,
                streaming_service: r.quality.streaming_service,
                release_group: r.quality.release_group,
                ..Default::default()
            },
            publish_date: r.publish_date,
            ..Default::default()
        })
        .collect()
}

// ── Fixture sources ────────────────────────────────────────────────────────
//
// Each constant embeds one kobload dump at compile time. To add a new fixture:
//   1. Save the kobload `search-and-download` JSON response to `api/scenarios/`.
//   2. Add an `include_str!` constant here.
//   3. Add a scenario constructor in `testbed::build_scenarios()` that calls
//      `load_dump(FIXTURE_X)` for the candidate list.

pub const PHM_2026_DUMP: &str = include_str!("../../scenarios/project-hail-mary-2026.json");
pub const SEVERANCE_S02E07_DUMP: &str = include_str!("../../scenarios/severance-s02e07-2025.json");
pub const MOTOGP_2025_R22_DUMP: &str =
    include_str!("../../scenarios/motogp-2025-r22-valencia.json");
pub const WSBK_2025_ROUND12_JEREZ_DUMP: &str =
    include_str!("../../scenarios/wsbk-2025-round12-jerez.json");
pub const F1_2025_R05_SAUDI_DUMP: &str =
    include_str!("../../scenarios/f1-2025-r05-saudi-arabian.json");
