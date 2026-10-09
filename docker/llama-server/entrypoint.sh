#!/bin/sh
set -e

# Default model = gemma-4-12b-it (unified vision + AUDIO mmproj).
#
# MODELS ARE NOT AUTO-DOWNLOADED. The gemma-4 main quant, the F16 unified
# mmproj and especially the grafted MTP draft have no public download URL — they
# are pre-placed GGUFs. To run this image: either bind-mount a directory
# containing those files at /models (compose does this from KOBLEM_MODELS_DIR),
# or set the *_URL env vars below to something curl can fetch. A missing file
# with no URL is a hard error — we will not start with a broken model path.
#
# Substitute your own GGUFs freely via LLAMA_MODEL / LLAMA_MMPROJ /
# LLAMA_DRAFT_MODEL; drop --model-draft and --spec-type if you have no draft.
#
# gemma4 flag notes: f16 KV is fastest at all depths (a quantised KV collapses
# MTP acceptance); ub768/imt768 is image-safe (non-causal vision attention needs
# n_ubatch >= the image chunk); --no-mmproj-offload keeps the featherweight
# unified projector in host RAM; MTP = a separate gemma4-assistant draft
# (--model-draft) plus --spec-type draft-mtp. Thinking is controlled per-request
# via chat_template_kwargs.enable_thinking against the GGUF-baked template.

MODEL_DIR="/models"

TEXT_MODEL="$MODEL_DIR/${LLAMA_MODEL:-gemma-4-12B-it-qat-UD-Q4_K_XL.gguf}"
MMPROJ_MODEL="$MODEL_DIR/${LLAMA_MMPROJ:-gemma-4-12b-it-mmproj-F16.gguf}"
DRAFT_MODEL="$MODEL_DIR/${LLAMA_DRAFT_MODEL:-mtp-gemma-4-12B-it.gguf}"

# Optional download sources — empty by default. Set these to curl-able URLs if
# you want the container to fetch its weights instead of finding them on /models.
TEXT_URL="${TEXT_URL:-}"
MMPROJ_URL="${MMPROJ_URL:-}"
DRAFT_URL="${DRAFT_URL:-}"

ensure_model() {
    file="$1"
    url="$2"
    desc="$3"

    if [ -f "$file" ]; then
        echo "$desc found at $file"
        return 0
    fi

    if [ -z "$url" ]; then
        echo "ERROR: $desc missing at $file and no download URL set." >&2
        echo "       Bind-mount a /models dir containing this GGUF, or set its" >&2
        echo "       *_URL env var (TEXT_URL / MMPROJ_URL / DRAFT_URL)." >&2
        exit 1
    fi

    echo "$desc not found at $file, downloading..."
    mkdir -p "$MODEL_DIR"
    if [ -n "$HF_TOKEN" ]; then
        curl -L --fail --progress-bar -H "Authorization: Bearer $HF_TOKEN" -o "$file" "$url"
    else
        curl -L --fail --progress-bar -o "$file" "$url"
    fi
    echo "Download complete: $desc"
}

ensure_model "$TEXT_MODEL"   "$TEXT_URL"   "gemma-4-12B main quant"
ensure_model "$MMPROJ_MODEL" "$MMPROJ_URL" "gemma-4-12B unified vision+audio mmproj (F16)"
ensure_model "$DRAFT_MODEL"  "$DRAFT_URL"  "gemma-4-12B MTP draft"

# Default args when nothing was passed via `command:` / `docker run … <args>`.
# Thinking is left to the GGUF-baked template plus the per-request
# chat_template_kwargs.enable_thinking flag koblem sends.
if [ "$#" -eq 0 ]; then
    set -- \
        -m "$TEXT_MODEL" \
        --mmproj "$MMPROJ_MODEL" \
        --model-draft "$DRAFT_MODEL" \
        -ngl 99 \
        --ctx-size "${CONTEXT_SIZE:-73728}" \
        --parallel 1 \
        --ctx-checkpoints 4 --checkpoint-min-step 1024 \
        --ubatch-size "${LLAMA_UBATCH:-768}" \
        --image-max-tokens "${LLAMA_IMAGE_MAX_TOKENS:-768}" \
        --no-mmproj-offload \
        -fa 1 \
        --jinja \
        --temp 1.0 --top-p 0.95 --top-k 64 \
        --spec-type "${LLAMA_SPEC_TYPE:-draft-mtp}" \
        --spec-draft-n-max "${LLAMA_DRAFT_N_MAX:-4}" \
        --host 0.0.0.0 --port 8080
fi

exec /app/llama-server "$@"
