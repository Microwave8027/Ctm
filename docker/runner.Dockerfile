# Sandbox image for docker-mode runs: one throw-away container per run.
# Build: docker build -f docker/runner.Dockerfile -t ctm-runner:latest docker
FROM node:22-bookworm-slim

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      git ca-certificates openssh-client curl jq ripgrep python3 build-essential \
 && rm -rf /var/lib/apt/lists/* \
 && npm install -g @anthropic-ai/claude-code \
 && npm cache clean --force

# Pre-create mount points owned by the unprivileged user so fresh named
# volumes inherit the right ownership.
RUN mkdir -p /workspace /home/node/.claude \
 && chown -R node:node /workspace /home/node/.claude

COPY runner-entrypoint.sh /usr/local/bin/ctm-runner-entrypoint
RUN chmod +x /usr/local/bin/ctm-runner-entrypoint

USER node
WORKDIR /workspace
ENTRYPOINT ["/usr/local/bin/ctm-runner-entrypoint"]
CMD ["claude", "--help"]
