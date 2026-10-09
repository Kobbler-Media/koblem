use clap::Args;
use koblem_client::types::BackgroundRemoval;

use crate::config::Config;

/// The matting knob set, declared once. `kob rmbg` sends it as a query string
/// (`/api/v1/matting/remove`); `kob image generate --rmbg` nests it in the
/// render request as `remove_background`. Both read `[defaults.rmbg]`.
#[derive(Args, Debug, Default, Clone)]
pub struct BgArgs {
    #[arg(long)]
    pub backend: Option<String>,
    #[arg(long)]
    pub process_res: Option<u32>,
    #[arg(long)]
    pub sensitivity: Option<f32>,
    #[arg(long)]
    pub mask_blur: Option<i32>,
    #[arg(long)]
    pub mask_offset: Option<i32>,
    /// Refine the alpha edge (slower, cleaner hair/fur).
    #[arg(long)]
    pub refine: bool,
    /// Invert the mask (keep the background, drop the subject).
    #[arg(long)]
    pub invert: bool,
    /// Background mode (e.g. transparent | color).
    #[arg(long)]
    pub bg_mode: Option<String>,
    /// Background color (e.g. "#00ff00") when bg_mode fills.
    #[arg(long)]
    pub bg_color: Option<String>,
}

impl BgArgs {
    pub fn resolve(&self, cfg: &Config, enabled: bool) -> BackgroundRemoval {
        BackgroundRemoval {
            enabled,
            backend: self
                .backend
                .clone()
                .or_else(|| cfg.def_str("rmbg", "backend")),
            process_res: self
                .process_res
                .or_else(|| cfg.def_u32("rmbg", "process_res")),
            sensitivity: self
                .sensitivity
                .or_else(|| cfg.def_f32("rmbg", "sensitivity")),
            mask_blur: self.mask_blur.or_else(|| cfg.def_i32("rmbg", "mask_blur")),
            mask_offset: self
                .mask_offset
                .or_else(|| cfg.def_i32("rmbg", "mask_offset")),
            refine: flag(self.refine, cfg.def_bool("rmbg", "refine")),
            invert: flag(self.invert, cfg.def_bool("rmbg", "invert")),
            bg_mode: self
                .bg_mode
                .clone()
                .or_else(|| cfg.def_str("rmbg", "bg_mode")),
            bg_color: self
                .bg_color
                .clone()
                .or_else(|| cfg.def_str("rmbg", "bg_color")),
        }
    }
}

fn flag(set: bool, default: Option<bool>) -> Option<bool> {
    if set { Some(true) } else { default }
}

/// Flatten the knobs into `/api/v1/matting/remove`'s query string, which the
/// server parses back into a field-identical `RemoveQuery`. `enabled` has no
/// query counterpart (hitting the endpoint IS the toggle).
pub fn to_query(bg: &BackgroundRemoval) -> Vec<(&'static str, String)> {
    let mut q: Vec<(&'static str, String)> = Vec::new();
    let mut push = |k: &'static str, v: Option<String>| {
        if let Some(v) = v {
            q.push((k, v));
        }
    };
    push("backend", bg.backend.clone());
    push("process_res", bg.process_res.map(|v| v.to_string()));
    push("sensitivity", bg.sensitivity.map(|v| v.to_string()));
    push("mask_blur", bg.mask_blur.map(|v| v.to_string()));
    push("mask_offset", bg.mask_offset.map(|v| v.to_string()));
    push("refine", bg.refine.map(|v| v.to_string()));
    push("invert", bg.invert.map(|v| v.to_string()));
    push("bg_mode", bg.bg_mode.clone());
    push("bg_color", bg.bg_color.clone());
    q
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config::default()
    }

    #[test]
    fn every_matting_knob_survives_into_the_request() {
        let args = BgArgs {
            backend: Some("birefnet".into()),
            process_res: Some(2048),
            sensitivity: Some(0.42),
            mask_blur: Some(3),
            mask_offset: Some(-2),
            refine: true,
            invert: true,
            bg_mode: Some("color".into()),
            bg_color: Some("#00ff00".into()),
        };
        let bg = args.resolve(&cfg(), true);
        assert!(bg.enabled);
        assert_eq!(bg.backend.as_deref(), Some("birefnet"));
        assert_eq!(bg.process_res, Some(2048));
        assert_eq!(bg.sensitivity, Some(0.42));
        assert_eq!(bg.mask_blur, Some(3));
        assert_eq!(bg.mask_offset, Some(-2));
        assert_eq!(bg.refine, Some(true));
        assert_eq!(bg.invert, Some(true));
        assert_eq!(bg.bg_mode.as_deref(), Some("color"));
        assert_eq!(bg.bg_color.as_deref(), Some("#00ff00"));
    }

    #[test]
    fn query_carries_the_same_ten_knobs_the_server_parses() {
        let bg = BgArgs {
            backend: Some("rmbg2".into()),
            process_res: Some(1024),
            sensitivity: Some(0.5),
            mask_blur: Some(1),
            mask_offset: Some(2),
            refine: true,
            invert: true,
            bg_mode: Some("transparent".into()),
            bg_color: Some("#fff".into()),
        }
        .resolve(&cfg(), true);
        let q = to_query(&bg);
        let keys: Vec<&str> = q.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            keys,
            vec![
                "backend",
                "process_res",
                "sensitivity",
                "mask_blur",
                "mask_offset",
                "refine",
                "invert",
                "bg_mode",
                "bg_color",
            ]
        );
        assert!(q.contains(&("refine", "true".to_string())));
        assert!(q.contains(&("invert", "true".to_string())));
    }

    #[test]
    fn unset_knobs_are_omitted_rather_than_sent_as_defaults() {
        let bg = BgArgs::default().resolve(&cfg(), false);
        assert!(!bg.enabled);
        assert!(to_query(&bg).is_empty());
        assert_eq!(bg.refine, None);
        assert_eq!(bg.invert, None);
    }
}
