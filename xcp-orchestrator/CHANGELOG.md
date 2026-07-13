# Changelog

All notable changes to the XCP-orchestrator workspace are documented in this file.

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
