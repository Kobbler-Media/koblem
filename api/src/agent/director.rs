//! Reading the Director library: which projects exist, what shots they hold and
//! which of those shots have a render on disk.
//!
//! **Read-only, and deliberately so.** `routes::ltx_director` owns every
//! mutation of a timeline; the agent only retrieves. The one discipline carried
//! over from there is `validate_chat_actions`': a name is RESOLVED to exactly
//! one row or the caller is told it was ambiguous — never guessed at.
//!
//! What is searchable is text the editor already writes down: a shot's `prompt`,
//! its one-line `gist`, the `idea` it was drafted from, plus the project's name
//! and its LLM synopsis. Ranking happens here rather than in SQL because the
//! corpus is a few hundred shots and a pure function is testable without a
//! database.

use serde_json::Value;
use sqlx::PgPool;

/// The shots any one search will look at. Well past a plausible library, and a
/// bound rather than an expectation.
const MAX_PROJECTS: i64 = 200;
const MAX_RENDERS: i64 = 4000;

/// Words that carry no signal in "find me the shot where…". Dropped so a query
/// phrased as a sentence does not match every shot equally.
const STOPWORDS: [&str; 24] = [
    "the", "and", "for", "with", "that", "this", "shot", "clip", "video", "scene", "from", "into",
    "was", "were", "has", "had", "there", "where", "which", "what", "some", "any", "one", "you",
];

#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    pub prompt: String,
    pub gist: String,
    pub idea: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub summary: String,
    pub clips: Vec<Clip>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Render {
    pub project_id: String,
    pub tier: String,
    pub shot_index: i32,
    pub prompt: String,
    pub content_type: String,
    pub takes: i64,
}

/// One searchable shot, with whatever was rendered of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Shot {
    pub project_id: String,
    pub project: String,
    /// The PROMPT-FILTERED index — what the editor and the render store both
    /// call "shot N". `-1` for a shot that has no prompt yet and so can never
    /// have been rendered.
    pub index: i32,
    pub prompt: String,
    pub gist: String,
    pub idea: String,
    pub summary: String,
    pub render: Option<ShotRender>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ShotRender {
    pub tier: String,
    pub url: String,
    pub mime: String,
    pub takes: i64,
    /// The shot has been edited since this clip was rendered — the render store
    /// keeps the prompt AS RENDERED precisely so this is knowable.
    pub stale: bool,
}

pub enum Resolved<'a> {
    Unique(&'a Project),
    Ambiguous(Vec<String>),
    NotFound,
}

pub async fn load(db: &PgPool) -> Result<(Vec<Project>, Vec<Render>), sqlx::Error> {
    // Only the text fields are pulled out of the timeline doc: it also holds the
    // compiled render request and per-shot settings, and dragging all of that
    // across for a substring match would be most of a project per row.
    let projects = sqlx::query!(
        r#"
        SELECT p.id,
               p.name,
               COALESCE(p.timeline -> 'editor' ->> 'summary', '') AS "summary!",
               COALESCE(
                   (SELECT jsonb_agg(jsonb_build_object(
                               'prompt', COALESCE(c ->> 'prompt', ''),
                               'gist',   COALESCE(c ->> 'gist', ''),
                               'idea',   COALESCE(c ->> 'idea', '')))
                    FROM jsonb_array_elements(
                             CASE WHEN jsonb_typeof(p.timeline -> 'clips') = 'array'
                                  THEN p.timeline -> 'clips'
                                  ELSE '[]'::jsonb END) AS c),
                   '[]'::jsonb) AS "clips!"
        FROM director_projects p
        ORDER BY p.updated_at DESC
        LIMIT $1
        "#,
        MAX_PROJECTS,
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|r| Project {
        id: r.id,
        name: r.name,
        summary: r.summary,
        clips: clips_from(&r.clips),
    })
    .collect();

    // The SELECTED take of each shot — the one the editor plays and a rebuilt
    // cut concatenates. `shot_index = -1` is the stitched whole cut, not a shot.
    let renders = sqlx::query!(
        r#"
        SELECT r.project_id,
               r.tier,
               r.shot_index,
               r.prompt,
               r.content_type,
               (SELECT COUNT(*) FROM director_shot_renders t
                 WHERE t.project_id = r.project_id
                   AND t.tier = r.tier
                   AND t.shot_index = r.shot_index) AS "takes!"
        FROM director_shot_renders r
        WHERE r.selected AND r.shot_index >= 0
        ORDER BY r.project_id, r.shot_index, r.tier
        LIMIT $1
        "#,
        MAX_RENDERS,
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|r| Render {
        project_id: r.project_id,
        tier: r.tier,
        shot_index: r.shot_index,
        prompt: r.prompt,
        content_type: r.content_type,
        takes: r.takes,
    })
    .collect();

    Ok((projects, renders))
}

fn clips_from(v: &Value) -> Vec<Clip> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|c| Clip {
                    prompt: text(c, "prompt"),
                    gist: text(c, "gist"),
                    idea: text(c, "idea"),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn text(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .trim()
        .to_string()
}

/// koblem's URL for one persisted render. Mirrors `routes::ltx_director`'s own
/// `render_url` — the route owns the path, this is the only other place that
/// spells it.
pub fn render_url(project_id: &str, tier: &str, shot_index: i32) -> String {
    format!("/api/v1/ltx-director/projects/{project_id}/renders/{tier}/{shot_index}")
}

/// Flatten the library into searchable shots.
///
/// A clip's position in the timeline is NOT its shot number: the engine drops
/// empty-prompt clips before rendering, so the render store is keyed by the
/// prompt-FILTERED index. That count is rebuilt here, and the render is only
/// trusted when its stored prompt agrees — otherwise it is attached as stale
/// rather than presented as this shot's current look.
pub fn shots(projects: &[Project], renders: &[Render]) -> Vec<Shot> {
    let mut out = Vec::new();
    for p in projects {
        let mut filtered = 0i32;
        let mut claimed: Vec<i32> = Vec::new();
        for clip in &p.clips {
            let index = match clip.prompt.is_empty() {
                true => -1,
                false => {
                    let i = filtered;
                    filtered += 1;
                    i
                }
            };
            if clip.prompt.is_empty() && clip.gist.is_empty() && clip.idea.is_empty() {
                continue;
            }
            let render = best_render(renders, &p.id, index).map(|r| ShotRender {
                tier: r.tier.clone(),
                url: render_url(&p.id, &r.tier, r.shot_index),
                mime: r.content_type.clone(),
                takes: r.takes,
                stale: r.prompt.trim() != clip.prompt,
            });
            if render.is_some() {
                claimed.push(index);
            }
            out.push(Shot {
                project_id: p.id.clone(),
                project: p.name.clone(),
                index,
                prompt: clip.prompt.clone(),
                gist: clip.gist.clone(),
                idea: clip.idea.clone(),
                summary: p.summary.clone(),
                render,
            });
        }
        // A render whose clip is gone (the timeline was edited after the run) is
        // still a real, playable shot — it just has nothing left to describe it
        // but its own prompt.
        for r in renders
            .iter()
            .filter(|r| r.project_id == p.id && !claimed.contains(&r.shot_index))
        {
            if best_render(renders, &p.id, r.shot_index).map(|b| &b.tier) != Some(&r.tier) {
                continue;
            }
            out.push(Shot {
                project_id: p.id.clone(),
                project: p.name.clone(),
                index: r.shot_index,
                prompt: r.prompt.clone(),
                gist: String::new(),
                idea: String::new(),
                summary: p.summary.clone(),
                render: Some(ShotRender {
                    tier: r.tier.clone(),
                    url: render_url(&p.id, &r.tier, r.shot_index),
                    mime: r.content_type.clone(),
                    takes: r.takes,
                    stale: false,
                }),
            });
        }
    }
    out
}

/// `final` is the full-resolution stream and `proxy` the cheap preview, so the
/// former wins when a shot has both.
fn best_render<'a>(renders: &'a [Render], project_id: &str, shot_index: i32) -> Option<&'a Render> {
    if shot_index < 0 {
        return None;
    }
    let mut hit: Option<&Render> = None;
    for r in renders
        .iter()
        .filter(|r| r.project_id == project_id && r.shot_index == shot_index)
    {
        if r.tier == "final" {
            return Some(r);
        }
        hit = Some(r);
    }
    hit
}

/// Resolve a project NAME the way `validate_chat_actions` resolves a shot id:
/// to exactly one row, or to the candidates, never to a guess.
pub fn resolve_project<'a>(projects: &'a [Project], needle: &str) -> Resolved<'a> {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return Resolved::NotFound;
    }
    if let Some(p) = projects.iter().find(|p| p.name.to_lowercase() == needle) {
        return Resolved::Unique(p);
    }
    for narrow in [true, false] {
        let hits: Vec<&Project> = projects
            .iter()
            .filter(|p| {
                let name = p.name.to_lowercase();
                match narrow {
                    true => name.starts_with(&needle),
                    false => name.contains(&needle),
                }
            })
            .collect();
        match hits.len() {
            0 => continue,
            1 => return Resolved::Unique(hits[0]),
            _ => return Resolved::Ambiguous(hits.iter().map(|p| p.name.clone()).collect()),
        }
    }
    Resolved::NotFound
}

/// The shots that match, best first. Score is a weighted best-field token
/// coverage: a shot whose PROMPT contains every query word beats one whose
/// project synopsis happens to mention them.
pub fn search(query: &str, shots: &[Shot], limit: usize) -> Vec<Shot> {
    let terms = terms(query);
    if terms.is_empty() {
        return Vec::new();
    }
    let phrase = query.trim().to_lowercase();
    let mut scored: Vec<(f32, &Shot)> = shots
        .iter()
        .map(|s| (score(&terms, &phrase, s), s))
        .filter(|(v, _)| *v > 0.0)
        .collect();
    // Rendered shots win ties: "get me the video for the shot that does X" is
    // the ask, and a shot with nothing to play is a worse answer.
    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then(b.1.render.is_some().cmp(&a.1.render.is_some()))
            .then(a.1.index.cmp(&b.1.index))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(_, s)| s.clone())
        .collect()
}

fn score(terms: &[String], phrase: &str, shot: &Shot) -> f32 {
    let fields = [
        (1.0f32, &shot.prompt),
        (0.95, &shot.gist),
        (0.85, &shot.idea),
        (0.6, &shot.project),
        (0.5, &shot.summary),
    ];
    let mut best = 0.0f32;
    for (weight, text) in fields {
        let text = text.to_lowercase();
        let hits = terms.iter().filter(|t| text.contains(*t)).count();
        if hits == 0 {
            continue;
        }
        let mut v = weight * (hits as f32 / terms.len() as f32);
        if text.contains(phrase) {
            v += 0.15;
        }
        best = best.max(v);
    }
    best
}

fn terms(query: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in query
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2 && !STOPWORDS.contains(w))
    {
        if !out.iter().any(|w| w == word) {
            out.push(word.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(prompt: &str, gist: &str) -> Clip {
        Clip {
            prompt: prompt.to_string(),
            gist: gist.to_string(),
            idea: String::new(),
        }
    }

    fn project(id: &str, name: &str, clips: Vec<Clip>) -> Project {
        Project {
            id: id.to_string(),
            name: name.to_string(),
            summary: String::new(),
            clips,
        }
    }

    fn render(project_id: &str, tier: &str, shot_index: i32, prompt: &str) -> Render {
        Render {
            project_id: project_id.to_string(),
            tier: tier.to_string(),
            shot_index,
            prompt: prompt.to_string(),
            content_type: "video/webm".to_string(),
            takes: 1,
        }
    }

    #[test]
    fn an_empty_prompt_clip_does_not_consume_a_shot_number() {
        // The engine drops empty-prompt clips before rendering, so the render
        // store's shot 1 is the SECOND clip that actually has a prompt.
        let p = project(
            "p1",
            "Storm",
            vec![
                clip("a lighthouse in a storm", ""),
                clip("", "a placeholder the user has not written yet"),
                clip("waves breaking over the rocks", ""),
            ],
        );
        let renders = vec![
            render("p1", "final", 0, "a lighthouse in a storm"),
            render("p1", "final", 1, "waves breaking over the rocks"),
        ];
        let shots = shots(&[p], &renders);
        assert_eq!(shots.len(), 3);
        assert_eq!(shots[0].index, 0);
        assert_eq!(
            shots[1].index, -1,
            "a promptless clip is not a rendered shot"
        );
        assert!(shots[1].render.is_none());
        assert_eq!(shots[2].index, 1);
        assert_eq!(
            shots[2].render.as_ref().unwrap().url,
            "/api/v1/ltx-director/projects/p1/renders/final/1"
        );
    }

    #[test]
    fn a_shot_edited_since_its_render_is_flagged_stale_not_hidden() {
        let p = project("p1", "Storm", vec![clip("a lighthouse at dawn", "")]);
        let shots = shots(&[p], &[render("p1", "final", 0, "a lighthouse in a storm")]);
        let r = shots[0].render.as_ref().unwrap();
        assert!(r.stale, "the prompt has changed since this clip rendered");
        assert_eq!(r.url, "/api/v1/ltx-director/projects/p1/renders/final/0");
    }

    #[test]
    fn the_full_resolution_tier_wins_over_the_preview() {
        let p = project("p1", "Storm", vec![clip("a lighthouse", "")]);
        let renders = vec![
            render("p1", "proxy", 0, "a lighthouse"),
            render("p1", "final", 0, "a lighthouse"),
        ];
        assert_eq!(
            shots(&[p], &renders)[0].render.as_ref().unwrap().tier,
            "final"
        );
    }

    #[test]
    fn a_preview_only_shot_still_plays() {
        let p = project("p1", "Storm", vec![clip("a lighthouse", "")]);
        let renders = vec![render("p1", "proxy", 0, "a lighthouse")];
        assert_eq!(
            shots(&[p], &renders)[0].render.as_ref().unwrap().tier,
            "proxy"
        );
    }

    #[test]
    fn a_render_whose_clip_was_deleted_is_still_findable() {
        let p = project("p1", "Storm", vec![]);
        let renders = vec![render("p1", "final", 0, "a lighthouse in a storm")];
        let shots = shots(&[p], &renders);
        assert_eq!(shots.len(), 1);
        assert_eq!(shots[0].prompt, "a lighthouse in a storm");
        assert!(shots[0].render.is_some());
    }

    #[test]
    fn a_deleted_clips_render_is_not_listed_once_per_tier() {
        let p = project("p1", "Storm", vec![]);
        let renders = vec![
            render("p1", "proxy", 0, "a lighthouse"),
            render("p1", "final", 0, "a lighthouse"),
        ];
        let shots = shots(&[p], &renders);
        assert_eq!(shots.len(), 1);
        assert_eq!(shots[0].render.as_ref().unwrap().tier, "final");
    }

    #[test]
    fn shots_are_never_attributed_to_another_project() {
        let a = project("p1", "Storm", vec![clip("a lighthouse", "")]);
        let b = project("p2", "Desert", vec![clip("a lighthouse", "")]);
        let renders = vec![render("p2", "final", 0, "a lighthouse")];
        let shots = shots(&[a, b], &renders);
        assert!(shots[0].render.is_none(), "p1 has no render of its own");
        assert!(shots[1].render.as_ref().unwrap().url.contains("p2"));
    }

    #[test]
    fn search_ranks_the_shots_own_prompt_above_its_projects_blurb() {
        let mut library = project(
            "p1",
            "Lighthouse film",
            vec![clip("a fox crossing a snowfield", "")],
        );
        library.summary = "a lighthouse in a storm".to_string();
        let other = project("p2", "Untitled", vec![clip("a lighthouse in a storm", "")]);
        let shots = shots(&[library, other], &[]);
        let hits = search("lighthouse storm", &shots, 5);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].project_id, "p2", "the matching PROMPT must win");
    }

    #[test]
    fn a_rendered_shot_outranks_an_identical_unrendered_one() {
        let a = project("p1", "A", vec![clip("a lighthouse in a storm", "")]);
        let b = project("p2", "B", vec![clip("a lighthouse in a storm", "")]);
        let renders = vec![render("p2", "final", 0, "a lighthouse in a storm")];
        let hits = search("lighthouse storm", &shots(&[a, b], &renders), 5);
        assert_eq!(hits[0].project_id, "p2");
    }

    #[test]
    fn a_query_of_nothing_but_filler_matches_nothing() {
        let p = project("p1", "A", vec![clip("a lighthouse in a storm", "")]);
        let shots = shots(&[p], &[]);
        assert!(search("the that this shot", &shots, 5).is_empty());
        assert!(search("", &shots, 5).is_empty());
        assert!(search("   ", &shots, 5).is_empty());
    }

    #[test]
    fn a_query_matching_no_shot_returns_no_shot() {
        let p = project("p1", "A", vec![clip("a lighthouse in a storm", "")]);
        assert!(search("submarine", &shots(&[p], &[]), 5).is_empty());
    }

    #[test]
    fn more_matching_words_ranks_higher() {
        let p = project(
            "p1",
            "A",
            vec![
                clip("a lighthouse on a calm morning", ""),
                clip("a lighthouse in a violent storm at night", ""),
            ],
        );
        let hits = search("lighthouse storm night", &shots(&[p], &[]), 5);
        assert_eq!(hits[0].index, 1);
    }

    #[test]
    fn search_respects_its_limit() {
        let clips: Vec<Clip> = (0..20)
            .map(|i| clip(&format!("a lighthouse {i}"), ""))
            .collect();
        let p = project("p1", "A", clips);
        assert_eq!(search("lighthouse", &shots(&[p], &[]), 5).len(), 5);
    }

    #[test]
    fn a_project_name_resolves_exactly_or_reports_the_candidates() {
        let projects = vec![
            project("p1", "Storm Film", vec![]),
            project("p2", "Storm Film (old)", vec![]),
            project("p3", "Desert", vec![]),
        ];
        // An exact name beats a prefix that also matches a second project.
        assert!(matches!(
            resolve_project(&projects, "storm film"),
            Resolved::Unique(p) if p.id == "p1"
        ));
        assert!(matches!(
            resolve_project(&projects, "des"),
            Resolved::Unique(p) if p.id == "p3"
        ));
        match resolve_project(&projects, "storm") {
            Resolved::Ambiguous(names) => assert_eq!(names.len(), 2),
            _ => panic!("two projects start with \"storm\" — it must not guess"),
        }
        assert!(matches!(
            resolve_project(&projects, "nothing"),
            Resolved::NotFound
        ));
        assert!(matches!(
            resolve_project(&projects, "  "),
            Resolved::NotFound
        ));
    }

    #[test]
    fn a_substring_match_is_the_last_resort_not_the_first() {
        let projects = vec![
            project("p1", "Desert Storm", vec![]),
            project("p2", "Storm Chasers", vec![]),
        ];
        // "storm" prefixes exactly one; the other only contains it.
        assert!(matches!(
            resolve_project(&projects, "storm"),
            Resolved::Unique(p) if p.id == "p2"
        ));
    }

    #[test]
    fn clips_survive_a_timeline_with_no_clip_array() {
        assert!(clips_from(&serde_json::json!(null)).is_empty());
        assert!(clips_from(&serde_json::json!("not an array")).is_empty());
        assert_eq!(
            clips_from(&serde_json::json!([{"prompt": " a fox "}])),
            vec![Clip {
                prompt: "a fox".to_string(),
                gist: String::new(),
                idea: String::new()
            }]
        );
    }
}
