FROM rust:1-bookworm

RUN apt-get update \
    && apt-get install -y --no-install-recommends gnupg2 libasound2-dev libxdo-dev pkg-config cmake build-essential clang-15 \
    && rm -rf /var/lib/apt/lists/*
