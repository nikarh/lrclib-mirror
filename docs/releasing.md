# Releasing

There are no versions: the builds follow the web search's upstream and the dependencies.

- **Latest** (every push to `main`): the `v2` tag moves to the new commit, the `v2` release (GitHub's latest) gets
  the build's files, and the image is pushed as `v2` and `v2-sha-<commit>`. (The `latest` and `dev` releases and the
  `main`, `sha-<commit>` and `dev` images are the previous version's, built from the `legacy-sqlite` branch, and no
  longer updated: setups that follow them don't switch to this version unasked.)
- **Upstream** (daily, 04:17 UTC, or by hand): moves the web search submodule to the newest upstream commit and runs
  `cargo update`. When anything changed, it force-pushes the `upstream-update` branch (one commit on top of `main`)
  and opens a pull request, or updates the open one, listing the upstream commits, upstream changelog additions, the
  `Cargo.lock` changes and whether `patches/` still apply. Once merged, the branch is deleted and the next change
  opens a new pull request.
- **Dev** (every push to `upstream-update`): the `v2-dev` pre-release and the `v2-dev` image, to try an update before
  merging it.
- **Publish** (called by Latest and Dev): builds the static x86_64 binary (musl, web search embedded), wraps it in the
  `FROM scratch` image, checks the image runs read-only without capabilities, pushes it to `ghcr.io/nikarh/lrclib-mirror`,
  writes `SHA256SUMS` and its GPG signature, and uploads them with the binary.

| Asset | Contents |
| --- | --- |
| `lrclib-mirror-x86_64-unknown-linux-musl` | The static binary |
| `LICENSE-lrclib`, `LICENSE-lrclib-homepage` | The MIT licenses of LRCLIB's server (code ported to this crate) and web search (built in) |
| `SHA256SUMS`, `SHA256SUMS.asc` | Checksums of the files and their detached GPG signature |

Dependabot's pull requests (actions, Docker base images) are merged by **Auto-merge** once every required check is
green.

## One-time setup

The workflows act as the GitHub App `nikarh-release-bot` (*Contents* and *Pull requests* read & write), installed on
the repository: what it pushes and opens triggers CI and Dev, which `GITHUB_TOKEN` wouldn't. The `release`
environment allows the `main` and `upstream-update` branches and holds:

| Secret | Value |
| --- | --- |
| `APP_CLIENT_ID` | The app's client ID |
| `APP_PRIVATE_KEY` | A private key of the app (PEM) |
| `GPG_PRIVATE_KEY` | The armored secret signing key |
| `GPG_PASSPHRASE` | Its passphrase |

## Signing key

`46D28A16740135310E5676BCCEA1A710FEC72835`, ed25519, signing only, expires 2029-11-06. The public key is
`docs/signing-key.asc`.
