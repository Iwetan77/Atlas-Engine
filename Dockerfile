FROM rust:1-bookworm AS rust-build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --locked --release -p engine-service

FROM node:22-bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR /app/privy-bridge
COPY crates/engine-service/privy-bridge/package.json crates/engine-service/privy-bridge/package-lock.json ./
RUN npm ci --omit=dev
COPY crates/engine-service/privy-bridge/server.mjs crates/engine-service/privy-bridge/paradex-onboarding.mjs crates/engine-service/privy-bridge/signer-config.mjs crates/engine-service/privy-bridge/trade-subkey.mjs crates/engine-service/privy-bridge/sui-swap.mjs ./
COPY --from=rust-build /src/target/release/engine-service /usr/local/bin/engine-service
COPY scripts/run-render.sh /usr/local/bin/run-render
RUN chmod 755 /usr/local/bin/run-render
ENV PRIVY_BRIDGE_URL=http://127.0.0.1:3101
EXPOSE 10000
CMD ["/usr/local/bin/run-render"]
