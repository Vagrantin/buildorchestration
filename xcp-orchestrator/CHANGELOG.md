# Changelog

All notable changes to the XCP-orchestrator workspace are documented in this file.

## 2026-07-13 (fourth batch)

### Changed

- **iso-agent**: xolite-ce builds no longer float to the newest upstream
  xo-lite release (v0.23.0 broke every build since July 12). Like xoa-hl's
  `XO_COMMIT` pin, the upstream ref is now pinned in an `UPSTREAM_TAG` file at
  the root of the xolite-ce repo (initially `xo-lite-v0.21.0`, the last
  known-good version); the agent reads that pin to decide versions/rebuilds,
  and the `build-xolite-ce.yml` workflow clones the same pin. Bumping the pin
  is a normal commit, which the existing HEAD-change detection turns into an
  `UpstreamBump`. If the pin file is missing the agent warns and falls back to
  the old latest-release behaviour, so deployment order doesn't matter.

### Added

- `shared::fetch_pinned_xolite_tag` / `parse_pinned_xolite_tag` (+ unit tests).

## 2026-07-13 (third batch)

### Fixed

- **xoa-vm-agent**: "code unchanged" no longer implies "nothing to do". The xoa-hl
  repo carries two kinds of releases — RPM releases from the `build-xoa.yml`
  workflow and this agent's own `xoa-image-{date}-{sha7}` XVA releases — and the
  previous skip check ("latest release matches HEAD") only proved the RPM was
  current, skipping before the VM image was ever built. The agent now skips only
  when an `xoa-image-*` release **with an XVA asset** exists for HEAD; if the RPM
  release covers HEAD but the image is missing, it skips just the workflow
  dispatch and proceeds with the Packer build and image upload. The local
  fast-path also requires `last_tag` to be an image tag, which self-heals the
  state poisoned by the previous backfill.
- **xoa-vm-agent**: `resolve_xoa_hl_rpm_url` used `releases/latest`, which breaks
  once an image release becomes the latest (no RPM asset). It now scans the
  release list for the newest release carrying an `.rpm` asset.
- **orchestrator**: the Ollama diagnostic was never actually called since the
  workspace split — `llm_hint` was a hardcoded string, which is why no analysis
  appeared despite `ollama serve` running. On failure the orchestrator now pulls
  the failed run's job-log tail from GitHub Actions and feeds it to
  `qwen3-coder:30b` at `localhost:11434` (restoring the `1f87fe4` behaviour),
  logging the attempt and outcome to the journal. Any GitHub/Ollama error is
  non-fatal: it logs a warning, falls back to a static hint, and still renders
  the dashboard.

### Added

- `shared::fetch_releases` (release list with tags, URLs and asset names) and
  unit tests for the image-release matching and run-URL parsing logic.

## 2026-07-13 (second batch)

### Fixed

- **Dashboard restored**: the HTML report regressed during the workspace split to a
  bare page showing only the ISO status. The styled card layout with a status badge
  and a "Logs" link per component (as of commit `1f87fe4`) is back, extended with
  XOA-HL and XOA Image rows. Agents now record per-component status and URLs in
  their status files (`AgentStatus.components`, backward compatible).
- **Rebuilds without changes**: the workspace split renamed the version-state files,
  orphaning the recorded "last built" state — and state was only saved after a fully
  successful run, so any failure re-triggered every build forever (xoa-proxy releases
  `v0.1.1.1`/`.2`/`.3` all point at the same commit). Agents now cross-check the
  **latest GitHub release** of each repo (xolite-ce, xoa-proxy, xoa-hl, xcp-ng-ce-iso):
  if it already points at HEAD with the expected version, nothing is rebuilt and the
  local state is backfilled from it (self-healing after state loss).

### Changed

- **orchestrator**: on failure it now logs one concise `ERROR` line per failed
  agent/component (phase, detail, log URL) to the journal, replacing the
  "Dispatching diagnostic tracking operations..." stub.

## 2026-07-13

### Fixed

- **xoa-vm-agent**: workflow dispatch failed with `404 Not Found` because the agent
  targeted `.github/workflows/build.yml`, while the workflow in `Vagrantin/xoa-hl`
  is `build-xoa.yml`. The constant now matches the real filename.
- **iso-agent**: pushing a tag that already exists on `xoa-proxy` (e.g. `v0.1.1`)
  was a hard failure — the collision retry in `create_and_push_tag` only knew how
  to increment `-ceN` suffixes, which xoa-proxy tags no longer carry. Collisions
  on plain version tags now retry with a fourth numeric counter segment
  (`v0.1.1` → `v0.1.1.1`, `v0.1.1.2` → `v0.1.1.3`), matching the existing
  PatchBump tag format and the `v[0-9]*` workflow trigger.
- **iso-agent / xoa-vm-agent**: workflow status polling loops aborted the whole
  agent on the first transient GitHub API error. They now log a warning and keep
  polling, giving up only after 5 consecutive failures.

### Changed

- **systemd**: `xcp-orchestrator.timer` moved from 05:00 to 09:00 so the status
  aggregation runs after both iso-agent (04:00) and xoa-vm-agent (06:00) have
  finished, instead of between them.

### Added

- Unit tests for the tag collision increment logic (`next_tag_candidate` in
  `shared/src/github.rs`).
