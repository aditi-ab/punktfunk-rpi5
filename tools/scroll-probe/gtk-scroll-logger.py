#!/usr/bin/env python3
"""What a real GtkScrolledWindow does with incoming scroll (GTK 4, PyGObject).

Usage: gtk-scroll-logger.py <logfile> [seconds] [max] [bottom]

Run inside the host's session and scroll over the window, from a client or with
`scroll-probe --inject-at WxH`. `max` maximizes the window so an injection at the desktop's
centre lands on it; `bottom` starts scrolled to the end so an upward script moves content.

Each line carries the wall clock and milliseconds since start, then one of:
  begin / end            a scroll gesture as GTK decodes it
  scroll dx dy unit src  one scroll event: unit surface|wheel, source device kind, event time
  decelerate vx vy       GTK starting its own kinetic glide, with the velocity it measured
  frame t value delta    the scrolled window's position whenever a frame moved it
"""
import sys
import time

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import GLib, Gtk  # noqa: E402

LOG = open(sys.argv[1], "a", buffering=1)
SECONDS = int(sys.argv[2]) if len(sys.argv) > 2 else 600
OPTIONS = sys.argv[3:]
T0 = time.monotonic()


def log(kind, *args):
    ms = (time.monotonic() - T0) * 1000.0
    LOG.write(f"{time.time():.4f} {ms:10.1f} {kind} " + " ".join(str(a) for a in args) + "\n")


class App(Gtk.Application):
    def do_activate(self):
        win = Gtk.ApplicationWindow(application=self, title="punktfunk scroll logger")
        win.set_default_size(1400, 1000)
        sw = Gtk.ScrolledWindow()
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        for i in range(4000):
            box.append(Gtk.Label(label=f"line {i:05d} — scroll over this window", xalign=0))
        sw.set_child(box)
        win.set_child(sw)
        adj = sw.get_vadjustment()
        state = {"last": adj.get_value()}

        def tick(_widget, clock):
            v = adj.get_value()
            if v != state["last"]:
                log("frame", f"{clock.get_frame_time() / 1000.0:.1f}", f"{v:.2f}", f"{v - state['last']:+.2f}")
                state["last"] = v
            return True

        sw.add_tick_callback(tick)

        flags = Gtk.EventControllerScrollFlags.BOTH_AXES | Gtk.EventControllerScrollFlags.KINETIC
        ctl = Gtk.EventControllerScroll.new(flags)
        ctl.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)

        def on_scroll(c, dx, dy):
            ev = c.get_current_event()
            src, t = "?", 0
            if ev is not None:
                t = ev.get_time()
                dev = ev.get_device()
                if dev is not None:
                    src = dev.get_source().value_nick
            log("scroll", f"{dx:+.3f}", f"{dy:+.3f}", c.get_unit().value_nick, src, t)
            return False

        ctl.connect("scroll-begin", lambda _c: log("begin"))
        ctl.connect("scroll", on_scroll)
        ctl.connect("scroll-end", lambda _c: log("end"))
        ctl.connect("decelerate", lambda _c, vx, vy: log("decelerate", f"{vx:.1f}", f"{vy:.1f}"))
        win.add_controller(ctl)
        if "max" in OPTIONS:
            win.maximize()
        win.present()

        def to_bottom():
            adj.set_value(adj.get_upper() - adj.get_page_size())
            state["last"] = adj.get_value()
            log("at-bottom", f"{adj.get_value():.0f}")
            return False

        if "bottom" in OPTIONS:
            GLib.timeout_add(1200, to_bottom)
        log("start", "gtk", Gtk.get_major_version(), Gtk.get_minor_version(), Gtk.get_micro_version())
        GLib.timeout_add_seconds(SECONDS, self.quit)


App(application_id="io.unom.punktfunk.scrolllogger").run([])
