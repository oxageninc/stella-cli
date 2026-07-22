# Paid readiness sentinel

`synthetic-adapter-sentinel` is a deliberately non-benchmark Harbor task used
once before calibration. It proves that a paid provider call, tool execution,
container handoff, durable accounting, and verifier execution work end to end.

This task is not part of Terminal-Bench 2.1. Its reward, tokens, cost, and wall
clock are permanently ineligible for calibration, model selection, confirmatory
analysis, or any public performance claim. The preregistered run is one attempt
with `openrouter/deepseek/deepseek-v4-pro`; after that attempt, execution stops
regardless of reward. A failure may justify an instrumentation fix and a new
public source commit, but it may not justify changing the benchmark task list,
selection rule, or claim threshold.

The fixture contains hostile project-local `.env` and `.stella/settings.json`
values. A valid run must still use the trusted launcher model, endpoint,
reasoning posture, and anonymous credential handoff. The real provider secret
must be absent from every task service environment before Stella is installed.
