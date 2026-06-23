# Agent Guidelines for confluence2md

This document defines rules that AI coding agents **must** follow when working on this project.

## Mandatory Checks

- **Always run `mise run ci` and confirm it passes** before considering any task complete. This runs fmt check, clippy, build, tests, and cargo-machete in sequence.

## Project Context

- See [README.md](README.md) for project overview, usage, and configuration.
- See [CONTRIBUTING.md](CONTRIBUTING.md) for development setup, project structure, and workflow.
- See [ARCHITECTURE.md](ARCHITECTURE.md) for the system architecture, module responsibilities, and processing flows.

## Key Rules

- **No confidential information:** Do not include any sensitive data (e.g. design documents, internal URL, credentials) in test data, code, logs, or documentation. If you need to use examples, use clearly fictional placeholders (e.g. `https://confluence.example.com`).
- **Runtime:** This project is implemented in **Rust** (Cargo workspace at the repository root). The implementation lives under `src/*.rs`.
- **Toolchain:** Use stable Rust (`rustup default stable` or `RUSTUP_TOOLCHAIN=stable`). MSRV tracks the latest stable release.
- **Modular architecture:** Source is split by responsibility under `src/`:
  - `src/main.rs` — binary entry point (CLI)
  - `src/lib.rs` — library root declaring the modules below
  - `src/utils.rs` — string, URL, filename helpers; central Confluence macro preprocessing dispatcher
  - `src/jira.rs` — Jira macro/link normalization; derives browse URLs from rendered REST response HTML and emits simple issue links
  - `src/confluence.rs` — Confluence REST API client; page/attachment fetching; regular HTML image rewriting after macro-specific asset handling
  - `src/drawio.rs` — draw.io resolution for storage macros and rendered included-content images; PNG `tEXt` chunk embedding
  - `src/plantuml.rs` — PlantUML source extraction; `!include` rewriting
  - `src/html.rs` — HTML→Markdown converter (built on `htmd` + `markup5ever_rcdom`); rewrites Confluence TOC heading links to Markdown heading anchors
  - `src/logger.rs` — leveled logging
- **Type co-location:** Types are defined in the module that owns them. Import them directly from the owning module (`use crate::utils::AssetsInfo;`). Do not introduce a shared `types.rs`.
- **Testing:** All exported functions must have tests. Use Rust's built-in test harness:
  - Unit tests live in `#[cfg(test)] mod tests { ... }` at the bottom of each `src/*.rs` file.
  - Integration tests that span modules live in `tests/*.rs` (e.g. `tests/integration.rs`).
  - Async tests use `#[tokio::test]`. HTTP integration tests use [`wiremock`](https://crates.io/crates/wiremock).
  - Tests share the fixture at `tests/input/confluence_content.json`.
- **Strict warnings:** The project must compile cleanly under `cargo clippy --all-targets -- -D warnings`. Avoid `unsafe`. Do not introduce `#[allow(...)]` to silence lints without justification.
- **No extra dependencies:** Do not add new crates without explicit approval. The current dependency set is listed in `Cargo.toml`.
- **Jira links:** Do not hardcode an organization-specific Jira browse URL. Jira issue links must be derived from rendered Jira issue links in the Confluence REST API response when available; otherwise keep the issue key as plain text.
- **draw.io assets:** Keep native-page draw.io macros and rendered draw.io images from included content on the same resolution path. draw.io processing must run before generic image downloading so `/download/attachments/{pageId}/...` still identifies external attachment pages, such as Table Excerpt Include sources.
- **Exported symbols:** Public functions are exported directly from their owning module via `pub fn`. Tests import them with `use confluence2md::<module>::<name>;`. Do not remove or rename public items without updating tests and callers.
- **Documentation:** Update `README.md`, `CONTRIBUTING.md`, `AGENTS.md`, and `ARCHITECTURE.md` as needed to reflect changes in usage, configuration, development workflow, or system architecture. When you modify the module structure, processing pipeline, or supported macros, you must update `ARCHITECTURE.md` accordingly.

## Build & Scripts Reference

| Command                                     | Purpose                                                 |
| ------------------------------------------- | ------------------------------------------------------- |
| `cargo build`                               | Build debug binary at `target/debug/confluence2md`      |
| `cargo run -- <pageUrl>`                    | Run CLI in dev mode                                     |
| `cargo test`                                | Run unit + integration tests                            |
| `mise run ci`                               | **Full validation** (fmt, clippy, build, test, machete) |
| `cargo clippy --all-targets -- -D warnings` | Lint directly via cargo                                 |
| `cargo fmt --all`                           | Apply rustfmt                                           |
| `cargo build --release`                     | Compile optimized binary to `target/release/`           |
| `cargo clean`                               | Remove build artifacts                                  |

