FROM rust:1.97.1-slim-trixie AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# koblibs is a sibling checkout referenced by path in api/Cargo.toml; compose and
# the Makefile pass it as the `koblibs` build context. It must land at /koblibs
# so that `../../koblibs/...` resolves from /build/api.
COPY --from=koblibs . /koblibs

WORKDIR /build
COPY api/Cargo.toml api/Cargo.lock* api/
COPY api/src api/src
COPY api/migrations api/migrations
# Offline sqlx query cache — avoids needing a live DB during image build.
# Regenerate with `make sqlx-prepare` when queries/migrations change.
COPY api/.sqlx api/.sqlx
# Stress-test fixtures embedded via include_str! in testbed_fixtures.rs —
# without these the testbed module fails to compile.
COPY api/scenarios api/scenarios
# Vendored CMU pronunciation dict embedded via include_str! in rhyme.rs —
# without it the crate fails to compile.
COPY api/resources api/resources
# MiniMax's H3 prompt guides embedded via include_str! in h3_prompting.rs —
# without them the crate fails to compile.
COPY api/assets api/assets

ENV SQLX_OFFLINE=true

# Cache mounts persist across builds (BuildKit). Target cache is ephemeral to
# this RUN, so copy the binary out before the step ends.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/build/api/target \
    cd api && cargo build --release && \
    cp /build/api/target/release/koblem /build/koblem-binary

FROM debian:trixie-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates curl ffmpeg \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/koblem-binary /usr/local/bin/koblem

ENV LLAMA_SERVER_URL=http://llama-server:8080
ENV BIND_ADDRESS=0.0.0.0:8090
ENV CONTEXT_SIZE=131072

EXPOSE 8090

CMD ["koblem"]
