//! `POST /api/v1/character_sheet` — one LLM call, one structured sheet for the thing a player just
//! typed. Built for paperworld, which summons from a line of free text.
//!
//! **`height_m` is why this exists.** The caller had no idea how big a summon was, so an ant and a
//! cathedral both arrived at the sprite's default scale. Everything else on the sheet is what the
//! client needs to place, animate and simulate the thing without a second round trip — how it
//! moves, which of its parts move, what it does when it dies.
//!
//! **One call, no repair pass.** An out-of-schema reply is a 502 carrying the parse error, not a
//! defaulted sheet: every field is required and unknown fields are rejected, so a model that
//! renames `height_m` fails loudly instead of handing the game a 0-metre cathedral.
//!
//! **Auth is `/oneshot`'s shared-secret header**, not `require_auth` — same caller, same doorstop.
//! See [`crate::routes::oneshot::key_ok`].
//!
//! `routes/summon.rs` streams this sheet as the first stage of a summon, through [`sheet_for`] —
//! the same call, the same prompt, the same gate.

use std::time::Instant;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::warn;

use crate::AppState;
use crate::repositories::gpu_logs::CreateLog;
use crate::routes::oneshot::{API_KEY_HEADER, key_ok};
use crate::routes::tier2_helpers::{extract_caller, log_async};

/// One line of player text. Generous for a sentence, small enough that nobody parks the LLM behind
/// a pasted novel.
const CHARACTER_SHEET_BODY_LIMIT: usize = 16 * 1024;

/// Its own gpu-log bucket, so sheet calls stay separable from `/oneshot`'s renders.
const SERVICE: &str = "character_sheet";
const ENDPOINT: &str = "character_sheet";

/// The sheet is small and fixed-shape; the only thing that can run long is `parts`.
const MAX_TOKENS: u32 = 600;

/// Low, but not zero: the sheet is mostly recall (how tall IS a heron), while `name` and
/// `sound_desc` want a little life.
const TEMPERATURE: f64 = 0.3;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/character_sheet", post(character_sheet))
        .layer(DefaultBodyLimit::max(CHARACTER_SHEET_BODY_LIMIT))
}

// ── wire types ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CharacterSheetRequest {
    /// What the player typed, verbatim.
    pub text: String,
}

/// The sheet, returned as the whole response body. `Deserialize` is the load-bearing half — it is
/// how the model's reply is read — and `deny_unknown_fields` plus the absence of any `#[serde(
/// default)]` is what makes an off-schema reply an error rather than a half-filled sheet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CharacterSheet {
    pub name: String,
    /// Prose for the image generator, which conditions on a vision-language description rather
    /// than a tag list. Unbounded here; the prompt asks for 15–40 words.
    pub subject: String,
    /// The noise, not the thing making it. The prompt asks for one even from something silent, so
    /// this is empty only when the model declined — which the caller still honours by skipping the
    /// sound render rather than generating silence.
    pub sound_desc: String,
    pub height_m: f64,
    pub speed_mps: f64,
    pub mass_kg: f64,
    pub hp: i32,
    pub strength: i32,
    pub kind: Kind,
    /// Every way the thing can move, not just its main one — the game picks per situation.
    pub locomotion: Vec<Locomotion>,
    /// Whether it chooses to move while nobody is controlling it. Kept
    /// independent of locomotion: a car can drive but wait for a driver, while
    /// a camel can walk and wander on its own.
    pub roaming: bool,
    /// Capabilities independent of kind and movement.
    pub skills: Vec<Skill>,
    pub parts: Vec<Part>,
    pub death: Death,
    /// Only meaningful when `death` is `explode`.
    pub explosion_radius_m: f64,
    pub mountable: bool,
    pub buoyant: bool,
    /// Whether the far side of the thing looks like the near side.
    ///
    /// The caller builds a body out of one picture, and one view cannot say
    /// what is behind a subject — so without this every summon gets the answer
    /// for a person, and a toaster comes back with a blank rounded back when
    /// its real back is nearly its front. The model already knows which is
    /// which; this is the only field on the sheet that is purely about how the
    /// thing is *built* rather than how it behaves.
    ///
    /// A bool rather than a `same_as_front | flat | unknown` enum, which was
    /// the obvious shape and is the wrong one. Measured against the live model
    /// over 26 subjects, the same worked contrast either way: the bool called 0
    /// of 60 asymmetric samples symmetric, where a three-way enum called 43 of
    /// 60 and a two-way 30 — a red sports car, a wooden chair and a knight in
    /// plate armour among them, every one of which is a second face drawn on
    /// the back of a head.
    ///
    /// The enums look *more* consistent than the bool on the symmetric
    /// subjects, and that is the same fact seen from the other side. It is not
    /// the ordering: listing the symmetric arm **last** made it worse, not
    /// better — 44 of 44 asymmetric samples, including the man in a suit the
    /// first ordering had got right every time. Naming the answer as a
    /// label is what does it — the model reaches for the label, and only a
    /// yes/no makes it weigh the subject. `unknown` was never once chosen in
    /// 156 samples, so the third case earns nothing either.
    pub front_back_symmetric: bool,
    pub emits: Emits,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Part {
    pub name: String,
    pub motion: Motion,
    pub count: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Static,
    Fighter,
    Lover,
    Medic,
    Weapon,
    Mount,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Locomotion {
    Walk,
    Fly,
    Drive,
    Swim,
    Hop,
    Slither,
    Static,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Motion {
    Spin,
    Swing,
    Flap,
    Bob,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Skill {
    Shoot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Death {
    None,
    Explode,
    Shatter,
    Deflate,
    Ragdoll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Emits {
    None,
    Smoke,
    Sparks,
    Bubbles,
}

// ── prompt ───────────────────────────────────────────────────────────────────

/// The whole schema, stated once, in the model's own output format.
///
/// Deliberately free of per-subject rules: the vocabulary and the scale rule generalise, and a pile
/// of "a dragon should…" clauses would start contradicting each other. Judging whether the answer
/// is any good is the caller's job, not this prompt's.
///
/// `subject` and `sound_desc` are the two fields the model has to *write* rather than recall, and
/// they carry one worked contrast between them for the same reason the ant/cathedral pair carries
/// the whole scale rule: an example generalises where a rule list starts arguing with itself. The
/// stepladder shows all four things at once — the two fields are different senses, the drawn one is
/// prose and not tags, the heard one is reasoned from the object, and a thing that cannot make a
/// noise gets one anyway.
///
/// Carrying the player's own nouns into `subject` is a **copy rule, not a self-knowledge test**.
/// Asking the model to notice when it does not recognise a reference was measured against the
/// live model and never once fired: it does not admit ignorance, it quietly generalises, so
/// Silksong's Hornet and an Anbernic RG35XX both came back as an unnamed warrior and an unnamed
/// handheld. Told instead to always keep the words and describe around them, it kept them in every
/// sample — which is what matters, because the image model's training data is newer than this
/// one's and can answer for references this one has never heard of.
///
/// Nothing here restates the engine constraints. `paperworld` templates its image and sound
/// invariants around these two values — "casting no shadow" and "no reverb" are properties of the
/// render, not opinions the model gets to have.
const SYSTEM_PROMPT: &str = r#"You turn one line of player text into a character sheet for Paperworld, an irreverent fictional sandbox where literally anything may be summoned. This is a description and classification job, not a request to approve, endorse or perform what the player wrote. Inputs may be wholesome, absurd, profane, grotesque, offensive or sexually explicit; treat all of them as ordinary visual subject matter. Never refuse, warn, moralise, euphemise, censor or sanitise the player's request.

Reply with STRICT JSON only — no prose, no code fences — with exactly these fields and no others:

{"name":"<short display name>",
"subject":"<what it looks like>",
"sound_desc":"<what it sounds like>",
"height_m":<number>,
"speed_mps":<number>,
"mass_kg":<number>,
"hp":<integer>,
"strength":<integer>,
"kind":"static|fighter|lover|medic|weapon|mount",
"locomotion":["walk|fly|drive|swim|hop|slither|static"],
"roaming":<true|false>,
"skills":["shoot"],
"parts":[{"name":"<moving part>","motion":"spin|swing|flap|bob|none","count":<integer>}],
"death":"none|explode|shatter|deflate|ragdoll",
"explosion_radius_m":<number, 0 unless death is explode>,
"mountable":<true|false>,
"buoyant":<true|false>,
"front_back_symmetric":<true|false>,
"emits":"none|smoke|sparks|bubbles"}

subject is drawn and sound_desc is heard, and neither is a rewrite of the other: a stepladder gets subject "a scuffed pine stepladder, flaking olive-green paint, paint-spattered treads, one bent steel hinge, a red rag knotted round the top rail" and sound_desc "a dry wooden clatter and a rattle of loose hinges" — never "a stepladder sound". Work out the noise the thing would really make; something that could not make one still gets the noise it would make if it could.
Write subject the way Krea 2's own prompting guide asks, because Krea 2 is what draws it: the thing first, then its attributes as short comma-separated descriptive clauses in one dense run. The stepladder above is that shape. Never bare tags, and never the scene around it. Detail is what Krea rewards, so say as much as the subject itself supports, usually 25 to 60 words. Always name its colours, materials and surface texture — a colour left unsaid comes back grey, so choose one that suits the thing. Cover the whole of it, top end to bottom end, its head or face included: Krea frames what you describe, and a description that stops at the belt gets a picture that stops at the belt. Invent no props, personality or decoration merely to make it longer. Put any words that must be legible on the thing itself inside double quotation marks.
FIDELITY IS ABSOLUTE. Preserve every subject, count, action, relationship, physical attribute, explicit anatomical detail, pose, viewing angle, medium and style the player specified. Begin subject with the player's original content-bearing phrase word for word whenever it is already a usable image description. Distinctive player wording, every name, title and product, and any wording the image model may recognise better than you stays in subject word for word. Never replace it with a familiar substitute. Never turn realism into a cartoon, mascot, toy, game asset or joke; never add clothing or conceal anatomy; never make a serious request funny or a funny request serious. Comedy is context for the sandbox, not a house style.
When the player did not specify a pose or viewing angle, choose a calm, animation-ready intermediate pose natural to the exact subject, using the locomotion and moving parts you put elsewhere in this same sheet. Keep the main body stable and readable; let existing movable parts rest slightly away from the body and one another so their shapes and attachment points remain visible. Avoid extreme action, strong foreshortening, crossed appendages and self-occlusion. Do not add, remove or redesign parts for the pose, and do not add a branch, perch, plinth, stand or other support prop. For the input "owl", begin subject "owl, seen from the front, standing upright on its feet with both wings slightly lifted from its body" — never say perched and never add a branch. This is plausible while idle but leaves both wings visible enough to segment and flap. If the player did specify a pose or angle, preserve it even when a different one would rig more easily.
A part touching the body cannot be told apart from it afterwards, so pose it with background visible between each side limb and the torso, and between its legs. An arm resting against the side reads as more torso and is lost.
Always say which angle the thing is seen from, in subject, alongside the pose. Decide it from the shape of this particular subject. Something that stands upright on two legs, or that has a definite face or front it is normally met from — a person, an owl, a robot, a fridge — is seen from the front. Something whose length carries its shape, and whose two sides are near enough mirror images — a horse, a fish, a beetle, a car, a sofa — is seen from a level side profile, because head-on it is mostly its own chest and its whole length is lost. When neither fits, pick whichever single view shows the most of the parts you listed. For the input "cat", begin subject "cat, seen from the side in a level profile view, standing on all four legs with its tail out behind it".
When the player requested a visual medium or style, preserve it exactly. When they did not, do not choose one: describe the subject and pose without saying photograph, illustration, cartoon, render, game asset or any other medium. A cathedral is still a cathedral.
Height, speed and mass are the REAL-WORLD numbers for the thing itself — an ant is 0.005 m, a cathedral is 60 m — never a balanced game stat. hp and strength follow from them.
List EVERY way it can move: something winged that also has legs gets both "fly" and "walk". Use "static" only for something that cannot move at all.
roaming asks whether it moves around under its own direction when nobody is controlling it. A car waits for a driver, so roaming is false even though locomotion includes "drive"; a camel wanders, so roaming is true. A fixed turret is false even when its kind is fighter.
List every skill it can actively use. "shoot" means it deliberately attacks at range, not merely that it flies, throws itself, makes a noise or emits something. Use "shoot" only when the player requested a ranged attack or the subject is unambiguously a ranged weapon or ranged attacker; an ordinary owl, penis, chair or camel does not shoot. Use an empty list when it has no active skill. Kind still decides who it helps, loves or attacks, while roaming and skills decide how.
List every structural part that should move independently and how many there are — a car has 4 spinning wheels; a bird has 2 flapping wings and 2 swinging legs. Include all major repeated appendages the rig will need, and make the subject pose expose the same parts you list. parts is NOT a visual inventory. Never list colours, markings, textures, veins, pores, feathers or other surface details as parts. Do not split one continuous body into decorative regions merely to fill the list: a limbless penis character has parts [], because its head and veins are regions of one continuous body. Give an empty list when the whole subject moves as one piece.
front_back_symmetric asks whether the far side of the thing looks like the near side: walk round a toaster and there is a toaster again, walk round a person and there is their back.
Fill every field for whatever is asked for, however strange. Never refuse, never explain, never add a field."#;

fn llm_body(text: &str) -> Value {
    json!({
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": text},
        ],
        "max_tokens": MAX_TOKENS,
        "temperature": TEMPERATURE,
        "response_format": character_sheet_format(),
        "chat_template_kwargs": {"enable_thinking": false},
    })
}

/// Constrain llama.cpp while it is sampling rather than asking the parser to
/// clean up afterwards. `json_object` only guarantees braces and valid tokens;
/// on Gemma it has produced syntactically valid objects made of duplicate empty
/// keys. The full schema prevents that failure and the off-schema variants the
/// semantic validator would otherwise have to reject after spending the call.
fn character_sheet_format() -> Value {
    let strings = |values: &[&str]| json!({"type": "string", "enum": values});
    json!({
        "type": "json_schema",
        "json_schema": {
            "name": "paperworld_character_sheet",
            "strict": true,
            "schema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "name": {"type": "string"},
                    "subject": {"type": "string"},
                    "sound_desc": {"type": "string"},
                    "height_m": {"type": "number"},
                    "speed_mps": {"type": "number"},
                    "mass_kg": {"type": "number"},
                    "hp": {"type": "integer"},
                    "strength": {"type": "integer"},
                    "kind": strings(&["static", "fighter", "lover", "medic", "weapon", "mount"]),
                    "locomotion": {
                        "type": "array",
                        "items": strings(&["walk", "fly", "drive", "swim", "hop", "slither", "static"])
                    },
                    "roaming": {"type": "boolean"},
                    "skills": {
                        "type": "array",
                        "items": strings(&["shoot"])
                    },
                    "parts": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "name": {"type": "string"},
                                "motion": strings(&["spin", "swing", "flap", "bob", "none"]),
                                "count": {"type": "integer"}
                            },
                            "required": ["name", "motion", "count"]
                        }
                    },
                    "death": strings(&["none", "explode", "shatter", "deflate", "ragdoll"]),
                    "explosion_radius_m": {"type": "number"},
                    "mountable": {"type": "boolean"},
                    "buoyant": {"type": "boolean"},
                    "front_back_symmetric": {"type": "boolean"},
                    "emits": strings(&["none", "smoke", "sparks", "bubbles"])
                },
                "required": [
                    "name", "subject", "sound_desc", "height_m", "speed_mps", "mass_kg", "hp",
                    "strength", "kind", "locomotion", "roaming", "skills", "parts", "death",
                    "explosion_radius_m", "mountable", "buoyant", "front_back_symmetric", "emits"
                ]
            }
        }
    })
}

// ── handler ──────────────────────────────────────────────────────────────────

/// The body contract alone, with no auth check — split out from [`preflight`] so
/// it is testable with no configured doorstop key. There is no built-in key any
/// more (see `oneshot::key_ok`), so a unit test can no longer produce a header
/// that passes auth; this is the seam that lets it still exercise parsing and
/// validation.
fn validate_body(body: &[u8]) -> Result<CharacterSheetRequest, (StatusCode, String)> {
    let value: Value = serde_json::from_slice(body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            format!("invalid request JSON: {e}"),
        )
    })?;
    let req: CharacterSheetRequest =
        crate::strict_json::from_value_strict(value, "character_sheet")
            .map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    if req.text.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "text is required".to_string()));
    }
    Ok(req)
}

/// Everything that can reject a request before the LLM is touched. Split out so the non-2xx
/// branches are testable without an [`AppState`].
fn preflight(
    headers: &HeaderMap,
    body: &[u8],
) -> Result<CharacterSheetRequest, (StatusCode, String)> {
    if !key_ok(headers) {
        return Err((
            StatusCode::UNAUTHORIZED,
            format!("missing or wrong {API_KEY_HEADER}"),
        ));
    }
    validate_body(body)
}

async fn character_sheet(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let req = match preflight(&headers, &body) {
        Ok(r) => r,
        Err((status, msg)) => return err(status, &msg),
    };

    match sheet_for(&state, req.text.trim(), extract_caller(&headers)).await {
        Ok(sheet) => Json(sheet).into_response(),
        Err(e) => err(StatusCode::BAD_GATEWAY, &e),
    }
}

/// The whole route minus the HTTP shell, so `/summon` streams the same sheet from
/// the same one-shot call rather than growing a second prompt to drift from.
///
/// The LLM goes through `chat_completion_json`, which is where the gate's LLM
/// budget guard is taken — a call that reached llama-server any other way would
/// let a concurrent flux render OOM the card.
pub(crate) async fn sheet_for(
    state: &AppState,
    text: &str,
    caller: Option<String>,
) -> Result<CharacterSheet, String> {
    let started = Instant::now();

    let mut body = llm_body(text);
    crate::sampling::apply_sampling(&state.db, "sampling_character_sheet", &mut body).await;
    let reply = match state.llama.chat_completion_json(&body).await {
        Ok((_lease, r)) => r,
        Err(e) => {
            warn!(error = %e, "character sheet: llm call failed");
            return Err(format!("llm: {e}"));
        }
    };
    let content = reply
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let sheet = parse_sheet(content);

    log_async(
        state.db.clone(),
        CreateLog {
            service: SERVICE.into(),
            endpoint: ENDPOINT.into(),
            request_meta: json!({ "text": text }),
            response_meta: match &sheet {
                Ok(s) => json!({ "sheet": s }),
                Err(e) => json!({ "error": e, "reply": content }),
            },
            sample_id: None,
            gate_wait_ms: 0,
            duration_ms: started.elapsed().as_millis() as i64,
            status: if sheet.is_ok() { 200 } else { 502 },
            caller,
        },
    );

    sheet.inspect_err(|e| warn!(error = %e, "character sheet: unusable model reply"))
}

/// Read the sheet out of a chat reply.
///
/// Uses the shared extractor rather than a bare parse: the local model still wraps JSON in fences
/// or prose often enough that a strict parse alone would fail summons the reply actually answered.
/// Strictness lives on the TYPE — every field required, unknown fields rejected — so recovering the
/// object from prose never softens the schema.
///
/// Not routed through [`crate::strict_json::from_value_strict`]: its message is written for a
/// client that posted the wrong body to an endpoint, which is the wrong story to tell about a
/// model reply. Serde's own `deny_unknown_fields` error names the field and lists what was expected.
fn parse_sheet(content: &str) -> Result<CharacterSheet, String> {
    let value = crate::think::extract_json_payload(content, None)
        .ok_or_else(|| format!("model reply was not JSON: {}", clip(content)))?;
    let sheet: CharacterSheet =
        serde_json::from_value(value).map_err(|e| format!("model reply was off-schema: {e}"))?;
    sheet.validate()?;
    Ok(sheet)
}

impl CharacterSheet {
    /// Reject what the type system can't: a negative or non-finite quantity, or an empty identity.
    /// The game divides by, scales from and sorts on these, so a NaN height is a corrupt world
    /// rather than an ugly one.
    fn validate(&self) -> Result<(), String> {
        for (field, v) in [
            ("height_m", self.height_m),
            ("speed_mps", self.speed_mps),
            ("mass_kg", self.mass_kg),
            ("explosion_radius_m", self.explosion_radius_m),
        ] {
            if !v.is_finite() || v < 0.0 {
                return Err(format!(
                    "{field} must be a finite non-negative number, got {v}"
                ));
            }
        }
        if self.hp < 0 || self.strength < 0 {
            return Err("hp and strength must not be negative".to_string());
        }
        if let Some(p) = self.parts.iter().find(|p| p.count < 0) {
            return Err(format!("part `{}` has a negative count", p.name));
        }
        if self.name.trim().is_empty() || self.subject.trim().is_empty() {
            return Err("name and subject must not be empty".to_string());
        }
        if self.locomotion.is_empty() {
            return Err(
                "locomotion must name at least one way to move (`static` for something that \
                 cannot)"
                    .to_string(),
            );
        }
        Ok(())
    }
}

/// Keep a runaway reply out of the error string (and out of the log row).
fn clip(s: &str) -> String {
    s.trim().chars().take(200).collect()
}

fn err(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sheet_json() -> Value {
        json!({
            "name": "Heron",
            "subject": "a grey heron mid-stride in shallow water",
            "sound_desc": "a harsh croaking squawk",
            "height_m": 1.0,
            "speed_mps": 11.0,
            "mass_kg": 1.5,
            "hp": 8,
            "strength": 3,
            "kind": "fighter",
            "locomotion": ["walk", "fly"],
            "roaming": true,
            "skills": [],
            "parts": [
                {"name": "wings", "motion": "flap", "count": 2},
                {"name": "legs", "motion": "swing", "count": 2},
            ],
            "death": "ragdoll",
            "explosion_radius_m": 0.0,
            "mountable": false,
            "buoyant": true,
            "front_back_symmetric": false,
            "emits": "none",
        })
    }

    fn sheet() -> CharacterSheet {
        serde_json::from_value(sheet_json()).expect("fixture must parse")
    }

    /// The reply shapes the local model actually produces. Every one of these must yield the same
    /// sheet — the recovery is in the extractor, never in the schema.
    #[test]
    fn a_sheet_is_recovered_from_every_reply_wrapping() {
        let raw = sheet_json().to_string();
        for (label, reply) in [
            ("bare", raw.clone()),
            ("fenced", format!("```json\n{raw}\n```")),
            ("prose then fence", format!("Sure!\n\n```json\n{raw}\n```")),
            ("think block", format!("<think>a bird</think>{raw}")),
            ("doubled braces", raw.replacen('{', "{{", 1) + "}"),
            // The stutter the local model actually falls into, whole-response and deterministic
            // per input: every quote after the first comma is doubled. Left unrepaired it is a
            // permanent 502 for whichever summons trigger it.
            ("doubled key quotes", raw.replace(",\"", ",\"\"")),
        ] {
            assert_eq!(parse_sheet(&reply).expect(label), sheet(), "{label}");
        }
    }

    /// The whole point of the strict deserialize: none of these may come back as a defaulted or
    /// half-filled sheet.
    #[test]
    fn an_off_schema_reply_is_an_error_not_a_default() {
        let drop_field = |field: &str| {
            let mut v = sheet_json();
            v.as_object_mut().unwrap().remove(field).unwrap();
            v
        };
        let with = |field: &str, value: Value| {
            let mut v = sheet_json();
            v[field] = value;
            v
        };
        // The needle is whatever serde can name: the field for a missing or unknown one, the
        // offending token for a variant or a type error (those carry no field path).
        let cases = [
            // A field the model forgot — silently defaulting height_m to 0 is the bug this route
            // exists to prevent.
            ("missing height", drop_field("height_m"), "height_m"),
            ("missing kind", drop_field("kind"), "kind"),
            ("missing parts", drop_field("parts"), "parts"),
            ("missing roaming", drop_field("roaming"), "roaming"),
            ("missing skills", drop_field("skills"), "skills"),
            // A bool the model left out is the worst of the missing fields,
            // because the value a lenient parse would invent for it — false —
            // is a real answer about a real subject rather than an obvious gap.
            (
                "missing symmetry",
                drop_field("front_back_symmetric"),
                "front_back_symmetric",
            ),
            // A renamed field: dropped silently by a lenient parse, so it must name itself here.
            (
                "renamed field",
                with("height_meters", json!(1.0)),
                "height_meters",
            ),
            // Vocabulary the game has no code for.
            ("invented kind", with("kind", json!("boss")), "boss"),
            (
                "invented locomotion",
                with("locomotion", json!(["teleport"])),
                "teleport",
            ),
            (
                "invented motion",
                with(
                    "parts",
                    json!([{"name": "wheel", "motion": "roll", "count": 4}]),
                ),
                "roll",
            ),
            (
                "invented skill",
                with("skills", json!(["teleport"])),
                "teleport",
            ),
            ("invented death", with("death", json!("vanish")), "vanish"),
            ("invented emit", with("emits", json!("glitter")), "glitter"),
            // Right names, wrong types.
            (
                "string height",
                with("height_m", json!("1.0")),
                "invalid type",
            ),
            ("float hp", with("hp", json!(2.5)), "invalid type"),
            (
                "worded symmetry",
                with("front_back_symmetric", json!("yes")),
                "invalid type",
            ),
        ];
        for (label, body, mentions) in cases {
            let e = parse_sheet(&body.to_string()).expect_err(label);
            assert!(e.contains(mentions), "{label}: unhelpful message: {e}");
        }
        // Not JSON at all.
        assert!(parse_sheet("I'm afraid I can't do that.").is_err());
    }

    /// Numbers the type system accepts and the game cannot use.
    #[test]
    fn nonsense_quantities_are_rejected() {
        let with = |field: &str, value: Value| {
            let mut v = sheet_json();
            v[field] = value;
            parse_sheet(&v.to_string()).expect_err(field)
        };
        for field in ["height_m", "speed_mps", "mass_kg", "explosion_radius_m"] {
            assert!(with(field, json!(-1.0)).contains(field));
        }
        assert!(with("hp", json!(-3)).contains("hp"));
        assert!(with("strength", json!(-3)).contains("strength"));
        assert!(with("name", json!("   ")).contains("name"));
        assert!(with("subject", json!("")).contains("subject"));
        assert!(with("locomotion", json!([])).contains("locomotion"));
        assert!(
            with(
                "parts",
                json!([{"name": "wheel", "motion": "spin", "count": -4}])
            )
            .contains("wheel")
        );
        // NaN/Infinity aren't JSON literals, so they can only arrive as a bare token — which fails
        // at parse, not validation. The finite check guards the internal path.
        assert!(
            CharacterSheet {
                height_m: f64::NAN,
                ..sheet()
            }
            .validate()
            .is_err()
        );
        // An empty sound is a silent thing, not a broken one.
        let mut quiet = sheet_json();
        quiet["sound_desc"] = json!("");
        assert!(parse_sheet(&quiet.to_string()).is_ok());
    }

    /// The prompt IS the schema — the model only ever sees this string, so a field or a variant
    /// that exists in Rust and not in the prompt is a field the model will never fill.
    ///
    /// Named in the JSON block and not merely somewhere in the prose: the block is the shape the
    /// reply is copied from, and a field discussed only in a sentence below it is one the model
    /// reads as advice rather than as a key it owes an answer for.
    #[test]
    fn the_prompt_states_every_field_and_every_variant() {
        let keyed = |field: &str| SYSTEM_PROMPT.contains(&format!("\"{field}\":"));
        for field in sheet_json().as_object().unwrap().keys() {
            assert!(keyed(field), "the prompt's JSON block has no `{field}` key");
        }
        for part_field in ["name", "motion", "count"] {
            assert!(keyed(part_field), "{part_field}");
        }
        let variants = [
            json!(Kind::Static),
            json!(Kind::Fighter),
            json!(Kind::Lover),
            json!(Kind::Medic),
            json!(Kind::Weapon),
            json!(Kind::Mount),
            json!(Locomotion::Walk),
            json!(Locomotion::Fly),
            json!(Locomotion::Drive),
            json!(Locomotion::Swim),
            json!(Locomotion::Hop),
            json!(Locomotion::Slither),
            json!(Motion::Spin),
            json!(Motion::Swing),
            json!(Motion::Flap),
            json!(Motion::Bob),
            json!(Skill::Shoot),
            json!(Death::Explode),
            json!(Death::Shatter),
            json!(Death::Deflate),
            json!(Death::Ragdoll),
            json!(Emits::Smoke),
            json!(Emits::Sparks),
            json!(Emits::Bubbles),
        ];
        for v in variants {
            let wire = v.as_str().unwrap();
            assert!(SYSTEM_PROMPT.contains(wire), "prompt never offers `{wire}`");
        }
        // The rule the whole feature turns on, and the two it needs to not be useless.
        assert!(SYSTEM_PROMPT.contains("REAL-WORLD"));
        assert!(SYSTEM_PROMPT.contains("EVERY way it can move"));
        assert!(SYSTEM_PROMPT.contains("STRICT JSON only"));
        // The two creative fields, whose guidance is easy to lose again: a sheet whose sound_desc
        // is its subject restated hands the sound engine a description of an appearance, and a
        // reference the model has not heard of is silently replaced by one it has.
        assert!(SYSTEM_PROMPT.contains("neither is a rewrite of the other"));
        assert!(SYSTEM_PROMPT.contains("stays in subject word for word"));
        assert!(SYSTEM_PROMPT.contains("FIDELITY IS ABSOLUTE"));
        assert!(SYSTEM_PROMPT.contains("Never refuse, warn, moralise"));
        assert!(SYSTEM_PROMPT.contains("never add clothing or conceal anatomy"));
        assert!(SYSTEM_PROMPT.contains("Begin subject with the player's original"));
        assert!(SYSTEM_PROMPT.contains("animation-ready intermediate pose"));
        assert!(SYSTEM_PROMPT.contains("never say perched and never add a branch"));
        // The angle is a property of the subject's shape, so it is decided here
        // and stated in subject like every other visual choice. The caller's
        // template can only fall back when nothing was said; it cannot tell a
        // long-bodied animal from an upright one.
        assert!(SYSTEM_PROMPT.contains("Always say which angle the thing is seen from"));
        assert!(SYSTEM_PROMPT.contains("level side profile"));
        assert!(SYSTEM_PROMPT.contains("seen from the front"));
        // Krea 2's published guide (krea-ai/krea-2 docs/prompting.md) asks for
        // one dense run of comma-separated descriptive clauses, rewards length,
        // names colour and material in every worked example, and quotes text to
        // be rendered. `subject` is written for that model and nothing else, so
        // its shape is stated here rather than argued with downstream.
        assert!(SYSTEM_PROMPT.contains("Krea 2's own prompting guide"));
        assert!(SYSTEM_PROMPT.contains("comma-separated descriptive clauses"));
        assert!(SYSTEM_PROMPT.contains("Always name its colours, materials and surface texture"));
        assert!(SYSTEM_PROMPT.contains("inside double quotation marks"));
        assert!(SYSTEM_PROMPT.contains("do not choose one"));
        // The one field whose name does not say what it means. Measured against
        // the live model, the definition is what makes it answerable: given the
        // vocabulary alone the same model calls a red sports car symmetric.
        assert!(SYSTEM_PROMPT.contains("the far side of the thing looks like the near side"));
    }

    #[test]
    fn the_prompt_separates_movement_kind_and_skills() {
        for contrast in [
            "A car waits for a driver",
            "a camel wanders",
            "A fixed turret is false",
            "\"shoot\" means it deliberately attacks at range",
            "an ordinary owl, penis, chair or camel does not shoot",
            "Kind still decides",
        ] {
            assert!(
                SYSTEM_PROMPT.contains(contrast),
                "behavior contrast disappeared: {contrast}"
            );
        }
    }

    #[test]
    fn the_prompt_keeps_surface_details_out_of_the_rig() {
        for rule in [
            "structural part that should move independently",
            "Include all major repeated appendages",
            "make the subject pose expose the same parts",
            "parts is NOT a visual inventory",
            "Never list colours, markings, textures, veins, pores, feathers",
            "a limbless penis character has parts []",
            "whole subject moves as one piece",
        ] {
            assert!(
                SYSTEM_PROMPT.contains(rule),
                "parts contract disappeared: {rule}"
            );
        }
    }

    /// The render invariants live in paperworld's prompt templates, which wrap these two values.
    /// Restating any of them here would put them in the model's output as well, where they would
    /// be duplicated into the engine prompt and argued with.
    #[test]
    fn the_prompt_leaves_the_engine_constraints_to_the_caller() {
        for templated in [
            "reverb",
            "echo",
            "mono",
            "close-miked",
            "shadow",
            "silhouette",
            "watermark",
            "centred",
            "centered",
            "lighting",
        ] {
            assert!(
                !SYSTEM_PROMPT.to_lowercase().contains(templated),
                "prompt duplicates the templated constraint `{templated}`"
            );
        }
    }

    /// Every koblem route baselines thinking OFF; a thinking trace here would burn the token budget
    /// the sheet needs and land the JSON in `reasoning_content`.
    #[test]
    fn the_call_is_one_shot_with_thinking_off() {
        let body = llm_body("a heron");
        assert_eq!(
            body["chat_template_kwargs"]["enable_thinking"],
            json!(false)
        );
        assert_eq!(body["messages"][0]["content"], json!(SYSTEM_PROMPT));
        assert_eq!(body["messages"][1]["content"], json!("a heron"));
        assert_eq!(body["messages"].as_array().unwrap().len(), 2);
        assert_eq!(body["response_format"]["type"], json!("json_schema"));
        assert_eq!(
            body["response_format"]["json_schema"]["schema"]["additionalProperties"],
            json!(false)
        );
        assert_eq!(
            body["response_format"]["json_schema"]["schema"]["required"]
                .as_array()
                .unwrap()
                .len(),
            sheet_json().as_object().unwrap().len()
        );
        assert!(body.get("stream").is_none());
    }

    fn headers(key: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(v) = key {
            h.insert(API_KEY_HEADER, v.parse().unwrap());
        }
        h
    }

    /// Same doorstop as `/oneshot`: with no `ONESHOT_API_KEY` configured (the
    /// case in this test binary), every caller is rejected before the body is
    /// even parsed — a malformed body from an unauthorised caller still reads
    /// as 401, not 400.
    #[test]
    fn requests_are_rejected_before_the_llm_is_touched() {
        let good = json!({ "text": "a heron" }).to_string();

        for bad in [None, Some("hunter2"), Some("paperworld-shoddy-key")] {
            assert_eq!(
                preflight(&headers(bad), good.as_bytes())
                    .expect_err("no doorstop is configured in tests")
                    .0,
                StatusCode::UNAUTHORIZED,
                "accepted {bad:?}"
            );
        }
        assert_eq!(
            preflight(&headers(Some("hunter2")), b"{not json")
                .expect_err("bad key")
                .0,
            StatusCode::UNAUTHORIZED
        );
    }

    /// The body contract on its own, bypassing auth entirely via [`validate_body`] —
    /// see its doc comment for why.
    #[test]
    fn the_body_contract_is_checked_once_auth_would_have_passed() {
        assert_eq!(
            validate_body(json!({ "text": "a heron" }).to_string().as_bytes())
                .unwrap()
                .text,
            "a heron"
        );

        let reject = |b: &str| validate_body(b.as_bytes()).expect_err(b);
        assert_eq!(reject("{not json").0, StatusCode::BAD_REQUEST);
        assert_eq!(reject(&json!({}).to_string()).0, StatusCode::BAD_REQUEST);
        assert_eq!(
            reject(&json!({ "text": "   " }).to_string()).0,
            StatusCode::BAD_REQUEST
        );
        // A field this endpoint doesn't have would be silently dropped by a plain parse, and the
        // caller would be left wondering why it had no effect. It must name itself.
        let (status, msg) = reject(&json!({ "text": "a heron", "prompt": "a goose" }).to_string());
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(msg.contains("unknown field `prompt`"), "{msg}");
        // The whole-body typo just reads as the missing field.
        assert!(
            reject(&json!({ "prompt": "a heron" }).to_string())
                .1
                .contains("text")
        );
    }
}
