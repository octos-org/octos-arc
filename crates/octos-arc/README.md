# Native Octos ARC workflow

This Rust crate provides the `octos arc` command. It orchestrates the same
Octos executable's existing `chat` coding runtime, not a separate model loop.
The CLI build needs the `api` feature for real coding. Unix process supervision
and Node.js/npm are currently required for application validation.

## Implemented

- Parse official YAML/JSON requirement trees, reject duplicate IDs, unknown
  dependencies and cycles, and hash the normalized specification.
- Separate `create` (empty source tree) from `evolve` (existing frontend/backend
  and prior requirements). Compute added, changed, unchanged, removed and
  transitively affected IDs. Never automatically replace the old project.
- Verify the actual executing binary, full embedded source commit, clean-build
  marker and target against an explicit release manifest. No runtime fallback.
- Execute the real `octos chat --profile coding --json` entry with bounded
  iterations and a shared wall-clock budget. Use isolated runtime configuration
  and pass the model key only to the coding child, not validation commands.
- Run npm install/ci, frontend build, configured project tests and backend HTTP
  startup on an ephemeral port. Preserve logs and exit codes. Allow at most two
  repair turns; a failed coding process itself is not silently retried.
- Save requirement snapshots only after local validation. A failed evolution
  keeps the previous successful snapshot, but does not roll back changed source.

The requirement policy is compiled into the Rust binary. It tells the coding
agent to preserve existing behavior and not inspect hidden evaluator tests.
This is not a security boundary against adversarial generated code. Filesystem
checks reject source symlinks and metadata symlinks, and the process supervisor
terminates its own process group; deliberately detached processes may escape it.

## Prepare without a model

After building the full CLI:

```sh
cargo build --locked -p octos-cli --features api
target/debug/octos arc crates/octos-arc/tests/fixtures/smoke--counter.yaml \
  --output-dir /tmp/octos-counter-project --mode create --prepare-only
```

Preparation writes `.arc/octos/runs/<run-id>/requirements.json`,
`previous-requirements.json`, `delta.json` and `report.json`. It does not generate
an application or record completion. Keep the requirements file outside the
source output directory.

For an existing npm frontend/backend project, pass `--mode evolve` with the new
requirements. The command reads `.arc/octos/spec.json` from a prior successful
run, or accepts `--previous-requirements FILE` when importing a project. Import
does not prove the original project passed any tests.

## Release required before actual coding

The repository's `arc-runtime-lock.json` contains the verified Linux x86_64
release manifest. Actual coding verifies this immutable state before execution.
A release manifest must contain:

```json
{
  "schema_version": 1,
  "runtime_release": {
    "version": "v2.0.3-rc.11-arc.10",
    "source_commit": "<40-character build commit>",
    "target": "x86_64-unknown-linux-gnu",
    "binary_sha256": "<64-character executable SHA-256>",
    "archive_sha256": "<64-character archive SHA-256>",
    "url": "https://github.com/octos-org/octos-arc/releases/download/<immutable downstream tag>/<artifact>"
  }
}
```

Use a clean checkout and fresh build for release provenance. Create the final
manifest after building; do not embed the binary's own checksum in its source.
This crate verifies the current executable. `arc/main.py` downloads the pinned
archive and verifies its SHA-256 before extraction; the environment may override
the URL only explicitly for controlled tests.

Normal execution additionally needs `OPENAI_API_KEY`, explicit `--model`/`MODEL`
and `--base-url`/`OPENAI_BASE_URL`. Optional `--temperature` records an explicit
value; when absent the provider's default is not a claim of reproducibility.
Budget defaults to 1800 seconds, 80 iterations per turn and one repair turn.

## Evidence is not a leaderboard score

`report.json` always keeps `official_evaluation: "not_run"` and `score: null`.
`local_validation_complete` means install/build/configured local tests/startup
completed. It does not mean the official Playwright tests passed. Projects
without a test script are explicitly recorded as `not_configured`.

Only runner-level lifecycle events are written to `.arc/runner-events.jsonl`.
No per-requirement official pass events are synthesized from a model response.
The platform launcher, platform traceability contract, published binary and
real Counter/Evolution submissions remain follow-up work.

## Tests

```sh
cargo test --locked -p octos-arc
cargo clippy --locked -p octos-arc --all-targets -- -D warnings
```

The command-failure and HTTP-supervision tests use a clearly synthetic npm
fixture; these are not application builds or benchmark results. The HTTP test
launches a Rust fixture server in an owned child process and verifies cleanup.
Its subprocess-only fixture is intentionally ignored in the ordinary test list.
The public requirement fixtures were
retrieved on 2026-09-10 from:

- `https://arc-bench.com/api/requirements/smoke--counter?catalog=competition`
- `https://arc-bench.com/api/requirements/smoke-evolution--counter?catalog=competition`

Only `requirements_yaml` is retained. No evaluator implementation is included.
