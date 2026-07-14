#!/usr/bin/env python3
"""
throughput-sweep.py — punktfunk data-plane throughput diagnostic.

Drives the built-in `punktfunk-probe --speed-test` up a ladder of target
bitrates and tabulates, per point:

    target | delivered | efficiency | link_loss | host_drop | send_dropped

The probe bursts synthetic FEC filler over the REAL UDP + GF(2^16) FEC + AES-GCM
data plane with video PAUSED (host `run_probe_burst`), so this isolates the
transport from NVENC entirely. It separates the two failure modes the raw
`iperf` number can't:

  * host_drop   = wire packets the host could not even hand to the kernel
                  (send buffer full / send thread can't keep up)   -> HOST side
  * link_loss   = wire packets sent but never arrived
                  (link saturation, or client recv buffer/CPU drop) -> LINK/CLIENT

How to read the result:

  * delivered ~= target, ~0 loss all the way to 2-3 Gbps
        -> the transport is NOT your wall. The ~500 Mbps ceiling is the ENCODER
           (CBR undershoot with no filler / codec level cap). Chase that next.
  * delivered climbs then flattens while host_drop rises
        -> host send buffer / single send thread. (SO_SNDBUF, USO chunk size.)
  * delivered flattens while link_loss rises, host_drop ~0
        -> the link itself, or the client recv path (8 MB RCVBUF clamp / recv CPU).
           Cross-check with `iperf3 -u -b 2G` between the two boxes to decide
           which: if iperf also caps, it's the link/OS, not punktfunk.

Prereq (one-time) — pair the probe with your host. Arm pairing on the host,
then:

    punktfunk-probe --connect HOST:9777 --pair <PIN>

(the probe stores its identity in ~/.config/punktfunk/). After that the sweep
reuses the trusted identity; pass --pin <HOSTFP> to also pin the host.

Usage:

    scripts/throughput-sweep.py HOST[:PORT] [options]

Options:
    --pin HEX64        pin the host cert fingerprint (host logs it at startup)
    --mode WxHxFPS     request this virtual-display mode (default: probe default)
    --ladder A,B,C     target bitrates in Mbps
                       (default: 250,500,750,1000,1250,1500,2000,3000)
    --dur-ms N         burst duration per point in ms (default: 2000)
    --bin PATH         path to punktfunk-probe (default: target/release/punktfunk-probe)
    --build            (re)build the probe before sweeping
    --warmup-video-kbps N   encoder bitrate during warmup (default: 20000 = 20 Mbps)
    --dry-run          print the probe commands without running them
    --self-test        run the output parser against a synthetic line and exit

Examples:
    scripts/throughput-sweep.py 192.168.1.173
    scripts/throughput-sweep.py 192.168.1.173:9777 --pin a1b2... --ladder 500,1000,2000
"""

from __future__ import annotations

import argparse
import math
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
DEFAULT_BIN = REPO / "target" / "release" / "punktfunk-probe"
DEFAULT_LADDER = "250,500,750,1000,1250,1500,2000,3000"
DEFAULT_PORT = 9777
SPEED_LINE = "SPEED TEST complete"


# ---- output parsing -------------------------------------------------------

def _field(line: str, key: str) -> str | None:
    """Extract `key=value` or `key="value"` from a tracing log line (order-agnostic)."""
    m = re.search(rf'\b{re.escape(key)}=(?:"([^"]*)"|(\S+))', line)
    if not m:
        return None
    return m.group(1) if m.group(1) is not None else m.group(2)


def parse_speed_line(text: str) -> dict | None:
    """Find the 'SPEED TEST complete' line in probe output and pull its fields.

    Field names mirror clients/probe/src/main.rs:698-708 exactly."""
    line = next((ln for ln in text.splitlines() if SPEED_LINE in ln), None)
    if line is None:
        return None

    def num(key: str) -> float | None:
        v = _field(line, key)
        if v is None:
            return None
        try:
            return float(v.rstrip("%"))
        except ValueError:
            return None

    return {
        "target_mbps": num("target_mbps"),
        "delivered_mbps": num("delivered_mbps"),
        "link_loss_pct": num("link_loss_pct"),
        "host_drop_pct": num("host_drop_pct"),
        "wire_pkts_sent": num("wire_pkts_sent"),
        "wire_pkts_recv": num("wire_pkts_recv"),
        "send_dropped": num("send_dropped"),
    }


def scan_warnings(text: str) -> list[str]:
    """Surface client-side red flags that change interpretation of the numbers."""
    flags = []
    if "UDP socket buffer capped" in text:
        flags.append("client SO_RCVBUF capped below target (raise kern.ipc.maxsockbuf)")
    if "falling back to per-packet sends" in text or "USO unsupported" in text:
        flags.append("USO/GSO unsupported on this path -> scalar per-packet sends")
    if "recvmsg_x" in text and "disabl" in text.lower():
        flags.append("recvmsg_x batching latched off -> one syscall per packet")
    return flags


# ---- probe invocation -----------------------------------------------------

def build_probe() -> None:
    env = dict(os.environ)
    # audiopus_sys / opus vendored CMake needs this on recent CMake (see project memory).
    env.setdefault("CMAKE_POLICY_VERSION_MINIMUM", "3.5")
    print("building punktfunk-probe (release)...", flush=True)
    subprocess.run(
        ["cargo", "build", "--release", "-p", "punktfunk-probe"],
        cwd=REPO, env=env, check=True,
    )


def seconds_for(dur_ms: int) -> int:
    """Receive-loop cap: 2s probe warmup + burst + slack for connect/settle/report."""
    return math.ceil(2 + dur_ms / 1000 + 3)


def probe_cmd(bin_path: Path, host: str, target_mbps: int, dur_ms: int,
              seconds: int, args: argparse.Namespace) -> list[str]:
    cmd = [
        str(bin_path),
        "--connect", host,
        "--bitrate", str(args.warmup_video_kbps),
        "--speed-test", f"{target_mbps * 1000}:{dur_ms}",
        "--seconds", str(seconds),
        "--quit",  # tear the host session down cleanly between points
    ]
    if args.pin:
        cmd += ["--pin", args.pin]
    if args.mode:
        cmd += ["--mode", args.mode]
    return cmd


def run_point(bin_path: Path, host: str, target_mbps: int,
              args: argparse.Namespace) -> tuple[dict | None, list[str], str]:
    seconds = seconds_for(args.dur_ms)
    cmd = probe_cmd(bin_path, host, target_mbps, args.dur_ms, seconds, args)
    env = dict(os.environ)
    env.setdefault("RUST_LOG", "info")
    try:
        proc = subprocess.run(
            cmd, cwd=REPO, env=env, capture_output=True, text=True,
            timeout=seconds + 25,
        )
    except subprocess.TimeoutExpired as e:
        out = (e.stdout or "") + (e.stderr or "")
        return None, ["probe timed out (no SPEED TEST line)"], out
    out = (proc.stdout or "") + (proc.stderr or "")
    return parse_speed_line(out), scan_warnings(out), out


# ---- reporting ------------------------------------------------------------

def verdict(rows: list[dict]) -> str:
    done = [r for r in rows if r.get("delivered_mbps") is not None]
    if not done:
        return "No successful measurements — check pairing/--pin and host reachability."
    peak = max(r["delivered_mbps"] for r in done)
    top = max(done, key=lambda r: r["target_mbps"] or 0)
    hd = top.get("host_drop_pct") or 0.0
    ll = top.get("link_loss_pct") or 0.0
    lines = [f"Peak delivered: ~{peak:.0f} Mbps."]
    if peak >= (top["target_mbps"] or 0) * 0.9 and hd < 1 and ll < 1:
        lines.append(
            "Transport tracked the target with ~0 loss to the top of the ladder: "
            "the transport is NOT the wall. Your ~500 Mbps ceiling is the ENCODER "
            "(CBR undershoot / no filler / codec level cap) — investigate that next.")
    elif hd >= ll and hd >= 1:
        lines.append(
            f"host_drop dominates at the top ({hd:.1f}% vs link_loss {ll:.1f}%): "
            "the HOST send path is the wall (SO_SNDBUF / single send thread / USO "
            "chunk=16). Fix host-side.")
    elif ll >= 1:
        lines.append(
            f"link_loss dominates at the top ({ll:.1f}% vs host_drop {hd:.1f}%): "
            "the LINK or CLIENT recv path caps here. Cross-check with "
            "`iperf3 -u -b <peak+50%>M` between the boxes — if iperf also caps, "
            "it's the link/OS, not punktfunk; if iperf is clean, it's the client "
            "recv buffer (8 MB) / recv CPU.")
    else:
        lines.append("Loss stayed low but delivered plateaued below target — "
                     "likely the encoder warmup rate or a soft CPU ceiling; "
                     "re-run with a longer --dur-ms to confirm.")
    return "\n".join(lines)


def print_table(rows: list[dict]) -> None:
    hdr = ["target", "delivered", "eff", "link_loss", "host_drop", "send_drop"]
    widths = [8, 10, 6, 10, 10, 10]

    def fmt_row(cells):
        return "  ".join(str(c).rjust(w) for c, w in zip(cells, widths))

    print()
    print(fmt_row(hdr))
    print(fmt_row(["-" * w for w in widths]))
    for r in rows:
        if r.get("delivered_mbps") is None:
            print(fmt_row([f"{r['target_mbps']:.0f}", "FAIL", "-", "-", "-", "-"]))
            continue
        t = r["target_mbps"] or 0
        d = r["delivered_mbps"]
        eff = f"{(d / t * 100):.0f}%" if t else "-"
        print(fmt_row([
            f"{t:.0f}", f"{d:.0f}", eff,
            f"{r.get('link_loss_pct', 0):.1f}%",
            f"{r.get('host_drop_pct', 0):.1f}%",
            f"{int(r.get('send_dropped') or 0)}",
        ]))
    print("\n(all rates in Mbps; probe bursts filler with video paused)\n")


# ---- main -----------------------------------------------------------------

SELF_TEST_LINE = (
    '2026-07-13T12:00:00.000Z  INFO punktfunk_probe: SPEED TEST complete '
    'target_kbps=2000000 target_mbps=2000 delivered_mbps=512 '
    'link_loss_pct="3.4%" host_drop_pct="0.0%" wire_pkts_sent=170000 '
    'wire_pkts_recv=164220 send_dropped=0'
)


def self_test() -> int:
    got = parse_speed_line(SELF_TEST_LINE)
    want = {
        "target_mbps": 2000.0, "delivered_mbps": 512.0,
        "link_loss_pct": 3.4, "host_drop_pct": 0.0,
        "wire_pkts_sent": 170000.0, "wire_pkts_recv": 164220.0,
        "send_dropped": 0.0,
    }
    ok = got == want
    print("parser self-test:", "PASS" if ok else "FAIL")
    if not ok:
        print("  got: ", got)
        print("  want:", want)
    return 0 if ok else 1


def main() -> int:
    p = argparse.ArgumentParser(add_help=True, description="punktfunk throughput sweep")
    p.add_argument("host", nargs="?", help="HOST[:PORT] of the punktfunk host")
    p.add_argument("--pin")
    p.add_argument("--mode")
    p.add_argument("--ladder", default=DEFAULT_LADDER)
    p.add_argument("--dur-ms", type=int, default=2000)
    p.add_argument("--bin", type=Path, default=DEFAULT_BIN)
    p.add_argument("--build", action="store_true")
    p.add_argument("--warmup-video-kbps", type=int, default=20000)
    p.add_argument("--dry-run", action="store_true")
    p.add_argument("--self-test", action="store_true")
    args = p.parse_args()

    if args.self_test:
        return self_test()

    if not args.host:
        p.error("HOST is required (e.g. 192.168.1.173 or 192.168.1.173:9777)")

    host = args.host if ":" in args.host else f"{args.host}:{DEFAULT_PORT}"
    try:
        ladder = [int(x) for x in args.ladder.split(",") if x.strip()]
    except ValueError:
        p.error(f"bad --ladder {args.ladder!r} (want comma-separated Mbps ints)")

    bin_path = args.bin
    if args.build or not bin_path.exists():
        if shutil.which("cargo") is None:
            p.error("cargo not found and probe binary missing; build it or pass --bin")
        build_probe()
    if not bin_path.exists():
        p.error(f"probe binary not found at {bin_path}")

    if args.dry_run:
        print("# dry run — commands that WOULD run:\n")
        for t in ladder:
            secs = seconds_for(args.dur_ms)
            print("  " + " ".join(probe_cmd(bin_path, host, t, args.dur_ms, secs, args)))
        return 0

    print(f"sweeping {host}  ladder={ladder} Mbps  dur={args.dur_ms}ms\n")
    rows: list[dict] = []
    all_flags: set[str] = set()
    for t in ladder:
        print(f"  -> {t} Mbps ...", end="", flush=True)
        parsed, flags, raw = run_point(bin_path, host, t, args)
        all_flags.update(flags)
        if parsed is None:
            print(" FAIL")
            # surface why, briefly
            tail = "\n".join(raw.strip().splitlines()[-3:])
            if tail:
                print("     " + tail.replace("\n", "\n     "))
            rows.append({"target_mbps": float(t), "delivered_mbps": None})
        else:
            parsed["target_mbps"] = parsed.get("target_mbps") or float(t)
            print(f" delivered {parsed['delivered_mbps']:.0f} Mbps  "
                  f"link_loss {parsed.get('link_loss_pct', 0):.1f}%  "
                  f"host_drop {parsed.get('host_drop_pct', 0):.1f}%")
            rows.append(parsed)

    print_table(rows)
    if all_flags:
        print("Flags seen in probe logs:")
        for f in sorted(all_flags):
            print(f"  ! {f}")
        print()
    print(verdict(rows))
    return 0


if __name__ == "__main__":
    sys.exit(main())
