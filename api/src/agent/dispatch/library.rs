//! `find_shots` — retrieval over the saved Director projects.
//!
//! The only tool that touches no GPU. It answers three shapes of question the
//! editor's own chat cannot ("which project had the shot where…", "show me that
//! shot again", "what did I write for the storm project"), and it never edits a
//! timeline: `routes::ltx_director` owns every mutation.

use serde_json::{Value, json};

use crate::agent::director::{self, Resolved, Shot};
use crate::agent::wire::StepOutput;

use super::{Ctx, ToolOutcome, str_arg, truncate};

/// Shots described back to the model, and — since every rendered one gets a
/// player — the only bound on how many clips a search offers. Enough to say "it
/// was in the storm project, shot 4" without turning the observation into a
/// catalogue.
const MAX_HITS: usize = 5;

pub(super) async fn find_shots(ctx: &Ctx, args: &Value) -> ToolOutcome {
    let Some(query) = str_arg(args, "query") else {
        return ToolOutcome::failed("find_shots needs something to look for");
    };
    let (projects, renders) = match director::load(&ctx.state.db).await {
        Ok(v) => v,
        Err(e) => {
            return ToolOutcome::failed(format!("the project library could not be read: {e}"));
        }
    };
    if projects.is_empty() {
        return ToolOutcome {
            ok: true,
            observation: json!({"ok": true, "shots": [], "note": "there are no saved video projects yet"}),
            outputs: Vec::new(),
            detail: "no saved projects".to_string(),
        };
    }

    // A named project is resolved to exactly one row or not used at all —
    // searching the wrong project silently is the failure mode worth avoiding.
    let scope = match str_arg(args, "project") {
        None => None,
        Some(name) => match director::resolve_project(&projects, &name) {
            Resolved::Unique(p) => Some(p.id.clone()),
            Resolved::Ambiguous(names) => return ask_which(&name, names),
            Resolved::NotFound => {
                return ask_which(&name, projects.iter().map(|p| p.name.clone()).collect());
            }
        },
    };

    let mut shots = director::shots(&projects, &renders);
    if let Some(id) = &scope {
        shots.retain(|s| &s.project_id == id);
    }
    let hits = director::search(&query, &shots, MAX_HITS);
    if hits.is_empty() {
        return ToolOutcome {
            ok: true,
            observation: json!({
                "ok": true,
                "shots": [],
                "searched": scope.is_some().then(|| str_arg(args, "project")).flatten(),
                "note": "nothing in the saved projects matches that",
            }),
            outputs: Vec::new(),
            detail: format!("no shot matches “{}”", truncate(&query, 60)),
        };
    }

    let outputs = clips_for(&hits);
    ToolOutcome {
        ok: true,
        observation: json!({
            "ok": true,
            "shots": hits.iter().map(describe).collect::<Vec<_>>(),
            "played_back": outputs.len(),
        }),
        detail: detail(&hits, outputs.len()),
        outputs,
    }
}

/// Never guess which project was meant — hand the names back and let the model
/// ask. Reported as a success because nothing failed: the answer is a question.
fn ask_which(asked: &str, mut names: Vec<String>) -> ToolOutcome {
    names.truncate(20);
    ToolOutcome {
        ok: true,
        observation: json!({
            "ok": true,
            "shots": [],
            "note": format!("\"{asked}\" does not name exactly one project — ask which of these is meant"),
            "projects": names,
        }),
        outputs: Vec::new(),
        detail: format!("“{asked}” matches no single project"),
    }
}

fn describe(s: &Shot) -> Value {
    let render = s.render.as_ref();
    json!({
        "project": s.project,
        "shot": (s.index >= 0).then_some(s.index),
        "prompt": truncate(&s.prompt, 400),
        "gist": (!s.gist.is_empty()).then(|| truncate(&s.gist, 200)),
        "rendered": render.is_some(),
        "tier": render.map(|r| r.tier.clone()),
        "takes": render.map(|r| r.takes),
        "edited_since_render": render.map(|r| r.stale),
    })
}

fn detail(hits: &[Shot], played: usize) -> String {
    let projects: Vec<&str> = hits.iter().fold(Vec::new(), |mut acc, s| {
        if !acc.contains(&s.project.as_str()) {
            acc.push(&s.project);
        }
        acc
    });
    format!(
        "{} shot(s) across {} — {played} played back",
        hits.len(),
        projects.join(", ")
    )
}

/// The director render route serves each matched clip in place. Stored
/// token-less like every other step output — the read path stamps the viewer's
/// own `?token=` on, which that route accepts as a fallback for exactly this.
fn clips_for(hits: &[Shot]) -> Vec<StepOutput> {
    hits.iter()
        .filter_map(|s| s.render.as_ref())
        .map(|r| StepOutput {
            kind: "video".to_string(),
            url: r.url.clone(),
            generation_id: None,
            mime: r.mime.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::director::ShotRender;
    use crate::agent::wire::with_token;

    fn shot(project: &str, index: i32, prompt: &str, rendered: bool) -> Shot {
        Shot {
            project_id: project.to_string(),
            project: project.to_string(),
            index,
            prompt: prompt.to_string(),
            gist: String::new(),
            idea: String::new(),
            summary: String::new(),
            render: rendered.then(|| ShotRender {
                tier: "final".to_string(),
                url: director::render_url(project, "final", index),
                mime: "video/webm".to_string(),
                takes: 3,
                stale: true,
            }),
        }
    }

    #[test]
    fn a_hit_names_its_project_and_shot_number() {
        let v = describe(&shot("Storm", 4, "a lighthouse in a storm", true));
        assert_eq!(v["project"], "Storm");
        assert_eq!(v["shot"], 4);
        assert_eq!(v["rendered"], true);
        assert_eq!(v["tier"], "final");
        assert_eq!(v["takes"], 3);
        assert_eq!(v["edited_since_render"], true);
    }

    #[test]
    fn an_unrendered_shot_says_so_rather_than_pretending() {
        let v = describe(&shot("Storm", 4, "a lighthouse", false));
        assert_eq!(v["rendered"], false);
        assert_eq!(v["tier"], Value::Null);
        assert_eq!(v["takes"], Value::Null);
    }

    #[test]
    fn a_promptless_shot_has_no_shot_number_to_quote() {
        assert_eq!(describe(&shot("Storm", -1, "", false))["shot"], Value::Null);
    }

    #[test]
    fn a_long_prompt_is_elided_before_it_becomes_prompt_tokens() {
        let v = describe(&shot("Storm", 0, &"x".repeat(900), false));
        assert!(v["prompt"].as_str().unwrap().chars().count() <= 401);
    }

    #[test]
    fn an_ambiguous_project_asks_instead_of_choosing() {
        let o = ask_which(
            "storm",
            vec!["Storm Film".into(), "Storm Film (old)".into()],
        );
        assert!(
            o.ok,
            "a question is not a failure — it must not burn a retry"
        );
        assert!(o.outputs.is_empty());
        assert_eq!(o.observation["shots"].as_array().unwrap().len(), 0);
        assert_eq!(o.observation["projects"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn the_candidate_list_is_capped() {
        let names: Vec<String> = (0..50).map(|i| format!("p{i}")).collect();
        let o = ask_which("p", names);
        assert_eq!(o.observation["projects"].as_array().unwrap().len(), 20);
    }

    #[test]
    fn a_matched_clip_plays_from_the_director_route_itself() {
        let outputs = clips_for(&[shot("Storm", 4, "a lighthouse", true)]);
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].kind, "video");
        assert_eq!(outputs[0].mime, "video/webm");
        assert_eq!(
            outputs[0].url, "/api/v1/ltx-director/projects/Storm/renders/final/4",
            "the render is served in place — no copy into the agent gallery"
        );
        assert!(
            outputs[0].generation_id.is_none(),
            "nothing was persisted, so there is no gallery row to point at"
        );
    }

    #[test]
    fn every_rendered_hit_gets_a_player_not_just_the_first() {
        let hits = vec![
            shot("Storm", 0, "a", true),
            shot("Storm", 1, "b", true),
            shot("Desert", 0, "c", false),
            shot("Desert", 1, "d", true),
        ];
        assert_eq!(clips_for(&hits).len(), 3);
    }

    #[test]
    fn an_unrendered_hit_contributes_no_output() {
        assert!(clips_for(&[shot("Storm", 4, "a lighthouse", false)]).is_empty());
        assert!(clips_for(&[]).is_empty());
    }

    #[test]
    fn the_read_path_can_stamp_a_token_onto_a_render_url() {
        // The one way this could 401 in the browser: the stamping helper is
        // shared with /api/v1/generations/… and a render URL is a different
        // path, so prove it lands as the FIRST query param and is what
        // `get_render`'s `?token=` fallback reads.
        let out = &clips_for(&[shot("Storm", 4, "a lighthouse", true)])[0];
        assert!(!out.url.contains('?'), "stored URLs must be token-less");
        assert_eq!(
            with_token(&out.url, "sk-live/1+2"),
            "/api/v1/ltx-director/projects/Storm/renders/final/4?token=sk%2Dlive%2F1%2B2"
        );
    }

    #[test]
    fn the_step_row_names_every_project_a_hit_came_from_once() {
        let hits = vec![
            shot("Storm", 0, "a", true),
            shot("Storm", 1, "b", true),
            shot("Desert", 0, "c", false),
        ];
        assert_eq!(
            detail(&hits, 2),
            "3 shot(s) across Storm, Desert — 2 played back"
        );
    }
}
