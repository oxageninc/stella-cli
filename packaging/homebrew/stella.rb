# Homebrew formula for the Stella CLI.
#
# Build-from-source formula: it compiles `stella` from the tagged source with
# cargo, so it stays correct across releases without per-release/per-platform
# bottle sha256 placeholders to maintain. Bump `tag:` (and, ideally, add a
# matching `revision:`) on each release.
#
# The stable `url` uses Homebrew's git download strategy (url ending in `.git`
# with a `tag:`), which needs no `sha256`. To pin exactly, add
# `revision: "<full-commit-sha>"` alongside the tag.
#
# To distribute prebuilt bottles later (faster installs, no Rust toolchain
# needed), publish tarballs from .github/workflows/release.yml and add a
# `bottle do ... end` block with `sha256` lines per platform, or move this
# formula into a `homebrew-tap` repo that CI updates automatically.
class Stella < Formula
  desc "Fast, BYOK, model-agnostic terminal coding agent"
  homepage "https://github.com/macanderson/stella"
  url "https://github.com/macanderson/stella.git", tag: "v0.4.37"
  version "0.4.37"
  license "MIT OR Apache-2.0"
  head "https://github.com/macanderson/stella.git", branch: "main"

  depends_on "rust" => :build

  def install
    # Builds the `stella` binary from the `stella-cli` package and installs it
    # into the formula prefix (std_cargo_args adds --locked --root <prefix>).
    system "cargo", "install", *std_cargo_args(path: "stella-cli")
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/stella --version")
  end
end
