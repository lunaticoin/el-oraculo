FROM rust:1.93-bookworm AS builder

RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY src/ src/

RUN cargo build --release --locked

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

RUN useradd -m -s /bin/bash oracle

COPY --from=builder /build/target/release/bitcoin-price-oracle /usr/local/bin/

RUN printf '#!/bin/sh\nset -e\nchown -R oracle:oracle /data\nexec su -s /bin/sh oracle -c "bitcoin-price-oracle $*"\n' > /usr/local/bin/entrypoint.sh \
    && chmod +x /usr/local/bin/entrypoint.sh

RUN mkdir -p /data

EXPOSE 3200

ENTRYPOINT ["entrypoint.sh"]
