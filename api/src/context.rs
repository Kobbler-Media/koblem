use kob_providers::LibraryProvider;
use koblem_client::types::{ReviewRequest, WantContext};
use serde::Serialize;
use sqlx::PgPool;
use tracing::warn;

use crate::repositories::corrections;

/// Side-effecting context assembly outputs. The render engine layers this on
/// top of the raw `ReviewRequest` to build the per-template render context.
///
/// All fields are structured (not pre-rendered strings) — the actual
/// formatting lives in the user-prompt template, which can iterate
/// `disambiguation.siblings`, group rejected candidates etc. as it sees fit.
#[derive(Debug, Default, Clone, Serialize)]
pub struct ReviewContext {
    /// Compact row label used by the History list view. Derived cheaply from
    /// the request — no LLM-facing role, just "American Dad! [tv_season]"-style
    /// tagging so a user scanning past sessions can identify them at a glance.
    pub wanted_summary: String,
    pub disambiguation: Disambiguation,
    pub corrections: Corrections,
    /// Which structured context sources contributed (non-empty after assembly).
    /// Surfaced in the response + history list as debug chips.
    pub context_used: Vec<String>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Disambiguation {
    pub siblings: Vec<DisambiguationSibling>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct DisambiguationSibling {
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// When the wanted item is a TV episode, the engine looks up the matching
    /// episode title on each sibling show so the prompt can disambiguate by
    /// episode-name overlap as well as show-name overlap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub season: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode_title: Option<String>,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Corrections {
    pub rules: Vec<CorrectionRule>,
    pub past_mistakes: Vec<PastMistake>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CorrectionRule {
    pub rule: String,
    /// Either "release-classification" (generic, applied to every review) or
    /// the entity scope name when the rule is entity-specific.
    pub scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PastMistake {
    pub model_decision: String,
    pub release_name: String,
    pub correct_decision: String,
    pub reason: String,
}

/// Assemble structured context for a review request. Runs before the LLM
/// semaphore so library / DB I/O happens off the GPU critical path.
pub async fn assemble_context(
    request: &ReviewRequest,
    library_provider: Option<&dyn LibraryProvider>,
    pool: &PgPool,
) -> ReviewContext {
    let wanted_summary = derive_wanted_summary(request);
    let disambiguation = build_disambiguation(request, library_provider).await;
    let corrections = build_corrections(request, pool).await;

    let mut context_used = Vec::new();
    context_used.push("wanted_metadata".to_string());
    if request.quality_profile.is_some() {
        context_used.push("quality_preferences".to_string());
    }
    if !disambiguation.siblings.is_empty() {
        context_used.push("library_disambiguation".to_string());
    }
    if !corrections.rules.is_empty() || !corrections.past_mistakes.is_empty() {
        context_used.push("corrections".to_string());
    }

    ReviewContext {
        wanted_summary,
        disambiguation,
        corrections,
        context_used,
    }
}

/// Render the `WANTED:` block as a single string for the History list label
/// AND for the stored `review_sessions.wanted_summary` column.
///
/// This MUST stay in lock-step with the default user template's `WANTED:`
/// rendering — see `default_user.tmpl`. Snapshot tests in `prompt_engine`
/// pin the template output; if you change one, change the other and re-bake
/// the snapshots.
fn derive_wanted_summary(request: &ReviewRequest) -> String {
    let title = &request.wanted_title;
    match &request.want {
        WantContext::TvEpisode {
            season,
            episode,
            episode_title,
            air_date,
            year,
        } => {
            let mut parts = vec![titled_with_year(title, *year)];
            parts.push(format!("S{season}E{episode}"));
            if let Some(t) = episode_title {
                parts.push(format!("\"{t}\""));
            }
            if let Some(d) = air_date {
                parts.push(format!("[air date: {d}]"));
            }
            parts.join(" ")
        }
        WantContext::TvSeason {
            season,
            year,
            episodes_needed,
        } => {
            let mut parts = vec![titled_with_year(title, *year)];
            match episodes_needed.as_deref() {
                Some([single]) => {
                    // Single needed-episode: title/air_date unambiguously
                    // belong to it, so render them inline. The bracket form
                    // distinguishes "monitoring season 22, currently want
                    // S22E17" from a bare TvEpisode `S22E17`.
                    parts.push(format!("[episodes needed: S{season}E{}]", single.episode));
                    if let Some(t) = single.title.as_ref() {
                        parts.push(format!("\"{t}\""));
                    }
                    if let Some(d) = single.air_date.as_ref() {
                        parts.push(format!("[air date: {d}]"));
                    }
                }
                Some(eps) if !eps.is_empty() => {
                    // Multi-entry: sort by sort_order (kobbler puts the most
                    // recently-asked episode first, which the model latches
                    // onto if we don't re-sequence). title/air_date dropped
                    // because they're ambiguous across multiple eps.
                    let mut sorted: Vec<&nats_jobs::EpisodeRef> = eps.iter().collect();
                    sorted.sort_by_key(|e| {
                        e.sort_order
                            .unwrap_or_else(|| e.episode.parse::<i32>().unwrap_or(i32::MAX))
                    });
                    let joined = sorted
                        .iter()
                        .map(|e| format!("S{season}E{}", e.episode))
                        .collect::<Vec<_>>()
                        .join(", ");
                    parts.push(format!("[episodes needed: {joined}]"));
                }
                _ => {
                    parts.push(format!(
                        "S{season} (full season — any episode of this season, or a season pack, is valid)"
                    ));
                }
            }
            parts.join(" ")
        }
        WantContext::Movie { year } => titled_with_year(title, *year),
        WantContext::SportEvent {
            league_name,
            season,
            round,
            location,
            session_type,
            event_title,
            event_date,
        } => {
            // Multi-line per-field block — semicolons in a single line are
            // too easy for the model to misread.
            let mut lines = vec![format!("\"{title}\"")];
            lines.push(format!("LEAGUE: {league_name}"));
            lines.push(format!("SEASON: {season}"));
            if let Some(r) = round {
                lines.push(format!("ROUND: {r}"));
            }
            if let Some(loc) = location {
                lines.push(format!("LOCATION: {loc}"));
            }
            if let Some(s) = session_type {
                lines.push(format!("SESSION: {s}"));
            }
            if let Some(t) = event_title {
                lines.push(format!("EVENT: \"{t}\""));
            }
            if let Some(d) = event_date {
                lines.push(format!("EVENT DATE: {d}"));
            }
            lines.join("\n")
        }
        WantContext::Other => format!("\"{title}\""),
    }
}

fn titled_with_year(title: &str, year: Option<i32>) -> String {
    match year {
        Some(y) => format!("\"{title}\" ({y})"),
        None => format!("\"{title}\""),
    }
}

async fn build_disambiguation(
    request: &ReviewRequest,
    library_provider: Option<&dyn LibraryProvider>,
) -> Disambiguation {
    let Some(provider) = library_provider else {
        return Disambiguation::default();
    };
    if request.source_id.is_none() {
        return Disambiguation::default();
    };

    let wanted_year = match &request.want {
        WantContext::TvEpisode { year, .. }
        | WantContext::TvSeason { year, .. }
        | WantContext::Movie { year } => *year,
        _ => None,
    };
    let wanted_episode = match &request.want {
        WantContext::TvEpisode {
            season, episode, ..
        } => Some((season.clone(), episode.clone())),
        _ => None,
    };

    let shows = match provider.search_shows(&request.wanted_title).await {
        Ok(shows) => shows,
        Err(e) => {
            warn!("library provider search failed: {e}");
            return Disambiguation::default();
        }
    };

    let mut siblings = Vec::new();
    for show in shows.iter().filter(|s| match wanted_year {
        Some(year) => s.year != Some(year),
        None => true,
    }) {
        let mut sibling = DisambiguationSibling {
            title: show.title.clone(),
            year: show.year,
            status: show.status.clone(),
            ..Default::default()
        };
        if let Some((ref season, ref episode)) = wanted_episode {
            match provider.get_episode_title(&show.id, season, episode).await {
                Ok(Some(ep_title)) => {
                    sibling.season = Some(season.clone());
                    sibling.episode = Some(episode.clone());
                    sibling.episode_title = Some(ep_title);
                }
                Ok(None) => {}
                Err(e) => {
                    warn!("failed to get episode title for {}: {e}", show.id);
                }
            }
        }
        siblings.push(sibling);
    }

    Disambiguation { siblings }
}

async fn build_corrections(request: &ReviewRequest, pool: &PgPool) -> Corrections {
    let mut rules = Vec::new();
    let mut past_mistakes = Vec::new();

    if let Ok(generic) = corrections::list_generic_rules(pool, Some("release-classification")).await
    {
        for r in generic {
            rules.push(CorrectionRule {
                rule: r.rule,
                scope: "release-classification".to_string(),
                entity_name: None,
            });
        }
    }

    if let Some(ref entity_id) = request.source_id {
        if let Ok(entity_rules) = corrections::list_entity_rules(pool, Some(entity_id)).await {
            for r in entity_rules {
                rules.push(CorrectionRule {
                    rule: r.rule,
                    scope: "entity".to_string(),
                    entity_name: r.entity_name,
                });
            }
        }
        if let Ok(decisions) = corrections::list_recent_corrections(pool, Some(entity_id), 10).await
        {
            for dc in decisions {
                past_mistakes.push(PastMistake {
                    model_decision: dc.model_decision,
                    release_name: dc.release_name,
                    correct_decision: dc.correct_decision,
                    reason: dc.reason,
                });
            }
        }
    }

    Corrections {
        rules,
        past_mistakes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use koblem_client::types::WantContext;
    use nats_jobs::EpisodeRef;

    fn ep(num: &str, season: &str, title: Option<&str>, air_date: Option<&str>) -> EpisodeRef {
        EpisodeRef {
            season: season.to_string(),
            episode: num.to_string(),
            sort_order: None,
            air_date: air_date.map(str::to_string),
            in_library: None,
            library_video_height: None,
            title: title.map(str::to_string),
        }
    }

    fn ep_sorted(num: &str, season: &str, sort_order: i32) -> EpisodeRef {
        EpisodeRef {
            season: season.to_string(),
            episode: num.to_string(),
            sort_order: Some(sort_order),
            air_date: None,
            in_library: None,
            library_video_height: None,
            title: None,
        }
    }

    fn req(title: &str, want: WantContext) -> ReviewRequest {
        ReviewRequest {
            wanted_title: title.to_string(),
            want,
            ..Default::default()
        }
    }

    #[test]
    fn tv_episode_uses_sxxeyy_form() {
        let r = req(
            "Law & Order: SVU",
            WantContext::TvEpisode {
                season: "27".into(),
                episode: "17".into(),
                episode_title: None,
                air_date: None,
                year: Some(1999),
            },
        );
        assert_eq!(
            derive_wanted_summary(&r),
            "\"Law & Order: SVU\" (1999) S27E17"
        );
    }

    #[test]
    fn tv_episode_includes_title_and_air_date_when_present() {
        let r = req(
            "Severance",
            WantContext::TvEpisode {
                season: "2".into(),
                episode: "7".into(),
                episode_title: Some("Chikhai Bardo".into()),
                air_date: Some("2026-02-21".into()),
                year: Some(2022),
            },
        );
        assert_eq!(
            derive_wanted_summary(&r),
            "\"Severance\" (2022) S2E7 \"Chikhai Bardo\" [air date: 2026-02-21]"
        );
    }

    #[test]
    fn tv_season_single_needed_uses_episodes_needed_bracket_not_bare_sxxeyy() {
        // Critical distinction from TvEpisode: a season with one needed
        // episode means "monitoring season X, currently want S X E Y" — the
        // bracket form preserves that semantic.
        let r = req(
            "American Dad!",
            WantContext::TvSeason {
                season: "22".into(),
                year: Some(2005),
                episodes_needed: Some(vec![ep("17", "22", None, None)]),
            },
        );
        assert_eq!(
            derive_wanted_summary(&r),
            "\"American Dad!\" (2005) [episodes needed: S22E17]"
        );
    }

    #[test]
    fn tv_season_single_needed_keeps_episode_title_and_air_date() {
        let r = req(
            "Severance",
            WantContext::TvSeason {
                season: "2".into(),
                year: Some(2022),
                episodes_needed: Some(vec![ep(
                    "7",
                    "2",
                    Some("Chikhai Bardo"),
                    Some("2026-02-21"),
                )]),
            },
        );
        assert_eq!(
            derive_wanted_summary(&r),
            "\"Severance\" (2022) [episodes needed: S2E7] \"Chikhai Bardo\" [air date: 2026-02-21]"
        );
    }

    #[test]
    fn tv_season_multi_needed_renders_in_sort_order() {
        let r = req(
            "American Dad!",
            WantContext::TvSeason {
                season: "22".into(),
                year: Some(2005),
                episodes_needed: Some(vec![
                    ep_sorted("7", "22", 7),
                    ep_sorted("1", "22", 1),
                    ep_sorted("3", "22", 3),
                ]),
            },
        );
        assert_eq!(
            derive_wanted_summary(&r),
            "\"American Dad!\" (2005) [episodes needed: S22E1, S22E3, S22E7]"
        );
    }

    #[test]
    fn tv_season_no_needed_renders_full_season() {
        let r = req(
            "American Dad!",
            WantContext::TvSeason {
                season: "22".into(),
                year: Some(2005),
                episodes_needed: None,
            },
        );
        assert!(
            derive_wanted_summary(&r).contains("S22 (full season"),
            "got: {}",
            derive_wanted_summary(&r)
        );
    }

    #[test]
    fn movie_uses_titled_with_year() {
        let r = req("Dune: Part Three", WantContext::Movie { year: Some(2026) });
        assert_eq!(derive_wanted_summary(&r), "\"Dune: Part Three\" (2026)");
    }

    #[test]
    fn other_just_quotes_title() {
        assert_eq!(
            derive_wanted_summary(&req("Three Body Problem", WantContext::Other)),
            "\"Three Body Problem\""
        );
    }

    #[test]
    fn sport_event_renders_labeled_block() {
        let r = req(
            "MotoGP 2026",
            WantContext::SportEvent {
                league_name: "MotoGP".into(),
                season: "2026".into(),
                round: Some(4),
                location: Some("Spain".into()),
                session_type: Some("Race".into()),
                event_title: Some("Spanish GP".into()),
                event_date: Some("2026-04-12".into()),
            },
        );
        let s = derive_wanted_summary(&r);
        assert!(s.starts_with("\"MotoGP 2026\""), "got: {s}");
        assert!(s.contains("LEAGUE: MotoGP"), "got: {s}");
        assert!(s.contains("ROUND: 4"), "got: {s}");
        assert!(s.contains("SESSION: Race"), "got: {s}");
    }
}
