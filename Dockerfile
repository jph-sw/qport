FROM rust:1.85-slim AS builder

WORKDIR /app
COPY Cargo.toml ./
COPY src ./src

RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/qport /usr/local/bin/

ENV PORT_FILE=/gluetun/forwarded_port
ENV QB_URL=http://qbittorrent:8080
ENV QB_USER=admin
ENV QB_PASS=adminadmin
ENV RUST_LOG=info

ENTRYPOINT ["qport"]
