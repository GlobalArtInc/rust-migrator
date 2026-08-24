FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --features bin --bin sqlmig

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/sqlmig /usr/local/bin/sqlmig
# the schema comes off a mount: one image runs every one of them
VOLUME ["/migrations"]
ENTRYPOINT ["/usr/local/bin/sqlmig"]
CMD ["status"]
