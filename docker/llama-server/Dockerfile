# llama-server image for koblem — VANILLA upstream llama.cpp pinned to a SHA,
# plus one small patch series applied on top.
#
# Patch series (docker/llama-server/patches/*.patch):
#   0001  LLAMA_WORKER_ISOLATION parent/child split + /v1/admin/{drain,load,unload}
#         — a CUDA-free parent owns the HTTP port and fork+execv's a child that
#           holds the model and the GPU. `POST /v1/admin/unload` SIGKILLs the
#           child, tearing down the CUDA primary context and returning ALL VRAM
#           so the heavy engines can borrow the card (koblibs kob-gpu-gate).
#           Opt-in via LLAMA_WORKER_ISOLATION=1; the hot path is byte-identical
#           to upstream otherwise.
#
# BUMPING UPSTREAM: move LLAMA_REF to the new SHA, rebase the patch series onto
# it in your own llama.cpp checkout, fix rejects, then regenerate these files
# with `git format-patch <sha>..HEAD -o`. Keeping the series small is what keeps
# this cheap.
#
# CUDA_ARCHITECTURES below is set for Ampere (86) and Blackwell (120). Change it
# to match your own card — see the CUDA GPU compute-capability table. sm120
# needs CUDA >= 12.8.
FROM nvidia/cuda:13.4.3-devel-ubuntu24.04 AS build
ENV DEBIAN_FRONTEND=noninteractive

ARG LLAMA_REPO=https://github.com/ggml-org/llama.cpp.git
# Pinned upstream SHA — the patch series is git-format-patch output against it.
ARG LLAMA_REF=60eeeb6082c1126bb8bc72902c83123cd056811b

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
RUN cmake -B build -G Ninja \
    -DCMAKE_BUILD_TYPE=Release \
    -DGGML_CUDA=ON \
    -DCMAKE_CUDA_ARCHITECTURES="86;120" \
    -DLLAMA_CURL=ON \
    -DCMAKE_C_FLAGS="-Wno-error" \
    -DCMAKE_CXX_FLAGS="-Wno-error" \
    -DCMAKE_EXE_LINKER_FLAGS="-L/usr/local/cuda/lib64/stubs -lcuda -Wl,-rpath-link,/usr/local/cuda/lib64/stubs" \
    -DCMAKE_SHARED_LINKER_FLAGS="-L/usr/local/cuda/lib64/stubs -lcuda -Wl,-rpath-link,/usr/local/cuda/lib64/stubs" \
    && cmake --build build -j --target llama-server

FROM nvidia/cuda:13.4.3-runtime-ubuntu24.04
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
