# Static musl binary in a scratch image. Build: docker build -t kaban .
# Run (stdio MCP):  docker run --rm -i -v kaban:/data kaban
# The builder image is pinned to the workspace toolchain minor (rust-toolchain.toml: 1.99.0);
# rust-toolchain.toml itself is not copied so rustup does not download a second toolchain.
FROM rust:1.99-bookworm AS build
RUN apt-get update && apt-get install -y --no-install-recommends musl-tools && rm -rf /var/lib/apt/lists/* \
 && rustup target add "$(uname -m)-unknown-linux-musl"
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY .cargo ./.cargo
RUN T="$(uname -m)-unknown-linux-musl" && cargo build --release --locked --target "$T" && cp "target/$T/release/kaban" /kaban

FROM scratch
COPY --from=build /kaban /kaban
COPY examples/policy.yaml /etc/kaban/policy.yaml
VOLUME /data
ENV KABAN_DB=/data/kaban.db KABAN_POLICY=/etc/kaban/policy.yaml KABAN_AUDIT_EXPORT=/data/audit.jsonl
ENTRYPOINT ["/kaban"]
