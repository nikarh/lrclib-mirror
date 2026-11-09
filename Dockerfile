# syntax=docker/dockerfile:1
# Builds the image from source. CI publishes the same image from a binary it has built already (Dockerfile.ci).

# The web search, built once, natively.
FROM --platform=$BUILDPLATFORM node:26.13.0-alpine AS web
WORKDIR /src/web
COPY web/package.json web/package-lock.json ./
RUN --mount=type=cache,target=/root/.npm npm ci --no-audit --no-fund
COPY web/ ./
RUN npm run build

# A static musl binary with the web search embedded.
FROM rust:1.99.1-alpine AS build
RUN apk add --no-cache musl-dev gcc ca-certificates
WORKDIR /src
COPY Cargo.toml Cargo.lock build.rs ./
COPY src src
COPY tools tools
COPY --from=web /src/web/dist web/dist
ARG LRCLIB_MIRROR_VERSION_SUFFIX=""
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    set -eux; \
    cargo build --locked --release -p lrclib-mirror; \
    mkdir -p /rootfs/etc/ssl/certs; \
    cp target/release/lrclib-mirror /rootfs/lrclib-mirror; \
    cp /etc/ssl/certs/ca-certificates.crt /rootfs/etc/ssl/certs/; \
    # A new named volume mounted here copies this mode, so it is writable whichever user the container runs as.
    mkdir -m 1777 /rootfs/data

FROM scratch
# Copying the directory's contents keeps the mode of /data, which copying the directory itself would not.
COPY --from=build /rootfs/ /
LABEL org.opencontainers.image.source="https://github.com/nikarh/lrclib-mirror" \
      org.opencontainers.image.description="LRCLIB's API (read-only) and web search on a local copy of its public database dump" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"
# nobody:nogroup. Any other uid:gid works too (`user:` in compose).
USER 65534:65534
EXPOSE 3300
VOLUME /data
HEALTHCHECK --interval=30s --timeout=5s CMD ["/lrclib-mirror", "healthcheck"]
ENTRYPOINT ["/lrclib-mirror"]
