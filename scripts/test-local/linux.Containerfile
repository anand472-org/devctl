FROM docker.io/library/ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive
# build-essential supplies the linker Rust needs; it is not a crate dependency.
RUN apt-get update \
    && apt-get install -y --no-install-recommends git curl ca-certificates build-essential xz-utils \
    && rm -rf /var/lib/apt/lists/*

# Non-root user, so tests that check file permissions behave as on a developer machine.
# Ubuntu 24.04 ships a default "ubuntu" user at uid 1000; remove it first.
RUN userdel -r ubuntu 2>/dev/null || true \
    && useradd --create-home --shell /bin/bash --uid 1000 dev \
    && mkdir -p /home/dev/target /home/dev/work \
    && chown dev:dev /home/dev/target /home/dev/work

USER dev
ENV HOME=/home/dev
ENV PATH=/home/dev/.cargo/bin:/home/dev/.local/bin:/home/dev/.local/share/mise/shims:$PATH
ENV CARGO_TARGET_DIR=/home/dev/target

RUN git config --global user.name "devctl test" \
    && git config --global user.email "devctl-test@example.com" \
    && curl -fsSL -o /tmp/rustup-init.sh https://sh.rustup.rs \
    && sh /tmp/rustup-init.sh -y --default-toolchain none --profile minimal \
    && curl -fsSL -o /tmp/mise-install.sh https://mise.run \
    && MISE_VERSION=v2026.8.16 sh /tmp/mise-install.sh \
    && rm -f /tmp/rustup-init.sh /tmp/mise-install.sh \
    && mkdir -p /home/dev/.local/share/mise

WORKDIR /home/dev/work
