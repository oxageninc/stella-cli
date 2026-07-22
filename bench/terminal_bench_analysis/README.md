# Terminal-Bench 2.1 analysis

This utility turns one or more Harbor job directories into a complete audit
table. It includes requested slots that Harbor never instantiated, running or
interrupted trials, verifier failures, agent errors, and completed trials. It
never treats missing usage as zero. Token spend is always:

```text
Harbor prompt/input tokens + Harbor completion/output tokens
```

Harbor prompt tokens already include cache hits, so cache tokens are retained
as a reported subset and are not added again.

Run it with the same Harbor 0.6.1 environment used by the benchmark:

```bash
python bench/terminal_bench_analysis/tb21_analysis.py \
  /path/to/mandatory-fixed-glm-5.1-primary-harbor-job \
  --comparator /path/to/frozen-comparator-manifest.json \
  --calibration-job /path/to/stella-tb21-calibration-20260721 \
  --calibration-ledger-job /path/to/excluded-job-1 \
  --study-manifest /path/to/study-manifest.json \
  --run-ledger /checkout/bench/evidence/stella-tb21-run-ledger.json \
  --github-public-timing-evidence /checkout/bench/evidence/github-comments.json \
  --output-dir /path/to/analysis
```

The output directory contains `trials.csv`, `report.json`, and `report.md`.
ATIF files are validated with Harbor's official `Trajectory` model, not a
home-grown approximation.

## Frozen study manifest

A JSON study manifest is required for `scientific_artifact_eligible` to become
true. If the option is absent or any job/trial differs, estimates are still
reported as descriptive results, but the report lists explicit blocking
reasons. Without `--github-public-timing-evidence`, offline analysis always
leaves `public_timing_verified=false` and `claim_established=false`. With that
option, the same analyzer process performs fresh read-only GitHub API and
committed-content verification; it never accepts a saved report or a manifest
boolean as timing evidence.
The production schema is `stella-tb21-study-manifest-v6`. The identity fields
marked `REPLACE_AFTER_FREEZE` must be filled from the final binary and completed
calibration; leaving a placeholder makes the analyzer reject the claim:

Harbor launch commands must use the combined literal
`--dataset terminal-bench/terminal-bench-2-1@sha256:7d7bdc1cbedad549fc1140404bd4dc45e5fd0ea7c4186773687d177ad3a0699a`.
In the normalized manifest and Harbor result, `dataset.name` remains the base
name and `dataset.ref` carries the same `sha256:` digest separately.

```jsonc
{
  "schema_version": "stella-tb21-study-manifest-v6",
  "preregistration": {
    "study_id": "stella-tb21-scientific-study-v1",
    "run_ledger_path": "bench/evidence/stella-tb21-run-ledger.json",
    "readiness_commit": "REPLACE_WITH_PUBLIC_40_HEX_COMMIT",
    "calibration_commit": "REPLACE_WITH_FINAL_SUT_40_HEX_COMMIT"
  },
  "sut": {
    "model": "openrouter/z-ai/glm-5.1",
    "allowed_call_models": ["z-ai/glm-5.1"],
    "binary_sha256": "REPLACE_AFTER_FREEZE_WITH_64_HEX",
    "source_commit": "REPLACE_AFTER_FREEZE_WITH_40_HEX",
    "source_commit_embedded": true,
    "agent_version": "REPLACE_AFTER_FREEZE_WITH_EXACT_REPORTED_VERSION",
    "adapter_version": "0.6.0",
    "adapter_sha256": "REPLACE_AFTER_FINAL_ADAPTER_FREEZE_WITH_64_HEX",
    "budget_usd": 0.17,
    "disable_reflection": true,
    "base_url": "https://openrouter.ai/api/v1",
    "provider_route_policy": "openrouter-auto",
    "host_credential_source": "anonymous-seekable-fd-v1",
    "host_credential_name": "OPENROUTER_API_KEY",
    "host_credential_bundle_count": 1,
    "engine_posture_version": "stella-tb21-engine-posture-v1",
    "engine_posture": {
      "default_model": "openrouter/z-ai/glm-5.1",
      "allowed_models": ["openrouter/z-ai/glm-5.1"],
      "auto_mode": "off",
      "effort_auto": "off",
      "reasoning_auto": "off",
      "agents": {
        "default": {"effort": "high", "reasoning": "on"},
        "worker": {"effort": "high", "reasoning": "on"},
        "judge": {"effort": "high", "reasoning": "on"},
        "triage": {"effort": "low", "reasoning": "off"}
      }
    },
    "engine_posture_sha256": "98511188b8338637afe0f2ffde1998c26f048db2f9c936549f75bd222600cf76"
  },
  "analysis": {
    "sha256": "REPLACE_AFTER_FINAL_ANALYZER_FREEZE_WITH_64_HEX",
    "public_timing_sha256": "REPLACE_AFTER_FINAL_PUBLIC_VERIFIER_FREEZE_WITH_64_HEX"
  },
  "dataset": {
    "name": "terminal-bench/terminal-bench-2-1",
    "ref": "sha256:7d7bdc1cbedad549fc1140404bd4dc45e5fd0ea7c4186773687d177ad3a0699a",
    "task_set_sha256": "REPLACE_WITH_FROZEN_89_TASK_REFS_AND_CHECKSUMS_SHA256",
    "path": null,
    "version": null,
    "registry_url": null,
    "registry_path": null,
    "overwrite": false,
    "download_dir": null,
    "exclude_task_names_json": "null",
    "n_tasks": null
  },
  "design": {
    "tasks": 89,
    "attempts_per_task": 5
  },
  "harbor": {
    "version": "0.6.1",
    "sha256": "ae71004577634f20c8bf16e4d8571ade78b3d4023dc18b54de973b3f4410f3fb",
    "timeout_multiplier": 1.0,
    "agent_timeout_multiplier": null,
    "verifier_timeout_multiplier": null,
    "agent_setup_timeout_multiplier": null,
    "environment_build_timeout_multiplier": null,
    "environment_type": "docker",
    "environment_import_path": null,
    "environment_force_build": false,
    "environment_delete": true,
    "environment_suppress_override_warnings": false,
    "environment_mounts_json": null,
    "environment_env_json": "{}",
    "environment_kwargs_json": "{}",
    "environment_override_cpus": null,
    "environment_override_memory_mb": null,
    "environment_override_storage_mb": null,
    "environment_override_gpus": null,
    "agent_override_timeout_sec": null,
    "agent_override_setup_timeout_sec": null,
    "agent_max_timeout_sec": null,
    "agent_name": null,
    "agent_env_json": "{}",
    "agent_kwargs_json": "{}",
    "verifier_override_timeout_sec": null,
    "verifier_max_timeout_sec": null,
    "verifier_disable": false,
    "verifier_env_json": "{}",
    "retry_max_retries": 0,
    "retry_include_exceptions_json": "null",
    "retry_exclude_exceptions_json": "[\"VerifierOutputParseError\",\"RewardFileEmptyError\",\"RewardFileNotFoundError\",\"AgentTimeoutError\",\"VerifierTimeoutError\"]",
    "retry_wait_multiplier": 1.0,
    "retry_min_wait_sec": 1.0,
    "retry_max_wait_sec": 60.0,
    "artifacts_json": "[]",
    "metrics_json": "[]",
    "quiet": false,
    "debug": false,
    "tasks_json": "[]"
  },
  "comparator": {
    "public_job_id": "fd8707bb-51e8-56fa-8e46-769a82a531ae",
    "manifest_sha256": "7963a7af2b306fd4b6e82963fdadf9374e701ec16f47b194300e2843c8002a76",
    "trial_data_sha256": "f7b916c7d3028c62003bb12eeb1fff3df0bb41a82ce21ba6e59b3a1b50139a99",
    "agent_name": "claude-code",
    "agent_version": "2.1.123",
    "model": "glm-5.1",
    "reasoning_effort": "max",
    "submission": {
      "repository": "https://github.com/harbor-framework/terminal-bench-2-1",
      "commit": "327a5a0b2ee4675871dc57e1d53fff2d2cf974e1",
      "path": "leaderboard/submissions/2026-05-01-glm-5-1-max-claude-code.json",
      "sha256": "36d20c181be246dc55965bf4320a3005f292c737f31511bbde19ba1808a2bd2c"
    },
    "expected": {
      "rows": 445,
      "tasks": 89,
      "attempts_per_task": 5,
      "reward_total": 261.0,
      "token_spend_total": 398783761
    }
  },
  "calibration": {
    "seed": 20260721,
    "tasks": [
      "fix-git", "filter-js-from-html", "kv-store-grpc",
      "large-scale-text-editing", "regex-log",
      "schemelike-metacircular-eval", "sqlite-with-gcov",
      "bn-fit-modify", "make-mips-interpreter", "train-fasttext"
    ],
    "model_order": [
      "openrouter/deepseek/deepseek-v4-pro",
      "openrouter/z-ai/glm-5.2",
      "openrouter/x-ai/grok-4.5"
    ],
    "call_models_by_config": {
      "openrouter/deepseek/deepseek-v4-pro": ["deepseek/deepseek-v4-pro"],
      "openrouter/z-ai/glm-5.2": ["z-ai/glm-5.2"],
      "openrouter/x-ai/grok-4.5": ["x-ai/grok-4.5"]
    },
    "engine_postures_by_config": {
      "openrouter/deepseek/deepseek-v4-pro": {
        "version": "stella-tb21-engine-posture-v1",
        "posture": {
          "default_model": "openrouter/deepseek/deepseek-v4-pro",
          "allowed_models": ["openrouter/deepseek/deepseek-v4-pro"],
          "auto_mode": "off", "effort_auto": "off", "reasoning_auto": "off",
          "agents": {
            "default": {"effort": "high", "reasoning": "on"},
            "worker": {"effort": "high", "reasoning": "on"},
            "judge": {"effort": "high", "reasoning": "on"},
            "triage": {"effort": "low", "reasoning": "off"}
          }
        },
        "sha256": "fb18233aadf78077bc70fe52cdb1dcacc1f840600473a92226a88e932a138fd6"
      },
      "openrouter/z-ai/glm-5.2": {
        "version": "stella-tb21-engine-posture-v1",
        "posture": {
          "default_model": "openrouter/z-ai/glm-5.2",
          "allowed_models": ["openrouter/z-ai/glm-5.2"],
          "auto_mode": "off", "effort_auto": "off", "reasoning_auto": "off",
          "agents": {
            "default": {"effort": "high", "reasoning": "on"},
            "worker": {"effort": "high", "reasoning": "on"},
            "judge": {"effort": "high", "reasoning": "on"},
            "triage": {"effort": "low", "reasoning": "off"}
          }
        },
        "sha256": "de2a31097dbb71ba16d5b7e505e2cfcbd837deab387962635d4fe2f438a45860"
      },
      "openrouter/x-ai/grok-4.5": {
        "version": "stella-tb21-engine-posture-v1",
        "posture": {
          "default_model": "openrouter/x-ai/grok-4.5",
          "allowed_models": ["openrouter/x-ai/grok-4.5"],
          "auto_mode": "off", "effort_auto": "off", "reasoning_auto": "off",
          "agents": {
            "default": {"effort": "high", "reasoning": "on"},
            "worker": {"effort": "high", "reasoning": "on"},
            "judge": {"effort": "high", "reasoning": "on"},
            "triage": {"effort": "low", "reasoning": "off"}
          }
        },
        "sha256": "f43d8a25c68cee0f424e6bb3ce91891c48921ddb2f3231157a2c35dc12d66e07"
      }
    },
    "job_name": "stella-tb21-calibration-20260721",
    "job_id": "REPLACE_AFTER_CALIBRATION_WITH_EXACT_JOB_ID",
    "attempts_per_model_task": 2,
    "n_concurrent_trials": 3,
    "minimum_passes": 14,
    "projection_trials": 445,
    "projected_spend_limit_usd": 75.0,
    "selected_model": "REPLACE_WITH_MECHANICALLY_DERIVED_WINNER",
    "trial_data_sha256": "REPLACE_AFTER_CALIBRATION_WITH_64_HEX",
    "excluded_job_ids": [
      "9b704487-9d21-46a7-8103-e5396cb7d4ea",
      "0c44d9ee-4389-4c7a-8445-ea4be2404115",
      "c5686c41-1d2d-41cf-a275-177c2e6878b3",
      "37ee4276-8595-4ff9-8507-be21adb891cc",
      "7e59ed1e-2abe-40b9-bf7e-6b24c7f9a350",
      "REPLACE_WITH_READINESS_OUTCOME_JOB_ID"
    ],
    "excluded_ledger_sha256": "REPLACE_WITH_FROZEN_LEDGER_64_HEX"
  },
  "confirmatory": {
    "job_name": "REPLACE_WITH_PREREGISTERED_FIXED_GLM_5_1_PRIMARY_JOB_NAME",
    "n_concurrent_trials": 1
  }
}
```

Harbor 0.6.1 models `retry.exclude_exceptions` as a set, so its serialized
array order is not stable across Python hash seeds. The analyzer parses
`retry_exclude_exceptions_json` and requires exactly the five unique string
members shown above while treating their order as irrelevant. Missing,
duplicate, extra, non-array, and non-string values still fail closed.

Calibration and excluded-ledger digests use logical job/trial identities and
deliberately omit the machine-local absolute `source_input` path. Copying the
same immutable evidence tree to a public review root therefore preserves both
digests; job IDs, slots, task identities, results, tokens, costs, exceptions,
and artifact identities remain bound.

`confirmatory.job_id` is deliberately absent. Harbor creates that UUID only
after launch, so preregistering it would be circular. The analyzer derives the
single mandatory primary ID from Harbor rows and binds it to the later
append-only outcome. Calibration IDs and the readiness ID are known by the
confirmatory freeze and may appear in its manifest. `sut.model` remains the
fixed GLM-5.1 primary; `calibration.selected_model` is independently derived and
does not replace it.

## Append-only paid-run ledger and launch receipt

Claim mode also requires `--run-ledger`. Its exact top-level schema is
`stella-tb21-run-ledger-v2` with these eight fields:

```jsonc
{
  "schema_version": "stella-tb21-run-ledger-v2",
  "study_id": "stella-tb21-scientific-study-v1",
  "ledger_path": "bench/evidence/stella-tb21-run-ledger.json",
  "historical_spend_disclosure": {
    "known_lower_bound_usd": 0.2429614978,
    "unknown_cancellation_spend": true,
    "new_authorized_budget_usd": 200.0
  },
  "preregistrations": [
    {
      "sequence": 1,
      "kind": "readiness",
      "commit": "40_lowercase_hex",
      "study_manifest_sha256": null,
      "declared_at": "timezone-aware ISO-8601"
    },
    {
      "sequence": 2,
      "kind": "calibration",
      "commit": "final_sut_source_commit",
      "study_manifest_sha256": null,
      "declared_at": "timezone-aware ISO-8601"
    },
    {
      "sequence": 3,
      "kind": "confirmatory_freeze",
      "commit": "distinct_40_lowercase_hex",
      "study_manifest_sha256": "exact_frozen_manifest_file_sha256",
      "declared_at": "timezone-aware ISO-8601"
    }
  ],
  "intents": [
    {
      "sequence": 4,
      "intent": {
        "intent_id": "unique-id",
        "stage": "readiness | calibration | confirmatory | historical_excluded",
        "historical": false,
        "job_name": "literal-Harbor-job-name",
        "models": ["openrouter/provider/model"],
        "dataset": {
          "name": "dataset identity",
          "ref": "sha256:...",
          "task_count": 1,
          "task_set_sha256": "64_lowercase_hex"
        },
        "requested_trials": 1,
        "attempts_per_task": 1,
        "n_concurrent_trials": 1,
        "retry_max_retries": 0,
        "per_trial_budget_usd": 0.17,
        "artifacts": {
          "binary_sha256": "64_lowercase_hex",
          "source_commit": "40_lowercase_hex",
          "agent_version": "exact version",
          "adapter_version": "exact version",
          "adapter_sha256": "64_lowercase_hex",
          "analysis_sha256": "64_lowercase_hex",
          "public_timing_sha256": "64_lowercase_hex",
          "harbor_version": "0.6.1",
          "harbor_sha256": "64_lowercase_hex",
          "engine_posture_sha256_by_model": {"model": "64_lowercase_hex"}
        },
        "execution": {
          "base_url": "https://openrouter.ai/api/v1",
          "provider_route_policy": "openrouter-auto",
          "disable_reflection": true
        },
        "provider_key": {
          "fingerprint_sha256": "nonsecret_64_lowercase_hex",
          "label": "dedicated-key-label",
          "limit_usd": 180.0,
          "usage_before_usd": 0.0,
          "snapshot_at": "timezone-aware ISO-8601 before declaration and launch"
        },
        "declared_at": "timezone-aware ISO-8601",
        "preregistration_commit": "40_lowercase_hex"
      },
      "intent_sha256": "sha256_of_exact_canonical_intent_object_only"
    }
  ],
  "publications": [
    {
      "sequence": 5,
      "subject_type": "preregistration | intent",
      "subject_id": "prereg-kind-or-intent-sha256",
      "ledger_commit": "later_distinct_40_lowercase_hex",
      "public_url": "https://github.com/macanderson/stella/commit/later_distinct_40_lowercase_hex",
      "published_at": "timezone-aware ISO-8601"
    }
  ],
  "outcomes": [
    {
      "sequence": 6,
      "intent_sha256": "declared-intent-sha256",
      "job_id": "Harbor-generated-UUID",
      "status": "excluded | complete | historical_excluded",
      "started_at": "exact Harbor time",
      "completed_at": "exact Harbor time",
      "artifact_tree_sha256": "64_lowercase_hex",
      "provider_usage_before_usd": 0.0,
      "provider_usage_after_usd": 0.01,
      "provider_usage_delta_usd": 0.01,
      "telemetry_cost_sum_usd": 0.01,
      "reconciliation_status": "reconciled",
      "reconciliation_tolerance_usd": 0.000001,
      "recorded_at": "timezone-aware ISO-8601"
    }
  ]
}
```

Intent objects are never edited to add publication or outcome data. Separate
records make the Git history append-only and leave each intent hash stable.
There must be exactly three paid intents in order: the one readiness sentinel,
the one 60-trial calibration, and the mandatory fixed-GLM-5.1 445-trial
primary. The five known earlier jobs used the old shared key, are permanently
excluded, and use explicit-null historical intent/outcome fields because exact
per-job provider spend is unavailable. Their disclosed observed lower bound is
retained in the top-level object; cancellation spend is explicitly unknown and
is not silently converted to zero. The dedicated-key fingerprint, label, and
limit must remain unchanged; every
job's provider delta must reconcile to telemetry; adjacent usage snapshots
must be continuous; and final minus initial usage must equal all registered
deltas. A gap is treated as possible unregistered spend. Tolerance is capped at
one cent. The dedicated key must have one no-reset hard limit of exactly
`$180.00`; the all-in user authorization remains `$200.00` with a `$15.00`
conservative historical reserve. The executable v6 plan contains exactly
readiness, calibration, and primary, for `$86.02` in nominal new-call spend.
No selected-winner follow-up is authorized by this manifest or launcher. That
planning amount is not substituted for provider reconciliation or the hard key
limit.

`provider_key.usage_before_usd` is the dedicated new key's cumulative usage,
not the OpenRouter account/hopper balance. Immediately before every job, the
secure launcher independently fetches OpenRouter `/credits` and requires live
available credit to cover that job's full nominal allocation. The receipt
records total credits, total usage, computed available credit, and the planned
allocation. That mutable balance is not substituted for the continuous
dedicated-key usage ledger.

Each paid job directory must contain a mode-`0600` regular file named
`stella-secure-launch-receipt.json`, created before Harbor starts. It has
exactly six top-level fields: `schema_version`, `job_name`, `models`,
`intent_sha256`, `public_intent_attestation`, and `launcher_controls`, under
schema `stella-harbor-secure-launch-receipt-v2`. The nested public proof has
schema `stella-harbor-public-intent-preflight-v2` and records an anonymous GET of the public fixed
repository, dedicated owner issue, exact unedited owner intent comment, and
bound ledger snapshot. It binds the intent digest and stage to distinct subject
and ledger commits, hashes the exact comment body bytes, records GitHub's server
timestamps, waits two seconds, and records a final anonymous comment GET. It
also binds the exact ledger bytes, strict source-to-subject-to-ledger ancestry,
completed prior-stage outcome, binary/adapter/Harbor/analyzer/verifier/engine
runtime identity, the exact `$180` no-reset key, live `/key` and `/credits`
budget evidence, and a full runtime rehash after the final GET. The
controls object is exactly:

```json
{
  "command": "harbor-run-only",
  "agent_import_path": "stella_harbor:StellaAgent",
  "environment": "docker",
  "credential_source": "anonymous-seekable-fd-v1",
  "fresh_job_directory": "atomic-create",
  "resume": "forbidden",
  "in_run_publication": "forbidden",
  "filesystem_settings": "disabled",
  "filesystem_credentials": "disabled",
  "project_env_files": "disabled",
  "subprocess_credential_scrub": "enabled",
  "harbor_clock_timezone": "UTC"
}
```

The analyzer verifies the exact nested preflight proof and receipt binding,
declared timestamps, and exact GitHub commit-link shapes offline and emits
`external_public_timing_audit_required`. The stdlib-only live
verifier fixes the repository to public `macanderson/stella`, deliberately
omits credentials, disables ambient proxies and CA overrides, requires public
system trust roots with TLS 1.2 or newer, refuses redirects, requires exact
requested URLs and HTTP 200, bounds every response to 8 MiB, rejects duplicate
JSON keys, performs anonymous GET requests only, and fetches the
dedicated issue, issue-comment records, commit/compare API records, and exact
ledger, protocol, analyzer, and public-verifier bytes. The publication's
`ledger_commit` must be distinct from and descend from its `subject_commit`.
Every historical ledger snapshot must be an exact array-prefix projection of
the final ledger and contain the bound payload; snapshots must themselves form
one ancestry chain in publication sequence. At the confirmatory subject freeze
the verifier requires the exact supplied manifest bytes. A separate
`final_ledger_commit` in the evidence map, not inside the self-bound ledger,
must contain the exact completed ledger bytes and descend from the last
publication snapshot. Audit schema
`stella-tb21-github-public-timing-audit-v3` emits each exact comment-body
SHA-256; claim analysis requires every paid publication's live body hash, URL,
comment ID, server timestamp, payload digest, stage, and commit pair to equal
the corresponding pre-execution receipt proof.

The evidence file has this exact schema:

```json
{
  "schema_version": "stella-tb21-github-public-timing-evidence-v2",
  "repository": "macanderson/stella",
  "protocol_path": "bench/terminal-bench-2.1-protocol.md",
  "analyzer_path": "bench/terminal_bench_analysis/tb21_analysis.py",
  "public_timing_path": "bench/terminal_bench_analysis/github_public_timing.py",
  "manifest_path": "bench/evidence/stella-tb21-study-manifest.json",
  "issue_url": "https://github.com/macanderson/stella/issues/123",
  "comments": [
    {
      "subject_type": "preregistration",
      "subject_id": "readiness",
      "html_url": "https://github.com/macanderson/stella/issues/123#issuecomment-456"
    }
  ],
  "final_ledger_commit": "completed_ledger_snapshot_40_lowercase_hex"
}
```

`comments` must cover exactly the three preregistrations and three paid primary-
study intents.
Each GitHub comment body is JSON with no additional fields. A preregistration
body is:

```json
{
  "schema_version": "stella-tb21-github-attestation-v2",
  "study_id": "stella-tb21-scientific-study-v1",
  "subject_type": "preregistration",
  "subject_id": "readiness",
  "kind": "readiness",
  "subject_commit": "frozen_subject_40_lowercase_hex",
  "ledger_commit": "later_payload_snapshot_40_lowercase_hex",
  "ledger_path": "bench/evidence/stella-tb21-run-ledger.json",
  "canonical_payload_sha256": "sha256_of_exact_preregistration_object"
}
```

An intent body has this exact parallel schema:

```json
{
  "schema_version": "stella-tb21-github-attestation-v2",
  "study_id": "stella-tb21-scientific-study-v1",
  "subject_type": "intent",
  "subject_id": "64_lowercase_hex_intent_sha256",
  "kind": "readiness | calibration | confirmatory",
  "subject_commit": "intent_preregistration_commit_40_lowercase_hex",
  "ledger_commit": "later_intent_snapshot_40_lowercase_hex",
  "ledger_path": "bench/evidence/stella-tb21-run-ledger.json",
  "intent_sha256": "same_64_lowercase_hex_intent_sha256"
}
```

`subject_commit` is the frozen preregistration/source commit and
`ledger_commit` is the necessarily later public snapshot whose ledger bytes
already contain the exact record. This separation removes the impossible
self-reference that would arise from asking a preregistration object to name
the commit containing itself.
The REST record must return the exact evidence `html_url`,
`created_at == updated_at`, and `created_at` exactly equal to the ledger's
`published_at`; that server time plus a conservative two-second resolution
margin must be no later than the corresponding authoritative Harbor root-job
`started_at`. All comments must be unedited, owner-authored, and on the one
dedicated owner-authored preregistration issue. Root job times come from the
job-level `result.json`; every instantiated/error slot must also have valid
trial-level start/finish boundaries within the root interval. Harbor's naive
timestamps are interpreted as UTC only when the pre-run receipt attests
`harbor_clock_timezone: UTC`.

Generate a deterministic standalone audit artifact with:

```bash
python bench/terminal_bench_analysis/github_public_timing.py \
  --run-ledger bench/evidence/stella-tb21-run-ledger.json \
  --study-manifest bench/evidence/stella-tb21-study-manifest.json \
  --evidence bench/evidence/github-comments.json \
  --output bench/evidence/github-public-timing-audit.json
```

The standalone JSON is for review and is deliberately not accepted back as a
claim switch. Claim mode uses `--github-public-timing-evidence` so the live
verification is freshly generated in the analysis process. Only a valid,
input-byte-bound live result can remove the external timing blocker and allow
`claim_established=true`.

Every attempted Stella trial must match the frozen binary, embedded source
commit, adapter/Harbor source hashes, endpoint, budget, reflection policy, and
allowed raw per-call model roster. It must also carry the exact canonical
engine-posture object, normalized JSON, schema version, and SHA-256 for its
configuration model: all roles inherit that model; default/worker/judge use
reasoning on at high effort; triage uses reasoning off at low effort; auto
modes and per-role model/provider overrides are absent. Accounting and the
terminal stream must be complete; Harbor totals must exactly match raw
`step_usage` totals; an independent terminal cost must reconcile; every
reward-positive trial needs a Harbor-validated ATIF-v1.7 trace. Both Harbor
context and ATIF must attest the anonymous seekable-FD credential handoff,
exactly one `OPENROUTER_API_KEY` bundle entry, and a successful live-container
absence check immediately before handoff. An interrupted stream remains in the
audit but can never support the token claim, even if its partial numeric totals
look exact.

Before calibration, it requires the single tracked local path-only task
`bench/readiness/synthetic-adapter-sentinel`, full task name
`stella/synthetic-adapter-sentinel`, a null Harbor `LocalTaskId.ref`, and the
observed task-directory checksum
`05a040c7df0fd77f66f533ba023cb5f16e2dd0f89957440b099374210e475ad6`.
The ledger's synthetic dataset identity remains the corresponding `sha256:`
value; it is not misreported as a registry ref emitted by Harbor. The gate also
requires the exact DeepSeek configuration, one attempt, concurrency one, and
zero registry datasets. The readiness artifact identities must match its own immutable row
evidence and public readiness commit, but may differ from the final SUT if the
sentinel exposed an instrumentation fix. In that case a replacement
`calibration` preregistration/publication must precede calibration; history is
retained rather than rewritten.

The analyzer ingests all 60 selection trials from exactly one physical Harbor
job named `stella-tb21-calibration-20260721` and every excluded pre-freeze job.
It verifies 3 models x 10 tasks x 2 attempts, each canonical Harbor package ref
and observed task-directory checksum, unique IDs, the canonical dataset/ref,
adapter import path, exact concurrency three, retry count zero, and the full
normalized Harbor config allowlist. Runtime row grouping is irrelevant. It
recomputes the registered ranking: verifier passes, then lower projected
445-trial USD cost from observed mean calibration cost, then earlier position
in the frozen DeepSeek V4 Pro, GLM-5.2, Grok-4.5 roster. A model needs at least
14 of 20 passes and must satisfy the remaining telemetry, trajectory, and
projected-spend gates. Tokens and wall time are descriptive only and never
break selection ties. The analyzer requires `calibration.selected_model` to be
the mechanically derived eligible winner. The primary
claim manifest keeps `sut.model=openrouter/z-ai/glm-5.1` for a same-model
comparison. The selected winner is recorded for reproducibility, but v6
supplies no executable follow-up contract and the secure launcher rejects one.
A future descriptive winner run requires a separately versioned protocol,
manifest, analyzer, ledger contract, and reviewed launcher after the primary is
complete. Supply the single selection job with `--calibration-job` and the disclosed ledger jobs with
`--calibration-ledger-job`.

The mandatory primary claim accepts exactly one input directory whose frozen
job name matches `confirmatory`; its post-launch job ID is bound only by Harbor
rows and the append-only outcome. It requires concurrency one, retry count zero,
445 requested, instantiated, and attempted rows; 445 unique trial and slot IDs;
and each of the comparator's 89 frozen task refs/checksums at attempt indices 1
through 5 exactly once. Multiple directories, resumes, stitched jobs, missing
slots, extras, duplicates, and replacement trials all block the claim.

No selected-winner follow-up is accepted by this primary analysis or by the v6
secure launcher. If a future separately versioned study implements one, do not
merge its rows, spend, accuracy, tokens, or wall time into this report.

## Comparator and preregistered bootstrap

Supply comparator trial-level data as Harbor job directories, a CSV, or a JSON
file. `--comparator` may be repeated:

```bash
python bench/terminal_bench_analysis/tb21_analysis.py \
  /path/to/stella-job \
  --comparator /path/to/claude-job \
  --study-manifest /path/to/study-manifest.json \
  --output-dir /path/to/analysis
```

CSV/JSON comparator rows must contain a task (`task` or `task_name`) and may
use either the analyzer's column names or Harbor-style token names. JSON can be
an array of rows or an object containing a `trials`, `rows`, or `entries` array.
The latter accepts the reproducible public-comparator `manifest.json`. Aggregate
leaderboard totals are deliberately insufficient: without per-task trial data,
the task-cluster bootstrap is reported as unavailable.
CSV, directories, and modified JSON remain useful for descriptive analysis,
but claim eligibility additionally requires the one frozen manifest byte hash,
its normalized trial-data hash, 445 unique public trial IDs, exact 89 x 5
coverage, and exact reward/token aggregates shown above.

The defaults implement the frozen study design: 89 shared tasks, five trials
per product per task, 50,000 draws, seed `20260721`, and a one-sided 98.33%
lower bound (alpha = 0.05 / 3). A dimension wins only when its point improvement
over all 89 confirmatory tasks is at least 10% **and** its lower bound is
strictly greater than 10%. The inferential point and bootstrap resampling use
only the 79 tasks not seen during model selection; all 10 calibration tasks are
excluded from the confidence calculation. This preserves the complete
confirmatory point estimate while preventing selection leakage into the LCB.
For this historical comparator, claim mode requires `wall_eligible=false`, so
the headline must win on accuracy and tokens. The exact point gates are at least
288 binary passes (`64.7191%`, the first attainable score above the continuous
`64.5168539%` threshold) and at most `358,905,384` tokens.

For synthetic tests or a separately preregistered design, the expected task
and trial counts can be changed explicitly with `--expected-tasks` and
`--expected-trials-per-task`; those settings are recorded in the report. Any
nondefault seed, draw count, task/attempt count, or `--wall-eligible` invocation
is descriptive and cannot establish the preregistered claim.

## Publication secret gate

Scan every complete job or publication tree in the same environment that holds
the active provider credential. The required-variable check prevents a falsely
clean scan caused by launching the scanner from an environment without the key:

```bash
python bench/terminal_bench_analysis/artifact_secret_scan.py \
  /path/to/publication-tree \
  --require-env OPENROUTER_API_KEY \
  --json
```

The scanner checks active sensitive environment values in raw, JSON-escaped,
URL-encoded, Base64, Base32, hex, reversed, and common unpadded forms, plus
high-confidence provider token formats. Supported gzip, bzip2, xz, tar, and ZIP
containers are inspected recursively; unsupported containers and scan limits
block publication. It never prints matching values or byte context. A finding,
unreadable path, missing required credential, symlink, or special file is a
blocking result. This is defense in depth after credential process isolation,
not a proof against arbitrary encryption or obfuscation.

## Tests

```bash
cd bench/terminal_bench_analysis
python -m pytest -q
ruff check .
ruff format --check .
```
