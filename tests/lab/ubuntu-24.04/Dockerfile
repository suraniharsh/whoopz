# Older, widely used versions (an LTS release): fish 3.7 here caught a bug fish 4 didn't show.
FROM ubuntu:24.04
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bash zsh fish dash tcsh xonsh elvish \
        coreutils findutils grep sed procps \
    && rm -rf /var/lib/apt/lists/*
