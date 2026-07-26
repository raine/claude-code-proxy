# Repository Guidelines

## Project Structure & Module Organization

This is a single Rust 2024 crate. `src/main.rs` defines the Clap CLI; `src/lib.rs` exports modules. `src/server.rs` handles Axum requests, `src/registry.rs` routes models, and `src/provider.rs` defines provider interfaces. Keep backend-specific authentication, clients, and translation under `src/providers/{codex,grok}/`; Anthropic types belong in `src/anthropic/`. Cross-cutting concerns remain in top-level modules.

Integration tests live in `tests/*.rs`, with fixtures in `tests/fixtures/`; unit tests stay beside implementation code. Scripts, hooks, and CI live in `scripts/`, `hooks/`, and `.github/workflows/`. Do not commit `target/` or `dist/`.

## Build, Test, and Development Commands

| Command | Purpose |
| --- | --- |
| `cargo run -- serve` | Run the proxy locally with the monitor when interactive. |
| `cargo run -- demo` | Exercise the TUI with simulated traffic. |
| `cargo build --all` | Build the crate and all targets. |
| `cargo test --all` | Run unit and integration tests. |
| `just check` | Run formatting, Clippy, build, and tests through Checkle. |

Use `cargo run -- serve --no-monitor` for plain logs. Install the pre-commit shim with `just install-hooks`.

## Coding Style & Naming Conventions

Use default `rustfmt` formatting and four-space indentation. Run `cargo fmt --all --check` and `cargo clippy --all-targets -- -D warnings`. Modules, functions, and tests use descriptive `snake_case`; types use `PascalCase`, and constants use `SCREAMING_SNAKE_CASE`. Preserve provider boundaries and centralize shared behavior.

## Testing Guidelines

Use Rust's `#[test]` or `#[tokio::test]` harnesses. Name tests after observable behavior, for example `unknown_model_returns_400_with_summary`. Run one integration target with `cargo test --test cli`, or one case with `cargo test --test cli version_aliases_print_expected_version`. No numeric coverage threshold is configured; every behavior change should include a regression test.

## Commit & Pull Request Guidelines

Use a short imperative subject without a trailing period, such as `improve Codex retry recovery`. Conventional Commit prefixes are optional; release commits use `release v0.1.N`. Keep commits focused. Pull requests should explain the change, list validation, link relevant issues, and include screenshots for visible TUI changes. Run `just check` before review.

## Security & Configuration Tips

Configuration precedence is environment, then `config.json`, then built-in defaults; use `CCP_CONFIG_DIR` for isolated runs. Never commit provider credentials or traffic captures, which can contain prompts and tool content. The proxy does not authenticate incoming requests, so preserve the default loopback-only trust boundary.
