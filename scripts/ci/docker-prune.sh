#!/usr/bin/env bash
# CI runner disk hygiene — invoked by docker-prune.service (every 2 min). Lives in a real script
# rather than inline ExecStart= lines because systemd does its OWN $-expansion on ExecStart and
# empties shell vars / $(...) before /bin/sh sees them (silently breaking the logic under `|| true`).
#
# See docker-prune.service for the full why. Sibling: docker-reclaim.sh (hourly) handles what
# act_runner *leaks* — per-job volumes, stale networks, old build cache. This one handles what
# CI legitimately *produces* and then abandons: per-SHA app tags and the layers they pin.
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

# 1) Routine: retire aged per-SHA app tags, then sweep what untagging released.
#    ⚠ NEVER `docker image prune -a` on this tick. `until=` filters on image CREATION time, so a
#    CI *base* image (built days ago) that merely has no container this instant counts as "aged" —
#    including one a job JUST PULLED whose container does not exist yet. Measured 2026-08-07:
#    this tick ran 07:36:09–:29 and a rust job's `docker create` failed at 07:36:29 with
#    "No such image: …punktfunk-rust-ci:latest" — three sampled failures that morning, each
#    coinciding with a prune run to the second — and every idle base image was re-pulled within
#    minutes (4–7 GB each), churning the LAN registry for nothing.
#    The only tag debris this host actually accretes is the per-SHA app tags (web/docs — their
#    creation time IS the local build time, so a 2h age gate is exact), and a dangling-only prune
#    cannot touch a tagged image, so neither step can race a starting job.
now=$(date +%s)
docker images --format '{{.Repository}}:{{.Tag}}' 2>/dev/null | grep ':sha-' | while read -r ref; do
  created=$(docker image inspect -f '{{.Created}}' "$ref" 2>/dev/null) || continue
  cts=$(date -d "$created" +%s 2>/dev/null) || continue
  if [ $((now - cts)) -ge 7200 ]; then
    docker rmi "$ref" >/dev/null 2>&1 || true
  fi
done
docker image prune      -f || true
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
#    cannot pull the rug from a live job — but the blanket `-a` prune below CAN race an image that
#    is pulled-but-not-yet-created (the section 1 lesson). That narrow window is accepted HERE
#    only: when the alternative is every concurrent job dying of ENOSPC, one job re-pulling loses.
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
