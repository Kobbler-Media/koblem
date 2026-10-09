//! Testbed: pre-canned test scenarios for prompt tuning.
//!
//! Each scenario has a ReviewRequest with known-good expected outcomes,
//! so you can tweak prompts and verify the LLM still gets them right.

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use koblem_client::types::{
    QualityProfile, ReviewCandidate, ReviewRequest, ReviewResponse, WantContext,
};
use serde::{Deserialize, Serialize};
use tracing::error;

use crate::AppState;
use crate::auth::require_auth;
use crate::context::assemble_context;
use crate::repositories::llm_logs::{self, CreateLog};
use crate::repositories::review_sessions::{self, StoreReviewParams};
use crate::review_helpers::{
    compute_diff, finish_reason, format_debug_prompt, parse_review_response, prepend_note,
    token_usage, truncation_note,
};
use crate::routes::chat::{grammar_tag, preset_tag};
use crate::routes::testbed_fixtures;
use koblem_client::types::{ReviewAccept, ReviewDelta};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/testbed/scenarios", get(list_scenarios))
        .route("/api/v1/testbed/run", post(run_scenario))
}

#[derive(Clone, Serialize)]
struct TestScenario {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    category: &'static str,
    expected_actions: Vec<ExpectedAction>,
    request: ReviewRequest,
}

#[derive(Clone, Serialize)]
struct ExpectedAction {
    release_name: &'static str,
    expected: &'static str,
    reason: &'static str,
}

#[derive(Serialize)]
struct ScenarioListItem {
    id: &'static str,
    name: &'static str,
    description: &'static str,
    category: &'static str,
    candidate_count: usize,
}

#[derive(Serialize)]
struct RunResult {
    scenario_id: String,
    scenario_name: String,
    review: ReviewResponse,
    expected: Vec<ExpectedAction>,
    scoring: ScoringBreakdown,
    session_id: Option<String>,
}

/// Per-scenario four-bucket scoring breakdown.
///
/// Replaces the old binary `passed/failed` counts with a finer split that
/// distinguishes *how* the LLM got it right (or wrong). For an "expected
/// accept" release: `agreed_correct` if it was in kobload's accept set and
/// the LLM kept it, `promoted_correct` if kobload had rejected it and the
/// LLM rescued it. For "expected reject": `vetoed_correct` if kobload had
/// it but the LLM dropped it, `silent_correct` if both kobload and the LLM
/// rejected it. Anything else lands in `wrong`.
#[derive(Serialize)]
struct ScoringBreakdown {
    /// Expected accept, in kobload's set, LLM kept it.
    agreed_correct: usize,
    /// Expected accept, NOT in kobload's set, LLM rescued it.
    promoted_correct: usize,
    /// Expected reject, in kobload's set, LLM caught it.
    vetoed_correct: usize,
    /// Expected reject, not in kobload's set, both rejected (silent agreement).
    silent_correct: usize,
    /// Each release where the LLM got it wrong, with diagnostic detail.
    wrong: Vec<WrongVerdict>,
}

#[derive(Serialize)]
struct WrongVerdict {
    release_name: String,
    /// What the scenario expected: "accept" or "reject".
    expected: String,
    /// Where the release actually landed: "agreed" / "vetoed" / "promoted" / "silent".
    actual_bucket: String,
    /// Short failure mode: "missed" (expected accept but LLM rejected) or
    /// "spurious" (expected reject but LLM accepted).
    sub_reason: String,
}

// ── Scenario constructor helpers ─────────────────────────────────────
//
// Each ReviewCandidate / ReviewRequest in a fixture only varies on a handful
// of fields (name, kobload_score, publish_date, seeders, etc.) — these
// helpers fill in the rest from `Default::default()` so the test data stays
// compact and the few fields that matter to a scenario are visually obvious.

fn cand(
    name: &str,
    kobload_accepted: bool,
    kobload_score: f64,
    publish_date: &str,
    seeders: Option<i32>,
) -> ReviewCandidate {
    ReviewCandidate {
        name: name.to_string(),
        kobload_accepted,
        kobload_score,
        publish_date: Some(publish_date.to_string()),
        seeders,
        ..Default::default()
    }
}

fn cand_with_votes(
    name: &str,
    kobload_accepted: bool,
    kobload_score: f64,
    publish_date: &str,
    seeders: Option<i32>,
    votes: Option<i32>,
) -> ReviewCandidate {
    ReviewCandidate {
        name: name.to_string(),
        kobload_accepted,
        kobload_score,
        publish_date: Some(publish_date.to_string()),
        seeders,
        votes,
        ..Default::default()
    }
}

fn cand_proper(
    name: &str,
    kobload_accepted: bool,
    kobload_score: f64,
    publish_date: &str,
    seeders: Option<i32>,
) -> ReviewCandidate {
    ReviewCandidate {
        name: name.to_string(),
        kobload_accepted,
        kobload_score,
        publish_date: Some(publish_date.to_string()),
        seeders,
        parsed_quality: nats_jobs::ParsedQuality {
            is_proper: true,
            ..Default::default()
        },
        ..Default::default()
    }
}

#[allow(clippy::too_many_arguments)]
fn tv_episode_request(
    title: &str,
    year: i32,
    season: &str,
    episode: &str,
    episode_title: Option<&str>,
    air_date: Option<&str>,
    quality_profile: Option<QualityProfile>,
    candidates: Vec<ReviewCandidate>,
) -> ReviewRequest {
    ReviewRequest {
        wanted_title: title.to_string(),
        want: WantContext::TvEpisode {
            season: season.to_string(),
            episode: episode.to_string(),
            episode_title: episode_title.map(str::to_string),
            air_date: air_date.map(str::to_string),
            year: Some(year),
        },
        quality_profile,
        candidates,
        ..Default::default()
    }
}

fn movie_request(
    title: &str,
    year: i32,
    quality_profile: Option<QualityProfile>,
    candidates: Vec<ReviewCandidate>,
) -> ReviewRequest {
    ReviewRequest {
        wanted_title: title.to_string(),
        want: WantContext::Movie { year: Some(year) },
        quality_profile,
        candidates,
        ..Default::default()
    }
}

#[allow(clippy::too_many_arguments)]
fn sport_event_request(
    league_name: &str,
    season: &str,
    round: Option<u32>,
    location: Option<&str>,
    session_type: Option<&str>,
    event_title: Option<&str>,
    event_date: Option<&str>,
    quality_profile: Option<QualityProfile>,
    candidates: Vec<ReviewCandidate>,
) -> ReviewRequest {
    ReviewRequest {
        wanted_title: format!("{league_name} {season}"),
        want: WantContext::SportEvent {
            league_name: league_name.to_string(),
            season: season.to_string(),
            round,
            location: location.map(str::to_string),
            session_type: session_type.map(str::to_string),
            event_title: event_title.map(str::to_string),
            event_date: event_date.map(str::to_string),
        },
        quality_profile,
        candidates,
        ..Default::default()
    }
}

fn build_scenarios() -> Vec<TestScenario> {
    vec![
        // ── Disambiguation ──────────────────────────────────────────
        TestScenario {
            id: "scrubs-disambiguation",
            name: "Scrubs 2001 vs 2026",
            description: "Release has wrong episode title — matches 2001 Scrubs not 2026 reboot",
            category: "disambiguation",
            expected_actions: vec![
                ExpectedAction {
                    release_name: "Scrubs.S01E08.My.Fifteen.Minutes.1080p.BiNGE.WEB-DL.H264-MuTT",
                    expected: "reject",
                    reason: "Episode title 'My Fifteen Minutes' doesn't match 2026 S01E08 'My Odds'",
                },
                ExpectedAction {
                    release_name: "Scrubs.S01E08.My.Odds.1080p.DSNP.WEB-DL.DDP5.1.H.264-FLUX",
                    expected: "accept",
                    reason: "Correct episode title for 2026 version",
                },
            ],
            request: tv_episode_request(
                "Scrubs",
                2026,
                "1",
                "8",
                Some("My Odds"),
                Some("2026-04-08"),
                Some(QualityProfile {
                    max_resolution: Some("1080p".to_string()),
                    preferred_sources: vec!["WEB-DL".to_string()],
                    rejected_sources: vec![
                        "CAM".to_string(),
                        "TS".to_string(),
                        "DVDRip".to_string(),
                    ],
                    ..Default::default()
                }),
                vec![
                    cand(
                        "Scrubs.S01E08.My.Fifteen.Minutes.1080p.BiNGE.WEB-DL.H264-MuTT",
                        true,
                        0.85,
                        "2026-04-08",
                        Some(25),
                    ),
                    cand(
                        "Scrubs.S01E08.My.Odds.1080p.DSNP.WEB-DL.DDP5.1.H.264-FLUX",
                        true,
                        0.92,
                        "2026-04-08",
                        Some(150),
                    ),
                ],
            ),
        },
        // ── Suspicious lone release (on air date, not pre-air) ─────
        TestScenario {
            id: "lone-perfect-release",
            name: "Lone perfect release among garbage",
            description: "One WEB-DL with 0 seeders among CAMs with hundreds of seeders — the 'good' one is likely fake",
            category: "suspicious",
            expected_actions: vec![
                ExpectedAction {
                    release_name: "Project.Hail.Mary.2026.1080p.WEB-DL.DDP5.1.H.264-GROUP",
                    expected: "reject",
                    reason: "0 seeders, unknown group, only good release = suspicious",
                },
                ExpectedAction {
                    release_name: "Project.Hail.Mary.2026.CAM.XviD-JUNK",
                    expected: "reject",
                    reason: "CAM quality",
                },
                ExpectedAction {
                    release_name: "Project.Hail.Mary.2026.TS.x264-CRAP",
                    expected: "reject",
                    reason: "Telesync quality",
                },
            ],
            request: movie_request(
                "Project Hail Mary",
                2026,
                Some(QualityProfile {
                    max_resolution: Some("1080p".to_string()),
                    preferred_sources: vec!["WEB-DL".to_string()],
                    rejected_sources: vec!["CAM".to_string(), "TS".to_string()],
                    ..Default::default()
                }),
                vec![
                    cand(
                        "Project.Hail.Mary.2026.1080p.WEB-DL.DDP5.1.H.264-GROUP",
                        true,
                        0.90,
                        "2026-04-10",
                        Some(0),
                    ),
                    cand(
                        "Project.Hail.Mary.2026.CAM.XviD-JUNK",
                        false,
                        0.0,
                        "2026-04-10",
                        Some(500),
                    ),
                    cand(
                        "Project.Hail.Mary.2026.TS.x264-CRAP",
                        false,
                        0.0,
                        "2026-04-10",
                        Some(300),
                    ),
                ],
            ),
        },
        // ── Pre-air date ────────────────────────────────────────────
        TestScenario {
            id: "pre-air-fake",
            name: "Release published before air date",
            description: "Release claims to be an episode that hasn't aired yet",
            category: "timing",
            expected_actions: vec![ExpectedAction {
                release_name: "The.Bear.S04E05.1080p.HULU.WEB-DL.DDP5.1.H.264-FLUX",
                expected: "reject",
                reason: "Published 2026-04-01 but episode airs 2026-04-15 — pre-air fake",
            }],
            request: tv_episode_request(
                "The Bear",
                2026,
                "4",
                "5",
                Some("Napkins"),
                Some("2026-04-15"),
                Some(QualityProfile {
                    max_resolution: Some("1080p".to_string()),
                    preferred_sources: vec!["WEB-DL".to_string()],
                    ..Default::default()
                }),
                vec![cand(
                    "The.Bear.S04E05.1080p.HULU.WEB-DL.DDP5.1.H.264-FLUX",
                    true,
                    0.95,
                    "2026-04-01",
                    Some(3),
                )],
            ),
        },
        // ── PROPER/REPACK ───────────────────────────────────────────
        TestScenario {
            id: "proper-repack",
            name: "PROPER replaces original",
            description: "PROPER release should be preferred over the original",
            category: "quality",
            expected_actions: vec![
                ExpectedAction {
                    release_name: "Severance.S02E10.1080p.ATVP.WEB-DL.DDP5.1.H.264-NTb",
                    expected: "accept",
                    reason: "Original release, fine quality",
                },
                ExpectedAction {
                    release_name: "Severance.S02E10.PROPER.1080p.ATVP.WEB-DL.DDP5.1.H.264-NTb",
                    expected: "accept",
                    reason: "PROPER = fixes issues in original, should be accepted",
                },
            ],
            request: tv_episode_request(
                "Severance",
                2022,
                "2",
                "10",
                Some("Cold Harbor"),
                Some("2026-03-21"),
                Some(QualityProfile {
                    max_resolution: Some("1080p".to_string()),
                    preferred_sources: vec!["WEB-DL".to_string()],
                    ..Default::default()
                }),
                vec![
                    cand(
                        "Severance.S02E10.1080p.ATVP.WEB-DL.DDP5.1.H.264-NTb",
                        true,
                        0.90,
                        "2026-03-21",
                        Some(200),
                    ),
                    cand_proper(
                        "Severance.S02E10.PROPER.1080p.ATVP.WEB-DL.DDP5.1.H.264-NTb",
                        false,
                        0.0,
                        "2026-03-22",
                        Some(180),
                    ),
                ],
            ),
        },
        // ── Wrong season/episode ────────────────────────────────────
        TestScenario {
            id: "wrong-episode-accepted",
            name: "Wrong episode accepted by fuzzy match",
            description: "Kobload accepted S01E04 when we wanted S01E08 — fuzzy match was too loose",
            category: "mismatch",
            expected_actions: vec![
                ExpectedAction {
                    release_name: "Scrubs.2026.S01E04.1080p.DSNP.WEB-DL.DDP5.1.H.264-FLUX",
                    expected: "reject",
                    reason: "Wrong episode — wanted E08 not E04",
                },
                ExpectedAction {
                    release_name: "Scrubs.2026.S01E08.1080p.DSNP.WEB-DL.DDP5.1.H.264-FLUX",
                    expected: "accept",
                    reason: "Correct episode",
                },
            ],
            request: tv_episode_request(
                "Scrubs",
                2026,
                "1",
                "8",
                Some("My Odds"),
                Some("2026-04-08"),
                None,
                vec![
                    cand(
                        "Scrubs.2026.S01E04.1080p.DSNP.WEB-DL.DDP5.1.H.264-FLUX",
                        true,
                        0.80,
                        "2026-03-12",
                        Some(50),
                    ),
                    cand(
                        "Scrubs.2026.S01E08.1080p.DSNP.WEB-DL.DDP5.1.H.264-FLUX",
                        true,
                        0.95,
                        "2026-04-08",
                        Some(200),
                    ),
                ],
            ),
        },
        // ── Negative votes ──────────────────────────────────────────
        TestScenario {
            id: "negative-votes",
            name: "Release with negative community votes",
            description: "Community flagged a release as bad — should not be accepted",
            category: "community",
            expected_actions: vec![
                ExpectedAction {
                    release_name: "Andor.S02E01.1080p.DSNP.WEB-DL.H.264-BadGroup",
                    expected: "reject",
                    reason: "Negative community votes — flagged as bad",
                },
                ExpectedAction {
                    release_name: "Andor.S02E01.1080p.DSNP.WEB-DL.DDP5.1.H.264-NTb",
                    expected: "accept",
                    reason: "Good release with positive signals",
                },
            ],
            request: tv_episode_request(
                "Andor",
                2022,
                "2",
                "1",
                None,
                Some("2026-04-22"),
                Some(QualityProfile {
                    max_resolution: Some("1080p".to_string()),
                    preferred_sources: vec!["WEB-DL".to_string()],
                    ..Default::default()
                }),
                vec![
                    cand_with_votes(
                        "Andor.S02E01.1080p.DSNP.WEB-DL.H.264-BadGroup",
                        true,
                        0.85,
                        "2026-04-22",
                        Some(5),
                        Some(-8),
                    ),
                    cand_with_votes(
                        "Andor.S02E01.1080p.DSNP.WEB-DL.DDP5.1.H.264-NTb",
                        true,
                        0.92,
                        "2026-04-22",
                        Some(350),
                        Some(12),
                    ),
                ],
            ),
        },
        // ── Stress scenarios from anonymised kobload dumps ──────────
        //
        // Candidates are loaded at runtime from JSON files in api/scenarios/
        // (see testbed_fixtures.rs). expected_actions remain hand-curated to
        // a small set of notable releases so the four-bucket scoring stays
        // meaningful even when the candidate count climbs into the hundreds.
        TestScenario {
            id: "phm-2026-real-dump",
            name: "Project Hail Mary 2026 (71 releases)",
            description: "Release-day kobload dump: ~71 mostly-CAM/TS/HDRip junk \
                 candidates plus a handful of WEB-DL and WEBRip variants. \
                 Tests the LLM's ability to reject obvious fakes and pick the \
                 real WEB-DL when surrounded by junk.",
            category: "stress",
            expected_actions: vec![
                ExpectedAction {
                    release_name: "Project.Hail.Mary.2026.1080p.WEB-DL.AAC.2.0.H264-EaZy",
                    expected: "accept",
                    reason: "Real WEB-DL from a release group, correct year and resolution.",
                },
                ExpectedAction {
                    release_name: "Project.Hail.Mary.2026.1080p.TELESYNC.V2.x264-SyncUP",
                    expected: "reject",
                    reason: "TELESYNC = filmed in cinema = always reject.",
                },
                ExpectedAction {
                    release_name: "Project Hail Mary 2026 1080p CAM Line Audio V3 x264-SyncUP",
                    expected: "reject",
                    reason: "CAM = filmed in cinema = always reject.",
                },
                ExpectedAction {
                    release_name: "Project.Hail.Mary.2026.1080p.HC.HDRip.CLEAN.English.Audio.x264.CHS-BOTHD",
                    expected: "reject",
                    reason: "HC HDRip with hardcoded CHS subs = poor quality.",
                },
                ExpectedAction {
                    release_name: "Project.Hail.Mary.2026.FANSUB.VOSTFR.1080p.WEBRip.x264-Slay3R",
                    expected: "reject",
                    reason: "French fansub, not the wanted English release.",
                },
            ],
            request: movie_request(
                "Project Hail Mary",
                2026,
                Some(QualityProfile {
                    max_resolution: Some("1080p".to_string()),
                    preferred_sources: vec!["WEB-DL".to_string()],
                    preferred_languages: vec!["English".to_string()],
                    rejected_sources: vec![
                        "CAM".to_string(),
                        "TS".to_string(),
                        "TELESYNC".to_string(),
                        "HDRip".to_string(),
                    ],
                    rejected_terms: vec!["foreign dubs".to_string()],
                    ..Default::default()
                }),
                testbed_fixtures::load_dump(testbed_fixtures::PHM_2026_DUMP),
            ),
        },
        TestScenario {
            id: "severance-s02e07-real-dump",
            name: "Severance S02E07 (deduped)",
            description: "Release dump for Severance S02E07 'Chikhai Bardo' — raw \
                 kobload response was 351 results, fixture loader dedupes to ~104 \
                 unique titles. Covers the canonical Apple TV+ 1080p WEB-DL H.264, \
                 PROPER/REPACK/Kitsune variants, 2160p x265 alternatives, season \
                 packs, wrong-episode siblings, foreign dubs, and the usual sample/\
                 subpack noise. Tests scale + episode-title disambiguation + the \
                 anchoring-on-first-block failure mode that broke every prompt \
                 version through v6 before grammar reorder and dedup landed.",
            category: "stress",
            expected_actions: vec![
                ExpectedAction {
                    release_name: "Severance S02E07 Chikhai Bardo 2160p ATVP WEB-DL DDP5 1 Atmos DV HDR H 265-FLUX",
                    expected: "reject",
                    reason: "2160p exceeds the 1080p quality want.",
                },
                ExpectedAction {
                    // Canonical perfect match. If the LLM returns 0 picks,
                    // this expectation fails loudly and the stress test
                    // catches the regression.
                    release_name: "Severance.S02E07.Chikhai.Bardo.1080p.ATVP.WEB-DL.DDP5.1.Atmos.H.264-FLUX",
                    expected: "accept",
                    reason: "1080p ATVP WEB-DL H.264 FLUX — exact match for the 1080p WEB-DL want.",
                },
                ExpectedAction {
                    // Second canonical match from a different release group.
                    release_name: "Severance.S02E07.Chikhai.Bardo.1080p.ATVP.WEB-DL.DDP5.1.Atmos.H.264-Kitsune",
                    expected: "accept",
                    reason: "1080p ATVP WEB-DL H.264 Kitsune — alternative exact match.",
                },
                ExpectedAction {
                    // German dub — should be rejected when English is wanted.
                    release_name: "Severance.2022.S02E07.German.DL.Atmos.1080p.DV.HDR.ATVP.WEB.H265-ZeroTwo",
                    expected: "reject",
                    reason: "German DL = German dub, non-English audio.",
                },
                ExpectedAction {
                    // x265/HEVC encode — the quality_rejects explicitly
                    // mentions x265 and HEVC.
                    release_name: "Severance.2022.S02E07.Chikhai.Bardo.1080p.ATVP.Webrip.x265.10bit.EAC3.5.1.Atmos.English.JBENTTAoE",
                    expected: "reject",
                    reason: "x265 HEVC encode — explicitly rejected by quality_rejects.",
                },
            ],
            request: tv_episode_request(
                "Severance",
                2022,
                "2",
                "7",
                Some("Chikhai Bardo"),
                Some("2025-02-21"),
                Some(QualityProfile {
                    max_resolution: Some("1080p".to_string()),
                    preferred_sources: vec!["WEB-DL".to_string()],
                    rejected_terms: vec![
                        "2160p".to_string(),
                        "x265".to_string(),
                        "HEVC".to_string(),
                        "season packs".to_string(),
                    ],
                    ..Default::default()
                }),
                testbed_fixtures::load_dump(testbed_fixtures::SEVERANCE_S02E07_DUMP),
            ),
        },
        TestScenario {
            id: "motogp-2025-r22-real-dump",
            name: "MotoGP 2025 R22 Valencia Race (515 releases)",
            description: "Release dump from a kobload search across the entire \
                 MotoGP 2025 season — 515 candidates spanning all 22 rounds, \
                 multiple sessions (Race, Sprint, Qualifying, Practice), \
                 multiple broadcasters, multiple resolutions, multiple naming \
                 conventions. Tests naming variance acceptance, session \
                 disambiguation, and round-number isolation at scale.",
            category: "stress",
            expected_actions: vec![
                ExpectedAction {
                    release_name: "MotoGP.2025.Round22.Spain.Valencia.Race.TNTSportsHD.2160p.HDTV.AAC2.0.UPSCALED.H.265.English-smcgill1969",
                    expected: "reject",
                    reason: "UPSCALED 2160p (fake resolution) and exceeds 1080p want.",
                },
                ExpectedAction {
                    release_name: "MotoGP.2025.Round22.Spain.Valencia.Qualifying.TNTSportsHD.2160p.HDTV.AAC2.0.UPSCALED.H.265.English-smcgill1969",
                    expected: "reject",
                    reason: "Qualifying session, not the wanted Race session.",
                },
                ExpectedAction {
                    release_name: "MotoGP.2025.Round21.Portugal.Race.TNTSportsHD.2160p.HDTV.AAC2.0.UPSCALED.H.265.English-smcgill1969",
                    expected: "reject",
                    reason: "Round 21 (Portugal), not the wanted Round 22 (Valencia).",
                },
                ExpectedAction {
                    release_name: "MotoGP 2025 Valencia 1080p WEB h264-VERUM",
                    expected: "accept",
                    reason: "Correct round (named by venue), correct year, correct quality, no session tag means main race.",
                },
                ExpectedAction {
                    release_name: "MotoGP 2025 Round22 Spain Valencia Sprint WEBRiP 1080p h264 English-MWR",
                    expected: "reject",
                    reason: "Sprint session, not the main Race.",
                },
                // Canonical perfect-match: explicit Round22 Valencia Race in
                // 1080p WEB-DL H.264 English. Must always be picked — if the
                // LLM vetoes this, something fundamental is broken.
                ExpectedAction {
                    release_name: "MotoGP.2025.Round22.Spain.Valencia.Race.WEB-DL.1080p.h264.English-MWR",
                    expected: "accept",
                    reason: "Explicit Round22 Valencia Race in 1080p WEB-DL H.264 English — canonical match.",
                },
                // Wrong round (Round 20 Malaysia) named via the "Race N" TNT
                // pattern. Catches the v4 regression where accept-first made
                // the model rescue wrong-round TNT releases.
                ExpectedAction {
                    release_name: "TNT Sports - 2025 Race 20 - Maleislië - MotoGP - Race - 1080p",
                    expected: "reject",
                    reason: "Race 20 (Malaysia), not the wanted Round 22.",
                },
                // Naming variance: the x22 format should be accepted.
                ExpectedAction {
                    release_name: "MotoGP.2025x22.Spain.Race.TNTSportsHD.1080p",
                    expected: "accept",
                    reason: "2025x22 = Round 22 (naming variance), Valencia Race in 1080p.",
                },
            ],
            request: sport_event_request(
                "MotoGP",
                "2025",
                Some(22),
                Some("Valencia"),
                Some("Race"),
                Some("Valencia Grand Prix - Race"),
                Some("2025-11-16"),
                Some(QualityProfile {
                    max_resolution: Some("1080p".to_string()),
                    preferred_sources: vec!["WEB-DL".to_string(), "HDTV".to_string()],
                    preferred_languages: vec!["English".to_string()],
                    rejected_terms: vec![
                        "Sprint".to_string(),
                        "Qualifying".to_string(),
                        "Practice".to_string(),
                        "Highlights".to_string(),
                        "UPSCALED".to_string(),
                        "2160p".to_string(),
                        "foreign audio".to_string(),
                    ],
                    ..Default::default()
                }),
                testbed_fixtures::load_dump(testbed_fixtures::MOTOGP_2025_R22_DUMP),
            ),
        },
        TestScenario {
            id: "wsbk-2025-r12-jerez-real-dump",
            name: "WSBK 2025 Round 12 Jerez Race One (165 releases)",
            description: "Kobload dump for World Superbike 2025 — 165 candidates \
                 spanning all 12 rounds, mostly MWR release group with clean naming. \
                 Tests multi-race-weekend session disambiguation (Race One vs Race Two \
                 vs Superpole Race vs Warm Up vs FP1–3 vs Superpole) and country-alone \
                 insufficiency (both Round 10 Aragon and Round 12 Jerez are in Spain, \
                 so country name alone does not identify the round).",
            category: "stress",
            expected_actions: vec![
                // Canonical exact match — Race One is the wanted session.
                ExpectedAction {
                    release_name: "WSBK.2025.Round12.Spain.Jerez.Race.One.WEB-DL.1080p.h264.English-MWR",
                    expected: "accept",
                    reason: "Explicit Round 12 Jerez Race One in 1080p WEB-DL H.264 English — canonical match.",
                },
                // Race Two is a different session within the same weekend.
                ExpectedAction {
                    release_name: "WSBK.2025.Round12.Spain.Jerez.Race.Two.WEB-DL.1080p.h264.English-MWR",
                    expected: "reject",
                    reason: "Race Two is a separate session from the wanted Race One.",
                },
                // Superpole Race is a short sprint-like race, not the main Race One.
                ExpectedAction {
                    release_name: "WSBK.2025.Round12.Spain.Jerez.Superpole.Race.WEB-DL.1080p.h264.English-MWR",
                    expected: "reject",
                    reason: "Superpole Race is a separate sprint-format race, not Race One.",
                },
                ExpectedAction {
                    release_name: "WSBK.2025.Round12.Spain.Jerez.Superpole.WEB-DL.1080p.h264.English-MWR",
                    expected: "reject",
                    reason: "Superpole is the qualifying session, not the wanted Race.",
                },
                ExpectedAction {
                    release_name: "WSBK.2025.Round12.Spain.Jerez.FP1.WEB-DL.1080p.h264.English-MWR",
                    expected: "reject",
                    reason: "FP1 is a free practice session, not the wanted Race.",
                },
                // Country-collision: Round 10 Aragon is also in Spain. Must not be
                // rescued based on "Spain" matching the wanted round.
                ExpectedAction {
                    release_name: "WSBK.2025.Round10.Spain.Aragon.Race.One.WEB-DL.1080p.h264.English-MWR",
                    expected: "reject",
                    reason: "Round 10 Aragon, not the wanted Round 12 Jerez — both are in Spain so country alone is not sufficient.",
                },
            ],
            request: sport_event_request(
                "WSBK",
                "2025",
                Some(12),
                Some("Jerez"),
                Some("Race One"),
                Some("Jerez Grand Prix - Race One"),
                Some("2025-10-19"),
                Some(QualityProfile {
                    max_resolution: Some("1080p".to_string()),
                    preferred_sources: vec!["WEB-DL".to_string()],
                    preferred_languages: vec!["English".to_string()],
                    rejected_terms: vec![
                        "Race Two".to_string(),
                        "Superpole Race".to_string(),
                        "Superpole".to_string(),
                        "Warm Up".to_string(),
                        "Free Practice".to_string(),
                        "FP1".to_string(),
                        "FP2".to_string(),
                        "FP3".to_string(),
                        "2160p".to_string(),
                        "x265".to_string(),
                        "HEVC".to_string(),
                        "foreign audio".to_string(),
                    ],
                    ..Default::default()
                }),
                testbed_fixtures::load_dump(testbed_fixtures::WSBK_2025_ROUND12_JEREZ_DUMP),
            ),
        },
        TestScenario {
            id: "f1-2025-r05-saudi-real-dump",
            name: "F1 2025 R05 Saudi Arabian GP Race (429 releases)",
            description: "Kobload dump spanning the F1 2025 season plus the F1 \
                 movie — 429 candidates covering heavy naming variance: SkyF1HD \
                 ordinal-prefixed format (`08.F1.2025.R05.Saudi.Arabian.Race.SkyF1HD`), \
                 DARKSPORT Formula1 format (`Formula1.2025.Saudi.Arabian.Grand.Prix.*`), \
                 WDTeam date-prefixed format (`Formula.1.2025.DD.MM.YYYY.R08.F1.Monaco...`), \
                 and an F1 2025 x265/HEVC movie trap. Tests that R05/Round 5 aliases, \
                 `Saudi.Arabian` without `Grand.Prix`, and ordinal prefixes are all \
                 recognized as the same Race, while F1 Show / Press Conference / \
                 Qualifying / Practice / wrong-round Races are rejected.",
            category: "stress",
            expected_actions: vec![
                // Canonical naming-variance win — ordinal-prefixed SkyF1HD format
                // with no "Grand Prix" token but R05 + Saudi.Arabian + Race is
                // sufficient to identify the wanted race.
                ExpectedAction {
                    release_name: "08.F1.2025.R05.Saudi.Arabian.Race.SkyF1HD.1080P",
                    expected: "accept",
                    reason: "R05 + Saudi Arabian + Race in 1080p SkyF1HD — naming variance, ordinal prefix, no 'Grand Prix' token but still unambiguous.",
                },
                // Session disambiguation — F1 Show is a talk show, not the race.
                ExpectedAction {
                    release_name: "Formula1.2025.Saudi.Arabian.Grand.Prix.The.F1.Show.1080p.AHDTV.x264-DARKSPORT-FTP",
                    expected: "reject",
                    reason: "The F1 Show is a studio talk show, not the Race session.",
                },
                ExpectedAction {
                    release_name: "07.F1.2025.R05.Saudi.Arabian.Qualifying.SkyF1HD.1080P",
                    expected: "reject",
                    reason: "Qualifying session, not the wanted Race.",
                },
                ExpectedAction {
                    release_name: "06.F1.2025.R05.Saudi.Arabian.Free.Practice.Three.SkyF1HD.1080P",
                    expected: "reject",
                    reason: "Free Practice Three, not the wanted Race.",
                },
                ExpectedAction {
                    release_name: "01.F1.2025.R05.Saudi.Arabian.Drivers.Press.Conference.SkyF1HD.1080P",
                    expected: "reject",
                    reason: "Press conference, not the wanted Race.",
                },
                // Wrong round — Bahrain is R04, not R05.
                ExpectedAction {
                    release_name: "09.F1.2025.R04.Bahrain.Grand.Prix.Race.SkyF1HD.1080P",
                    expected: "reject",
                    reason: "Round 4 Bahrain, not the wanted Round 5 Saudi Arabian.",
                },
                // 2160p 4K UHD release — exceeds 1080p quality want, plus wrong round.
                ExpectedAction {
                    release_name: "Formula.1.2025.25.05.2025.R08.F1.Monaco.Grand.Prix.Race.2160p.4K.UHD-WDTeam",
                    expected: "reject",
                    reason: "Round 8 Monaco and 2160p 4K — wrong round and exceeds 1080p want.",
                },
            ],
            request: sport_event_request(
                "Formula 1",
                "2025",
                Some(5),
                Some("Saudi Arabia"),
                Some("Race"),
                Some("Saudi Arabian Grand Prix - Race"),
                Some("2025-04-20"),
                Some(QualityProfile {
                    max_resolution: Some("1080p".to_string()),
                    preferred_sources: vec!["WEB-DL".to_string(), "HDTV".to_string()],
                    preferred_languages: vec!["English".to_string()],
                    rejected_sources: vec!["BluRay".to_string()],
                    rejected_terms: vec![
                        "2160p".to_string(),
                        "x265".to_string(),
                        "HEVC".to_string(),
                        "foreign audio".to_string(),
                    ],
                    reject_remux: true,
                    ..Default::default()
                }),
                testbed_fixtures::load_dump(testbed_fixtures::F1_2025_R05_SAUDI_DUMP),
            ),
        },
    ]
}

async fn list_scenarios(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let scenarios = build_scenarios();
    let items: Vec<ScenarioListItem> = scenarios
        .iter()
        .map(|s| ScenarioListItem {
            id: s.id,
            name: s.name,
            description: s.description,
            category: s.category,
            candidate_count: s.request.candidates.len(),
        })
        .collect();

    Json(items).into_response()
}

#[derive(Deserialize)]
struct RunRequest {
    scenario_id: String,
    /// Optional inline prompt override — use this instead of saved/default prompt.
    /// Lets you tweak the prompt per-run without changing the global setting.
    prompt_override: Option<String>,
    /// Optional per-run sampling override. Merged on top of the DB default
    /// (`sampling_review_candidates`) and the `Deterministic` baseline.
    /// Set `sampling.grammar` to A/B grammars without a redeploy.
    #[serde(default)]
    sampling: Option<koblem_client::types::SamplingConfig>,
}

async fn run_scenario(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(payload): Json<RunRequest>,
) -> impl IntoResponse {
    if require_auth(&state.db, state.api_key.as_deref(), &headers)
        .await
        .is_err()
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let scenarios = build_scenarios();
    let scenario = match scenarios.iter().find(|s| s.id == payload.scenario_id) {
        Some(s) => s.clone(),
        None => return (StatusCode::NOT_FOUND, "scenario not found").into_response(),
    };

    // Run through the full review pipeline
    let ctx_start = std::time::Instant::now();
    let library_ref = state
        .library_provider
        .as_ref()
        .map(|p| p.as_ref() as &dyn kob_providers::LibraryProvider);
    let ctx = assemble_context(&scenario.request, library_ref, &state.db).await;
    let context_assembly_ms = ctx_start.elapsed().as_millis() as u64;

    let mut sorted_candidates = scenario.request.candidates.clone();
    let sort_tokens = crate::prompts::build_sort_token_bag(&scenario.request);
    crate::prompts::sort_candidates_by_relevance(&mut sorted_candidates, &sort_tokens);

    // Same layering as /api/v1/review-candidates: baseline (Deterministic +
    // built-in `review` grammar) ← DB default ← per-run request override.
    let baseline = koblem_client::types::SamplingConfig {
        preset: Some(koblem_client::types::SamplingPreset::Deterministic),
        grammar: Some(koblem_client::types::GrammarMode::Review),
        ..koblem_client::types::SamplingConfig::default()
    };
    let db_default =
        crate::sampling::resolve_sampling(&state.db, "sampling_review_candidates").await;
    let request_override = payload.sampling.clone().unwrap_or_default();
    let sampling = baseline.overlay(&db_default).overlay(&request_override);
    // Per-run `prompt_override` swaps the system template for this single
    // request only; the saved per-kind / default-kind template is untouched.
    let overrides = crate::prompt_engine::TemplateOverrides {
        system: payload.prompt_override.as_deref(),
        user: None,
        variables: None,
    };
    let pair = match crate::prompt_engine::build_messages_with_overrides(
        &state.db,
        &ctx,
        &scenario.request,
        &sorted_candidates,
        &overrides,
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("testbed prompt engine error: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("template error: {e}"),
            )
                .into_response();
        }
    };
    let mut body = serde_json::json!({
        "messages": [
            {"role": "system", "content": pair.system},
            {"role": "user", "content": pair.user}
        ],
        "cache_prompt": true,
    });
    sampling.apply_to(&mut body);
    crate::prompts::apply_grammar_to_body(&state.db, &mut body, &sampling).await;
    let debug_prompt = format_debug_prompt(&body);

    // Open a running llm_logs row so this scenario shows up in LLM Logs and
    // its CoT trace surfaces under the linked review_sessions row.
    let log_id = {
        let mut seed = CreateLog::new(
            Some("testbed:run_scenario".to_string()),
            body.get("messages").cloned().unwrap_or_default(),
        );
        seed.set_sampling_from_body(
            &body,
            preset_tag(&sampling),
            grammar_tag(&sampling).as_deref(),
        );
        match llm_logs::start_log(&state.db, &seed).await {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!("failed to start testbed llm log: {e}");
                uuid::Uuid::nil()
            }
        }
    };

    let llm_start = std::time::Instant::now();
    let parts = match state
        .llama
        .chat_completion_streaming_with_log(
            &body,
            state.db.clone(),
            log_id,
            std::time::Duration::from_millis(1500),
        )
        .await
    {
        Ok((_permit, response_body)) => {
            let content = response_body
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
                .unwrap_or("{}");
            let finish = finish_reason(&response_body);
            let parsed = parse_review_response(content);
            let diff = compute_diff(&parsed.accepts, &sorted_candidates);
            let (pt, ct) = token_usage(&response_body);

            let mut n = parsed.notes;
            if let Some(err) = parsed.parse_error {
                n = Some(prepend_note(&n, &err));
            }
            if let Some(warning) = truncation_note(
                &finish,
                parsed.accepts.len(),
                scenario.request.candidates.len(),
            ) {
                n = Some(prepend_note(&n, &warning));
            }
            if !diff.dropped_indices.is_empty() {
                n = Some(prepend_note(
                    &n,
                    &format!(
                        "⚠ LLM emitted {} out-of-range index/indices: {:?}",
                        diff.dropped_indices.len(),
                        diff.dropped_indices
                    ),
                ));
            }
            let vj = serde_json::json!({
                "accepts": parsed.accepts,
                "agreed": diff.agreed,
                "vetoed": diff.vetoed,
                "promoted": diff.promoted,
                "dropped_indices": diff.dropped_indices,
            });
            TestbedParts {
                agreed: diff.agreed,
                vetoed: diff.vetoed,
                promoted: diff.promoted,
                accepts: parsed.accepts,
                dropped_indices: diff.dropped_indices,
                notes: n,
                raw: Some(content.to_string()),
                verdicts_json: Some(vj),
                prompt_tokens: pt,
                completion_tokens: ct,
            }
        }
        Err(e) => {
            error!("llama-server error in testbed: {e}");
            TestbedParts {
                agreed: vec![],
                vetoed: vec![],
                promoted: vec![],
                accepts: vec![],
                dropped_indices: vec![],
                notes: Some(format!("LLM error: {e}")),
                raw: None,
                verdicts_json: None,
                prompt_tokens: None,
                completion_tokens: None,
            }
        }
    };
    let llm_inference_ms = llm_start.elapsed().as_millis() as u64;

    // Score against expectations using the four-bucket breakdown. For each
    // expected_action we determine which diff bucket the release ended up in
    // and classify the outcome.
    let in_agreed: std::collections::HashSet<&str> =
        parts.agreed.iter().map(|d| d.name.as_str()).collect();
    let in_vetoed: std::collections::HashSet<&str> =
        parts.vetoed.iter().map(|d| d.name.as_str()).collect();
    let in_promoted: std::collections::HashSet<&str> =
        parts.promoted.iter().map(|d| d.name.as_str()).collect();

    let mut scoring = ScoringBreakdown {
        agreed_correct: 0,
        promoted_correct: 0,
        vetoed_correct: 0,
        silent_correct: 0,
        wrong: Vec::new(),
    };

    for expected in &scenario.expected_actions {
        let name = expected.release_name;
        let expected_accept = expected.expected.eq_ignore_ascii_case("accept");
        let actual_bucket = if in_agreed.contains(name) {
            "agreed"
        } else if in_promoted.contains(name) {
            "promoted"
        } else if in_vetoed.contains(name) {
            "vetoed"
        } else {
            "silent"
        };
        let llm_accepted = matches!(actual_bucket, "agreed" | "promoted");

        match (expected_accept, actual_bucket) {
            (true, "agreed") => scoring.agreed_correct += 1,
            (true, "promoted") => scoring.promoted_correct += 1,
            (false, "vetoed") => scoring.vetoed_correct += 1,
            (false, "silent") => scoring.silent_correct += 1,
            _ => {
                let sub_reason = if expected_accept && !llm_accepted {
                    "missed"
                } else {
                    "spurious"
                };
                scoring.wrong.push(WrongVerdict {
                    release_name: name.to_string(),
                    expected: expected.expected.to_string(),
                    actual_bucket: actual_bucket.to_string(),
                    sub_reason: sub_reason.to_string(),
                });
            }
        }
    }

    let session_id = review_sessions::store_review(
        &state.db,
        StoreReviewParams {
            request_json: serde_json::to_value(&scenario.request).unwrap_or_default(),
            ctx: &ctx,
            prompt_sent: &debug_prompt,
            llm_response: parts.raw.as_deref(),
            verdicts_json: parts.verdicts_json.clone(),
            notes: parts.notes.clone(),
            context_assembly_ms,
            llm_inference_ms,
            llm_log_id: (!log_id.is_nil()).then_some(log_id),
        },
    )
    .await;

    Json(RunResult {
        scenario_id: scenario.id.to_string(),
        scenario_name: scenario.name.to_string(),
        review: ReviewResponse {
            agreed: parts.agreed,
            vetoed: parts.vetoed,
            promoted: parts.promoted,
            accepts: parts.accepts,
            dropped_indices: parts.dropped_indices,
            context_used: ctx.context_used,
            notes: parts.notes,
            debug_prompt: Some(debug_prompt),
            raw_llm_response: parts.raw,
            prompt_tokens: parts.prompt_tokens,
            completion_tokens: parts.completion_tokens,
            context_assembly_ms: Some(context_assembly_ms),
            llm_inference_ms: Some(llm_inference_ms),
            session_id: session_id.clone(),
        },
        expected: scenario.expected_actions,
        scoring,
        session_id,
    })
    .into_response()
}

/// Per-scenario equivalent of `playground::ReviewParts`. Defined separately
/// so testbed can stay in its own module without pulling in playground.
struct TestbedParts {
    agreed: Vec<ReviewDelta>,
    vetoed: Vec<ReviewDelta>,
    promoted: Vec<ReviewDelta>,
    accepts: Vec<ReviewAccept>,
    dropped_indices: Vec<usize>,
    notes: Option<String>,
    raw: Option<String>,
    verdicts_json: Option<serde_json::Value>,
    prompt_tokens: Option<i32>,
    completion_tokens: Option<i32>,
}
