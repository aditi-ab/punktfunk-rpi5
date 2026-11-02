//! Fluent motion for windows-reactor: a value eased over component-timer frames, and the
//! system switch that turns every tween into a cut. A tween is driven by its component's own
//! messages, so it needs no thread and stops when the component drops it.

use std::sync::OnceLock;
use std::time::Duration;
use windows_reactor::{Component, ComponentContext, ComponentTimer};

/// One frame of a tween: 60 Hz, the cadence WinUI's own transitions step at.
pub const FRAME: Duration = Duration::from_millis(16);

/// `UISettings.AnimationsEnabled`, the system "Animation effects" switch. Read once per
/// process.
pub fn animations_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        windows::UI::ViewManagement::UISettings::new()
            .and_then(|s| s.AnimationsEnabled())
            .unwrap_or(true)
    })
}

/// A value eased from `from` to `to` over `frames` ticks. Each tick is a message the owning
/// component hands back to [`Tween::tick`]. The held timer cancels on drop, so replacing a
/// tween stops the one it replaces; with animations off it starts settled.
pub struct Tween {
    from: f64,
    to: f64,
    frame: u32,
    frames: u32,
    timer: Option<ComponentTimer>,
}

impl Tween {
    pub fn settled(value: f64) -> Tween {
        Tween {
            from: value,
            to: value,
            frame: 0,
            frames: 0,
            timer: None,
        }
    }

    pub fn start<C>(
        from: f64,
        to: f64,
        frames: u32,
        tick: C::Message,
        cx: &ComponentContext<C>,
    ) -> Tween
    where
        C: Component,
        C::Message: Send,
    {
        if !animations_enabled() || frames == 0 {
            return Tween::settled(to);
        }
        Tween {
            from,
            to,
            frame: 0,
            frames,
            timer: Some(cx.set_timeout(FRAME, tick)),
        }
    }

    /// Advance one frame; `tick` is the message the next frame arrives as.
    pub fn tick<C>(&mut self, tick: C::Message, cx: &ComponentContext<C>)
    where
        C: Component,
        C::Message: Send,
    {
        self.frame = (self.frame + 1).min(self.frames);
        self.timer = (self.frame < self.frames).then(|| cx.set_timeout(FRAME, tick));
    }

    fn progress(&self) -> f64 {
        if self.frames == 0 {
            1.0
        } else {
            f64::from(self.frame) / f64::from(self.frames)
        }
    }

    /// Where the tween ends.
    pub fn target(&self) -> f64 {
        self.to
    }

    pub fn linear(&self) -> f64 {
        self.from + (self.to - self.from) * self.progress()
    }

    /// Ease-out cubic between the ends.
    pub fn eased(&self) -> f64 {
        self.from + (self.to - self.from) * (1.0 - (1.0 - self.progress()).powi(3))
    }
}

#[cfg(test)]
mod tests {
    use super::Tween;

    fn at(frame: u32, frames: u32) -> Tween {
        Tween {
            from: 0.0,
            to: 1.0,
            frame,
            frames,
            timer: None,
        }
    }

    #[test]
    fn the_ease_runs_from_zero_to_one_and_decelerates() {
        assert_eq!(at(0, 14).eased(), 0.0);
        assert_eq!(at(14, 14).eased(), 1.0);
        assert_eq!(at(6, 12).eased(), 0.875);
        let steps: Vec<f64> = (0..=14).map(|i| at(i, 14).eased()).collect();
        assert!(steps.windows(2).all(|w| w[0] < w[1]));
        assert!(steps[1] - steps[0] > steps[14] - steps[13]);
        assert_eq!(Tween::settled(0.3).eased(), 0.3);
    }
}
