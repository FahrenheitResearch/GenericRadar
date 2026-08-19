use serde::{Deserialize, Serialize};

/// Monotonic identity for one mutable intent domain.
///
/// Generations are copied into asynchronous requests and results. A result is
/// installable only when every generation relevant to it still matches the
/// current state. This is intentionally stronger than comparing labels or raw
/// pointers, both of which can be reused across site and workspace changes.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Generation(u64);

impl Generation {
    pub const ZERO: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    fn successor(self) -> Self {
        let next = self.0.wrapping_add(1);
        // Keep zero as the uninitialized/default generation.
        Self(if next == 0 { 1 } else { next })
    }
}

/// UI-owned monotonic generation clock.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct GenerationClock {
    current: Generation,
}

impl GenerationClock {
    pub const fn new(initial: Generation) -> Self {
        Self { current: initial }
    }

    pub const fn current(self) -> Generation {
        self.current
    }

    pub fn bump(&mut self) -> Generation {
        self.current = self.current.successor();
        self.current
    }
}

/// Complete identity of a pane render request.
///
/// `pane_id` identifies the worker lane. The generation fields identify the
/// state that produced the request. A texture/result with a different stamp is
/// stale even when its dimensions and product label happen to match.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct RenderStamp {
    pub pane_id: u8,
    pub session: Generation,
    pub frame: Generation,
    pub pane: Generation,
    pub view: Generation,
    pub palette: Generation,
    /// How far round a still-arriving sweep the pane is allowed to paint.
    ///
    /// Separate from `frame` because a live volume grows without changing
    /// identity: the reveal advances many times over one frame's lifetime, and
    /// each step is a different picture of the same data. It advances only when
    /// the previous render has landed, so a request is never made stale by the
    /// animation that asked for it.
    pub sweep: Generation,
}

impl RenderStamp {
    pub fn is_current(self, current: Self) -> bool {
        self == current
    }
}

/// Identity of retained map/overlay data.
///
/// Camera center and exact scale are deliberately absent. Dataset geometry is
/// retained in world space and camera movement is a render transform.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SceneStamp {
    pub dataset: Generation,
    pub style: Generation,
    pub projection: Generation,
    pub lod_bucket: i16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_clock_never_returns_zero_after_bump() {
        let mut clock = GenerationClock::default();
        assert_eq!(clock.bump(), Generation::new(1));
        assert_eq!(clock.bump(), Generation::new(2));
    }

    #[test]
    fn render_stamp_rejects_any_changed_domain() {
        let base = RenderStamp {
            pane_id: 0,
            session: Generation::new(1),
            frame: Generation::new(2),
            pane: Generation::new(3),
            view: Generation::new(4),
            palette: Generation::new(5),
            sweep: Generation::new(6),
        };
        assert!(base.is_current(base));
        assert!(!base.is_current(RenderStamp {
            view: Generation::new(6),
            ..base
        }));
    }
}
