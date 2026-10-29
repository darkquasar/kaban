# Live hook capture image (M4 W2-D1, OWN-7, OWN-15).
# One pinned base image; the three host CLIs at the exact sandbox versions; jq and git;
# the sandbox proxy CA in the system trust store and in NODE_EXTRA_CA_CERTS; a non-root user.
# Build only through scripts/hooks-live/build.sh (it pins the base digest and passes the
# proxy build-args). Nothing from the sandbox user configs enters the image.
ARG BASE_IMAGE=node:22-bookworm
FROM ${BASE_IMAGE}

ARG CLAUDE_CODE_VERSION=2.1.295
ARG CODEX_VERSION=0.149.1
ARG PI_VERSION=0.87.1
# base64 of the sandbox proxy CA certificate (public key material, not a secret)
ARG PROXY_CA_CERT_B64=""

ENV DEBIAN_FRONTEND=noninteractive

# Proxy CA: system trust store (apt, curl, codex) and Node (claude, pi, npm).
RUN set -eux; \
    if [ -n "${PROXY_CA_CERT_B64}" ]; then \
      echo "${PROXY_CA_CERT_B64}" | base64 -d > /usr/local/share/ca-certificates/sandbox-proxy.crt; \
      update-ca-certificates; \
    fi
ENV NODE_EXTRA_CA_CERTS=/etc/ssl/certs/ca-certificates.crt \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt \
    npm_config_cafile=/etc/ssl/certs/ca-certificates.crt

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends jq git ca-certificates procps; \
    rm -rf /var/lib/apt/lists/*

# Exact host CLI versions (OWN-15). Global install only inside the image.
RUN set -eux; \
    npm install -g --no-fund --no-audit \
      "@anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}" \
      "@openai/codex@${CODEX_VERSION}" \
      "@earendil-works/pi-coding-agent@${PI_VERSION}"; \
    npm cache clean --force; \
    claude --version; codex --version; pi --version; jq --version; git --version

# Non-root user: the node image ships `node` (uid 1000, same as the sandbox user, so the
# read-only credential copies owned by uid 1000 with mode 0600 are readable).
RUN set -eux; \
    mkdir -p /work /capture /out /harness; \
    chown -R node:node /work /capture /out; \
    git config --system init.defaultBranch main; \
    git config --system safe.directory '*'
USER node
WORKDIR /work
ENV HOME=/home/node \
    CODEX_HOME=/home/node/.codex \
    CI=1 \
    TERM=dumb

ENTRYPOINT ["/bin/bash", "/harness/container/entrypoint.sh"]
