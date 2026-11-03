# GPU engines

koblem runs llama-server itself and calls every other engine over HTTP. Each one is
optional: leave its URL blank and that feature is hidden. This page lists, for each URL,
where the server comes from and where the weights come from, grouped by how much work it
takes to stand one up.

The servers are mostly ggml ports kept as forks of public projects. koblem talks to each
fork's own HTTP API, so build the fork at the listed commit rather than its upstream.

## Ready to build

Dockerfiles for these three ship in [kobbler](https://github.com/Kobbler-Media/kobbler),
in `docker-compose.gpu.yml`, pinned to the commits below:

```sh
cd ../kobbler
docker compose -f docker-compose.yml -f docker-compose.gpu.yml up -d embed-vision embed-text stt-parakeet-cpp tts-breeze
```

| koblem URL | Engine | Source | Weights |
|---|---|---|---|
| `VISION_SERVICE_URL=http://embed-vision:8080` | EmbeddingGemma-2 image (and fallback text) embeddings, GPU-gated as `vision` | llama-server, stock `ghcr.io/ggml-org/llama.cpp:server-cuda`, or koblem's `koblem-llama-tip` (`make docker-push-llama-tip`: upstream `8fc0376` + `docker/llama-server/patches-tip/`, which adds worker isolation: `X-Worker-Gpu`, `POST /unload`, `/v1/gpu/status`, idle unload) | `google/embeddinggemma-2` Q8_0 GGUF + mmproj Q8_0. Run with `--embeddings --mmproj … --image-max-tokens 70 -b 1024 -ub 1024 -c 8192 --parallel 4` |
| `TEXT_EMBED_SERVICE_URL=http://embed-text:8080` | EmbeddingGemma-2 text embeddings, always on (gate line `text_embed`, a standing reservation; requests never wait on the gate) | same image, no mmproj; `WORKER_LOAD_ON_START=1` so it loads at boot | the same Q8_0 GGUF. `--embeddings --parallel 1 -c 1024 -b 1024 -ub 1024`; set `TEXT_EMBED_MAX_TOKENS` to the `-ub`. Measured 411 MiB loaded / 513 MiB peak on a 5060 Ti (gate row 600); runs on CPU too (`--device none`, ~20 ms a query) |
| `STT_SERVICE_URL=http://stt-parakeet-cpp:8023` | Parakeet-TDT speech-to-text | [hbd-CrispASR](https://github.com/dbrain/hbd-CrispASR) @ `457252dd` (fork of [CrispASR](https://github.com/CrispStrobe/CrispASR)) | fetched on first request from [cstr/parakeet-tdt-0.6b-v3-GGUF](https://huggingface.co/cstr/parakeet-tdt-0.6b-v3-GGUF) |
| `TTS_BREEZE_SERVICE_URL=http://tts-breeze:8203` | BreezeTTS 2 | [hbd-qwen3-tts.cpp](https://github.com/dbrain/hbd-qwen3-tts.cpp) @ `d022511` | convert [BreezeBlue/Breeze-TTS-2](https://huggingface.co/BreezeBlue/Breeze-TTS-2) yourself, see kobbler's `docker/tts-breeze/README.md`. Non-commercial licence. |

Both embedding servers share one vector space (768-d, L2-normalised); scores koblem returns are
cosine similarity. koblem applies EmbeddingGemma-2's task prefixes itself
(`/v1/text_embeddings` `task`), so callers send plain text. Text over `TEXT_EMBED_MAX_TOKENS` is a
413 unless the request sets `chunk`. For calling these from other services, see [embeddings.md](embeddings.md).

Those hostnames only resolve if koblem-api shares a docker network with kobbler's stack.
Otherwise use the published host ports.

## Public source and weights, no Dockerfile here

Build the named CMake target from the fork, then run it with the weights directory
mounted.

| koblem URL | Engine | Source | Server target | Weights |
|---|---|---|---|---|
| `MATTING_SERVICE_URL`, `DEPTH_SERVICE_URL` | Background removal, Depth Anything | [hbd-vision.cpp](https://github.com/dbrain/hbd-vision.cpp) @ `324c84d` (fork of [vision.cpp](https://github.com/Acly/vision.cpp)) | `vision-server`, port 8898 | [gobeldan/RMBG-2.0-GGUF](https://huggingface.co/gobeldan/RMBG-2.0-GGUF), [Acly/Depth-Anything-V2-GGUF](https://huggingface.co/Acly/Depth-Anything-V2-GGUF) |
| `ACE_SERVER_URL` | ACE-Step 1.5 music | [hbd-acestep.cpp](https://github.com/dbrain/hbd-acestep.cpp) @ `de08e84` (fork of [acestep.cpp](https://github.com/ServeurpersoCom/acestep.cpp)) | `ace-server --models <dir>`, port 8088 | [Serveurperso/ACE-Step-1.5-GGUF](https://huggingface.co/Serveurperso/ACE-Step-1.5-GGUF), or the fork's `models.sh` |
| `SONGGEN_SERVER_URL`, `SA3_SERVER_URL` | Song generation, Stable Audio 3 | same fork and commit | `songgen-server` (8097), `sa3-server` (8104) | see the fork's README |

## Needs weights you convert yourself

These are all targets of [hbd-longcat-avatar.cpp](https://github.com/dbrain/hbd-longcat-avatar.cpp),
a fork of [stable-diffusion.cpp](https://github.com/leejet/stable-diffusion.cpp), built with
`--target sd-server`. The checkpoints exist upstream, but the GGUF files koblem's defaults
expect are local conversions made with the fork's convert mode, and none are published.
Expect real work here.

| koblem URL | Engine |
|---|---|
| `FLUX2_SERVICE_URL` | FLUX.2 Klein text-to-image and edit |
| `KREA2_SERVICE_URL` | Krea 2 Turbo text-to-image |
| `AVATAR_SERVER_URL` | LongCat-Video-Avatar talking heads |
| `LTX_2B_SERVICE_URL` | LTX-Video 2B |
| `WAN_VACE_SERVICE_URL` | Wan 2.2 VACE |
| `PIXAL3D_SERVICE_URL` | Image to rigged 3D (`longcat-rig`) |

## Other stacks

| koblem URL | What it expects |
|---|---|
| `LTX25_SERVICE_URL`, `MUSIC3_SERVICE_URL`, `UPSCALE_SERVICE_URL` | An HTTP shim in front of [ComfyUI](https://github.com/comfyanonymous/ComfyUI). The shim is not published. |
| `MINIMAX_H3_SERVICE_URL` | MiniMax H3 through [Wan2GP](https://github.com/deepbeepmeep/Wan2GP) |
| `SPAR3D_SERVICE_URL` | [SPAR3D](https://github.com/Stability-AI/stable-point-aware-3d) plus [RigAnything](https://github.com/Isabella98Liu/RigAnything) behind an HTTP worker. The worker is not published. |
| `BEEFY_LLM_SERVICE_URL`, `DIRECTOR_LLM_SERVICE_URL` | Any OpenAI-compatible chat endpoint, for example a second llama-server |

Whatever you run, register your GPUs and assign each engine a card on the **GPU
Scheduler** page (see the README's first-run section), or requests to it will wait
forever.
