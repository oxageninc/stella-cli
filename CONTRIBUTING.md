# Contributing to Stella

Thanks for your interest! A few ground rules:

- **DCO, not CLA.** Sign your commits (`git commit -s`) to certify the
  [Developer Certificate of Origin](https://developercertificate.org/).
  No copyright assignment, ever.
- **Definition of done is deterministic.** Changes land with a witness
  test: it fails on the previous code and passes on yours (`verify_done`
  encodes exactly this — use it).
- **Gate before you push:** `cargo fmt --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo test --workspace`.
- **License.** Dual MIT OR Apache-2.0; your contributions are accepted
  under the same terms.
