# Oxagen break-fix memories

- [Verified enrollment must arm authority before telemetry storage](verified-enrollment-must-arm-authority-before-telemetry-storage.md) — Signed authority precedes fail-open delivery setup · bug · 2026-07-21
- [Adjacent CLI command enums need delimiter coverage](telemetry-auth-command-merge-delimiter.md) — A missing enum brace after a merge broke every CLI test build · bug · 2026-07-21
- [Config test fixtures must track credential provenance](config-test-fixture-credential-source.md) — New required Config fields must be added to direct test initializers · bug · 2026-07-21
- [Renamed dependency aliases need a regenerated lockfile](renamed-dependency-aliases-need-lockfile-refresh.md) — A merge retained obsolete Cargo.lock dependency names · bug · 2026-07-21
- [Host media operation journal private SQLite](host-media-operation-journal-private-sqlite.md) — Hold validated file identity across SQLite initialization · bug · 2026-07-21
- [Model-call usage completeness must fail closed](model-call-usage-completeness-must-fail-closed.md) — Per-call accounting and persistence completeness are one monotonic export gate · bug · 2026-07-21
- [Successful retries preserve failed-attempt usage gaps](successful-retries-preserve-failed-attempt-usage-gaps.md) — A later success cannot recover an earlier dispatched attempt's unknown usage · bug · 2026-07-21
