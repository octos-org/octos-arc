#!/bin/sh
# Build the ARC platform submission bundle out of arc/.
#
# Platform contract: main.py and requirements.txt must sit at the zip root,
# and so must template/ (frontend + backend + README.md + template.yaml) --
# the platform lays template/ out as the initial workspace and hands it to
# main.py via ARCBENCH_TEMPLATE_DIR. The codegen prompts already assume that
# workspace ("Initial package.json files already exist. Preserve existing
# architecture"), and postflight warns that the runner rejects a workspace
# that still has no frontend/ + backend/. A bundle without a complete
# template/ was observed being rejected as "web template is incomplete"
# (2026-09-20); shipping one makes the initial workspace deterministic
# instead of whatever the runner falls back to.
#
# The bundle is staged in a temp dir and zipped from there, never from the
# worktree: what lands in the zip is exactly the explicit copy list below.
# Stale worktree artifacts cannot leak in, and dev files (tests, offline
# task copies, local runners) are excluded by construction rather than by
# zip patterns.
set -e
# Optional first argument is deployment policy, not generated app content.
ROUTES=""
if [ "$#" -gt 0 ]; then
    ROUTES="$(python3 -c 'import os,sys; print(os.path.abspath(sys.argv[1]))' "$1")"
fi
cd "$(dirname "$0")"
ROOT="$(pwd)"
if [ -n "$ROUTES" ]; then
    python3 -c 'import sys; from pathlib import Path; from llm_proxy import model_routes; model_routes(Path(sys.argv[1]).read_text())' "$ROUTES"
fi

# public-tests/ is deliberately NOT shipped. The runner mounts the public specs at
# /workspace/tests, which locate_acceptance_tests() prefers anyway: across 13 completed
# cloud runs (smoke, smoke-evolution, ticket-booking, arc-bench-web, 2026-09-17) every
# single one logged "[tests] N spec files at /workspace/tests" and none ever reached the
# bundled copy. Shipping it would only put a per-task, task-title-keyed set of spec files
# in the submission -- dead weight in the cloud, and indistinguishable from pre-loading
# task data for anyone auditing the bundle. Local runs are unaffected: run-task-local.py
# resolves BUNDLE_DIR to arc/ and reads arc/public-tests straight from the repo.

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
PKG="$STAGE/octos-arc-bundle"
mkdir -p "$PKG"

cp main.py rust_engine.py arc-policy.toml octos_stdio.py requirement_order.py \
      acceptance.py verify_app.py action_errors.cjs page_errors.ts guard.py \
      llm_proxy.py codegen.py requirements.txt "$PKG/"
cp -R prompts hooks arcbench_agent_runtime "$PKG/"
cp -R template "$PKG/template"
if [ -n "$ROUTES" ]; then
    cp "$ROUTES" "$PKG/model-routes.json"
fi

# Clean after all python steps, never before: the llm_proxy import above
# (and any future generation step) re-creates __pycache__ after it runs, so
# a pre-zip sweep here is the only one that holds. Excluding at zip time
# alone leaves the stale-worktree race open.
find "$PKG" -name __pycache__ -type d -exec rm -rf {} + 2>/dev/null || true
find "$PKG" \( -name '*.pyc' -o -name .DS_Store \) -delete

rm -f "$ROOT/../octos-arc-bundle.zip"
(cd "$PKG" && zip -qr "$ROOT/../octos-arc-bundle.zip" .)
echo "打包完成：$ROOT/../octos-arc-bundle.zip"
shasum -a 256 "$ROOT/../octos-arc-bundle.zip"
