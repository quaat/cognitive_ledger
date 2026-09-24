# Codex Cloud configuration

## Supported surfaces
Codex automatically discovers scoped `AGENTS.md` guidance. The current Codex customization model distinguishes repository `.codex/config.toml`, skills, installable plugins, MCP, and hooks. This repository is also a valid plugin root (`.codex-plugin/plugin.json`) whose canonical `skills/` can be installed/registered; merely committing a plugin does not prove a hosted workspace enabled it. No project Codex hooks are configured because hosted activation is environment/admin dependent; shared scripts remain callable adapters.

Configure the Cloud environment to run from repository root, install the stable toolchain from `rust-toolchain.toml`, allow crates.io/GitHub access during dependency fetch, and enable the repository plugin/capabilities according to workspace policy. Skills may also be read directly from `skills/` when explicitly requested.

Recommended tools are filesystem, Git, shell, Cargo/Rust, Python, `jq`, read-only official documentation, and Docker/Compose where granted. Avoid write-capable external MCP unless a real workflow requires it; never store credentials here.

## Hosted limitations observed
On 2026-09-24 this environment lacked Docker/Compose. Direct network requests and the official Codex manual helper were blocked by the proxy, although Cargo dependency resolution was available later through its configured channel. Accordingly, runtime Compose evidence is delegated to Docker-capable CI and is not claimed locally. Fluree execution remains blocked on an image digest and API adapter; CI exposes that blocker rather than claiming coverage.

Use managed subagents for bounded independent review/research (protocol, invariants, tests, security, reference behavior). Do not let agents concurrently modify the same core files; the primary session integrates decisions and evidence.
