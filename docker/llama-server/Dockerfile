# llama-server image for koblem — VANILLA upstream llama.cpp pinned to a SHA,
# plus one small patch series applied on top.
#
# 🔴 ONE image for every llama.cpp service: the chat LLM (gemma4) and the
# EmbeddingGemma-2 embedders (embed-text, embed-vision) all run
# koblem-llama:latest. There is one LLAMA_REF (below) and one patch series
# (patches/). Bump them together, once, for everything.
#
# Patch series (docker/llama-server/patches/*.patch):
#   0001  LLAMA_WORKER_ISOLATION parent/child split + lifecycle endpoints
#         (/v1/admin/{drain,load,unload}, POST /unload, /v1/gpu/status, idle
#         unload, WORKER_LOAD_ON_START). A CUDA-free parent owns the HTTP port
#         and fork+execv's a child that holds the model and the GPU; unloading
#         SIGKILLs the child, tearing down the CUDA primary context and returning
#         ALL VRAM so the heavy engines can borrow the card (koblibs
#         kob-gpu-gate). Opt-in via LLAMA_WORKER_ISOLATION=1; the hot path is
#         byte-identical to upstream otherwise.
#
# BUMPING UPSTREAM:
#   1. In a llama.cpp checkout: `git checkout --detach <new-sha>`,
#      `git am -3 docker/llama-server/patches/*.patch`, fix any conflict. Upstream
#      moves server internals often (main() -> server_run(), subprocess.h
#      signature changes), so expect the odd small fix.
#   2. Regenerate: `git format-patch <new-sha>..HEAD -o docker/llama-server/patches/`
#      (delete the old files first) and set LLAMA_REF below to <new-sha>.
#   3. `make docker-push-llama`, then on the engines host pull + recreate
#      llama-server, embed-text and embed-vision together.
#   4. Before shipping, A/B gemma4 against the old image with prod flags (decode
#      with/without MTP, prefill at depth, VRAM) and run an unload/load cycle.
#
# CUDA_ARCHITECTURES below is set for Ampere (86) and Blackwell (120). Change it
# to match your own card — see the CUDA GPU compute-capability table. sm120
# needs CUDA >= 12.8.
FROM nvidia/cuda:13.4.3-devel-ubuntu24.04 AS build
ENV DEBIAN_FRONTEND=noninteractive

ARG LLAMA_REPO=https://github.com/ggml-org/llama.cpp.git
# Pinned upstream SHA — the patch series is git-format-patch output against it.
ARG LLAMA_REF=bcb0152876e8f0b684ba70d3defcab6cdf35adb5

RUN apt-get update && apt-get install -y --no-install-recommends \
    git cmake build-essential ca-certificates libcurl4-openssl-dev ninja-build pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY patches/ /patches/

# CACHEBUST: pass --build-arg CACHEBUST=<unique> (the Makefile uses $(date +%s))
# to force a fresh clone + re-apply when the SHA or patches change.
ARG CACHEBUST=0
RUN echo "cachebust=${CACHEBUST}" \
    && git clone --filter=blob:none "${LLAMA_REPO}" llama.cpp \
    && git -C llama.cpp checkout --detach "${LLAMA_REF}" \
    && git -C llama.cpp config user.email build@kobbler.local \
    && git -C llama.cpp config user.name  "kobbler build" \
    && git -C llama.cpp am /patches/*.patch

WORKDIR /src/llama.cpp
# GGML_NATIVE would target the build machine's CPU: a build on an AVX-512 box
# then dies with SIGILL on the Zen 3 server. Pin the server's ISA instead.
RUN cmake -B build -G Ninja \
    -DCMAKE_BUILD_TYPE=Release \
    -DGGML_CUDA=ON \
    -DCMAKE_CUDA_ARCHITECTURES="86;120" \
    -DGGML_NATIVE=OFF -DGGML_AVX=ON -DGGML_AVX2=ON -DGGML_FMA=ON -DGGML_F16C=ON -DGGML_BMI2=ON \
    -DLLAMA_CURL=ON \
    -DCMAKE_C_FLAGS="-Wno-error" \
    -DCMAKE_CXX_FLAGS="-Wno-error" \
    -DCMAKE_EXE_LINKER_FLAGS="-L/usr/local/cuda/lib64/stubs -lcuda -Wl,-rpath-link,/usr/local/cuda/lib64/stubs" \
    -DCMAKE_SHARED_LINKER_FLAGS="-L/usr/local/cuda/lib64/stubs -lcuda -Wl,-rpath-link,/usr/local/cuda/lib64/stubs" \
    && cmake --build build -j --target llama-server

FROM nvidia/cuda:13.4.3-runtime-ubuntu24.04
LABEL org.opencontainers.image.source="https://github.com/Kobbler-Media/koblem"
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libcurl4 libgomp1 curl && rm -rf /var/lib/apt/lists/*
# build/bin/ holds the server binary + its vendored shared libs (libggml*.so,
# libllama.so, libmtmd.so) in modern llama.cpp.
COPY --from=build /src/llama.cpp/build/bin/ /app/
COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh
ENV LD_LIBRARY_PATH=/app:/usr/local/cuda/lib64
WORKDIR /app
EXPOSE 8080
ENTRYPOINT ["/entrypoint.sh"]
