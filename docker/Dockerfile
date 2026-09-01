# Builds the target app and the bench, then adds Chromium for the screen arms, which drive it
# headless over CDP. Nothing here can touch the host's screen, mouse or keyboard.
FROM rust:1.95-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates chromium procps curl \
  && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/app /usr/local/bin/app
COPY --from=build /src/target/release/bench /usr/local/bin/bench
COPY tasks /work/tasks
COPY docker/lanes.sh /usr/local/bin/lanes.sh
WORKDIR /work
ENTRYPOINT ["/usr/local/bin/lanes.sh"]
