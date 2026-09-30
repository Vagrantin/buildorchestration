#!/usr/bin/env bash
# What "checked" means for this repo (xcp-hl#148). Jenkins dev/buildorchestration runs it on every PR; run it locally too.
# Contract: exit 0 when clean; JUnit goes to $CI_RESULTS; on failure $CI_RESULTS/current-step names the failed check.
set -euo pipefail
cd "$(dirname "$0")/.."

OUT="$(mkdir -p "${CI_RESULTS:-ci-results}" && cd "${CI_RESULTS:-ci-results}" && pwd)"
step() { echo "==> $1"; echo "$1" > "$OUT/current-step"; }
# The Rust workspace (iso-agent, xoa-vm-agent, shared) lives in xcp-orchestrator/.
cd xcp-orchestrator
trap 'cp target/nextest/ci/junit.xml "$OUT/junit.xml" 2>/dev/null || true' EXIT

step "cargo fmt --check"
cargo fmt --all -- --check

step "cargo clippy"
cargo clippy --locked --workspace --all-targets -- -D warnings

step "cargo nextest (workspace)"
cargo nextest run --locked --workspace --profile ci

step "cargo test --doc"
cargo test --locked --workspace --doc

rm -f "$OUT/current-step"
echo "all checks passed"
