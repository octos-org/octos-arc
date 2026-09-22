# Octos ARC Runtime

Independent downstream repository for ARC coding experiments, established on 2026-09-10.
The original Octos and ARC adapter repositories remain unchanged. This repository preserves
their Git history; it is not a GitHub fork-network repository.

## Fixed starting point

- Octos repository: `octos-org/octos`
- Octos commit: `8558a3bff41f43838130808a1fa6cf0299e0bc40`
- Baseline tag: `arc-base-20260910`
- Cargo.lock SHA-256: `9761e8904a0949aa0cff189cdd75b6a5f0b00daf7c968a883cdf39786bbd3088`
- Inherited decision records and vendored docs: pinned by blob SHA in `decision-lock.json`
  (see [Decision records](#decision-records))
- `main` contains the Octos source and downstream provenance documents.
- `legacy` preserves the existing adapter at `b0999c95f7875c8d4ff3e58e733fb2c5abc8caf7`,
  plus its original locally submitted ZIP and provenance.
- `legacy-adapter-20260910` names the preserved legacy snapshot.

The upstream README describes upstream capabilities, not completed ARC modifications.
The initial source checkpoint contained no behavior changes. The second phase
adds the native `octos arc` workflow in `crates/octos-arc`, reusing the existing
Octos coding runtime. A verified Linux x86_64 downstream Release now exists;
there are still no new ARC submissions or new evaluation scores.

## Release and submission policy

`arc-runtime-lock.json` records the fixed source baseline and the verified Linux
x86_64 runtime Release. Model and submission fields remain null; the release URL
is immutable and both archive and binary SHA-256 values are required.

Before submitting a future modified runtime:

1. Record its full downstream source commit and exact build toolchain, target, and features.
2. Build with the committed Cargo.lock, then record each binary's SHA-256.
3. Publish a versioned artifact and record its URL and archive SHA-256.
4. Verify the downloaded bytes before execution; reject mismatches without fallback.
5. Record the actual model identifier, endpoint, generation settings, and execution budget.
   Credentials must come from the execution environment and must not enter Git.
6. Include the adapter source commit and this manifest in each submission's provenance.

Never use a `latest` release URL, a moving branch reference, or an unverified binary
from PATH as the selected ARC runtime. If the pinned artifact is unavailable, stop.
Changing a pin requires an explicit new version; do not silently replace an existing artifact.

## Vendored documentation snapshot

`book/` (21 files), `book-zh/` (20 files) and `docs/ARCHITECTURE.md` are **a pinned
snapshot of the upstream documentation at `8558a3bf`**, not a tracking copy. They
describe the runtime this repository actually builds. Upstream has since moved ahead in
five English chapters (`channels`, `cli-reference`, `configuration`, `memory-skills`,
`providers`) and three Chinese ones; those changes document post-pin behaviour — new
embedding defaults, a 17th provider, changed routing thresholds — and importing them
would make the book describe a runtime participants are not running.

Both books say so on their first page, and `decision-lock.json` pins every file, so the
snapshot can no longer drift silently in either direction.

### Intentional differences from upstream

Four files diverge from `8558a3bf` on purpose. Each carries a `delta.reason` in
`decision-lock.json`:

| File | Why |
| --- | --- |
| `book/src/cli-reference.md` | Backports the `--auth-token` process-list warning from upstream `e89e266fc` (octos#2381) |
| `book-zh/src/cli-reference.md` | The same warning in Chinese — upstream has not translated it |
| `book/src/introduction.md` | The pinned-snapshot banner |
| `book-zh/src/introduction.md` | The pinned-snapshot banner |

The `--auth-token` warning is operator-facing guidance that applies to the pinned build:
the flag, `OCTOS_AUTH_TOKEN`, and the config-file path all exist here. The rest of
upstream's fix is a code-side change that keeps tokens out of argv, and this baseline
does **not** carry it — so the warning matters more here, not less. The ARC harness
itself launches `octos serve --stdio --solo` with no token, so competition runs are
unaffected; the exposure is the dashboard/deploy path.

Prefer a narrow recorded delta to a resync. Backport a security correction; leave a
feature description alone.

## Decision records

The code pin above fixes what the baseline *builds*. `decision-lock.json` fixes what it
*inherited*: the 73 spec, ADR, UPCR, contract and protocol records carried over from
upstream, plus the 42 vendored documentation files above — each recorded by Git blob
SHA. Without it those records could be edited downstream, or superseded upstream, with
nothing noticing — the same exposure the Cargo.lock SHA-256 closes for dependencies.

`scripts/check-decision-lock.py` verifies the inventory:

```sh
python3 scripts/check-decision-lock.py              # offline; the CI gate
python3 scripts/check-decision-lock.py --upstream   # + confirm against upstream, report drift
python3 scripts/check-decision-lock.py --update     # re-pin after a deliberate change
```

The offline check fails if an inherited record was edited or deleted, if a new record
matching an inventoried group was added without being pinned, or if UPCR numbering grew
a collision or gap beyond the ones inherited from upstream. Numbers 022, 026 and 027 each
carry two documents and 013 is absent; all four come from upstream at the pin and are
recorded as known, so the gate reacts only to *new* numbering damage.

Diverging from upstream is allowed, but only on the record: an entry whose `arc_blob_sha`
differs from its `upstream_blob_sha` must carry a `delta.reason`. Prefer a narrow recorded
delta over a wholesale resync — resyncing a document to current upstream makes it describe
a runtime this baseline does not build.

Upstream moving past the pin is reported on request, never enforced and never polled.
Moving the baseline is a deliberate act; the lock exists so that it is a decision rather
than a surprise — not so that a robot files a weekly reminder about a pin that is
supposed to stay put.

## Repository isolation

The inherited upstream workflows were removed from `main` and are not enabled. Two
workflows run here:

- `.github/workflows/arc-linux-release.yml` — the standalone `workflow_dispatch`
  publisher. It builds and publishes only the ARC Linux bundle, and is the only
  workflow that produces an artifact.
- `.github/workflows/decision-lock.yml` — the decision-record pin. A single offline,
  read-only `verify` job. It publishes nothing, writes nothing, and reaches no network.
  There is no scheduled run: whether upstream has moved past the pin is a manual
  question, answered by `scripts/check-decision-lock.py --upstream`.

The `legacy` branch is historical reference only. Its old downloader and execution
behavior have intentionally not been modernized; it is not the new competition runtime.

## Current implementation and next phase

The Rust workflow now supports explicit create/evolve modes, requirement deltas,
fixed binary verification and honest local execution evidence. See
`crates/octos-arc/README.md` for limitations and validation commands. The B5
container smoke was executed on an Ubuntu Actions runner; official ARC scoring
and cloud submissions remain separate from these checks.

The Linux x86_64 bundle has been built and published with verified checksums;
the platform launcher now points at that immutable Release URL. Next, run the
official ARC Counter and Evolution tasks with the prior generated project as
input. No benchmark score is implied by the crate's unit tests. Do not fabricate
test results.
