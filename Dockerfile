FROM node:24-bookworm-slim AS node
FROM ghcr.io/astral-sh/uv:0.11.2 AS uv
FROM rust:1.95-bookworm AS build
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY static ./static
RUN cargo test --locked && cargo build --release --locked

FROM python:3.12-slim-bookworm
COPY --from=uv /uv /usr/local/bin/uv
WORKDIR /opt/youtube-downloader-runtime
ENV UV_PROJECT_ENVIRONMENT=/opt/ytdlp UV_LINK_MODE=copy UV_PYTHON_DOWNLOADS=never
COPY pyproject.toml uv.lock ./
RUN uv sync --locked --no-dev --no-install-project \
    && apt-get update && apt-get install -y --no-install-recommends ca-certificates ffmpeg tini && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 youtube-downloader && useradd --uid 10001 --gid youtube-downloader --no-create-home youtube-downloader \
    && mkdir -p /data && chown youtube-downloader:youtube-downloader /data
COPY --from=build /build/target/release/youtube_downloader /usr/local/bin/youtube_downloader
COPY --from=node /usr/local/bin/node /usr/local/bin/node
ENV APP_BIND=0.0.0.0:3000 APP_DATA=/data YTDLP_PATH=/opt/ytdlp/bin/yt-dlp
USER 10001:10001
EXPOSE 3000
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/youtube_downloader"]
