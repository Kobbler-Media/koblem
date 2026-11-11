Local AI service for the kob ecosystem. One Axum API (`:8090`) fronts llama-server
plus a stack of GPU engines — images, matting, TTS, STT, music, video — behind a
VRAM-budget GPU gate. Backed by PostgreSQL (users, API keys, settings, logs,
generation gallery); optional NATS review consumer. A SvelteKit web UI (`web/`)
and the `kob` CLI (`cli/`) sit on top.

Machine-specific notes — build boxes, deployment hosts, local paths — belong in
the gitignored `CLAUDE.local.md` / `AGENTS.local.md`, not here.

## Build

`make` — fmt, lint (clippy -D warnings), check, build, test across **api + web + cli**.
`make cli` / `make web` / `make quick` (skip tests) for subsets.

Requires a sibling `../koblibs` checkout. See CONTRIBUTING.md.

## Architecture

koblem (Axum :8090) → llama-server (llama.cpp :8080) + the GPU engine services.
The OpenAI-format LLM path proxies directly (llama-server handles the chat
template natively). `/api/v1/*` endpoints are auth-gated (`require_auth`: env key,
web-login session, or a named API key as `Authorization: Bearer`).

**GPU gate** (`kob-gpu-gate`): each engine is admitted onto a card iff its
`peak_mb` fits the card's remaining budget, after evicting/relocating residents
that name it in their `evictable_by` (no numeric priority). The LLM is the
always-resident, relocatable occupant; the text embedder holds a standing
reservation and never waits on the gate.

**MeiliSearch tool rewriting**: intercepts `_meiliSearchInIndex` tool defs,
swaps the `filter` string for semantic booleans (`monitored_only`,
`in_library_only`, `watched`), and translates back in responses.

**Domain endpoints**: `/filter-releases`, `/validate-release`, `/rank-releases`
build internal prompts with quality definitions for release classification.

## Conventions

Zero comments by default — names and types are the documentation. Comment only
non-obvious WHY (a constraint, a workaround, an invariant, cross-file coupling a
reader cannot guess).

Bug fixes are TDD: a failing integration-level test first, then the fix.

No backwards-compatibility shims or commented-out old code. Delete replaced code;
history is what version control is for.

Every `sqlx::query!`-family macro is compile-time checked against the schema. The
offline cache in `api/.sqlx` is committed and must stay in sync — run
`make sqlx-prepare` after changing a query or a migration, and `make sqlx-check`
(part of `make check`) verifies it.

## Shared types

Wire types in `koblibs/koblem-client/src/types.rs` — used by the server **and**
the `kob` CLI. Don't duplicate; promote a route-local request/response struct
here when a client needs it. `koblem-client` is a path dependency on the sibling
`../koblibs` checkout, so a change there is a cross-repo change: kobbler and
kobload build against the same crate.

## Ecosystem

kobbler (`../kobbler/`), kobload (`../kobload/`), koblibs (`../koblibs/`).
