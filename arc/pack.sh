#!/bin/sh
# Build the ARC platform submission bundle out of arc/.
#
# Platform contract: main.py and requirements.txt must sit at the zip root, and
# so must template/ (frontend + backend + README.md + template.yaml) -- the
# platform lays template/ out as the initial workspace and hands it to main.py
# via ARCBENCH_TEMPLATE_DIR. A bundle without a complete template/ was observed
# being rejected as "web template is incomplete" (2026-09-20).
#
# The bundle is staged in a temp dir and zipped from there, never from the
# worktree: what lands in the zip is exactly the explicit copy list below.
# Stale worktree artifacts cannot leak in, and dev files are excluded by
# construction rather than by zip patterns.
#
# Deliberately NOT shipped:
#   * public-tests/ and tasks/ -- platform test + task DATA. The runner mounts
#     the public specs at /workspace/tests, which main.py reads from there.
#     Shipping them would put task data in the submission.
#   * local-only instruments: path_split.py, postmortem.py, scoreboard.py,
#     metrics.py, integration/, action_errors.cjs, page_errors.ts,
#     grade-local.py, run-task-local.py, tests/. They analyse runs on a
#     developer machine and have no job inside the container.
#   * arcbench_agent_runtime/ -- a vendored copy of the `arcbench-runtime` pip
#     package that requirements.txt already declares (verified byte-identical).
#     The platform installs requirements.txt, so shipping it is dead weight
#     that can only drift from the real package.
#
# There is no static pipeline .dot to copy: the pipeline's nodes ARE this task's
# requirement nodes, so main.py emits <data_dir>/pipelines/arc_build.dot per run
# from the requirement tree plus prompts/pipeline-implement.md. The definition
# that ships is that generator + its prompt template + arc-policy.toml.
set -e
cd "$(dirname "$0")"
ROOT="$(pwd)"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
PKG="$STAGE/octos-arc-bundle"
mkdir -p "$PKG"

# Runtime: the glue, the stdio driver, the acceptance command, the policy.
cp main.py octos_stdio.py verify_node.py arc-policy.toml requirements.txt "$PKG/"
cp -R prompts "$PKG/prompts"
cp -R template "$PKG/template"

find "$PKG" -name __pycache__ -type d -exec rm -rf {} + 2>/dev/null || true
find "$PKG" \( -name '*.pyc' -o -name .DS_Store \) -delete

# Fail loudly rather than shipping a bundle that breaks the platform contract
# or silently re-introduces the local proxy / bespoke acceptance runner.
for required in main.py requirements.txt template; do
    [ -e "$PKG/$required" ] || { echo "pack: missing $required at the bundle root" >&2; exit 1; }
done
for banned in llm_proxy.py acceptance.py rust_engine.py verify_app.py path_split.py \
              postmortem.py scoreboard.py metrics.py integration action_errors.cjs \
              page_errors.ts public-tests tasks tests arcbench_agent_runtime; do
    [ ! -e "$PKG/$banned" ] || { echo "pack: $banned must not be in the bundle" >&2; exit 1; }
done
# 800 with the runtime fetch inlined; the container has no octos of its own and
# ours must come from the release, so the fetch (main.py OCTOS_RELEASE_URL)
# counts against the budget rather than being trimmed away. 1100 since the
# acceptance command bounds its own repairs and steps (verify_node.py) and the
# graph grew a seed node and a regression pass -- still glue, not a loop.
# 1200 once the timeouts, output caps and reasoning controls the profile
# runtime ignores in config.json moved onto the graph and the env (#230).
# 1300 for progressive delivery: verified states reach the output dir during
# the run, so a run killed from outside still ships working code.
LIMIT_PY=1300
PYLINES=$(find "$PKG" -name '*.py' -exec cat {} + | wc -l | tr -d ' ')
echo "打包内容：$(find "$PKG" -maxdepth 1 -mindepth 1 -printf '%f ' 2>/dev/null || ls "$PKG" | tr '\n' ' ')"
echo "包内 Python 行数：$PYLINES"
[ "$PYLINES" -le "$LIMIT_PY" ] || { echo "pack: bundle Python is $PYLINES lines (limit $LIMIT_PY)" >&2; exit 1; }

rm -f "$ROOT/../octos-arc-bundle.zip"
(cd "$PKG" && zip -qr "$ROOT/../octos-arc-bundle.zip" .)
echo "打包完成：$ROOT/../octos-arc-bundle.zip"
shasum -a 256 "$ROOT/../octos-arc-bundle.zip"
