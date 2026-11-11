//! The catalogue — the single list of tools the agent can ever offer.
//!
//! Adding a tool is one entry here plus one arm in `dispatch`; the coherence
//! tests in `super` and the exhaustive `match` in `dispatch` make anything less
//! a compile or test failure.

use kob_gpu_gate::ServiceKind;
use serde_json::json;

use super::{EngineOption, RequiredWhen, Tool, ToolDef};

/// Every tool the agent can ever offer, in the order it is shown to the model.
///
/// **krea2 leads `make_image` on purpose.** Under the VRAM-budget gate it is
/// 5060-Ti-only with `evictable_by = []` (`migrations/0030_krea2_engine.sql`),
/// so it can never evict the LLM — the model stays warm and the next turn keeps
/// its prefix KV cache. FLUX.2 CAN evict the LLM when it spills onto the 3060
/// (`migrations/0021_evictable_by.sql` seeds `llm.evictable_by = [flux,
/// songgen]`), which costs a ~5-10 s cold reload plus a full re-prefill. FLUX.2
/// stays offered because it is the only image engine that takes reference
/// images.
pub fn catalogue() -> &'static [ToolDef] {
    &[
        ToolDef {
            tool: Tool::MakeImage,
            description: "Render a still image from a text prompt.",
            engines: &[
                EngineOption {
                    id: "krea2",
                    service: Some(ServiceKind::Krea2),
                    blurb: "krea2 (Krea 2 Turbo) — dense, photographic caption prose. Write ONE \
                            flowing paragraph describing a photograph that already exists: subject \
                            and action, then composition and camera, then lighting, then colour, \
                            then medium. Roughly 60-140 words. Text-to-image ONLY. Prefer this \
                            engine unless the user needs an edit.",
                },
                EngineOption {
                    id: "flux2",
                    service: Some(ServiceKind::Flux),
                    blurb: "flux2 (FLUX.2 Klein) — subject-first natural-language sentences in \
                            weight order: subject, then action, then style or medium, then \
                            context and lighting. Roughly 40-100 words, affirmative phrasing only \
                            (it has no negative prompts). The only engine that can EDIT an \
                            existing image from a reference.",
                },
            ],
            extra_properties: || {
                json!({
                    "prompt": {
                        "type": "string",
                        "description": "The image prompt, written for the chosen engine.",
                    },
                    "aspect": {
                        "type": "string",
                        "enum": ["square", "landscape", "portrait", "wide"],
                        "description": "Frame shape. Defaults to square.",
                    },
                    "count": {
                        "type": "integer",
                        "description": "How many variations to render (1-4). Defaults to 1.",
                    },
                })
            },
            required: &["prompt", "engine"],
            required_when: &[],
        },
        ToolDef {
            tool: Tool::EditImage,
            // The engine list is not a choice here: Krea 2 ships a text-to-image
            // checkpoint and no edit model (`config.rs`), so this intent is
            // FLUX.2-only and never shows an `engine` argument.
            description: "Change the most recent image in this conversation, keeping everything the \
                          change does not touch.",
            engines: &[EngineOption {
                id: "flux2edit",
                service: Some(ServiceKind::Flux),
                blurb: "",
            }],
            // Two arguments, and `redraw` is the one that picks the render path
            // (`dispatch::edits::edit_body`). It is a selector the model can be trusted with: over
            // 28 sampled turns against this schema it wrote an instruction every time and set
            // `redraw` ONLY on the four "make the whole picture look like X" asks. The prompt's
            // own SHAPE cannot select instead — under the old finished-picture wording the model
            // rewrote "put a hat on him" into "the man wearing a stylish fedora" on every one, so
            // a shape sniffer would read every edit as a repaint.
            extra_properties: || {
                json!({
                    "instruction": {
                        "type": "string",
                        "description": "The change to make, as an instruction addressed to the \
                                        picture: \"put a straw hat on the man\", \"make it a rainy \
                                        night\", \"redraw it as a watercolour\". Name only what \
                                        changes — everything you do not mention is kept.",
                    },
                    "redraw": {
                        "type": "number",
                        "description": "Only for \"make the whole picture look like X\" restyles \
                                        where nothing in it needs to survive recognisably: how far \
                                        to move from the original, 0.1 to 1.0. Leave it out for any \
                                        ordinary change.",
                    },
                })
            },
            required: &["instruction"],
            required_when: &[],
        },
        ToolDef {
            tool: Tool::MakeVideo,
            description: "Render a short video clip from a text prompt. Takes minutes, not \
                          seconds.",
            engines: &[EngineOption {
                // The id the model emits — not the gate's budget line (LTX-2.5 queues as
                // `minimax_h3`).
                id: "ltx",
                service: Some(ServiceKind::Ltx),
                blurb: "ltx (LTX-2.5) — renders the picture with synchronised audio. Describe ONE \
                        continuous shot: what is in frame, what moves, how the camera moves.",
            }],
            extra_properties: || {
                json!({
                    "prompt": {
                        "type": "string",
                        "description": "One continuous shot: subject, motion, camera move, setting \
                                        and light.",
                    },
                    "seconds": {
                        "type": "number",
                        "description": "Clip length in seconds (2-8). Defaults to 4. Longer costs \
                                        proportionally more time.",
                    },
                    "aspect": {
                        "type": "string",
                        "enum": ["landscape", "portrait", "square"],
                        "description": "Frame shape. Defaults to landscape.",
                    },
                })
            },
            required: &["prompt"],
            required_when: &[],
        },
        ToolDef {
            tool: Tool::MakeMusic,
            description: "Compose a piece of music or a sound bed.",
            engines: &[
                EngineOption {
                    id: "acestep",
                    service: Some(ServiceKind::Music),
                    blurb: "acestep (ACE-Step) — fast, general-purpose songs with sung vocals. \
                            Give it a style/mood/instrument caption. It writes its own lyrics when \
                            you leave them blank. ~10 s to render.",
                },
                EngineOption {
                    id: "sa3",
                    service: Some(ServiceKind::Sa3),
                    blurb: "sa3 (Stable Audio 3) — instrumental beds, loops, textures and sound \
                            design. No vocals. Fast and light.",
                },
                EngineOption {
                    id: "songgen",
                    service: Some(ServiceKind::Songgen),
                    blurb: "songgen (LeVo2) — the best vocal quality by a distance, but 2-5 \
                            MINUTES per render and it cannot write its own lyrics. Use it only \
                            when the user asks for a real song and quality matters more than \
                            waiting.",
                },
            ],
            extra_properties: || {
                json!({
                    "style": {
                        "type": "string",
                        "description": "Style, mood, tempo and instrumentation.",
                    },
                    "lyrics": {
                        "type": "string",
                        "description": "Sung lyrics. Leave out to have the engine write them; pass \
                                        \"[Instrumental]\" for no vocals.",
                    },
                    "seconds": {
                        "type": "number",
                        "description": "Target length in seconds. Defaults to about 30.",
                    },
                })
            },
            required: &["style", "engine"],
            required_when: &[],
        },
        ToolDef {
            tool: Tool::TransformMusic,
            description: "Rework the most recent audio in this conversation: re-sing it with new \
                          lyrics, regenerate part of it, pull one instrument out of it, or add one \
                          to it.",
            engines: &[EngineOption {
                id: "acestep_transform",
                service: Some(ServiceKind::Music),
                blurb: "",
            }],
            extra_properties: || {
                json!({
                    "mode": {
                        "type": "string",
                        "enum": ["cover", "repaint", "extract", "lego", "complete"],
                        "description": "cover = re-sing the song with new lyrics, keeping its \
                                        melody and arrangement. repaint = regenerate the audio \
                                        afresh from the same source. extract = isolate one \
                                        instrument as its own file. lego = play a new instrument \
                                        over the source mix. complete = build a full arrangement \
                                        around a lone isolated stem.",
                    },
                    "style": {
                        "type": "string",
                        "description": "Style, mood, tempo and instrumentation of the result. \
                                        Required for cover and repaint; for a faithful cover, \
                                        describe the source's OWN style.",
                    },
                    "track": {
                        "type": "string",
                        "description": "Which instrument extract, lego and complete act on — \
                                        vocals, drums, bass, guitar or keyboard. Required for \
                                        those three; ignored by cover and repaint.",
                    },
                    "lyrics": {
                        "type": "string",
                        "description": "cover only: the replacement lyrics to sing, structured to \
                                        match the source.",
                    },
                })
            },
            required: &["mode"],
            required_when: &[
                RequiredWhen {
                    arg: "track",
                    when: "mode",
                    is_one_of: &["extract", "lego", "complete"],
                },
                RequiredWhen {
                    arg: "style",
                    when: "mode",
                    is_one_of: &["cover", "repaint"],
                },
            ],
        },
        ToolDef {
            tool: Tool::MakeSpeech,
            description: "Speak text aloud in a synthesised voice.",
            engines: &[EngineOption {
                id: "breeze",
                service: Some(ServiceKind::Tts),
                blurb: "",
            }],
            extra_properties: || {
                json!({
                    "text": {
                        "type": "string",
                        "description": "Exactly what should be spoken.",
                    },
                    "instructions": {
                        "type": "string",
                        "description": "Voice design, e.g. \"warm friendly young woman, gentle \
                                        accent\".",
                    },
                })
            },
            required: &["text"],
            required_when: &[],
        },
        ToolDef {
            tool: Tool::Transcribe,
            description: "Transcribe the most recent audio in this conversation to text.",
            engines: &[EngineOption {
                id: "parakeet",
                service: Some(ServiceKind::Stt),
                blurb: "",
            }],
            extra_properties: || {
                json!({
                    "language": {
                        "type": "string",
                        "description": "ISO language code hint, e.g. \"en\". Omit to auto-detect.",
                    },
                })
            },
            required: &[],
            required_when: &[],
        },
        ToolDef {
            tool: Tool::DescribeImage,
            // You can already SEE the image — the vision tower is part of this
            // model. This tool is only the zero-shot scorer, and its description
            // has to say so or it gets called as a "look at this" crutch.
            description: "Score the most recent image against a list of candidate labels and \
                          return them ranked, using a zero-shot image classifier. You can already \
                          see images yourself, so use this only when the user wants each candidate \
                          scored — never to find out what an image shows.",
            engines: &[EngineOption {
                id: "embeddinggemma-2",
                service: Some(ServiceKind::Vision),
                blurb: "",
            }],
            extra_properties: || {
                json!({
                    "labels": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "The candidate labels to score, 2 to 20 of them. Short \
                                        noun phrases work best: \"a golden retriever\", \"a wolf\".",
                    },
                })
            },
            required: &["labels"],
            required_when: &[],
        },
        ToolDef {
            tool: Tool::RemoveBackground,
            description: "Cut the subject out of the most recent image in this conversation, \
                          leaving a transparent background.",
            engines: &[EngineOption {
                id: "birefnet",
                service: Some(ServiceKind::Matting),
                blurb: "",
            }],
            extra_properties: || json!({}),
            required: &[],
            required_when: &[],
        },
        ToolDef {
            tool: Tool::MakeRig,
            // §5a-d: this pipeline was browser-only, so the agent could make an image and then
            // had no way to turn it into the asset the user was actually after. The description
            // says "most recent image" because that is how every other media tool here reads its
            // input — there is no upload argument in an agent turn.
            description: "Turn the most recent image in this conversation into a 3D model: a \
                          textured GLB, optionally with a skeleton. Works best on a single \
                          subject on a plain background; the background is cut out first \
                          automatically.",
            engines: &[EngineOption {
                id: "spar3d",
                service: Some(ServiceKind::Spar3d),
                blurb: "",
            }],
            extra_properties: || {
                json!({
                    "rig": {
                        "type": "boolean",
                        "description": "Include a skeleton (glTF skin data). Defaults to false — \
                                        ask for it when the user wants to animate the model.",
                    },
                    "texture_mode": {
                        "type": "string",
                        "enum": ["generated", "project_front", "two_sided_mirror"],
                        "description": "How the texture is made. `project_front` (the default) \
                                        paints the source image onto the visible front and is the \
                                        most faithful to the picture; `generated` invents a full \
                                        texture; `two_sided_mirror` mirrors the front onto the back.",
                    },
                })
            },
            required: &[],
            required_when: &[],
        },
        ToolDef {
            tool: Tool::FindShots,
            // Retrieval only. The director editor owns every mutation of a
            // timeline (`routes::ltx_director`); nothing here writes.
            description: "Search the saved video projects for a shot and play back what was \
                          rendered of it. Answers \"which project had the shot where…\" and \"show \
                          me that shot again\". Read-only — it never changes a project.",
            engines: &[EngineOption {
                id: "director",
                service: None,
                blurb: "",
            }],
            extra_properties: || {
                json!({
                    "query": {
                        "type": "string",
                        "description": "What the shot shows, in the words its prompt would use — \
                                        \"lighthouse in a storm\", \"close-up of hands on a piano\".",
                    },
                    "project": {
                        "type": "string",
                        "description": "Search inside one project by name. Omit to search them all.",
                    },
                })
            },
            required: &["query"],
            required_when: &[],
        },
    ]
}
