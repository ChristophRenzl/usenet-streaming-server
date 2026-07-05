# Build stage — cargo-chef caches the dependency layer across builds.
FROM rust:1-bookworm AS chef
RUN cargo install cargo-chef --locked
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release

# Runtime stage — ffmpeg is required for HLS remuxing, par2 for the
# download-and-repair fallback on partially-available releases.
FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ffmpeg par2 ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/usenet-streaming-server /usr/local/bin/
VOLUME ["/config", "/data"]
EXPOSE 8080
ENV APP_DATABASE__PATH=/data/usenet-streamer.db \
    APP_STORAGE__DOWNLOAD_DIR=/data/downloads
ENTRYPOINT ["usenet-streaming-server", "--config", "/config/config.toml"]
