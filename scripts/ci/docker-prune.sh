#!/usr/bin/env bash
# CI runner disk hygiene — invoked by docker-prune.service (every 30 min). Lives in a real script
# rather than inline ExecStart= lines because systemd does its OWN $-expansion on ExecStart and
# empties shell vars / $(...) before /bin/sh sees them (silently breaking the logic under `|| true`).
#
# See docker-prune.service for the full why. The headline: the act_runner cache server's blob store
# lives INSIDE the long-running runner container's writable layer, where `docker prune` can't reach
# it — left alone it grows to tens of GB and fills the disk on its own.
set -u
export PATH=/usr/bin:/bin:/usr/local/bin:$PATH

# The cache-server's blob store is a HOST directory: the fleet runs a standalone cache-server
# service (compose.yml) that bind-mounts this path to /data, and every replica points at it over
# HTTP (`external_server`). It used to live inside a runner container's writable layer, which is
# why this reached in with `docker exec` — that is no longer where it is, and the container name it
# looked for (`gitea-runner-runner`) does not exist either now that the replicas are named
# `gitea-runner-fleet-runner-N-1`. Both halves silently did nothing: the filter matched zero
# containers, so the cap and the burst-clear below were dead code. A plain host path needs neither.
CACHE_DIR=${CACHE_DIR:-/home/runner/gitea-runner-fleet/cache}
CAP_MB=${CAP_MB:-20000}                 # clear the cache store once it exceeds ~20 GB
BURST_PCT=${BURST_PCT:-80}              # full clear once the disk is this % full
MIN_FREE_GB=${MIN_FREE_GB:-45}          # ...or this little is left, whichever trips first

# 1) Routine: trim aged images / build cache / stopped containers. sha-<commit> tags aren't
#    dangling, so -a is required. until=2h, not 6h: on a busy day every image is younger than six
#    hours, so the filter matched nothing and a run reclaimed 0B while `docker system df` was
#    reporting 20+ GB reclaimable. Two hours still protects a re-run of the push being worked on.
docker image prune     -af --filter until=2h || true
docker builder prune   -af --filter until=2h || true
docker buildx prune    -af --filter until=2h || true
docker container prune  -f --filter until=2h || true

# 2) Cap the cache-server store. Clearing the blobs is safe — act_runner repopulates it and cache
#    keys are content-hashed, so this only drops stale entries.
if [ -d "$CACHE_DIR" ]; then
  SZ=$(du -sm "$CACHE_DIR" 2>/dev/null | cut -f1)
  if [ -n "${SZ:-}" ] && [ "$SZ" -ge "$CAP_MB" ]; then
    rm -rf "${CACHE_DIR:?}"/* && echo "cache-server store cleared (was ${SZ} MB)"
  fi
fi

# 3) Burst guard: a push-storm fills the disk WITHIN one interval — three concurrent Rust builds,
#    each with a multi-GB target/, on top of a ~40 GB containerd image baseline. Trigger on a free
#    -space FLOOR as well as a percentage: the percentage alone is the wrong instrument here,
#    because 80% of 123 G still leaves only ~25 G, which three jobs can swallow before the next
#    poll. In-use images are protected by the daemon, so this cannot pull the rug from a live job.
PCT=$(df --output=pcent / | tr -dc '0-9')
FREE_GB=$(df --output=avail -BG / | tr -dc '0-9')
if { [ -n "$PCT" ] && [ "$PCT" -ge "$BURST_PCT" ]; } ||
   { [ -n "$FREE_GB" ] && [ "$FREE_GB" -lt "$MIN_FREE_GB" ]; }; then
  echo "disk ${PCT}% used, ${FREE_GB}G free (thresholds ${BURST_PCT}% / ${MIN_FREE_GB}G) — burst clear"
  docker image prune -af   || true
  docker builder prune -af || true
  docker buildx prune -af  || true
  [ -d "$CACHE_DIR" ] && rm -rf "${CACHE_DIR:?}"/* || true
fi
