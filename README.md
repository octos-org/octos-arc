<div align="center">

<pre>
 ██████╗  ██████╗████████╗ ██████╗ ███████╗
██╔═══██╗██╔════╝╚══██╔══╝██╔═══██╗██╔════╝
██║   ██║██║        ██║   ██║   ██║███████╗
██║   ██║██║        ██║   ██║   ██║╚════██║
╚██████╔╝╚██████╗   ██║   ╚██████╔╝███████║
 ╚═════╝  ╚═════╝   ╚═╝    ╚═════╝ ╚══════╝
</pre>

</div>

# Octos for ARC-Bench

**The competition build of [Octos](https://github.com/octos-org/octos) for ARC-Bench:
a pinned kernel plus the whole compete workflow — platform adapter, public acceptance
tests, local solve/grade, packaging — in one repository.**

Participating in ARC-Bench needs this repository and nothing else.

[Three steps to participate](#three-steps-to-participate) · [What is in here](#what-is-in-here) ·
[Pinned base](#pinned-base) · [Kernel documentation](#kernel-documentation) · [中文](README-zh.md)

> **Looking for a coding agent to use day to day?** This is not it — this is a
> benchmark entry. Install [Octoscode](https://github.com/octos-org/octoscode) instead,
> or embed the upstream [`octos-org/octos`](https://github.com/octos-org/octos) kernel.

---

## Three steps to participate

The commands below are the short path. The full workflow — every orchestrator switch,
evolution mode, model routing, and the per-change before/after numbers — lives in
**[`arc/README.md`](arc/README.md)**, which is the document to read next.

### 1. Set up, once

```sh
pip install -r arc/requirements.txt     # pyyaml, arcbench-runtime
export ARCBENCH_API_KEY=ak_...          # from your arc-bench.com account page

# Get an octos binary: the released one, or your own build
export OCTOS_BIN=/path/to/octos         # defaults to ../target/release/octos
cargo build --release -p octos-cli --no-default-features --features api
```

### 2. Solve a task locally, then grade it

Roughly five minutes and well under a cent for the smoke task:

```sh
python3 arc/run-task-local.py arc/tasks/smoke--counter --name try1
python3 arc/grade-local.py arc/arc-output/try1 smoke--counter   # platform Playwright tests
python3 arc/metrics.py arc/arc-output/try1                      # rounds / tokens / cost / time
```

Change one thing — a prompt in `arc/main.py`, a launch argument in `arc/octos_stdio.py`,
or the kernel itself in `crates/` — then run both commands again and compare the numbers.

### 3. Pack and submit

```sh
sh arc/pack.sh                          # produces octos-arc-bundle.zip
```

Upload it on the competition page at arc-bench.com under **New submission**.

> **If you changed `crates/`, read this.** The platform downloads its own Octos at run
> time from `OCTOS_RELEASE_URL` in `arc/main.py`. A kernel change only reaches the
> platform after you build Linux x86-64, publish it as a release on this repository,
> point `OCTOS_RELEASE_URL` at that immutable URL, and re-run `pack.sh`. Otherwise the
> platform runs the stock binary and your change has no effect.

## What is in here

| Path | What it is |
| --- | --- |
| [`arc/`](arc/README.md) | **Start here.** Adapter, orchestrator, public acceptance tests, local solve/grade/pack scripts |
| `arc/tasks/` | Offline copies of the task requirement files (smoke, ticket-booking, six web tasks) |
| `arc/public-tests/` | The platform's published Playwright acceptance tests |
| `crates/` | The Octos kernel source, forked from the pinned upstream base |
| `book/`, `docs/` | Kernel documentation, vendored at the pinned base — see [Kernel documentation](#kernel-documentation) |
| `arc-runtime-lock.json` | The release/submission pin: source commit, target, and required SHA-256 values |
| [`ARC_BASELINE.md`](ARC_BASELINE.md) | What is pinned, and the rules for changing a pin |

Branches: **`main`** carries the modified kernel plus `arc/`. **`adapter`** carries only
the adapter package, for running it against stock Octos or a different agent.

## Pinned base

This repository is not tracking upstream `main`. It is pinned:

| | |
| --- | --- |
| Upstream | [`octos-org/octos`](https://github.com/octos-org/octos) |
| Commit | [`8558a3bf`](https://github.com/octos-org/octos/commit/8558a3bff41f43838130808a1fa6cf0299e0bc40) |
| Tag | `arc-base-20260910` |

The pin is what makes a submission reproducible, so treat a moving reference as a bug.
Never select a `latest` release URL, a branch reference, or an unverified binary from
`PATH` as the competition runtime. [`ARC_BASELINE.md`](ARC_BASELINE.md) states the full
policy and the steps required before submitting a modified runtime.

## Kernel documentation

`book/` and `docs/` are vendored copies taken at the pinned base, so they describe the
kernel this repository actually builds — not current upstream `main`.

- [Architecture](docs/ARCHITECTURE.md) · [CLI reference](book/src/cli-reference.md) ·
  [Configuration](book/src/configuration.md) · [Providers](book/src/providers.md)
- [OUP protocol specification](api/OCTOS_UI_PROTOCOL_V1_SPEC_2026-04-24.md)
- [`CLAUDE.md`](CLAUDE.md) — kernel architecture orientation for agents working in this tree

For upstream documentation that has moved past the pin, read the
[published site](https://octos-org.github.io/octos/) or the
[upstream README](https://github.com/octos-org/octos/blob/8558a3bff41f43838130808a1fa6cf0299e0bc40/README.md)
at the pinned commit. Where the two disagree, the vendored copy describes what runs here.

## License

Apache-2.0. See [LICENSE](LICENSE).
