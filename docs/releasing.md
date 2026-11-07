# Releasing

There are no versions: the builds follow upstream.

- **Latest** (every push to `main`): the `latest` tag moves to the new commit, the `latest` release (GitHub's latest)
  gets the build's files, and the image is pushed as `main` and `sha-<commit>`.
- **Upstream** (daily, 04:17 UTC, or by hand): moves the submodules to the newest upstream commits and runs
  `cargo update`. When anything changed, it force-pushes the `upstream-update` branch (one commit on top of `main`)
  and opens a pull request, or updates the open one, listing the upstream commits, upstream changelog additions, the
  `Cargo.lock` changes and whether `patches/` still apply. Once merged, the branch is deleted and the next change
  opens a new pull request.
- **Dev** (every push to `upstream-update`): the `dev` pre-release and the `dev` image, to try an update before
  merging it.
- **Publish** (called by Latest and Dev): builds the static x86_64 binary (musl, web search embedded), wraps it in the
  `FROM scratch` image, checks the image runs read-only without capabilities, pushes it to `ghcr.io/nikarh/lrclib-mirror`,
  writes `SHA256SUMS` and its GPG signature, and uploads them with the binary.

| Asset | Contents |
| --- | --- |
| `lrclib-mirror-x86_64-unknown-linux-musl` | The static binary |
| `LICENSE-lrclib`, `LICENSE-lrclib-homepage` | The upstream projects' MIT licenses, which apply to the code built in |
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
