# Witness authority boundary remediation

## What changed

- Resolve Worker and Judge before candidate creation and fail closed when their resolved model identities match.
- Give authored witnesses a candidate-local executor exposing only `read_file` and one atomic `create_witness_test` operation.
- Bind witness commands to the accepted artifact and a narrow test identity for Cargo, pytest, Vitest, Go, and .NET.

## Verification lesson

Post-hoc filesystem checks are defense in depth, not the primary authority boundary. The witness must begin with a restricted executor, and the create-only operation must enforce canonical-parent containment plus exclusive, no-follow creation. Test the mutex invariant and the filesystem `O_EXCL` invariant separately because they protect different races.

## Evidence

- `cargo test -p stella-pipeline`: 131 unit tests and 4 replay fixtures passed.
- `cargo test -p stella-cli candidate_ws::witness_tools::tests:: -- --nocapture`: 5 tests passed.
- `cargo clippy -p stella-pipeline -p stella-cli --all-targets -- -D warnings`: passed.
- Independent review reported no remaining Critical or Important C2/I1/I2 findings.
