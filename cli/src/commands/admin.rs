use anyhow::Result;
use clap::Subcommand;
use koblem_client::types::{
    ApiKeySummary, AuthMeResponse, CreateApiKeyRequest, CreateApiKeyResponse, GpuCardsResponse,
    GpuEnginesResponse, GpuOccupancyResponse, GpuServicesResponse, HealthResponse,
};

use crate::{Ctx, output};

#[derive(Subcommand, Debug)]
pub enum KeysCmd {
    /// List API keys.
    List,
    /// Mint a new API key. The plaintext value is printed once.
    Create { name: String },
    /// Revoke an API key by id.
    Revoke { id: String },
    /// Check whether the configured token authenticates.
    Whoami,
}

#[derive(Subcommand, Debug)]
pub enum GpuCmd {
    /// Per-service enable/busy state.
    Status,
    /// Configured cards.
    Cards,
    /// Live per-card VRAM budget and residents.
    Occupancy,
    /// Per-service placement config (home GPU, peak MB, eviction rules).
    Engines,
}

pub async fn health(ctx: &Ctx) -> Result<()> {
    let resp = ctx.client.get("/health", &[]).await?;
    let body: HealthResponse = resp.json().await?;
    if ctx.json {
        return output::print_json(&body);
    }
    println!("status: {}", body.status);
    let line = |name: &str, up: bool, model: &Option<String>| {
        println!(
            "{name:<10} {:<5} {}",
            if up { "up" } else { "down" },
            model.as_deref().unwrap_or("")
        );
    };
    line("llama", body.llama_server, &body.llama_model);
    line("vision", body.vision, &body.vision_model);
    line("tts", body.tts, &body.tts_model);
    line("stt", body.stt, &body.stt_model);
    Ok(())
}

pub async fn keys(ctx: &Ctx, cmd: KeysCmd) -> Result<()> {
    match cmd {
        KeysCmd::List => {
            let resp = ctx.client.get("/api/v1/auth/api-keys", &[]).await?;
            let body: Vec<ApiKeySummary> = resp.json().await?;
            if ctx.json {
                return output::print_json(&body);
            }
            if body.is_empty() {
                println!("(no API keys)");
            }
            for k in &body {
                println!(
                    "{}  {:<24} {}  created {}{}",
                    k.id,
                    k.name,
                    if k.revoked { "revoked" } else { "active " },
                    k.created_at,
                    k.last_used_at
                        .as_deref()
                        .map(|t| format!("  last used {t}"))
                        .unwrap_or_default()
                );
            }
            Ok(())
        }
        KeysCmd::Create { name } => {
            let resp = ctx
                .client
                .post_json("/api/v1/auth/api-keys", &[], &CreateApiKeyRequest { name })
                .await?;
            let body: CreateApiKeyResponse = resp.json().await?;
            if ctx.json {
                return output::print_json(&body);
            }
            println!("{}", body.key);
            eprintln!("id {}", body.id);
            eprintln!("save it with `kob config set --token {}`", body.key);
            Ok(())
        }
        KeysCmd::Revoke { id } => {
            ctx.client
                .delete(&format!("/api/v1/auth/api-keys/{id}"), &[])
                .await?;
            println!("revoked {id}");
            Ok(())
        }
        KeysCmd::Whoami => {
            let resp = ctx.client.get("/api/v1/auth/me", &[]).await?;
            let body: AuthMeResponse = resp.json().await?;
            if ctx.json {
                return output::print_json(&body);
            }
            println!(
                "{}",
                if body.authenticated {
                    "authenticated"
                } else {
                    "not authenticated"
                }
            );
            Ok(())
        }
    }
}

pub async fn gpu(ctx: &Ctx, cmd: GpuCmd) -> Result<()> {
    match cmd {
        GpuCmd::Status => {
            let resp = ctx.client.get("/api/v1/gpu/services", &[]).await?;
            let body: GpuServicesResponse = resp.json().await?;
            if ctx.json {
                return output::print_json(&body);
            }
            for s in &body.services {
                println!(
                    "{:<12} {:<9} {}",
                    s.name,
                    if s.enabled { "enabled" } else { "disabled" },
                    if s.busy { "busy" } else { "" }
                );
            }
            Ok(())
        }
        GpuCmd::Cards => {
            let resp = ctx.client.get("/api/v1/gpu/scheduler/cards", &[]).await?;
            let body: GpuCardsResponse = resp.json().await?;
            if ctx.json {
                return output::print_json(&body);
            }
            for c in &body.cards {
                println!(
                    "{}  {:<20} {} MB  {}{}",
                    c.uuid,
                    c.label,
                    c.total_mb,
                    if c.enabled { "enabled" } else { "DISABLED" },
                    if c.fast { "  fast" } else { "" }
                );
            }
            Ok(())
        }
        GpuCmd::Occupancy => {
            let resp = ctx
                .client
                .get("/api/v1/gpu/scheduler/occupancy", &[])
                .await?;
            let body: GpuOccupancyResponse = resp.json().await?;
            if ctx.json {
                return output::print_json(&body);
            }
            for c in &body.cards {
                let measured = c
                    .measured_free_mb
                    .map(|m| format!(" (measured free {m} MB)"))
                    .unwrap_or_default();
                println!(
                    "{}  {} / {} MB used, {} free{measured}{}",
                    c.uuid,
                    c.used_mb,
                    c.total_mb,
                    c.free_mb,
                    if c.enabled { "" } else { "  DISABLED" }
                );
                for r in &c.residents {
                    println!("    {:<12} {} MB", r.service, r.peak_mb);
                }
            }
            Ok(())
        }
        GpuCmd::Engines => {
            let resp = ctx.client.get("/api/v1/gpu/scheduler/engines", &[]).await?;
            let body: GpuEnginesResponse = resp.json().await?;
            if ctx.json {
                return output::print_json(&body);
            }
            for e in &body.engines {
                println!(
                    "{:<12} {} MB  home={}  candidates=[{}]  evictable_by=[{}]{}",
                    e.service,
                    e.peak_mb,
                    e.home_gpu,
                    e.candidate_gpus.join(","),
                    e.evictable_by.join(","),
                    if e.relocatable { "  relocatable" } else { "" }
                );
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_parses_the_full_aggregated_probe() {
        let body: HealthResponse = serde_json::from_str(
            r#"{"status":"ok","llama_server":true,"vision":true,"tts":false,"stt":true,
                "llama_model":"qwen3.5","stt_model":"parakeet"}"#,
        )
        .unwrap();
        assert_eq!(body.status, "ok");
        assert!(body.vision);
        assert!(!body.tts);
        assert_eq!(body.llama_model.as_deref(), Some("qwen3.5"));
        assert_eq!(body.vision_model, None);
    }

    #[test]
    fn health_still_parses_an_older_two_field_payload() {
        let body: HealthResponse =
            serde_json::from_str(r#"{"status":"degraded","llama_server":false}"#).unwrap();
        assert!(!body.llama_server);
        assert!(!body.vision);
        assert!(body.stt_model.is_none());
    }

    #[test]
    fn gpu_payloads_parse() {
        let s: GpuServicesResponse =
            serde_json::from_str(r#"{"services":[{"name":"flux","enabled":true,"busy":false}]}"#)
                .unwrap();
        assert_eq!(s.services[0].name, "flux");

        let c: GpuCardsResponse = serde_json::from_str(
            r#"{"cards":[{"uuid":"GPU-1","label":"5060 Ti","total_mb":16380,"fast":true,
                 "enabled":true,"sort_order":0,"updated_at":"2026-07-31T00:00:00Z"}]}"#,
        )
        .unwrap();
        assert_eq!(c.cards[0].total_mb, 16380);

        let o: GpuOccupancyResponse = serde_json::from_str(
            r#"{"cards":[{"uuid":"GPU-1","total_mb":16380,"used_mb":8000,"free_mb":8380,
                 "measured_free_mb":8000,"fast":true,"enabled":true,
                 "residents":[{"service":"llm","peak_mb":7000}]}]}"#,
        )
        .unwrap();
        assert_eq!(o.cards[0].residents[0].service, "llm");
        assert_eq!(o.cards[0].measured_free_mb, Some(8000));

        let e: GpuEnginesResponse = serde_json::from_str(
            r#"{"engines":[{"service":"llm","peak_mb":7000,"home_gpu":"GPU-1",
                 "candidate_gpus":["GPU-1"],"evictable_by":["flux","songgen"],
                 "relocatable":true,"rules_json":"{}","updated_at":"2026-07-31T00:00:00Z"}]}"#,
        )
        .unwrap();
        assert_eq!(e.engines[0].evictable_by, vec!["flux", "songgen"]);
        assert!(e.engines[0].relocatable);
    }

    #[test]
    fn api_key_rows_parse_with_and_without_a_plaintext_key() {
        let rows: Vec<ApiKeySummary> = serde_json::from_str(
            r#"[{"id":"6ba7b810-9dad-11d1-80b4-00c04fd430c8","name":"cli","key":"abc",
                 "last_used_at":null,"created_at":"2026-07-31T00:00:00Z","revoked":false},
                {"id":"6ba7b811-9dad-11d1-80b4-00c04fd430c8","name":"old","key":null,
                 "last_used_at":"2026-07-30T00:00:00Z","created_at":"2026-01-01T00:00:00Z",
                 "revoked":true}]"#,
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].key.as_deref(), Some("abc"));
        assert!(rows[1].revoked);
        assert!(rows[1].last_used_at.is_some());
    }
}
