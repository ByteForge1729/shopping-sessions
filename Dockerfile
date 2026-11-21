# Shopping Sessions: a single static-ish Rust binary.
# The scorers are linux/amd64 Intel Xeon (Cascade Lake or newer) and build this file
# natively; no FROM line pins a platform, so it also builds on arm64 for local loops.
FROM rust:1.86-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
# The scorer runs the container as uid 1000, with a read-only root filesystem.
RUN useradd --uid 1000 --no-create-home --home-dir /scratch app
COPY --from=build /src/target/release/sessions /app/sessions
USER 1000
CMD ["/app/sessions"]
