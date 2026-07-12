#!/usr/bin/env bash
#
# Run SWE-bench (Harbor) against the Stella coding CLI.
#
# Usage:
#   STELLA_MODEL=anthropic/claude-fable-5 ./run.sh
#   TASK_IDS="django__django-11099" N_CONCURRENT=1 ./run.sh
#   STELLA_BUDGET=10.0 ./run.sh
#
# Prereqs: docker running; provider API key exported (ANTHROPIC_API_KEY, etc.)
#
# Z.ai (GLM) users: Set the coding endpoint base URL:
#   export STELLA_BASE_URL=https://api.z.ai/api/coding/paas/v4
#   (The /coding/ segment is required for coding plans.)

set -euo pipefail

cd "$(dirname "$0")"
REPO_ROOT="$(cd ../.. && pwd)"

# Ensure Stella is built
if [ ! -f "$REPO_ROOT/target/release/stella" ]; then
    echo "Building Stella..."
    cd "$REPO_ROOT"
    cargo build --release -p stella-cli
    cd "$(dirname "$0")"
fi

# Configuration
# Set AGENT=oxagen to use the Oxagen runner's Harbor infrastructure
# We'll override the actual agent via HARBOR_EXTRA
AGENT="oxagen"
MODEL_SLUG="${STELLA_MODEL:-anthropic/claude-fable-5}"
DATASET="${DATASET:-swe-bench/swe-bench-verified}"
N_CONCURRENT="${N_CONCURRENT:-4}"
N_ATTEMPTS="${N_ATTEMPTS:-1}"
JOBS_DIR="${JOBS_DIR:-./results-stella}"

# Export for the adapter to pick up
export STELLA_MODEL="$MODEL_SLUG"
export STELLA_BUDGET="${STELLA_BUDGET:-5.0}"
export STELLA_BINARY="$REPO_ROOT/target/release/stella"

# Forward base URL if set (required for Z.ai coding plans)
if [ -n "${STELLA_BASE_URL:-}" ]; then
    export STELLA_BASE_URL
fi

# Override Harbor to use Stella agent instead of Oxagen
export HARBOR_EXTRA="--agent stella_harbor:StellaAgent ${HARBOR_EXTRA:-}"

# Build task ID args
TASK_ID_ARGS=()
if [ -n "${TASK_IDS:-}" ]; then
    for task in $TASK_IDS; do
        TASK_ID_ARGS+=("--include-task-name" "$task")
    done
fi

# Locate Harbor SWE-bench runner
# This can be in the oxagen-platform repo or installed via Harbor
HARBOR_RUNNER="${HARBOR_RUNNER:-}"

if [ -z "$HARBOR_RUNNER" ]; then
    # Try to find it in oxagen-platform
    OXAGEN_PLATFORM="${OXAGEN_PLATFORM:-$HOME/Workspaces/oxagen-platform}"
    if [ -f "$OXAGEN_PLATFORM/bench/swe-bench/run.sh" ]; then
        HARBOR_RUNNER="$OXAGEN_PLATFORM/bench/swe-bench/run.sh"
    else
        echo "Error: Cannot find Harbor SWE-bench runner."
        echo "Set HARBOR_RUNNER to the path to run.sh, or OXAGEN_PLATFORM to the oxagen-platform repo."
        exit 1
    fi
fi

echo "=== Stella SWE-bench run ==="
echo "Agent: $AGENT"
echo "Model: $MODEL_SLUG"
echo "Dataset: $DATASET"
echo "Concurrent: $N_CONCURRENT"
echo "Budget: \$${STELLA_BUDGET} per task"
echo "Jobs dir: $JOBS_DIR"
echo ""

# Build Harbor args
# Note: We override the agent via HARBOR_EXTRA instead of here
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

# Ensure adapter is available
echo "Setting up Stella Harbor adapter..."
ADAPTER_DIR="$(cd "$(dirname "$0")" && pwd)"

# Install to user site-packages (visible to most Python environments)
python3 -m pip install --user -e "$ADAPTER_DIR" --break-system-packages --quiet

# Get the user site-packages directory
USER_SITE=$(python3 -c "import site; print(site.USER_SITE)")
echo "User site-packages: $USER_SITE"

# Verify it can be imported
python3 -c "from stella_harbor import StellaAgent; print('✓ Stella agent importable')" 2>/dev/null || {
    echo "Error: stella_harbor package not importable"
    exit 1
}

# Run Harbor
echo "Running Harbor..."
echo "Adapter directory: $ADAPTER_DIR"

# Need to pass PYTHONPATH through the uv run command
# Add user site-packages where stella_harbor is installed
cd "$REPO_ROOT"
PYTHONPATH="${PYTHONPATH:-}:${USER_SITE}" exec "$HARBOR_RUNNER" "${HARBOR_ARGS[@]}"
