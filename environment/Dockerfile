FROM rust:1.83-slim-bookworm

ENV DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8 PAGER=cat

# procps: pkill/ps for the verifier and for debugging. No Python, no database libraries.
RUN apt-get update \
 && apt-get install -y --no-install-recommends procps \
 && rm -rf /var/lib/apt/lists/*

# The official image makes the toolchain world-writable. Lock it down so the agent cannot
# replace cargo/rustc (the verifier builds with this toolchain), and give the agent its own
# CARGO_HOME for caches.
RUN chmod -R go-w /usr/local/cargo /usr/local/rustup \
 && useradd --create-home --uid 1000 --shell /bin/bash agent \
 && mkdir -p /app /home/agent/.cargo \
 && chown -R agent:agent /home/agent

ENV CARGO_HOME=/home/agent/.cargo \
    RUSTUP_HOME=/usr/local/rustup \
    PATH=/usr/local/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

COPY workspace/kvstore/ /app/kvstore/
RUN chmod +x /app/kvstore/run_examples.sh \
 && chown -R agent:agent /app

WORKDIR /app/kvstore
