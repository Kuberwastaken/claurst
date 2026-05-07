FROM rust:1-bullseye

RUN apt-get update \
    && apt-get install -y --no-install-recommends gnupg2 libasound2-dev libxdo-dev pkg-config \
    && rm -rf /var/lib/apt/lists/*
