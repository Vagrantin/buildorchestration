```markdown
# XCP-orchestrator

## Overview

XCP-orchestrator is a Rust-based orchestration engine designed to automate and manage the build processes for several key components within the Xen ecosystem. It provides a centralized logic layer to handle complex workflows, ensuring consistency across different appliance types.

The orchestrator manages the builds for:
* XOA VM
* XOA-hl
* XO-lite-hl
* XCP-ng-ISO
* xoa-proxy

## Key Features

* Build Orchestration: Automated lifecycle management for multiple appliance targets.
* Version and State Management: Precise tracking of build versions and system states to ensure reproducible builds.
* GitHub Integration: Support for interacting with GitHub workflows and repositories.
* AI-Assisted Capabilities: Integration with Ollama to leverage LLMs within the orchestration workflow.
* Asynchronous Execution: High-performance, non-blocking operations powered by the Tokio runtime.
* Robust Status Monitoring: Real-time tracking of build progress and system health.

## Manual Agent Triggers

The dashboard (`build_report.html`) has "Run now" buttons for each agent.
They call a small local API (`orchestrator-api`, systemd unit
`orchestrator-api.service`) that starts the matching systemd unit
(`xcp-orchestrator.service`, `iso-agent.service`, `xoa-vm-agent.service`) on
request. It binds to `127.0.0.1:8787` only and requires a bearer token
(`/etc/xcp-hl-credentials/trigger_token`, set by `deploy.sh`) on every
request; the browser is prompted for the token on first use and remembers it
in `localStorage`.

`deploy.sh` installs and configures nginx to serve the static dashboard
(`/var/www/html/orchestrator`) and reverse-proxy `/api/` to
`127.0.0.1:8787` (config in `nginx/orchestrator.conf`, installed as the
`orchestrator` site and replacing the stock `default` site). Nothing further
to set up: after `deploy.sh` runs, the dashboard and its buttons are
reachable at `http://<host>/`.

## Tech Stack

* Language: Rust (Edition 2021)
* Runtime: Tokio (Async I/O)
* Serialization: Serde (JSON and data modeling)
* Networking: Reqwest (HTTP communication)
* Logging: Tracing (Structured logging and telemetry)
* Error Handling: Anyhow and Thiserror

