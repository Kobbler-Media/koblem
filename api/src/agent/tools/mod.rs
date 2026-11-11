//! The ONE tool registry. Everything the agent knows about its tools is derived
//! from [`Tool::ALL`] + [`catalogue`]: the OpenAI `tools` JSON Schema, the
//! engine-chooser text spliced into the system prompt, and the availability
//! filter. `dispatch.rs` matches exhaustively on [`Tool`], so a tool cannot be
//! advertised without a dispatcher (or the crate does not compile).
//!
//! Tools are INTENTS, not engines. The engine is an `enum` parameter, which is
//! what makes the model ask "flux2 or krea2?" instead of guessing. Each tool
//! shows the model 3-5 arguments; everything else comes from the engine route's
//! own defaults (`ImageGenerateRequest` has 17 fields and the model should see
//! four — `tool_rewrite.rs` documents that this model fills semantic enums
//! perfectly but drowns in wide structs).
//!
//! The catalogue data itself lives in its own file; this one owns the shapes,
//! the schema projection and the argument rules.

mod catalogue;

pub use catalogue::catalogue;

use kob_gpu_gate::ServiceKind;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tool {
    MakeImage,
    EditImage,
    MakeVideo,
    MakeMusic,
    TransformMusic,
    MakeSpeech,
    Transcribe,
    DescribeImage,
    RemoveBackground,
    MakeRig,
    FindShots,
}

impl Tool {
    pub const ALL: [Tool; 11] = [
        Tool::MakeImage,
        Tool::EditImage,
        Tool::MakeVideo,
        Tool::MakeMusic,
        Tool::TransformMusic,
        Tool::MakeSpeech,
        Tool::Transcribe,
        Tool::DescribeImage,
        Tool::RemoveBackground,
        Tool::MakeRig,
        Tool::FindShots,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Tool::MakeImage => "make_image",
            Tool::EditImage => "edit_image",
            Tool::MakeVideo => "make_video",
            Tool::MakeMusic => "make_music",
            Tool::TransformMusic => "transform_music",
            Tool::MakeSpeech => "make_speech",
            Tool::Transcribe => "transcribe",
            Tool::DescribeImage => "describe_image",
            Tool::RemoveBackground => "remove_background",
            Tool::MakeRig => "make_3d_asset",
            Tool::FindShots => "find_shots",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Tool::ALL.into_iter().find(|t| t.name() == name)
    }
}

/// One selectable engine behind an intent.
pub struct EngineOption {
    pub id: &'static str,
    /// The GPU service to health-probe. `None` is a tool that needs no engine at
    /// all — the director library is a database read — and is therefore always
    /// live.
    pub service: Option<ServiceKind>,
    /// Chooser text for the system prompt. Condensed from each engine's own
    /// prompt-craft rules (`routes::images::SUGGEST_SYSTEM_PROMPT`,
    /// `routes::krea2::SUGGEST_SYSTEM_PROMPT`) down to what the model needs to
    /// PICK an engine and write for it — the full rules stay on the
    /// `/suggest-prompt` routes that own them.
    pub blurb: &'static str,
}

/// An argument that only becomes mandatory once another argument takes one of a
/// set of values. JSON Schema's `required` is a flat list and this model does
/// not read `if`/`then` anyway, so the rule lives here and [`missing_required`]
/// enforces it before any dispatcher runs.
///
/// It exists because of a shipped bug: the CLI let `extract`/`lego`/`complete`
/// through without a `track` stem selector and ace-server rendered garbage
/// rather than refusing.
pub struct RequiredWhen {
    pub arg: &'static str,
    pub when: &'static str,
    pub is_one_of: &'static [&'static str],
}

pub struct ToolDef {
    pub tool: Tool,
    pub description: &'static str,
    /// Selectable engines, best default FIRST. A single-engine tool does not
    /// expose an `engine` argument at all.
    pub engines: &'static [EngineOption],
    /// Extra arguments beyond `engine`, as JSON Schema properties.
    pub extra_properties: fn() -> Value,
    pub required: &'static [&'static str],
    pub required_when: &'static [RequiredWhen],
}

pub fn def(tool: Tool) -> &'static ToolDef {
    catalogue()
        .iter()
        .find(|d| d.tool == tool)
        .expect("every Tool variant has a ToolDef")
}

/// Conditionally-required arguments the model left out. Checked for every tool
/// in `dispatch`, so a missing selector costs one cheap round trip instead of a
/// silently wrong render.
pub fn missing_required(tool: Tool, args: &Value) -> Vec<&'static RequiredWhen> {
    def(tool)
        .required_when
        .iter()
        .filter(|r| {
            args.get(r.when)
                .and_then(Value::as_str)
                .is_some_and(|v| r.is_one_of.contains(&v))
                && !is_present(args, r.arg)
        })
        .collect()
}

fn is_present(args: &Value, key: &str) -> bool {
    match args.get(key) {
        Some(Value::String(s)) => !s.trim().is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Null) | None => false,
        Some(_) => true,
    }
}

/// A tool as offered on THIS turn, with its engine list already filtered to
/// what is configured, enabled and answering.
pub struct LiveTool {
    pub tool: Tool,
    pub engines: Vec<&'static EngineOption>,
}

impl LiveTool {
    pub fn engine_ids(&self) -> Vec<&'static str> {
        self.engines.iter().map(|e| e.id).collect()
    }

    /// The OpenAI `tools[]` entry. The `engine` enum carries only live engines,
    /// so the model is never offered a down engine — and is omitted entirely
    /// when there is nothing to choose between.
    pub fn schema(&self) -> Value {
        let d = def(self.tool);
        let mut properties = (d.extra_properties)();
        let mut required: Vec<&str> = d.required.to_vec();
        let ids = self.engine_ids();
        if ids.len() > 1 {
            properties["engine"] = json!({
                "type": "string",
                "enum": ids,
                "description": "Which engine to render with.",
            });
        } else {
            required.retain(|r| *r != "engine");
        }
        json!({
            "type": "function",
            "function": {
                "name": self.tool.name(),
                "description": d.description,
                "parameters": {
                    "type": "object",
                    "properties": properties,
                    "required": required,
                },
            },
        })
    }

    /// The engine the model asked for, or the default (first live engine).
    pub fn resolve_engine(&self, args: &Value) -> Option<&'static EngineOption> {
        let asked = args.get("engine").and_then(|v| v.as_str());
        match asked {
            Some(id) => self
                .engines
                .iter()
                .copied()
                .find(|e| e.id == id)
                .or_else(|| self.engines.first().copied()),
            None => self.engines.first().copied(),
        }
    }
}

/// Frame geometry for an `aspect` argument. Multiples of 32 so every engine's
/// patch/VAE stride divides cleanly.
pub fn aspect_dims(aspect: Option<&str>, base: u32) -> (u32, u32) {
    let long = base;
    let short = (base * 9 / 16 / 32) * 32;
    let wide = (base * 5 / 12 / 32) * 32;
    match aspect.unwrap_or("square") {
        "landscape" => (long, short),
        "portrait" => (short, long),
        "wide" => (long, wide),
        _ => (long, long),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_variant_has_exactly_one_definition() {
        for t in Tool::ALL {
            let hits = catalogue().iter().filter(|d| d.tool == t).count();
            assert_eq!(hits, 1, "{} must have exactly one ToolDef", t.name());
        }
        assert_eq!(catalogue().len(), Tool::ALL.len());
    }

    #[test]
    fn every_definition_is_reachable_by_name() {
        for d in catalogue() {
            assert_eq!(
                Tool::parse(d.tool.name()),
                Some(d.tool),
                "{} must round-trip through parse",
                d.tool.name()
            );
        }
    }

    #[test]
    fn every_tool_has_at_least_one_engine_and_unique_engine_ids() {
        let mut seen: Vec<&str> = Vec::new();
        for d in catalogue() {
            assert!(!d.engines.is_empty(), "{} has no engines", d.tool.name());
            for e in d.engines {
                assert!(!seen.contains(&e.id), "duplicate engine id {}", e.id);
                seen.push(e.id);
            }
        }
    }

    #[test]
    fn every_declared_argument_exists_in_the_schema() {
        for d in catalogue() {
            let props = (d.extra_properties)();
            for r in d.required.iter().filter(|r| **r != "engine") {
                assert!(
                    props.get(r).is_some(),
                    "{} requires {r} but never offers it",
                    d.tool.name()
                );
            }
            for c in d.required_when {
                assert!(
                    props.get(c.arg).is_some(),
                    "{} conditionally requires {} but never offers it",
                    d.tool.name(),
                    c.arg
                );
                let allowed = props
                    .get(c.when)
                    .and_then(|w| w.get("enum"))
                    .and_then(|e| e.as_array())
                    .cloned()
                    .unwrap_or_default();
                for v in c.is_one_of {
                    assert!(
                        allowed.contains(&json!(v)),
                        "{}: {} is not a value {} can take",
                        d.tool.name(),
                        v,
                        c.when
                    );
                }
            }
        }
    }

    #[test]
    fn multi_engine_tools_expose_an_engine_enum_of_live_engines_only() {
        let d = def(Tool::MakeImage);
        let live = LiveTool {
            tool: Tool::MakeImage,
            engines: d.engines.iter().collect(),
        };
        let schema = live.schema();
        let params = &schema["function"]["parameters"];
        assert_eq!(
            params["properties"]["engine"]["enum"],
            json!(["krea2", "flux2"])
        );
        assert_eq!(schema["function"]["name"], "make_image");
        assert!(
            params["required"]
                .as_array()
                .unwrap()
                .contains(&json!("engine"))
        );
    }

    #[test]
    fn a_downed_engine_is_never_offered() {
        let d = def(Tool::MakeImage);
        let live = LiveTool {
            tool: Tool::MakeImage,
            // krea2 down; only flux2 answering.
            engines: d.engines.iter().filter(|e| e.id == "flux2").collect(),
        };
        let schema = live.schema();
        let params = &schema["function"]["parameters"];
        assert!(
            params["properties"].get("engine").is_none(),
            "a single live engine must not be offered as a choice"
        );
        assert!(
            !params["required"]
                .as_array()
                .unwrap()
                .contains(&json!("engine"))
        );
    }

    #[test]
    fn single_engine_tools_never_offer_an_engine_argument() {
        for t in [
            Tool::EditImage,
            Tool::TransformMusic,
            Tool::DescribeImage,
            Tool::FindShots,
        ] {
            let d = def(t);
            assert_eq!(d.engines.len(), 1, "{} must have one engine", t.name());
            let live = LiveTool {
                tool: t,
                engines: d.engines.iter().collect(),
            };
            assert!(
                live.schema()["function"]["parameters"]["properties"]
                    .get("engine")
                    .is_none(),
                "{} must not ask the model to choose an engine",
                t.name()
            );
        }
    }

    /// `redraw` is what picks Kontext vs img2img (`dispatch::edits::edit_body`), so the schema has
    /// to ask for an INSTRUCTION and leave `redraw` optional. A finished-picture `prompt` puts
    /// every edit — "put a hat on him" included — on the path that repaints the subject.
    #[test]
    fn editing_asks_for_an_instruction_and_keeps_redraw_optional() {
        let d = def(Tool::EditImage);
        let props = (d.extra_properties)();
        assert!(props.get("instruction").is_some());
        assert!(props.get("redraw").is_some());
        assert!(props.get("prompt").is_none());
        assert!(props.get("strength").is_none());
        assert_eq!(d.required, ["instruction"]);
    }

    #[test]
    fn krea2_is_the_default_image_engine() {
        // Under the VRAM gate krea2 can never evict the LLM; flux2 can. First
        // in the list is what `resolve_engine` falls back to.
        assert_eq!(def(Tool::MakeImage).engines[0].id, "krea2");
    }

    #[test]
    fn editing_an_image_is_flux2_only() {
        // Krea 2 ships a text-to-image checkpoint and no edit model
        // (`config.rs`: "TEXT-TO-IMAGE ONLY (no edit model)").
        let engines = def(Tool::EditImage).engines;
        assert_eq!(engines.len(), 1);
        assert_eq!(engines[0].service, Some(ServiceKind::Flux));
    }

    #[test]
    fn an_unknown_engine_falls_back_to_the_default_rather_than_failing() {
        let d = def(Tool::MakeImage);
        let live = LiveTool {
            tool: Tool::MakeImage,
            engines: d.engines.iter().collect(),
        };
        assert_eq!(
            live.resolve_engine(&json!({"engine": "midjourney"}))
                .unwrap()
                .id,
            "krea2"
        );
        assert_eq!(
            live.resolve_engine(&json!({"engine": "flux2"})).unwrap().id,
            "flux2"
        );
        assert_eq!(live.resolve_engine(&json!({})).unwrap().id, "krea2");
    }

    #[test]
    fn a_stem_mode_without_a_track_is_caught_before_dispatch() {
        // The CLI shipped without this check and ace-server rendered garbage
        // rather than refusing.
        for mode in ["extract", "lego", "complete"] {
            let missing = missing_required(
                Tool::TransformMusic,
                &json!({"mode": mode, "style": "warm indie rock"}),
            );
            assert_eq!(missing.len(), 1, "{mode} must demand a track");
            assert_eq!(missing[0].arg, "track");
        }
    }

    #[test]
    fn a_stem_mode_with_a_track_passes() {
        for mode in ["extract", "lego", "complete"] {
            assert!(
                missing_required(
                    Tool::TransformMusic,
                    &json!({"mode": mode, "track": "drums"})
                )
                .is_empty()
            );
        }
    }

    #[test]
    fn a_blank_track_does_not_count_as_supplied() {
        let missing = missing_required(
            Tool::TransformMusic,
            &json!({"mode": "lego", "track": "  "}),
        );
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].arg, "track");
    }

    #[test]
    fn modes_that_take_no_stem_are_not_asked_for_one() {
        for mode in ["cover", "repaint"] {
            let missing = missing_required(
                Tool::TransformMusic,
                &json!({"mode": mode, "style": "warm indie rock"}),
            );
            assert!(missing.is_empty(), "{mode} must not demand a track");
        }
    }

    #[test]
    fn a_caption_mode_without_a_style_is_caught_before_dispatch() {
        // ace-server 400s cover/repaint with an empty caption; the stem tasks
        // are the ones that legitimately skip it.
        for mode in ["cover", "repaint"] {
            let missing = missing_required(Tool::TransformMusic, &json!({"mode": mode}));
            assert_eq!(missing.len(), 1, "{mode} must demand a style");
            assert_eq!(missing[0].arg, "style");
        }
    }

    #[test]
    fn tools_without_conditional_rules_never_report_missing_arguments() {
        for t in Tool::ALL.into_iter().filter(|t| *t != Tool::TransformMusic) {
            assert!(
                missing_required(t, &json!({})).is_empty(),
                "{} reported a conditional gap with no rules",
                t.name()
            );
        }
    }

    #[test]
    fn a_library_tool_needs_no_gpu_service() {
        assert!(
            def(Tool::FindShots)
                .engines
                .iter()
                .all(|e| e.service.is_none()),
            "find_shots reads the database — it must never be gated on an engine probe"
        );
    }

    #[test]
    fn every_rendering_tool_names_a_service_to_probe() {
        for d in catalogue().iter().filter(|d| d.tool != Tool::FindShots) {
            assert!(
                d.engines.iter().all(|e| e.service.is_some()),
                "{} would never be health-checked",
                d.tool.name()
            );
        }
    }

    #[test]
    fn aspect_maps_to_engine_safe_dimensions() {
        assert_eq!(aspect_dims(None, 1024), (1024, 1024));
        assert_eq!(aspect_dims(Some("square"), 1024), (1024, 1024));
        assert_eq!(aspect_dims(Some("landscape"), 1024), (1024, 576));
        assert_eq!(aspect_dims(Some("portrait"), 1024), (576, 1024));
        assert_eq!(aspect_dims(Some("wide"), 1024), (1024, 416));
        for base in [768, 1024, 1280] {
            for a in ["square", "landscape", "portrait", "wide"] {
                let (w, h) = aspect_dims(Some(a), base);
                assert_eq!(w % 32, 0, "{a}@{base} width");
                assert_eq!(h % 32, 0, "{a}@{base} height");
            }
        }
    }

    #[test]
    fn unknown_tool_names_do_not_parse() {
        assert_eq!(Tool::parse("dalle.text2im"), None);
        assert_eq!(Tool::parse(""), None);
    }
}
