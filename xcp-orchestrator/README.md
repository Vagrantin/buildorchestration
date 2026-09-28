# XCP-orchestrator

## Overview

XCP-orchestrator holds the build agents for XCP-HL components. They run as
Jenkins jobs on the Jenkins host (`jenkins-infra`: `docs/iso-agent.md`,
`docs/xoa-vm-agent.md`), nightly at 19:00 UTC:

* **`iso-agent`**: checks `xolite-ce` and `xoa-proxy` for upstream changes, dispatches and monitors their GitHub Actions builds, then builds and tags `xcp-ng-ce-iso`.
* **`xoa-vm-agent`**: checks `xoa-hl` and `build-xoa-hl` for changes, pushes the `v{version}-ce{N}` tag that starts xoa-hl's RPM release, waits for it, then runs Packer against the Test XCP-ng host to build and publish the XOA VM (XVA) image.
* **`shared`**: GitHub API client, version-state and status types used by both agents.

Status, history and manual runs are Jenkins' own: the build console and
history, and "Build with Parameters" (`FORCE`) on each job.

## Key Features

* Version and state management: each agent persists its version-state JSON under `/var/lib/xcp-hl-orchestrator/` (bind-mounted from the Jenkins host and backed up nightly), so a rebuild only fires when what it depends on has moved.
* GitHub integration: tags and dispatches the component workflows and follows them to a published release.

## Secrets and configuration

The agents read `GITHUB_TOKEN`, and for `xoa-vm-agent` also
`XCPNG_PASSWORD` and `ALMALINUX_ROOT_PASSWORD`, from the environment, where
Jenkins' `with-secrets` puts the values resolved from the vault. An
unresolved `pass://` reference is rejected. `xoa-vm-agent` reads its build
config from the path in `XOA_BUILD_CONFIG` (the job renders it per run).
They still accept systemd credentials (`CREDENTIALS_DIRECTORY`), but no
systemd deployment is shipped any more (xcp-hl#77).

## Releases

Binaries come from `xcp-orchestrator-v*` releases built by
`.github/workflows/xcp-orchestrator.yml` (static x86_64 musl). Jenkins' agent
image pins one release by version and sha256 (`jenkins-infra/agent/Dockerfile`).

## History

Until 2026-09-28 the agents ran on a dedicated orchestrator VM from systemd
timers, with an `orchestrator` aggregator (dashboard `build_report.html` and
Ollama diagnostics of failed runs) and an `orchestrator-api` trigger service
on port 8787. Both were retired with that VM in xcp-hl#77; they remain in git
history.

## Tech Stack

* Language: Rust (Edition 2021)
* Runtime: Tokio (Async I/O)
* Serialization: Serde (JSON and data modeling)
* Networking: Reqwest (HTTP communication)
* Logging: Tracing (Structured logging and telemetry)
* Error Handling: Anyhow and Thiserror
