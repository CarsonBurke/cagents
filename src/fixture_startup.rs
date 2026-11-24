//! Startup phases a daemon fixture passes, so a test that times out can name
//! where startup stopped. The phases are reported here, below the daemon,
//! because shared code such as the update's ready proof reports them too;
//! the daemon's exit-drain tests register the observer that records them.

use std::sync::OnceLock;

/// What receives each phase, and the error a phase stopped on.
pub type Observer = fn(&'static str, Option<&str>);

static OBSERVER: OnceLock<Observer> = OnceLock::new();

/// Records phases from now on in this process. Only the first observer
/// registered receives them.
pub fn register(observer: Observer) {
    let _ = OBSERVER.set(observer);
}

/// Reports reaching `phase`, or stopping there with `error`.
pub fn observe(phase: &'static str, error: Option<&str>) {
    if let Some(observer) = OBSERVER.get() {
        observer(phase, error);
    }
}
