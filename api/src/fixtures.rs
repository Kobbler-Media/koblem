use koblem_client::types::ReviewRequest;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct FixtureSummary {
    pub id: &'static str,
    pub label: &'static str,
    pub media_kind: &'static str,
}

struct Fixture {
    id: &'static str,
    label: &'static str,
    media_kind: &'static str,
    body: &'static str,
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        id: "tv_episode",
        label: "TV episode — Severance S02E07",
        media_kind: "tv_episode",
        body: include_str!("fixtures/tv_episode.json"),
    },
    Fixture {
        id: "tv_season",
        label: "TV season — American Dad! S22 (multi-episode pack)",
        media_kind: "tv_season",
        body: include_str!("fixtures/tv_season.json"),
    },
    Fixture {
        id: "movie",
        label: "Movie — Dune: Part Three (2026)",
        media_kind: "movie",
        body: include_str!("fixtures/movie.json"),
    },
    Fixture {
        id: "sport_event",
        label: "Sport event — MotoGP 2026 Round 4 Spain Race",
        media_kind: "sport_event",
        body: include_str!("fixtures/sport_event.json"),
    },
    Fixture {
        id: "other",
        label: "Other — generic catch-all",
        media_kind: "other",
        body: include_str!("fixtures/other.json"),
    },
];

pub fn list_fixtures() -> Vec<FixtureSummary> {
    FIXTURES
        .iter()
        .map(|f| FixtureSummary {
            id: f.id,
            label: f.label,
            media_kind: f.media_kind,
        })
        .collect()
}

pub fn get_fixture_payload(id: &str) -> Option<ReviewRequest> {
    let raw = FIXTURES.iter().find(|f| f.id == id)?.body;
    serde_json::from_str(raw).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_fixture_parses_as_review_request() {
        for s in list_fixtures() {
            let req = get_fixture_payload(s.id);
            assert!(req.is_some(), "fixture {} did not parse", s.id);
        }
    }
}
