When adding dependencies don't edit manifests by hand - use the package managers (`cargo add`) so the latest versions
are used. Exceptions, kept at the major version LRCLIB's server uses so that answers stay byte for byte the same:
axum (query and path rejection messages), tantivy (search scores), serde_yaml (Lyricsfiles), secular and collapse (name
normalization).
Commit your changes. Use conventional commits. Split logical changes into individual commits.
Before committing run `mise run check` (formatting, clippy, tests, that the patch applies and the web search builds,
cargo-deny, action pins).

Commit messages, code, tests and docs describe bugs and their fixes in general terms: what goes wrong, under which
conditions, and what changed. Never frame them around a particular deployment, and don't carry over its details.

## Layout

- `upstream/lrclib-homepage` - the web search, a git submodule. Never commit changes inside it: change it through
  `patches/lrclib-homepage/*.patch` (`mise run patch` applies them, skipping applied ones). To change a patch: edit
  the submodule, then `git -C upstream/lrclib-homepage diff HEAD --binary > patches/lrclib-homepage/...`.
- `src/` - the library and the binary:
  - `sqlite.rs` reads the rows of an SQLite file from a stream (the dump is never unpacked to disk);
  - `store/` the store format: `import.rs` builds it from a dump, `read.rs` reads it;
  - `text.rs`, `lyricsfile.rs` - name normalization, LRC and Lyricsfile handling ported from LRCLIB's server;
  - `search.rs` - the Tantivy index, with LRCLIB's schema and queries;
  - `api.rs` - the lookups and their responses; `web.rs` the routes and the embedded web search;
  - `mirror.rs` - which store is served, imports and their continuation, the switch to a new one; `dump.rs` - listing,
    download (resumed when it breaks) and gunzip;
  - `config.rs`.
- `tools/compare` - compares answers and timings with LRCLIB's server on the same dump (`mise run compare`).
- `licenses/LICENSE-lrclib` - the license of LRCLIB's server, whose code `text.rs`, `lyricsfile.rs`, `search.rs` and
  `api.rs` port.
- `build.rs` embeds `upstream/lrclib-homepage/dist` (`mise run web:build`) and its commit for `--version`.

## Rules

- The API answers exactly as LRCLIB's server does (bodies, statuses, errors). A change to what it answers, or to
  what the store leaves out and computes back, is checked with `tools/compare` against LRCLIB's server on a dump.
  `/api/v2/search` (lrclib.net's web search, not in LRCLIB's published code) is checked against lrclib.net.
- The API stays read-only: only the lookups (`/api/get`, `/api/get/{id}`, `/api/search`, `/api/v2/search`), never
  publish, challenge or flag. What the web search needs and LRCLIB's server lacks goes under `/api/mirror/`, leaving
  the LRCLIB routes as they are.
- Reads allocate as little as possible: blocks go into per-thread buffers, records are borrowed views.
- A change to the store format bumps `FORMAT_VERSION`; a change to how left-out text is computed back (normalization,
  LRC, Lyricsfile YAML) bumps the `VERSION` in `store::derivations`. Stores of another version are imported again.
- Interruptions: an import must be continuable at any point (`tests/interrupt.rs` kills it and compares byte for
  byte), a store is marked finished (`meta.json`) only once all its files are on disk, nothing is removed in place
  (renamed to `.removing` first), and a store is removed only when shown damaged.
- The binary uses mimalloc: musl's allocator makes the static build several times slower.
- A change to the configuration updates `README.md` (and `src/config.rs`'s help) in the same commit.
- The container image must keep working read-only, as any uid, with no capabilities: no writes outside the data
  directory, no listening on ports below 1024, nothing needing a shell or files from a distribution.
- `Cargo.lock` is refreshed by the daily Upstream workflow (`cargo update`); Dependabot only handles actions and Docker.
