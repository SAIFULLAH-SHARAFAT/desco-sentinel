# Runs the two-way WhatsApp bot (`serve`). Mount a volume on /app/state to keep alert state.
FROM rust:1-slim AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake clang pkg-config \
    && rm -rf /var/lib/apt/lists/*
RUN cargo build --release --locked

FROM debian:stable-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --home /app sentinel \
    && mkdir -p /app/state /app/config \
    && chown -R sentinel /app
WORKDIR /app
COPY --from=build /src/target/release/desco-sentinel /usr/local/bin/desco-sentinel
USER sentinel
ENV STATE_DIR=/app/state PORT=8080
EXPOSE 8080
VOLUME ["/app/state"]
HEALTHCHECK --interval=60s --timeout=5s CMD ["sh", "-c", "exec 3<>/dev/tcp/127.0.0.1/${PORT} && printf 'GET /health HTTP/1.0\\r\\n\\r\\n' >&3 && grep -q ok <&3"]
ENTRYPOINT ["desco-sentinel"]
CMD ["serve"]
