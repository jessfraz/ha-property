FROM rust:1.99.1-bookworm AS build
RUN apt-get update && apt-get install --no-install-recommends -y cmake pkg-config && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY sync_migrations ./sync_migrations
COPY web ./web
RUN cargo build --locked --release --bins

FROM debian:bookworm-slim
ARG BUILD_VERSION=dev
LABEL org.opencontainers.image.title="Property" \
      org.opencontainers.image.source="https://github.com/jessfraz/ha-property" \
      org.opencontainers.image.licenses="MIT" \
      io.hass.name="Property" \
      io.hass.version="${BUILD_VERSION}" \
      io.hass.type="app" \
      io.hass.arch="aarch64|amd64"
RUN apt-get update && apt-get install --no-install-recommends -y ca-certificates gosu && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 property \
    && useradd --uid 10001 --gid property --no-create-home --shell /usr/sbin/nologin property \
    && install -d -o property -g property /data
COPY --from=build /src/target/release/property /usr/local/bin/property
COPY --from=build /src/target/release/ha-property /usr/local/bin/ha-property
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
ENV PROPERTY_BIND=0.0.0.0:8080 PROPERTY_DATABASE=/data/property.sqlite3
WORKDIR /data
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
