#!/usr/bin/env bash
#
# Run SWE-bench (Harbor) against the Stella coding CLI.
#
# Usage:
#   STELLA_MODEL=zai/glm-5.2 ./run.sh
#   TASK_IDS="django__django-11099" N_CONCURRENT=1 ./run.sh
#   STELLA_BUDGET=10.0 ./run.sh
#
# Prereqs: docker running; provider API key exported (ZAI_API_KEY, etc.)
# Requires: stella_harbor package installed (see below)

set -euo pipefail

cd "$(dirname "$0")"
REPO_ROOT="$(cd ../.. && pwd)"
ADAPTER_DIR="$(pwd)"

# Ensure Stella is built
if [ ! -f "$REPO_ROOT/target/release/stella" ]; then
    echo "Building Stella..."
    cd "$REPO_ROOT"
    cargo build --release -p stella-cli
    cd "$ADAPTER_DIR"
fi

# Configuration
AGENT="${AGENT:-stella}"
# Default model for this internal at-scale runner. Override with STELLA_MODEL
# or Harbor's -m for any provider (e.g. STELLA_MODEL=anthropic/claude-fable-5).
MODEL_SLUG="${STELLA_MODEL:-zai/glm-5.2}"
DATASET="${DATASET:-swe-bench/swe-bench-verified}"
N_CONCURRENT="${N_CONCURRENT:-4}"
N_ATTEMPTS="${N_ATTEMPTS:-1}"
JOBS_DIR="${JOBS_DIR:-./results-stella}"

# Export for the adapter to pick up
export STELLA_MODEL="$MODEL_SLUG"
export STELLA_BUDGET="${STELLA_BUDGET:-5.0}"
export STELLA_BINARY="$REPO_ROOT/target/release/stella"
# Only pin a base URL if the caller explicitly set one. Do NOT force a
# provider-specific endpoint by default — that silently routes e.g. Anthropic
# traffic to a different vendor. For Z.ai's coding plan, either export
# STELLA_BASE_URL=https://api.z.ai/api/coding/paas/v4 yourself or set
# ZAI_GLM_CODING_PLAN=1 (Stella then resolves the coding endpoint itself).
if [ -n "${STELLA_BASE_URL:-}" ]; then
    export STELLA_BASE_URL
fi

# Ensure adapter is installed and importable
echo "Setting up Stella Harbor adapter..."
python3 -m pip install -e "$ADAPTER_DIR" --user --break-system-packages --quiet

# Get user site-packages for PYTHONPATH
USER_SITE=$(python3 -c "import site; print(site.USER_SITE)")
echo "User site-packages: $USER_SITE"

# Verify import
python3 -c "from stella_harbor import StellaAgent; print('✓ Stella agent importable')" 2>/dev/null || {
    echo "Error: stella_harbor package not importable"
    exit 1
}

# Build task ID args
TASK_ID_ARGS=()
if [ -n "${TASK_IDS:-}" ]; then
    for t in $TASK_IDS; do
        TASK_ID_ARGS+=(--include-task-name "*$t")
    done
fi

# Locate the internal Harbor SWE-bench runner (oxagen-platform). This wrapper
# targets Oxagen's private at-scale orchestration; it is NOT the public entry
# point. Contributors without that repo should use one of the public paths.
OXAGEN_PLATFORM="${OXAGEN_PLATFORM:-$HOME/Workspaces/oxagen-platform}"
HARBOR_RUNNER="$OXAGEN_PLATFORM/bench/swe-bench/run.sh"

if [ ! -f "$HARBOR_RUNNER" ]; then
    cat >&2 <<'EOF'
This script wraps Oxagen's internal at-scale Harbor runner, which isn't part of
the public repo. Two public paths do not need it:

  • Direct Harbor (containerized, per your own Harbor install):
        harbor run --agent stella --dataset swe-bench/swe-bench-verified -m <model>
    (this adapter registers the `stella` agent; see README.md here)

  • The standalone, no-Harbor harness in the repo root:
        python3 ../run_swebench.py --limit 25 --model anthropic/claude-fable-5 --budget 2.0

To use THIS wrapper anyway, set OXAGEN_PLATFORM to a checkout that provides
bench/swe-bench/run.sh.
EOF
    exit 1
fi

echo "=== Stella SWE-bench run ==="
echo "Agent: $AGENT"
echo "Model: $MODEL_SLUG"
echo "Dataset: $DATASET"
echo "Concurrent: $N_CONCURRENT"
echo "Budget: \$${STELLA_BUDGET} per task"
echo "Jobs dir: $JOBS_DIR"
echo ""

# Build Harbor args (pass stella-specific args via env vars)
HARBOR_ARGS=(
    --dataset "$DATASET"
    -m "$MODEL_SLUG"
    --n-concurrent "$N_CONCURRENT"
    --n-attempts "$N_ATTEMPTS"
    --jobs-dir "$JOBS_DIR"
)

if [ ${#TASK_ID_ARGS[@]} -gt 0 ]; then
    HARBOR_ARGS+=("${TASK_ID_ARGS[@]}")
fi

if [ -n "${HARBOR_EXTRA:-}" ]; then
    HARBOR_ARGS+=($HARBOR_EXTRA)
fi

# Run Harbor with stella agent
# Set PYTHONPATH so Harbor's uv run can find stella_harbor
echo "Running Harbor..."
cd "$OXAGEN_PLATFORM/bench/swe-bench"
PYTHONPATH="${PYTHONPATH:-}:${USER_SITE}" AGENT=stella exec "$HARBOR_RUNNER" "${HARBOR_ARGS[@]}"
