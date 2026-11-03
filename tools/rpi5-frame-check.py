#!/usr/bin/env python3
"""Capture DRM delivery cadence and measure marked menu transitions."""

import argparse
from bisect import bisect_left
import importlib.util
import json
import math
import os
from pathlib import Path
import re
import time
import uuid


def analyze(text, crtc=None):
    """Count one delivery per sequence on one CRTC; reject incomplete captures."""
    entries = re.search(r'entries-in-buffer/entries-written:\s*(\d+)/(\d+)', text)
    if entries and entries[1] != entries[2]:
        raise ValueError('trace buffer overflow; use a shorter capture')
    streams, markers = {}, []
    begin = end = None
    last = 0.0
    for line in text.splitlines():
        match = re.search(r'\s(\d+\.\d+): (.*)', line)
        if not match:
            continue
        stamp, event = float(match[1]), match[2]
        last = max(last, stamp)
        if event.startswith('drm_vblank_event_delivered:'):
            fields = re.search(r'crtc=(\d+), seq=(\d+)', event)
            if fields:
                stream = streams.setdefault(int(fields[1]), {})
                sequence = int(fields[2])
                stream[sequence] = min(stream.get(sequence, stamp), stamp)
        elif 'tracing_mark_write:' in event:
            if 'PUNKT_TAB ' in event:
                markers.append(stamp)
            elif 'PUNKT_BEGIN' in event:
                begin = stamp
            elif 'PUNKT_END' in event:
                end = stamp
    if crtc is None:
        if len(streams) != 1:
            raise ValueError('select --crtc when the trace has zero or multiple CRTCs')
        crtc = next(iter(streams))
    frames = sorted(streams.get(crtc, {}).values())
    if len(frames) < 2:
        raise ValueError('trace needs at least two deliveries on the selected CRTC')
    if (begin is None) != (end is None):
        raise ValueError('capture is missing a boundary marker')
    start, stop = (min([frames[0], *markers]), last) if begin is None else (begin, end)
    if stop <= start:
        raise ValueError('capture duration must be positive')
    frames = [frame for frame in frames if start <= frame < stop]
    if len(frames) < 2:
        raise ValueError('capture needs at least two deliveries within its boundaries')
    counts, gaps = [], []
    for marker in sorted(markers):
        if marker < start or marker + 0.3 > stop:
            raise ValueError('trace contains an incomplete transition window')
        window = frames[bisect_left(frames, marker):bisect_left(frames, marker + 0.3)]
        counts.append(len(window))
        edges = [marker, *window, marker + 0.3]
        gaps.append(max(b - a for a, b in zip(edges, edges[1:])) * 1000)
    intervals = sorted((b - a) * 1000 for a, b in zip(frames, frames[1:]))
    return {
        'crtc': crtc, 'duration_s': stop - start, 'deliveries': len(frames),
        'delivery_fps': len(frames) / (stop - start),
        'frame_interval_p95_ms': intervals[math.ceil(len(intervals) * 0.95) - 1],
        'frame_interval_max_ms': max(intervals),
        'switches': len(counts),
        'first_300ms_fps': sum(counts) / (len(counts) * 0.3) if counts else None,
        'first_300ms_frame_counts': counts,
        'transition_max_gap_ms': max(gaps) if gaps else None,
    }


def capture(output, seconds, tabs):
    """Use a private tracing instance and release it even when input fails."""
    if os.name != 'posix' or os.geteuid() != 0:
        raise ValueError('run capture with sudo on the Pi')
    spec = importlib.util.spec_from_file_location('probe', Path(__file__).with_name('rpi5-ui-probe.py'))
    probe = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(probe)
    root = Path('/sys/kernel/tracing/instances') / ('punktfunk-check-' + uuid.uuid4().hex)
    with output.open('x') as saved:
        root.mkdir()
        try:
            (root / 'tracing_on').write_text('0')
            (root / 'buffer_size_kb').write_text('4096')
            (root / 'trace_clock').write_text('mono')
            (root / 'events/drm/drm_vblank_event_delivered/enable').write_text('1')
            from contextlib import nullcontext
            with probe.Keyboard() if tabs else nullcontext() as keyboard:
                (root / 'tracing_on').write_text('1')
                with (root / 'trace_marker').open('w') as marker:
                    def mark(value):
                        marker.write(value + '\n')
                        marker.flush()
                    mark('PUNKT_BEGIN')
                    deadline, step = time.monotonic() + seconds, 0
                    while time.monotonic() < deadline:
                        if tabs:
                            mark('PUNKT_TAB ' + str(step))
                            keyboard.tap(probe.KEYS['pagedown' if step % 2 == 0 else 'pageup'])
                            step += 1
                        time.sleep(0.54 if tabs else 0.1)
                    mark('PUNKT_END')
        finally:
            try:
                (root / 'tracing_on').write_text('0')
                saved.write((root / 'trace').read_text())
            finally:
                root.rmdir()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('trace', type=Path)
    parser.add_argument('--capture', action='store_true')
    parser.add_argument('--tabs', action='store_true', help='Send page keys; requires the kiosk menu')
    parser.add_argument('--seconds', type=float, default=20)
    parser.add_argument('--crtc', type=int)
    parser.add_argument('--min-transition-fps', type=float)
    args = parser.parse_args()
    if not math.isfinite(args.seconds) or not 1 <= args.seconds <= 60:
        parser.error('seconds must be between 1 and 60')
    if args.tabs and not args.capture:
        parser.error('--tabs requires --capture')
    if args.min_transition_fps is not None and (
        not math.isfinite(args.min_transition_fps) or args.min_transition_fps <= 0
    ):
        parser.error('minimum FPS must be positive and finite')
    if args.capture:
        capture(args.trace, args.seconds, args.tabs)
    report = analyze(args.trace.read_text(), args.crtc)
    print(json.dumps(report, indent=2))
    if args.min_transition_fps is not None:
        actual = report['first_300ms_fps']
        if actual is None or actual < args.min_transition_fps:
            raise SystemExit('transition cadence is below the required minimum or unmeasured')


if __name__ == '__main__':
    try:
        main()
    except (ValueError, OSError) as error:
        raise SystemExit(str(error))
