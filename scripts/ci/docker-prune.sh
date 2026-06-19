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

RUNNER=$(docker ps -q -f name=gitea-runner-runner | head -1)
ACTCACHE=/root/.cache/actcache/cache    # path INSIDE the runner container (HOME=/root there)
CAP_MB=20000                            # clear the actcache once its blob dir exceeds ~20 GB
BURST_PCT=80                            # full clear once the disk is this % full

# 1) Routine: trim aged images / build cache / stopped containers. sha-<commit> tags aren't
#    dangling, so -a is required; until=6h keeps very recent ones for quick re-runs.
docker image prune     -af --filter until=6h || true
docker builder prune   -af --filter until=6h || true
docker buildx prune    -af --filter until=6h || true
docker container prune  -f --filter until=6h || true

# 2) Cap the act_runner cache server store (the real disk filler). Clearing the blobs is safe —
#    act_runner repopulates it and cache keys are content-hashed, so this only drops stale entries.
if [ -n "$RUNNER" ]; then
  SZ=$(docker exec "$RUNNER" du -sm "$ACTCACHE" 2>/dev/null | cut -f1)
  if [ -n "${SZ:-}" ] && [ "$SZ" -ge "$CAP_MB" ]; then
    docker exec "$RUNNER" sh -c "rm -rf $ACTCACHE/*" && echo "actcache cleared (was ${SZ} MB)"
  fi
fi

# 3) Burst guard: a push-storm can fill the disk within one interval. Once >=BURST_PCT% full, prune
#    ALL idle images/cache AND clear the actcache, regardless of age. In-use images are protected.
PCT=$(df --output=pcent / | tr -dc '0-9')
if [ -n "$PCT" ] && [ "$PCT" -ge "$BURST_PCT" ]; then
  echo "disk ${PCT}% >= ${BURST_PCT}% — burst clear"
  docker image prune -af   || true
  docker builder prune -af || true
  docker buildx prune -af  || true
  [ -n "$RUNNER" ] && docker exec "$RUNNER" sh -c "rm -rf $ACTCACHE/*" || true
fi
