#!/usr/bin/env python3
"""The post-ship rows the other readers do not cover: a ramp cut short, the ramp at 0.5 %
loss, and a desktop going still.

    scripts/abr-rig/read-post-ship.py
"""
import glob
import json
import os
import re

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "out")
ANSI = re.compile(r"\x1b\[[0-9;]*m")


def records(tag):
    path = os.path.join(OUT, f"{tag}.jsonl")
    if not os.path.exists(path):
        return []
    with open(path) as f:
        return [json.loads(l) for l in f if l.strip()]


def log(tag, kind):
    path = os.path.join(OUT, f"{tag}-{kind}.log")
    if not os.path.exists(path):
        return ""
    with open(path, errors="replace") as f:
        return ANSI.sub("", f.read())


def windows(recs):
    return [r for r in recs if "t_ms" in r and "target_kbps" in r]


def summary(recs):
    return next((r for r in recs if "summary" in r), {})


def ramp(recs):
    return next((r for r in recs if r.get("ramp") == "done"), {})


def cut_short():
    print("== 9a: ramp cut short (wan_wg_12, 100 ms bring-up)")
    for tag in sorted(glob.glob(os.path.join(OUT, "ps-short-[0-9]*.jsonl"))):
        tag = os.path.basename(tag)[:-6]
        recs = records(tag)
        ws = [w for w in windows(recs) if not w.get("discarded")]
        ts = [w["t_ms"] for w in windows(recs)]
        gaps = [b - a for a, b in zip(ts, ts[1:])]
        host, client = log(tag, "host"), log(tag, "client")
        r, s = ramp(recs), summary(recs)
        print(
            f"  {tag}: ramp proven={r.get('proven_kbps')} opened={r.get('opening_kbps')} "
            f"| rejected={host.count('speed-test probe rejected')} "
            f"declined={client.count('capacity probe declined')} "
            f"burst_measured={client.count('startup link-capacity probe') - client.count('capacity probe declined')} "
            f"| max_report_gap_ms={max(gaps) if gaps else None} "
            f"| to90_s={s.get('to90_s')} cuts/10={s.get('cuts_per_10min')} lost/10={s.get('lost_per_10min')} "
            f"| judged_windows={len(ws)}"
        )


def ramp_loss():
    print("== 9c: ramp on a roomy link at 0.5 % loss (ramp_loss)")
    walls_low, rows = 0, []
    for path in sorted(glob.glob(os.path.join(OUT, "ps-ramp-*.jsonl")),
                       key=lambda p: int(re.findall(r"(\d+)\.jsonl", p)[0])):
        tag = os.path.basename(path)[:-6]
        recs = records(tag)
        r = ramp(recs)
        steps = [x for x in recs if "ramp_step" in x]
        refused = [x for x in steps if x["verdict"].startswith(("Wall", "Refused"))]
        low = bool(r.get("wall")) and (r.get("proven_kbps") or 0) < 10_000
        walls_low += low
        rows.append(
            f"  {tag}: wall={r.get('wall')} proven={r.get('proven_kbps')} steps={len(steps)} "
            f"refusals={[(x['asked_kbps'], x['offered_packets'] - x['delivered_packets']) for x in refused]}"
            + ("  <-- wall under 10 Mbps" if low else "")
        )
    print("\n".join(rows))
    print(f"  walls under 10 Mbps: {walls_low} of {len(rows)}")


def still():
    print("== 9c: still desktop (wan_still: tunnel 0.5 % loss, still from 60 s)")
    for path in sorted(glob.glob(os.path.join(OUT, "ps-still-[0-9]*.jsonl"))):
        tag = os.path.basename(path)[:-6]
        recs = records(tag)
        ws = windows(recs)
        busy = [w["target_kbps"] for w in ws if 40_000 <= w["t_ms"] <= 60_000]
        after = [w for w in ws if w["t_ms"] > 62_000]
        low = min((w["target_kbps"] for w in after), default=None)
        cuts = [w for w in after if w.get("request_kbps") is not None
                and w["request_kbps"] < w["target_kbps"]]
        client = log(tag, "client")
        caps = [l for l in client.splitlines() if "link cap learned" in l or "link cap measured" in l]
        print(
            f"  {tag}: busy_rate={busy[-1] if busy else None} still_min={low} "
            f"cuts_while_still={len(cuts)} still_windows={len(after)} "
            f"lost_while_still={sum(w.get('lost_frames', 0) for w in after)} "
            f"cap_lines={len(caps)}"
        )


if __name__ == "__main__":
    cut_short()
    ramp_loss()
    still()
