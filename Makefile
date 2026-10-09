.PHONY: all quick web cli fmt fmt-api fmt-web fmt-cli lint lint-api lint-web lint-cli check check-api check-web check-cli sqlx-check sqlx-prepare sqlx-migrate build build-api build-web build-cli test test-api test-cli test-web test-db-up test-db-down docker-push docker-push-koblem docker-push-llama docker-push-web

all: fmt lint check build test

# Quick pass — skips slow tests
quick: fmt lint check build

# Web-only checks — for frontend-only changes
web: fmt-web lint-web check-web build-web

# CLI-only checks — for kob CLI changes
cli: fmt-cli lint-cli build-cli

fmt: fmt-api fmt-web fmt-cli
lint: lint-api lint-web lint-cli
check: check-api check-web check-cli sqlx-check
build: build-api build-web build-cli
test: test-api test-cli test-web

fmt-api:
	cargo fmt --manifest-path api/Cargo.toml

lint-api:
	SQLX_OFFLINE=true RUSTFLAGS="-D warnings" cargo clippy --manifest-path api/Cargo.toml --all-targets -- -D warnings

check-api:
	SQLX_OFFLINE=true cargo check --manifest-path api/Cargo.toml

build-api:
	SQLX_OFFLINE=true RUSTFLAGS="-D warnings" cargo build --manifest-path api/Cargo.toml

# Verify api/.sqlx cache matches code. If stale, run: make sqlx-prepare
# `--all` so koblibs' nats-jobs (a dep, not a workspace member) is force-recompiled and
# re-emits its job_status queries. Without it those 9 entries appear or vanish depending
# only on whether cargo happened to rebuild the dep.
sqlx-check: test-db-up sqlx-migrate
	cd api && DATABASE_URL=postgres://koblem:koblem@localhost:5435/koblem_test cargo sqlx prepare --all --check -- --all-targets

# Regenerate api/.sqlx cache from current code + DB schema
sqlx-prepare: test-db-up sqlx-migrate
	cd api && DATABASE_URL=postgres://koblem:koblem@localhost:5435/koblem_test cargo sqlx prepare --all -- --all-targets

# Apply migrations to the test database
sqlx-migrate: test-db-up
	DATABASE_URL=postgres://koblem:koblem@localhost:5435/koblem_test cargo sqlx migrate run --source api/migrations

fmt-cli:
	cargo fmt --manifest-path cli/Cargo.toml

lint-cli:
	RUSTFLAGS="-D warnings" cargo clippy --manifest-path cli/Cargo.toml --all-targets -- -D warnings

check-cli:
	cargo check --manifest-path cli/Cargo.toml

build-cli:
	RUSTFLAGS="-D warnings" cargo build --manifest-path cli/Cargo.toml

fmt-web:
	npm --prefix web run format

lint-web:
	npm --prefix web run lint

check-web:
	npm --prefix web run check

build-web:
	cd web && npm install && npm run build

test-db-up:
	docker compose -f docker-compose.test.yml up -d --wait

test-db-down:
	docker compose -f docker-compose.test.yml down

test-api: test-db-up sqlx-migrate
	SQLX_OFFLINE=true \
	DATABASE_URL=postgres://koblem:koblem@localhost:5435/koblem_test \
	cargo test --manifest-path api/Cargo.toml -- --test-threads=4

test-cli:
	cargo test --manifest-path cli/Cargo.toml

test-web:
	npm --prefix web test

# docker-push-llama omitted from default: building llama.cpp (vanilla upstream
# @ pinned SHA + kobbler patch series, CUDA 13.3) takes ~15 minutes and the
# image rarely changes. Run `make docker-push-llama` ad-hoc.
REGISTRY ?= ghcr.io/kobbler-media

docker-push: docker-push-koblem docker-push-web

docker-push-koblem:
	cd api && cargo update -p koblem-client -p kob-providers
	docker build --build-context koblibs=../koblibs -t $(REGISTRY)/koblem:latest .
	docker push $(REGISTRY)/koblem:latest

docker-push-llama:
	docker build --build-arg CACHEBUST=$$(date +%s) -t $(REGISTRY)/koblem-llama:latest docker/llama-server/
	docker push $(REGISTRY)/koblem-llama:latest

docker-push-web:
	docker build --build-context koblibs=../koblibs --target prod -t $(REGISTRY)/koblem-web:latest web/
	docker push $(REGISTRY)/koblem-web:latest
