# Contributing to koblem

PRs are welcome. koblem is a small, specialised project with no support
commitment — but if you hit a bug and fix it, we would rather have the patch than
not.

By contributing you agree your contribution is licensed under **AGPL-3.0**, the
same as the rest of the repository.

## Before you can build

Two prerequisites, in order of how much they will surprise you.

### 1. A sibling `koblibs` checkout

[koblibs](https://github.com/Kobbler-Media/koblibs) is a shared workspace — Rust crates
(`koblem-client`, `kob-providers`, `kob-gpu-gate`, `kob-audio-norm`,
`nats-jobs`) and a Svelte UI package (`@koblibs/ui`). It is not on crates.io or
npm yet, so both builds expect it checked out next to this repository:

```
parent/
  koblem/
  koblibs/
```

- Rust: `api/Cargo.toml` and `cli/Cargo.toml` declare plain path dependencies
  (`path = "../../koblibs/<crate>"`).
- Web: `web/package.json` declares `"@koblibs/ui": "file:../../koblibs"`.
- Docker: compose passes the checkout as the `koblibs` build context
  (`KOBLIBS_DIR`, default `../koblibs`).

### 2. Nothing else — the sqlx cache is committed

**You do not need a database to build koblem.** The API uses 131
`sqlx::query!`-family compile-time-checked macros, and the corresponding offline
query cache is committed at `api/.sqlx/` (131 entries, one per query). Every
build target sets `SQLX_OFFLINE=true`, so:

```sh
cd api && SQLX_OFFLINE=true cargo check
```

is all a clean clone needs — no PostgreSQL, no migrations, no `DATABASE_URL`.

You **do** need a database if you change a query or a migration, because the
cache then has to be regenerated:

```sh
cargo install sqlx-cli --no-default-features --features postgres
make sqlx-prepare    # brings up the throwaway test DB, migrates, regenerates api/.sqlx
```

`make sqlx-check` verifies the cache matches the code and is part of `make
check`, so a stale cache fails the build rather than silently drifting. Commit
the regenerated `api/.sqlx/` files with your change.

`api/migrations/` holds 49 sequential migrations and is the complete schema —
`make sqlx-migrate` applies them to the test database from scratch.

## Build and test

```sh
make          # fmt, lint, check, build, test — across api + web + cli
make quick    # same minus tests
make cli      # CLI only
make web      # web only
```

`make` is the gate: it runs `cargo clippy` with `-D warnings` across all targets,
`svelte-check`, and the full test suite. Run it before opening a PR.

What each group needs:

| Target group | Needs |
|---|---|
| `fmt-*`, `lint-api`, `check-api`, `build-api`, `build-cli` | cargo + sibling koblibs. No database, no Docker, no network beyond crates.io. |
| `check` (includes `sqlx-check`), `test-api`, `sqlx-*` | the above **plus** Docker and `cargo sqlx` (sqlx-cli). Spins up PostgreSQL on port 5435 via `docker-compose.test.yml`. |
| `*-web` | Node plus the sibling koblibs for `@koblibs/ui`. |
| `docker-push-*` | Publishing only. Not needed to contribute — see below. |

`make test-db-down` tears the test database back down.

## Bug fixes are TDD

If you are fixing a bug or changing logic, **write the failing test first**.

1. Write an integration-level test that reproduces the issue. Prefer exercising a
   real route over unit-testing an inner function.
2. Run it and watch it fail. If it passes, you have not reproduced the bug.
3. Fix it.
4. Run it again and watch it pass.

Cover **every** entry point into the buggy code, not just the one the report came
through. A fix applied at one call site while three others still hit the old path
is not a fix.

## Style

- **Zero comments by default.** Names and types are the documentation. Comment
  only non-obvious *why* — a constraint, a workaround, an invariant, or coupling
  across files that a reader cannot infer. If code needs a comment to be
  readable, refactor it instead.
- **No compatibility shims and no commented-out code.** Delete what you replace;
  that is what version control is for.
- Keep files small and single-purpose. Check whether something already does the
  job before adding a second implementation of it.
- Wire types shared between the server and the `kob` CLI live in
  `koblibs/koblem-client/src/types.rs`. Do not duplicate them — promote a
  route-local struct there when a client needs it. Docker builds read koblibs
  from the sibling checkout, so local edits are picked up without pushing.
- Machine-specific notes (your build box, your deploy host, your paths) go in the
  gitignored `CLAUDE.local.md` / `AGENTS.local.md`, never in `CLAUDE.md`.

## Known rough edges

Honest list of things that will bite a newcomer. Patches welcome.

- **`make docker-push-*` is for maintainers.** Those targets `docker push` to
  `ghcr.io/kobbler-media/*` and will fail without org push rights. `docker compose up`
  builds the images locally from this checkout and needs none of that.
- **The engine services are not in this repository.** Image, music, avatar, TTS,
  STT, vision, depth and video are separate GPU services that koblem calls over
  HTTP; you cannot run them from here.
- **The GPU scheduler has no card-registration path.** `gpu_cards` rows can only
  be created with raw SQL — there is no POST/PUT route, no UI form and no CLI
  command, and the `nvidia-smi` reconciler only reads free VRAM for cards already
  in the table. Worse, an unregistered card is a silent hang rather than an
  error: `GpuLock::acquire` gates on `has_engine()` (which is true, because the
  engine budget rows *are* seeded) rather than on whether any card exists, so
  placement finds no candidate and the request parks in a 2-second retry loop
  forever. `Placer::is_configured()` in kob-gpu-gate checks exactly the right
  predicate (cards non-empty **and** engines non-empty) but has no callers.
  `ranked_candidates()` also lacks the empty-candidate fallback its three sibling
  functions have, so a blank `home_gpu` with `[]` candidates is an unclearable
  `NoEnabledCard` rather than the "no preference" its doc comment promises. Two
  small, well-isolated fixes; see the README's "First run: register your GPUs".
