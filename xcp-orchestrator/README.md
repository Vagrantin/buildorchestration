# XCP-orchestrator

## Overview

XCP-orchestrator automate and manage the build processes for XCP-HL components. Each binary is its own systemd service on its own daily timer:

* **`iso-agent`** (04:00) — checks `xolite-ce` and `xoa-proxy` for upstream changes, dispatches/monitors their GitHub Actions builds, then builds and tags `xcp-ng-ce-iso`.
* **`xoa-vm-agent`** (04:00) — checks `xoa-hl` and `build-xoa-hl` for changes, pushes the `v{version}-ce{N}` tag that starts xoa-hl's RPM release, waits for it, then runs Packer locally against the target XCP-ng host to build and publish the XOA VM (XVA) image.
* **`orchestrator`** (09:00, after the two agents above) — aggregates their status/history, runs AI diagnostics on any failed build via Ollama, and renders the dashboard.
* **`orchestrator-api`** — API behind the dashboard's to trigger manual build and status update.
* **`shared`** — GitHub API client, version-state types, and status/report types used by all of the above.

The builds are managed across these agents:
* XOA VM / xoa-hl(via `xoa-vm-agent`)
* XO Lite CE / xoa-proxy / xcp-ng-ce-iso (via `iso-agent`)

## Key Features

* Version and State Management: Each agent persists its own version-state JSON under `/var/lib/xcp-hl-orchestrator/`, so a rebuild only fires when what it actually depends on has moved.
* Automated build: Build are triggered automatically daily at 4 AM.
* GitHub Integration: Trigger github workflow for build and releases.
* Monitoring: Dashboard to visualize where is the status of each build.

## Manual Agent Triggers

The dashboard (`build_report.html`) has "Run now" buttons for each agent.
They call a small API (`orchestrator-api`, systemd unit
`orchestrator-api.service`) that starts the matching systemd unit
(`xcp-orchestrator.service`, `iso-agent.service`, `xoa-vm-agent.service`) on
request. It binds to `0.0.0.0:8787` and requires a bearer token
(`/etc/xcp-hl-credentials/trigger_token`, set by `deploy.sh`) on every
request; the browser is prompted for the token on first use and remembers it
in `localStorage`.

The dashboard's JS calls this API directly on port 8787 (CORS-enabled), so
if a firewall is active on the host, allow inbound TCP 8787 from
wherever the dashboard is viewed from.

From the command line, `force-run.sh` runs the same agents outside the
dashboard, with the credentials each systemd unit declares:

```bash
sudo ./force-run.sh                 # run all agents in force mode
sudo ./force-run.sh xoa-vm-agent    # run only one agent
```

## Tech Stack

* Language: Rust (Edition 2021)
* Runtime: Tokio (Async I/O)
* Serialization: Serde (JSON and data modeling)
* Networking: Reqwest (HTTP communication)
* Logging: Tracing (Structured logging and telemetry)
* Error Handling: Anyhow and Thiserror
