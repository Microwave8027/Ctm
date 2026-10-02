# ctm server image: dashboard + API + scheduler + executor.
# It can run Claude locally (the CLI is installed here) or start sibling
# containers through the host's Docker socket for docker-mode runs.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
# Cache dependencies separately from the app source.
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release && rm -rf src
COPY src src
COPY assets assets
COPY migrations migrations
RUN touch src/main.rs && cargo build --release

FROM docker:28-cli AS docker-cli

FROM node:22-bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends git ca-certificates openssh-client tini \
 && rm -rf /var/lib/apt/lists/* \
 && npm install -g @anthropic-ai/claude-code \
 && npm cache clean --force
COPY --from=docker-cli /usr/local/bin/docker /usr/local/bin/docker
COPY --from=build /src/target/release/ctm /usr/local/bin/ctm

ENV CTM_BIND=0.0.0.0:7878 \
    CTM_DATA_DIR=/data
VOLUME /data
EXPOSE 7878
HEALTHCHECK --interval=30s --timeout=3s CMD node -e "fetch('http://127.0.0.1:7878/healthz').then(r=>process.exit(r.ok?0:1)).catch(()=>process.exit(1))"
ENTRYPOINT ["/usr/bin/tini", "--", "ctm"]
CMD ["serve"]
