# koblem

A GPU media-services box for [kobbler](https://github.com/Kobbler-Media/kobbler). One
Axum API (`:8090`) fronts local LLM inference plus a stack of image, audio,
speech and vision models, arbitrates them onto the available GPUs with a
VRAM-budget scheduler, and exposes the lot over HTTP with a SvelteKit web UI and
a `kob` CLI on top.

## Read this first

**koblem is experimental and is published for people to adapt, not to install
as-is.** It is tuned to one specific hardware setup (two NVIDIA cards, a
particular set of models and VRAM budgets), and not all of the GPU engine images
it talks to are published — some of their build contexts are private (see
[`docs/engines.md`](docs/engines.md)). Expect to change code and configuration
to fit your own machine.

**koblem is optional. kobbler works fine without it.** Nothing in kobbler
requires koblem to be running; koblem adds AI-assisted features, and if its URL
is unset those features are simply absent.

**This is a specialised service, and most people will not want it.** It assumes
an NVIDIA GPU and that you are comfortable sourcing multi-gigabyte model weights
yourself. There is no support commitment, and the install is not one-click.

If that is still interesting, read on.

## What it actually does

The API is the only thing in this repository that runs your GPU work directly —
and even then only by proxying. Everything is split three ways:

1. **llama-server** — llama.cpp with a small patch series, built from
   `docker/llama-server/`. This is the one engine koblem's compose files start
   for you. It is the always-resident occupant of the GPU gate.
2. **The other engines** — image, music, avatar, TTS, STT, vision, matting,
   depth, video, 3D. koblem **does not run these**. They are separate GPU
   services; koblem calls them over HTTP at URLs you configure.
   [`docs/engines.md`](docs/engines.md) lists where each server and its weights
   come from. If you do not run one, set no URL and the corresponding feature
   disappears from the UI.
3. **koblem itself** — the Axum API, the PostgreSQL schema behind it (users, API
   keys, settings, LLM/GPU logs, the generation gallery), the job queue, and the
   GPU gate that decides who gets the card.

The **GPU gate** (`kob-gpu-gate`, from koblibs) is the interesting part. Each
engine has a worst-case VRAM peak; it is admitted onto a card only if that peak
fits the card's remaining budget, possibly after evicting or relocating
residents that list it in their `evictable_by`. The LLM is resident by default,
and a render that needs its VRAM drains and unloads it, then hands the card
back. That is what the llama.cpp patch series exists to make possible — a
parent/child process split so the CUDA context can actually be torn down and the
VRAM genuinely returned.

Also in here: an OpenAI-compatible chat proxy, a MeiliSearch tool-parameter
rewriter, and release-classification endpoints (`/rank-releases`,
filter/validate) that kobbler uses to judge candidate downloads.

## Prerequisites

These are hard requirements, not suggestions.

- **An NVIDIA GPU**, a working driver, and **nvidia-container-toolkit** on the
  Docker host. Both the API container (for `nvidia-smi` VRAM reads) and
  llama-server request GPU devices. AMD/CPU are possible for llama-server alone
  by changing `LLAMA_IMAGE`, but the rest of the stack is CUDA-shaped and
  untested that way.
- **Model weights, supplied by you.** Nothing is downloaded automatically. The
  default llama-server config wants a gemma-4-12b main quant, its F16 unified
  vision+audio mmproj, and an MTP draft — these specific GGUFs are not published
  anywhere. Put your own in `$KOBLEM_MODELS_DIR` and name them via `LLAMA_MODEL`
  / `LLAMA_MMPROJ` / `LLAMA_DRAFT_MODEL`, or set `TEXT_URL` / `MMPROJ_URL` /
  `DRAFT_URL` to something `curl` can fetch. A missing file with no URL is a hard
  error and the container will refuse to start.
- **To build from source only:** a sibling checkout of
  [koblibs](https://github.com/Kobbler-Media/koblibs) at `../koblibs`. The
  `koblem`, `koblem-web` and `koblem-llama` images are published to
  `ghcr.io/kobbler-media` (linux/amd64), so running the stack doesn't need it. koblibs is a shared Rust
  workspace (plus a Svelte UI package) that is not on crates.io or npm yet, so
  both the Rust and the web builds expect it next to this repository:

  ```
  parent/
    koblem/     <- this repo
    koblibs/    <- sibling checkout
  ```

  The Cargo path dependencies (`../../koblibs/<crate>`) and `web/package.json`
  (`file:../../koblibs`) both depend on that layout. Override the directory for
  Docker builds with `KOBLIBS_DIR`.
- **Docker with BuildKit** for the compose builds, and **PostgreSQL** — supplied
  by the compose stack, so you only need your own if you build natively.

## Running it

```sh
cp .env.example .env     # edit it
docker compose -f docker-compose.yml -f docker-compose.llm.yml pull
docker compose -f docker-compose.yml -f docker-compose.llm.yml up -d
```

Then point kobbler's **Koblem Service URL** at `http://koblem:8090`.

### Compose layout

The stack is split into a core plus per-service overlays so you can run only the
parts you have hardware for. Combine them with repeated `-f`:

| File | Contains |
|------|----------|
| `docker-compose.yml` | Core: the API, the web UI, PostgreSQL. Starts no engines, needs no GPU. |
| `docker-compose.gpu-visibility.yml` | Gives koblem-api `nvidia-smi` access so the scheduler sees real VRAM use. Only if the API is co-located with the GPUs. |
| `docker-compose.llm.yml` | llama-server — the only engine this repo builds and runs. |
| `docker-compose.vision.yml` | URLs for classification, matting and depth. |
| `docker-compose.tts.yml` | URL for the Breeze TTS engine. |
| `docker-compose.stt.yml` | URL for the Parakeet STT engine. |
| `docker-compose.media.yml` | URLs for the image, music and avatar engines. |
| `docker-compose.test.yml` | Throwaway PostgreSQL for the test suite. |

The four engine overlays only set environment variables: those engines are
separate services that run elsewhere, so there is nothing for koblem to start.
They exist so each engine is one `-f` to switch on, and so the URLs are
documented next to what they configure. If the engines live in another compose
project, koblem also needs to be on that project's network.

Every host path in the stack is an environment variable with a default — see
[`.env.example`](.env.example), which lists all of them. The only bind mount is
the model directory; everything else is a named volume, because the job stores
must outlive the container (an orphaned in-flight render keeps burning the card
and its output is never collected).

### First run: register your GPUs

**You must do this before the scheduler will admit any work, and the failure mode
if you skip it is a hang rather than an error.**

The GPU gate places work by card UUID. A fresh install ships with **no cards** —
GPU UUIDs are specific to one physical machine, so there is nothing sensible to
seed — and every engine's budget row ships with a blank `home_gpu` and an empty
`candidate_gpus`. Until you fill those in, `try_place` finds no candidate card
and the request parks in a retry loop: no error, no 503, no timeout, no log line.
The job queue degrades more politely — queued jobs show `NoCardFits` with a
reason — but a direct HTTP call to an engine route simply never returns.

Get your card UUIDs from the host:

```sh
nvidia-smi --query-gpu=uuid,name,memory.total --format=csv
```

Then insert them. **There is currently no API, web UI or CLI command that creates
a card** — the scheduler page can toggle and edit existing cards but cannot add
one, and the `nvidia-smi` reconciler only reads free VRAM for cards already in
the table. Raw SQL is the only route:

```sh
docker compose exec -T koblem-db psql -U koblem -d koblem <<'SQL'
INSERT INTO gpu_cards (uuid, label, total_mb, fast, enabled, sort_order) VALUES
  ('GPU-xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx', 'RTX 4090 24GB', 24564, TRUE, TRUE, 0);
SQL
```

With at least one card registered, open **GPU Scheduler** in the web UI and set
each engine's home and candidate cards. Each engine's row holds:

| Field | Meaning |
|---|---|
| `home_gpu` | The card it prefers. |
| `candidate_gpus` | Every card it may be placed on. |
| `peak_mb` | Its worst-case VRAM use plus a margin. It is admitted onto a card only if this fits the card's free budget. |
| `evictable_by` | Engines allowed to evict it to make room. Empty means it is never evicted automatically. A busy, non-relocatable engine is waited for, never killed. |
| `relocatable` | Whether eviction moves it to another candidate card (even mid-request) instead of stopping it. |

The seeded peaks were measured on the author's cards and models. Keep them while
you get things running, then re-measure your own engines (`nvidia-smi` during a
worst-case job) before tightening them. Some are deliberately large so two
heavy engines never share a card.

Two engines (`spar3d` and `pixal3d`) are pinned to a single card by their own
deployment. Give each of them a `candidate_gpus` naming exactly the card its
service is pinned to, or its reservations will describe a card the render cannot
reach.

### Auth

The OpenAI-compatible proxy (`/v1/chat/completions`) is open for in-mesh callers.
Everything under `/api/v1/*` requires a bearer token: a web-login session or a
named API key created in the UI under **Settings → API Keys**, sent as
`Authorization: Bearer <key>`.

## Endpoints (a selection)

There are well over a hundred routes; these are the load-bearing ones.

| Endpoint | Description |
|----------|-------------|
| `GET /health` | Health of llama-server and each configured engine |
| `POST /v1/chat/completions` | OpenAI-compatible chat (in-mesh, unauthenticated) |
| `POST /api/v1/chat` | Raw chat |
| `POST /rank-releases` | Release classification for kobbler |
| `POST /api/v1/images/generate` | FLUX.2 text-to-image / edit |
| `POST /api/v1/krea2/generate` | Krea-2-Turbo text-to-image |
| `POST /api/v1/matting/remove` | Background removal |
| `POST /api/v1/depth` | Depth estimation |
| `POST /api/v1/3d/generate` | Image → textured, rigged GLB bundle |
| `POST /api/v1/tts/generate` | Text-to-speech |
| `POST /api/v1/stt/transcribe` | Speech-to-text |
| `POST /api/v1/music/{generate,transform,understand}` | Music |
| `POST /api/v1/avatar/generate` | Talking-head video |
| `POST /api/v1/vision/classify` | Zero-shot image scoring |
| `GET /api/v1/gpu/scheduler/*` | What is on each card and why |
| `/api/v1/auth/api-keys`, `/api/v1/generations`, `/api/v1/settings` | Keys, gallery, config |

**Exposure:** `/v1/chat/completions`, `/v1/models`, `/filter-releases`, `/validate-release` and
`/rank-releases` are unauthenticated, for kobbler and kobload to call in-mesh. Compose publishes
`KOBLEM_PORT` on every interface, so firewall it to those hosts or put it behind a reverse
proxy that only forwards `/api/`. Never expose it directly to the internet.

## CLI

`kob` wraps the API for scripts and the terminal — see
[`cli/README.md`](cli/README.md):

```sh
make cli                                   # → cli/target/debug/kob
kob config set --url http://host:8090 --token <key>
kob rmbg photo.png                         # → photo-cutout.png
kob image generate "a red apple" -o apple.png
```

## Development

```sh
make        # fmt, lint, check, build, test (api + web + cli)
make quick  # skip tests
make cli    # CLI only
make web    # web only
```

See [CONTRIBUTING.md](CONTRIBUTING.md) for what each target needs, the sqlx
offline cache, and the TDD expectation for bug fixes.

## Licence

AGPL-3.0. See [LICENSE](LICENSE).
