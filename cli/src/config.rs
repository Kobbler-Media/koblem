use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

pub const DEFAULT_URL: &str = "http://localhost:8090";

/// Annotated, fully-commented config scaffold. Lists every `[defaults.<cmd>]`
/// key the commands actually read (keep in sync with the `cfg.def_*` calls).
/// Used to seed a fresh config and printed by `kob config defaults`.
pub const DEFAULTS_TEMPLATE: &str = r##"# kob config — uncomment any value to set a default for that command.
# Precedence: explicit flag > config default > server default.

# url = "http://localhost:8090"
# token = "paste-an-api-key-from-Settings-→-API-Keys"

[defaults.chat]
# temperature = 0.7          # float
# top_p = 0.95               # float
# top_k = 20                 # int
# min_p = 0.0                # float
# max_tokens = 1024          # int
# grammar = "off"            # off | review | <name in the grammars table>

[defaults.image]
# model = "auto"             # auto | base | edit
# negative = "blurry, text"  # string
# width = 1024               # int
# height = 1024              # int
# steps = 12                 # int
# cfg = 5.0                  # float
# sampler = "euler"          # string
# scheduler = "simple"       # string
# batch = 1                  # int
# vae_tiling = false         # bool

[defaults.krea2]
# mode = "auto"              # auto | t2i | edit
# negative = "blurry, text"  # string
# width = 1024               # int
# height = 1024              # int
# steps = 8                  # int
# cfg = 1.0                  # float
# sampler = "euler"          # string
# scheduler = "simple"       # string
# batch = 1                  # int
# grounding_px = 768         # int (edit mode)

# Read by BOTH `kob rmbg` and `kob image generate --rmbg` — one knob set.
[defaults.rmbg]
# backend = "birefnet"       # string
# process_res = 1024         # int
# sensitivity = 0.5          # float
# mask_blur = 0              # int
# mask_offset = 0            # int
# refine = false             # bool
# invert = false             # bool
# bg_mode = "alpha"          # alpha (RGBA cut-out) | color (flatten onto bg_color)
# bg_color = "#00ff00"       # string

[defaults.tts]
# voice = "narrator"         # string (a cloned voice id)
# format = "mp3"             # wav | mp3 | pcm
# bitrate = 64               # int (kbps, mp3 only)
# temperature = 0.7          # float

[defaults.stt]
# language = "en"            # string
# beam_size = 4              # int
# granularity = "segment"    # segment | word | char

[defaults.music]
# format = "mp3"             # string
# vocal_language = "en"      # string
# steps = 8                  # int

[defaults.avatar]
# resolution = "480p"        # 480p | 720p | WxH
# segment_frames = 53        # int
# steps = 8                  # int

# Read by `kob video generate`. Keys an engine has no field for are skipped for
# that engine (model/profile/upscaler_ratio are inert on LTX-2.5; frames/quant wan).
[defaults.video]
# negative = "blurry, text"  # string
# width = 1280               # int
# height = 704               # int
# fps = 24                   # int
# steps = 8                  # int (ltx, ltx-2b)
# cfg = 1.0                  # float (ltx, ltx-2b)
# sampler = "euler_a"        # string (ltx, ltx-2b)
# scheduler = "simple"       # string (ltx, ltx-2b)
# segment_frames = 97        # int (ltx, ltx-2b)
# cont_latent_frames = 3     # int (ltx, ltx-2b)
# frames = 65                # int (wan only)
# quant = "nvfp4"            # nvfp4 | q4_k (wan only)
"##;

/// On-disk config (`~/.config/kob/config.toml`). `defaults` is a table keyed by
/// command name (`image`, `tts`, …); each value is a table of per-command flag
/// defaults that fill in for any flag the caller leaves unset.
#[derive(Debug, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "toml::Table::is_empty")]
    pub defaults: toml::Table,
}

pub fn config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("KOB_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("kob").join("config.toml"))
}

impl Config {
    pub fn load() -> Config {
        let Some(path) = config_path() else {
            return Config::default();
        };
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Config::default();
        };
        match toml::from_str(&raw) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("warning: ignoring malformed {}: {e}", path.display());
                Config::default()
            }
        }
    }

    pub fn save(&self) -> Result<PathBuf> {
        let path =
            config_path().context("cannot determine config path (set HOME or KOB_CONFIG)")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let body = toml::to_string(self).context("serialising config")?;
        std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }

    #[cfg(test)]
    pub fn with_defaults(defaults_toml: &str) -> Config {
        Config {
            defaults: toml::from_str(defaults_toml).expect("test defaults must parse"),
            ..Config::default()
        }
    }

    pub fn resolved_url(&self, flag: Option<String>) -> String {
        flag.or_else(|| self.url.clone())
            .unwrap_or_else(|| DEFAULT_URL.to_string())
    }

    pub fn resolved_token(&self, flag: Option<String>) -> Option<String> {
        flag.or_else(|| self.token.clone())
    }

    fn raw(&self, cmd: &str, key: &str) -> Option<&toml::Value> {
        self.defaults.get(cmd)?.as_table()?.get(key)
    }

    pub fn def_str(&self, cmd: &str, key: &str) -> Option<String> {
        self.raw(cmd, key)?.as_str().map(str::to_string)
    }
    pub fn def_u32(&self, cmd: &str, key: &str) -> Option<u32> {
        u32::try_from(self.raw(cmd, key)?.as_integer()?).ok()
    }
    pub fn def_i32(&self, cmd: &str, key: &str) -> Option<i32> {
        i32::try_from(self.raw(cmd, key)?.as_integer()?).ok()
    }
    pub fn def_i64(&self, cmd: &str, key: &str) -> Option<i64> {
        self.raw(cmd, key)?.as_integer()
    }
    pub fn def_u64(&self, cmd: &str, key: &str) -> Option<u64> {
        u64::try_from(self.raw(cmd, key)?.as_integer()?).ok()
    }
    pub fn def_f32(&self, cmd: &str, key: &str) -> Option<f32> {
        let v = self.raw(cmd, key)?;
        v.as_float()
            .map(|f| f as f32)
            .or_else(|| v.as_integer().map(|i| i as f32))
    }
    pub fn def_f64(&self, cmd: &str, key: &str) -> Option<f64> {
        let v = self.raw(cmd, key)?;
        v.as_float().or_else(|| v.as_integer().map(|i| i as f64))
    }
    pub fn def_bool(&self, cmd: &str, key: &str) -> Option<bool> {
        self.raw(cmd, key)?.as_bool()
    }
}

fn mask(token: &str) -> String {
    if token.len() <= 8 {
        "•".repeat(token.len())
    } else {
        format!("{}…{}", &token[..4], &token[token.len() - 4..])
    }
}

/// `kob config <set|show|path|edit>`.
pub fn run(cmd: ConfigCmd) -> Result<()> {
    match cmd {
        ConfigCmd::Set { url, token } => {
            let mut cfg = Config::load();
            if let Some(u) = url {
                cfg.url = Some(u);
            }
            if let Some(t) = token {
                cfg.token = Some(t);
            }
            let path = cfg.save()?;
            println!("saved {}", path.display());
            println!("tip: `kob config defaults` lists the tunable per-command defaults.");
        }
        ConfigCmd::Show => {
            let cfg = Config::load();
            if let Some(p) = config_path() {
                println!("config: {}", p.display());
            }
            println!("url:    {}", cfg.url.as_deref().unwrap_or(DEFAULT_URL));
            println!(
                "token:  {}",
                cfg.token
                    .as_deref()
                    .map(mask)
                    .unwrap_or_else(|| "(unset)".into())
            );
            if cfg.defaults.is_empty() {
                println!("defaults: (none — run `kob config defaults` to see what's tunable)");
            } else {
                println!("defaults:\n{}", toml::to_string(&cfg.defaults)?);
            }
        }
        ConfigCmd::Defaults => {
            print!("{DEFAULTS_TEMPLATE}");
        }
        ConfigCmd::Path => {
            let p = config_path().context("cannot determine config path")?;
            println!("{}", p.display());
        }
        ConfigCmd::Edit => {
            let path = config_path().context("cannot determine config path")?;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            if !path.exists() {
                // Seed a fresh file with the annotated template so the user can
                // see every tunable key instead of staring at a blank file.
                std::fs::write(&path, DEFAULTS_TEMPLATE)
                    .with_context(|| format!("writing {}", path.display()))?;
            }
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
            let status = std::process::Command::new(editor).arg(&path).status()?;
            if !status.success() {
                anyhow::bail!("editor exited with {status}");
            }
        }
    }
    Ok(())
}

#[derive(clap::Subcommand, Debug)]
pub enum ConfigCmd {
    /// Set the koblem URL and/or API token.
    Set {
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        token: Option<String>,
    },
    /// Print the current configuration (token masked).
    Show,
    /// Print an annotated list of every tunable per-command default.
    Defaults,
    /// Print the path to the config file.
    Path,
    /// Open the config file in $EDITOR (for the [defaults.*] tables).
    Edit,
}
