# Contributing to confluence2md

Thank you for your interest in contributing! This guide covers everything you need to get started with the Rust implementation.

## Prerequisites

```bash
curl https://mise.run | sh
echo 'eval "$(~/.local/bin/mise activate bash)"' >> ~/.bashrc
source ~/.bashrc
mise trust mise.toml
mise install
```

## Getting Started

```bash
git clone <repo-url>
cd confluence2md

# Build & run all checks
mise run ci
```

## Project Structure

```
confluence2md/
├── Cargo.toml
├── Cargo.lock
├── src/
│   ├── main.rs                    # Binary (CLI) entry point
│   ├── lib.rs                     # Library root
│   ├── utils.rs                   # String/URL/filename helpers; macro preprocessing
│   ├── confluence.rs              # Confluence REST API client
│   ├── drawio.rs                  # draw.io fallback handling, PNG tEXt embedding
│   ├── plantuml.rs                # PlantUML source extraction & rewriting
│   ├── export_html.rs           # HTML→Markdown converter
│   └── logger.rs                  # Leveled logging
├── tests/
│   └── integration.rs             # Cross-module integration tests
├── test/
│   └── input/                     # Shared JSON fixtures
├── README.md
├── CONTRIBUTING.md
└── AGENTS.md
```

Logic modules live under `src/*.rs`. Types are co-located with the module that owns them — there is no shared `types.rs`. Unit tests live inline in each module; cross-module tests live in `tests/`.

## Development Workflow

### [OPTIONAL] For windows build to succeed

- Install Visual Studio Build Tools (recommended, smallest install):
   - Download the Visual Studio Installer from: https://visualstudio.microsoft.com/downloads/ (scroll down to Tools for Visual Studio → Build Tools for Visual Studio 2022)
   - Run the installer.
   - Select "C++ build tools" workload.
   - Make sure these individual components are checked:
      - MSVC v143 - VS 2022 C++ x64/x86 build tools (Latest)
      - Windows 10/11 SDK (latest version)
   - Click Install (it’s in GB).
- After installation, restart your terminal / VS Code (very important — refreshes PATH). Follow below build instructions

### Build

```bash
cargo build              # debug binary at target/debug/confluence2md
cargo build --release    # optimized binary at target/release/confluence2md
```

### Run in development mode

```bash
cargo run -- <pageUrl>
```

Optional CLI flags / environment variables (same as the legacy TS implementation):

| CLI flag                    | Env var                          | Purpose                                   |
| --------------------------- | -------------------------------- | ----------------------------------------- |
| `--output-path <DIR>`       | `CONFLUENCE2MD_OUTPUT_PATH`      | Output directory for the Markdown file    |
| `--log-level <LEVEL>`       | `CONFLUENCE2MD_LOG_LEVEL`        | `DEBUG` \| `INFO` \| `WARNING` \| `ERROR` |
| `--table-conversion <MODE>` | `CONFLUENCE2MD_TABLE_CONVERSION` | `default` \| `always`                     |

Authentication: set `CONFLUENCE2MD_PERSONAL_ACCESS_TOKEN` in your environment. See [README.md](README.md) for full details.

### Tests

```bash
mise run ci
```

> **Important:** Always ensure `mise run ci` passes before submitting changes.

### Lint & Format

```bash
cargo clippy --all-targets -- -D warnings
cargo fmt --all
cargo fmt --all -- --check       # CI-style check
```

The codebase must build cleanly with all clippy warnings treated as errors. Do not introduce `#[allow(...)]` to silence lints without justification.

### Clean build artifacts

```bash
cargo clean
```

## Testing Guidelines

- Unit tests live in `#[cfg(test)] mod tests { ... }` at the bottom of each `src/*.rs` file.
- Integration tests that span modules live in `tests/*.rs` (currently `tests/integration.rs`).
- Async tests use `#[tokio::test]`.
- HTTP integration tests use [`wiremock`](https://crates.io/crates/wiremock) (see `src/confluence.rs` tests).
- Shared fixtures are at `tests/input/confluence_content.json` (also used by the legacy TS tests).
- All exported (`pub`) functions must have corresponding tests.

## Code Style

- Idiomatic Rust 2021 edition.
- Prefer `Result<T, anyhow::Error>` for fallible internal operations; surface user-facing errors with `anyhow::Context`.
- Prefer borrow over clone; use `&str` / `&[T]` in function signatures unless ownership is required.
- Avoid `unsafe`.
- Public APIs should have brief `///` doc comments.
- Type-narrow at boundaries — do not propagate `serde_json::Value` deeper than the REST client.

## Making Changes

1. Create a branch.
2. Make edits in the appropriate `src/*.rs` module (and add or update tests).
3. Run `mise run ci`.
4. Commit your changes with a [Conventional Commit](https://www.conventionalcommits.org/en/v1.0.0/) message and [sign your commits](https://docs.github.com/en/authentication/managing-commit-signature-verification/signing-commits).
5. Submit a pull request.

## Releasing

1. Update the version in `Cargo.toml` following [Semantic Versioning](https://semver.org/).
2. Commit the change:
   ```bash
   git add Cargo.toml Cargo.lock
   git commit -m "version: x.y.z"
   ```
3. Create a git tag:
   ```bash
   git tag vx.y.z -m "Release version x.y.z"
   ```
4. Push the tag to GitHub:
   ```bash
   git push origin vx.y.z
   ```
5. Publish a GitHub Release for the tag automatically using GitHub Workflow
