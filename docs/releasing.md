# Releasing

There are no versions: the builds follow `main`.

- **Latest** (every push to `main`): the `v2` tag moves to the new commit, the `v2` release ("Latest", GitHub's
  latest) gets the build's files, and the image is pushed as `v2`, `main` and `v2-sha-<commit>`. (The `v1` and
  `v1-dev` releases and images, and the `sha-<commit>` images, are the previous version's, built from the
  `legacy-sqlite` branch, and no longer updated.)
- **Publish** (called by Latest): builds the static x86_64 binary (musl, web search embedded), wraps it in the
  `FROM scratch` image, checks the image runs read-only without capabilities, pushes it to `ghcr.io/nikarh/lrclib-mirror`,
  writes `SHA256SUMS` and its GPG signature, and uploads them with the binary.

| Asset | Contents |
| --- | --- |
| `lrclib-mirror-x86_64-unknown-linux-musl` | The static binary |
| `LICENSE-lrclib`, `LICENSE-lrclib-homepage` | The MIT licenses of LRCLIB's server (code ported to this crate) and web search (reimplemented in `web/`) |
| `SHA256SUMS`, `SHA256SUMS.asc` | Checksums of the files and their detached GPG signature |

Dependabot's pull requests (actions, Docker base images) are merged by **Auto-merge** once every required check is
green.

## One-time setup

The workflows act as the GitHub App `nikarh-release-bot` (*Contents* and *Pull requests* read & write), installed on
the repository, to move the release tag and update the release. The `release` environment allows the `main` branch
and holds:

| Secret | Value |
| --- | --- |
| `APP_CLIENT_ID` | The app's client ID |
| `APP_PRIVATE_KEY` | A private key of the app (PEM) |
| `GPG_PRIVATE_KEY` | The armored secret signing key |
| `GPG_PASSPHRASE` | Its passphrase |

## Signing key

`46D28A16740135310E5676BCCEA1A710FEC72835`, ed25519, signing only, expires 2029-11-06. The public key is
`docs/signing-key.asc`.
