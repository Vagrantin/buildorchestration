# Changelog

All notable changes to the XCP-orchestrator workspace are documented in this file.

## Unreleased

### Fixed

- **iso-agent, xoa-vm-agent**: messages logged by the `shared` crate were
  filtered out, so the "only non-build files changed" decision of v0.3.4
  never showed in the Jenkins console, and iso-agent also hid `shared`
  warnings such as tag retries. Both now log `shared` at info, and other
  crates at warn.

## 2026-09-29 (xcp-orchestrator-v0.3.4)

### Changed

- **iso-agent, xoa-vm-agent**: a new commit on `main` no longer releases by
  itself when it touches only non-build files: `AGENTS.md`, `README.md`,
  `CLAUDE.md`, `CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`,
  `LICENSE`, `docs/`, and GitHub issue and PR templates. The agents compare
  the last built commit with `main` through GitHub's compare API; any other
  file, a truncated or failed comparison, or an empty state still counts as
  a change. On 2026-09-29 an `AGENTS.md`-only merge in five repos had
  produced component, RPM and image releases.

### Fixed

- **iso-agent**: when the ISO workflow dispatch fails, the tag pushed just
  before it is deleted again, so a failure no longer leaves a release-less
  `-ceN` tag (and the next run no longer skips a number). On 2026-09-29 a
  403 on the dispatch had left `v8.3-ce39` behind.

## 2026-09-29 (xcp-orchestrator-v0.3.3)

### Changed

- **iso-agent** (xcp-hl#129): a failed upstream bump check (GitHub error,
  rate limit, timeout) used to be reported as `Skipped`, the same as "no
  change", and the ISO could then build on a stale component pin. The run
  now aborts right after the checks, before any tag, dispatch or state
  write: it exits non-zero, so the Jenkins build fails, and the status file
  shows the new `CheckFailed` state per component. The next run retries.

### Removed

- **shared**: leftovers of the retired aggregator (xcp-hl#77): the unused
  `storage` module, `OllamaError` and `HeaderValueError`, `STATE_DIR`,
  `GHJob`/`GHJobsResponse`, `util::load_json_with_default` and
  `AgentStatus::load_from_file`. Stale "dashboard" comments reworded.

## 2026-09-28 (xcp-orchestrator-v0.3.2)

### Fixed

- **xoa-vm-agent**: v0.3.1 always set Packer's `iso_name`, but the xenserver
  plugin treats `iso_name` as "the ISO is already on the host": it skips the
  download and upload and halts when no such disk exists, so any build with
  no reusable disk failed at once. The agent now checks the build host
  through XAPI before Packer: it removes older detached uploads (including
  the plain-named one), sets `iso_name` only when the checksum-named disk
  exists, and otherwise lets Packer download and upload, then renames that
  upload to the checksum name after a successful build. The ISO is uploaded
  once per AlmaLinux release.
- **xoa-vm-agent**: a `--force` rebuild on a day that already had an image
  reused that day's release and then failed its upload, because GitHub
  refuses a second asset named `XOA-hl.xva`. The new XVA now goes up as
  `XOA-hl.xva.new`, the previous asset is deleted, and the new one is renamed,
  so the release keeps one `XOA-hl.xva` and its download URL throughout.

### Removed

- **orchestrator, orchestrator-api, orchestratorhost** (xcp-hl#77): both
  agents run under Jenkins, which provides status, history and manual runs.
  Removed the `orchestrator` aggregator (dashboard `build_report.html`,
  run history, Ollama diagnostics of failed runs), the `orchestrator-api`
  trigger service on `0.0.0.0:8787`, the systemd units, `deploy.sh` (which
  wrote the plaintext `/etc/xcp-hl-credentials/`), `force-run.sh`, and the
  `orchestratorhost/` VM builder. From `shared`: the `ollama` module,
  `extract_failed_log_context`, `PipelineStatus`, `get_badge_class` and the
  dashboard and Ollama constants. The agents are unchanged.

## 2026-09-28 (xcp-orchestrator-v0.3.1)

### Fixed

- **xoa-vm-agent**: the AlmaLinux ISO disk on the build host is now named
  after its checksum (`iso_name`, e.g.
  `AlmaLinux-9-latest-x86_64-minimal-7762a4b45a66.iso`). Packer reuses a disk
  of that exact name without uploading, so an unchanged ISO is uploaded once
  and reused by every later build; a new AlmaLinux release gets a new name and
  is uploaded once. The disk used to be named after the URL only, so a newer
  "latest" would have kept reusing the old disk. After a successful build the
  agent removes older detached uploads of the same ISO (plugin marker
  `other_config temp=temp`, no VBD) and keeps the current one; a kept failed
  build's ISO is still attached, so it stays.

## 2026-09-28 (xcp-orchestrator-v0.3.0)

### Fixed

- **xoa-vm-agent**: the kickstart set the root password with an unquoted
  `rootpw --plaintext`, and the Packer template embedded passwords without
  JSON escaping. Kickstart splits lines like a shell, so a password holding
  `#`, quotes, backslashes or spaces was silently changed and Packer never got
  in over SSH. The kickstart now carries a SHA-512 crypt hash
  (`rootpw --iscrypted --allow-ssh`), and template values are JSON-encoded.

### Changed

- **xoa-vm-agent**: can run under Jenkins (Vagrantin/xcp-hl#76).
  `XCPNG_PASSWORD` and `ALMALINUX_ROOT_PASSWORD` go through `load_credential`
  like the GitHub token, so they come from systemd credentials or from a
  resolved environment variable. `XOA_BUILD_CONFIG` points at another
  build.config, and the new `MIN_FREE_DISK_GB` key replaces the hardcoded
  100 GB free-space check (default unchanged). The build config is now read
  before any tag is pushed.
- **xoa-vm-agent**: a successful build now removes its VM and disks (the
  uploaded AlmaLinux ISO included) from the XCP-ng host through XAPI; a failed
  build still leaves them for inspection. Packer keeps `keep_vm: always`,
  because the xenserver plugin ignores `-on-error` and would otherwise clean
  up failures too. The VM is identified by the uuid Packer reports, so VMs
  left by earlier failed runs are never touched.

## 2026-09-28

### Fixed

- **iso-agent**: after a tag collision, `create_and_push_tag` moves on to
  the next free tag, but the agent still recorded the counter it *asked*
  for. The ISO state ended up at `ce_counter: 21` with `last_tag:
  v8.3-ce38`, so every later build first walked through about 17 existing
  tags (and would stop at the 99-attempt cap). The counter is now read back
  from the tag actually pushed, for the ISO, xolite-ce and xoa-proxy alike.
- **shared / iso-agent / xoa-vm-agent**: `locate_tag_triggered_run` listed the
  latest push runs of *every* workflow in the repo and took the first one whose
  branch matched the tag. Since `secret-scan` also runs on every tag push and
  finishes in seconds, it could be picked instead of the RPM build: iso-agent
  then saw "success" and dispatched the ISO build while the xolite-ce RPM was
  still building, so `v8.3-ce37` failed at "Fetch community xo-lite RPM" (the
  ISO was dispatched at 19:01:11, the release published at 19:02:27). The
  lookup now takes the workflow file and only lists that workflow's runs
  (`build-xolite-ce.yml`, `xoa-proxy.yml`, `build-xoa.yml`).

### Added

- **shared**: `load_credential` falls back to an environment variable of the
  same name when `CREDENTIALS_DIRECTORY` is not set, so Jenkins can inject
  vault-resolved secrets. An unresolved `pass://` reference is rejected rather
  than used as a secret. The systemd path is unchanged, so rolling back to the
  timers needs no rebuild.
- **CI**: `.github/workflows/xcp-orchestrator.yml` tests every change and, on
  `xcp-orchestrator-v*` tags, publishes static x86_64 musl builds of
  `iso-agent` and `xoa-vm-agent` with sha256 files. Jenkins pins one.

## 2026-08-18

### Changed

- **xoa-vm-agent**: xoa-hl adopted the ce release model (fat RPM, one release
  per build, `v{version}-ce{N}` tags, no tarball asset), so the agent now drives
  it the way iso-agent drives xolite-ce. The version oracle is the committed
  `UPSTREAM_XO` pin, read over the contents API: the version string is
  `{XO_VERSION}_{XO_COMMIT:0:8}`, e.g. `5.113.2_e281c536`. Nothing is rebuilt
  when both that version and the repo HEAD match the persisted state; a version
  change restarts the ce counter at 1, anything else takes the next counter.
  The agent then pushes `v{version}-ce{N}` (`create_and_push_tag`, whose 422
  retry means the returned tag is the authoritative one), which is what starts
  the RPM workflow, and waits for its run. Counter, tag and source SHA are
  persisted under a new `rpm` object in the agent's version-state file (absent
  in older files, defaulted), and backfilled from the published releases when
  local state is lost. xoa-hl's release list still carries legacy `xoa-image-*`
  tags and the bare `v5.113.2_e281c536` tag, so the release scan only accepts
  tags that parse as `v{version}-ce{N}` whose version half matches the pin.
  The `workflow_dispatch` trigger is gone: the workflow builds on tag push.

### Fixed

- **xoa-vm-agent**: `resolve_xoa_hl_rpm_url` took the first `.rpm` asset found
  while walking the release list. GitHub returns assets oldest-first, so this
  baked a July 2026 RPM (no systemd units) into every image built since. The
  URL is now resolved against `releases/tags/{tag}` for the exact tag the run
  is building, and the filename is read from the asset, never predicted.

### Added

- **xoa-vm-agent**: after the tag push the agent waits for the release carrying
  that tag to appear *with* its RPM asset (`RPM_RELEASE_TIMEOUT`, 15 minutes,
  polled every 20 s) before resolving the URL. The workflow completing is not
  enough: the release is published in its last step and the API lags it.
- `shared::fetch_repo_text_file` (generalised out of `fetch_pinned_xolite_tag`),
  `shared::fetch_xoa_hl_upstream_pin` / `parse_upstream_xo` / `UpstreamXoPin`,
  and `shared::fetch_release_by_tag`, all with unit tests.

## 2026-07-30

### Changed

- **xoa-vm-agent**: XVA image releases are now published on
  `Vagrantin/build-xoa-hl`, the repo the image is actually built from,
  instead of `Vagrantin/xoa-hl`, which only holds the XO source and its RPM
  releases (fixes `Vagrantin/xcp-hl#22`). The tag format is unchanged
  (`xoa-image-{date}-{sha7}`, `{sha7}` still the xoa-hl source commit), but the
  release is no longer anchored with `target_commitish`: that SHA does not
  exist in `build-xoa-hl`, so the tag is created on its default branch and the
  source commit is recorded in the release body. The Phase 1 "already built?"
  check now reads image releases from `build-xoa-hl` and RPM releases from
  `xoa-hl` in two separate calls. Image releases published before this move
  stay on `xoa-hl` so already-shipped ISOs keep resolving them, which is why
  `resolve_xoa_hl_rpm_url` still has to skip `xoa-image-*` tags.

## 2026-07-13 (fifth batch)

### Fixed

- **xoa-vm-agent**: infrastructure values (XCP-ng host/user, SR, network, VM
  name/disk/memory, ISO and xe-guest-utilities URLs) were baked into
  `BuildConfig::default()`, the `build.config` concept from the old shell
  orchestrator was lost in the Rust rewrite. The agent now overlays
  `/etc/xcp-orchestrator/build.config` (shell-style KEY="VALUE", installed by
  `deploy.sh` from `xoa-vm-agent/build.config.sample` if missing, never
  overwritten) on those defaults. A missing file warns and keeps the old
  defaults; a malformed file is fatal. Secrets stay exclusively in systemd
  `LoadCredential`. `SR_NAME`, `VM_DISK_SIZE_MB` and `VM_MEMORY_MB` are newly
  configurable (they were literals in the Packer template).

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
  repo carries two kinds of releases, RPM releases from the `build-xoa.yml`
  workflow and this agent's own `xoa-image-{date}-{sha7}` XVA releases, and the
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
  workspace split, `llm_hint` was a hardcoded string, which is why no analysis
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
  orphaning the recorded "last built" state, and state was only saved after a fully
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
  was a hard failure, the collision retry in `create_and_push_tag` only knew how
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
