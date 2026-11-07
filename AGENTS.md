When adding dependencies don't edit manifests by hand - use the package managers (`cargo add`) so the latest versions
are used. Exception: crates the upstream server crate also uses (axum, tokio, tower-http, reqwest, rusqlite, ...) are
added at upstream's major version, so that they are compiled once and their types match upstream's.
Commit your changes. Use conventional commits. Split logical changes into individual commits.
Before committing run `mise run check` (formatting, clippy, tests, that the patches apply and the web search builds,
cargo-deny, action pins).

Commit messages, code, tests and docs describe bugs and their fixes in general terms: what goes wrong, under which
conditions, and what changed. Never frame them around a particular deployment, and don't carry over its details.

## Layout

- `upstream/lrclib`, `upstream/lrclib-homepage` - the upstream projects, as git submodules. Never commit changes inside
  them: change them through `patches/<project>/*.patch` (`mise run patch` applies them, skipping applied ones).
  To change a patch: edit the submodule, then `git -C upstream/<project> diff HEAD --binary > patches/<project>/...`.
- `src/` - the binary: `mirror.rs` (which dump is served, downloads, the switch to a new dump), `dump.rs` (listing,
  download, checks), `web.rs` (the read-only API routes and the embedded web search), `config.rs`.
- `build.rs` embeds `upstream/lrclib-homepage/dist` (`mise run web:build`) and the upstream commits for `--version`.

## Rules

- The API stays read-only: only upstream's lookup handlers are routed, never publish, challenge or flag.
- Keep the patches small; prefer doing things in this crate over patching upstream.
- A change to the configuration updates `README.md` (and `src/config.rs`'s help) in the same commit.
- The container image must keep working read-only, as any uid, with no capabilities: no writes outside the data
  directory, no listening on ports below 1024, nothing needing a shell or files from a distribution.
- `Cargo.lock` is refreshed by the daily Upstream workflow (`cargo update`); Dependabot only handles actions and Docker.
- The `deny.toml` advisory ignores come with upstream's dependencies; drop each once upstream has moved on.
