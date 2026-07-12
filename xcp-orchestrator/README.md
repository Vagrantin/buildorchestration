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

## Tech Stack

* Language: Rust (Edition 2021)
* Runtime: Tokio (Async I/O)
* Serialization: Serde (JSON and data modeling)
* Networking: Reqwest (HTTP communication)
* Logging: Tracing (Structured logging and telemetry)
* Error Handling: Anyhow and Thiserror

