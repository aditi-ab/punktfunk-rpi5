import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('check', Path(__file__).with_name('rpi5-frame-check.py'))
check = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check)


def event(stamp, body):
    return f'worker-1 [000] .... {stamp:.6f}: {body}\n'


def trace(rate=60):
    result = event(1, 'tracing_mark_write: PUNKT_BEGIN')
    result += event(1, 'tracing_mark_write: PUNKT_TAB 0')
    result += ''.join(event(1 + (i + 0.5) / rate,
                           f'drm_vblank_event_delivered: file=abc, crtc=2, seq={i}')
                      for i in range(rate))
    return result + event(2, 'tracing_mark_write: PUNKT_END')


class FrameCheckTests(unittest.TestCase):
    def test_full_rate_and_transition(self):
        result = check.analyze(trace())
        self.assertEqual(result['delivery_fps'], 60)
        self.assertEqual(result['first_300ms_frame_counts'], [18])
        self.assertAlmostEqual(result['frame_interval_p95_ms'], 16.667, places=3)

    def test_low_rate_is_visible_during_transition(self):
        result = check.analyze(trace(20))
        self.assertEqual(result['first_300ms_fps'], 20)
        self.assertAlmostEqual(result['transition_max_gap_ms'], 50)

    def test_duplicate_delivery_does_not_inflate_fps(self):
        extra = event(1.01, 'drm_vblank_event_delivered: file=def, crtc=2, seq=0')
        self.assertEqual(check.analyze(trace() + extra)['delivery_fps'], 60)

    def test_multiple_displays_require_selection(self):
        extra = trace().replace('crtc=2', 'crtc=3')
        with self.assertRaisesRegex(ValueError, 'multiple CRTCs'):
            check.analyze(trace() + extra)
        self.assertEqual(check.analyze(trace() + extra, 2)['delivery_fps'], 60)

    def test_overflow_is_not_a_valid_result(self):
        with self.assertRaisesRegex(ValueError, 'overflow'):
            check.analyze('# entries-in-buffer/entries-written: 20/100\n' + trace())

    def test_partial_transition_is_rejected(self):
        with self.assertRaisesRegex(ValueError, 'incomplete transition'):
            check.analyze(trace() + event(1.9, 'tracing_mark_write: PUNKT_TAB 1'))

    def test_interrupted_capture_is_rejected(self):
        with self.assertRaisesRegex(ValueError, 'boundary marker'):
            check.analyze(trace().replace('PUNKT_END', 'OTHER'))

    def test_missing_deliveries_are_rejected(self):
        with self.assertRaises(ValueError):
            check.analyze('')

    def test_unmarked_trace_does_not_claim_transition_coverage(self):
        result = check.analyze(trace().replace('PUNKT_TAB', 'OTHER'))
        self.assertIsNone(result['first_300ms_fps'])

    def test_cli_threshold_fails_for_regression_or_absent_transitions(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / 'capture.trace'
            for data, expected in ((trace(), 0), (trace(20), 1),
                                   (trace().replace('PUNKT_TAB', 'OTHER'), 1)):
                path.write_text(data)
                result = subprocess.run([sys.executable, str(Path(check.__file__)),
                                         str(path), '--min-transition-fps', '30'],
                                        capture_output=True)
                self.assertEqual(result.returncode, expected, result.stderr)


if __name__ == '__main__':
    unittest.main()
