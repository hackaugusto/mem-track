use std::cell::Cell;

thread_local! {
    static STATE: Cell<State> = Cell::new(State::CanTrack);
}

/// Tracking must be disabled when running the tracker code itself.
///
/// This is because tracking the tracker would cause infinite recursion or cause
/// deadlock when changing the underlying containers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum State {
    CanTrack = 0,
    MustNotTrack = 1,
}

/// Must be used for all public APIs (include the allocator itself).
///
/// This prevents dead locks and infinite recursion by forbidding running the tracking code multiple times.
pub(crate) struct StateGuard {
    old: State,
}

impl StateGuard {
    pub(crate) fn new() -> Self {
        let old = STATE.replace(State::MustNotTrack);
        Self { old }
    }

    pub(crate) fn track() -> Option<Self> {
        match STATE.get() {
            State::CanTrack => Some(Self::new()),
            State::MustNotTrack => None,
        }
    }
}

impl Drop for StateGuard {
    fn drop(&mut self) {
        STATE.set(self.old);
    }
}
