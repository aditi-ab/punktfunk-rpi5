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

# The actions cache no longer lives on this box AT ALL: home-ci-core (192.168.1.58, see
# unom/infra runners/ci-core/) serves every runner host, sized and GC'd there. The old local
# store cap + burst-clear are gone with it — they were self-defeating anyway: under disk
# pressure they deleted exactly the cache that made the next job smaller, which is how
# runner-2 ended up cold-building every Rust job with an empty 28 KB cache dir.
BURST_PCT=${BURST_PCT:-80}              # burst-clear docker debris once the disk is this % full
MIN_FREE_GB=${MIN_FREE_GB:-60}          # ...or this little is left, whichever trips first.
                                        # 60, not 45: this has to fire BEFORE the disk is
                                        # actually tight, because the clear only reclaims idle
                                        # images (~18 G) while three concurrent jobs can eat
                                        # the remainder inside one poll interval. Measured
                                        # 2026-07-29: zero burst clears fired in six hours
                                        # while deb still died of ENOSPC between polls.

# 1) Routine: trim aged images / build cache / stopped containers. sha-<commit> tags aren't
#    dangling, so -a is required. until=2h, not 6h: on a busy day every image is younger than six
#    hours, so the filter matched nothing and a run reclaimed 0B while `docker system df` was
#    reporting 20+ GB reclaimable. Two hours still protects a re-run of the push being worked on.
docker image prune     -af --filter until=2h || true
docker builder prune   -af --filter until=2h || true
docker buildx prune    -af --filter until=2h || true
docker container prune  -f --filter until=2h || true

# 2) Leaked job networks. act_runner leaks per-job GITEA-ACTIONS-TASK-* bridges when jobs are
#    killed; enough of them exhausted the docker address pool once (it then swallowed the DMZ
#    subnet — see unom/infra runners/ci-core/README.md) and each one is another interface for
#    the host dnsmasq to bind. until=2h protects the networks of live jobs.
docker network prune -f --filter until=2h || true

# 3) Burst guard: a push-storm fills the disk WITHIN one interval — three concurrent Rust builds,
#    each with a multi-GB target/, on top of a ~40 GB containerd image baseline. Trigger on a free
#    -space FLOOR as well as a percentage: the percentage is the wrong instrument on its own, since
#    what matters is absolute headroom for three concurrent target/ dirs, not a ratio — and the
#    ratio moves whenever the disk is resized (it went 123 G -> 175 G on 2026-07-29) while the
#    headroom three jobs need does not. In-use images are protected by the daemon, so a burst clear
#    cannot pull the rug from a live job.
PCT=$(df --output=pcent / | tr -dc '0-9')
FREE_GB=$(df --output=avail -BG / | tr -dc '0-9')
# Two flat tests into a flag rather than one multi-line `{ …; } || { …; }` condition: the brace-group
# form is easy to get subtly wrong across a line break, and this reads as what it is — either signal
# alone is enough. Empty values (a df that failed) simply leave the flag unset, i.e. no clear.
BURST=0
[ -n "$PCT" ] && [ "$PCT" -ge "$BURST_PCT" ] && BURST=1
[ -n "$FREE_GB" ] && [ "$FREE_GB" -lt "$MIN_FREE_GB" ] && BURST=1
if [ "$BURST" = 1 ]; then
  echo "disk ${PCT}% used, ${FREE_GB}G free (thresholds ${BURST_PCT}% / ${MIN_FREE_GB}G) — burst clear"
  docker image prune -af   || true
  docker builder prune -af || true
  docker buildx prune -af  || true
fi
