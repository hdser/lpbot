# The lpbot binary, for the service that runs it: the watcher
# (`lpbot watch-pools`). Built from the repository root by `make up` the
# first time, and by `make watch-build` after a change.
#
# The build keeps Cargo's downloads and build folder in BuildKit caches, so
# a rebuild compiles only what changed. CARGO_BUILD_JOBS stays at 2 on the
# node machine, which runs hot under more.

FROM rust:1.100-slim-trixie AS build
ARG CARGO_BUILD_JOBS=2
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS}
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p lpbot \
    && cp target/release/lpbot /usr/local/bin/lpbot

FROM debian:trixie-slim
COPY --from=build /usr/local/bin/lpbot /usr/local/bin/lpbot
COPY config /lpbot/config
WORKDIR /lpbot
ENTRYPOINT ["lpbot"]
