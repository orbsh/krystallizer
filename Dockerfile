# k10r as a container: one process, one store directory, one HTTP door.
#
# The build context is WORKSPACE-SHAPED, not repo-shaped: `cargo build` here runs
# on a tree where this repo sits at `krystallizer/` and every path dependency sits
# beside it (memory-stack/build.sh assembles it). k10r's only cross-repo dependency
# is okm, which comes from git, so nothing but this directory is copied in — but
# keeping one context shape for all three images is worth more than the copy it
# saves, because a per-image shape means three different `COPY` lines to keep true.
FROM rust:1-bookworm AS build

WORKDIR /src
COPY krystallizer/ krystallizer/

# `--locked`: the lockfile is in the context (this repo's is gitignored, so it
# travels with the build, not with the history). A dependency drift then fails the
# build instead of changing the image silently.
#
# The binary is copied OUT of `target/` inside the same step because a build harness
# may mount `target/` as an incremental cache (memory-stack/build.sh does), and a
# mounted directory is not part of the stage's filesystem — `COPY --from` would not
# find it. Copying to `/out` works whether or not the cache is mounted.
RUN cargo build --release --locked \
      --manifest-path krystallizer/Cargo.toml \
      -p mem-server --bin krystallizer \
 && mkdir -p /out \
 && cp krystallizer/target/release/krystallizer /out/krystallizer

FROM debian:bookworm-slim

# ca-certificates only: the default embedding engine runs in-process, but
# `embedding { mode "remote" }` is a supported configuration and a container
# without a trust store fails it in a way that reads like a network outage.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

COPY --from=build /out/krystallizer /usr/local/bin/krystallizer
COPY krystallizer/config/krystallizer.kdl /etc/krystallizer/krystallizer.kdl

# The config file is the deployment, and it says `$HOME/...`: on a machine that is
# /home/master, here it is /data. One file, no container fork, no path flags — the
# volume below is what makes the two readings the same deployment.
ENV HOME=/data
VOLUME ["/data"]
EXPOSE 7760

# 0.0.0.0 is the correct default INSIDE a container: loopback would only accept
# connections from this process. Which network may reach the published port is the
# runtime's decision.
ENTRYPOINT ["/usr/local/bin/krystallizer", \
            "--config", "/etc/krystallizer/krystallizer.kdl", \
            "--addr", "0.0.0.0:7760"]