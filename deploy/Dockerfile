FROM node:22-bookworm-slim AS web
WORKDIR /source/crates/dashboard/web
COPY crates/dashboard/web/package*.json ./
RUN npm ci
COPY crates/dashboard/web/ ./
RUN npm run check && npm run build

FROM rust:1-bookworm AS service
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /source
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
COPY --from=web /source/crates/dashboard/web/build/ ./crates/dashboard/web/build/
RUN cargo build --locked --release -p travel

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 poppler-utils curl && rm -rf /var/lib/apt/lists/* && useradd --uid 10001 --create-home travel
COPY --from=service /source/target/release/travel /usr/local/bin/travel
USER travel
ENV TRAVEL_HOME=/data
VOLUME /data
EXPOSE 3150
HEALTHCHECK --interval=30s --timeout=5s CMD curl --fail --silent http://127.0.0.1:3150/api/health || exit 1
ENTRYPOINT ["travel"]
CMD ["serve", "--bind", "0.0.0.0"]
