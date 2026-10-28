//! Provenance of one runtime-delivered logical physical-key press cycle.
//! This preserves the shell's existing global-physical input domain. It does
//! not claim hardware-device separation or reconcile focus-loss key state.
use super::key::Physical;
use std::{
    cell::RefCell,
    marker::PhantomData,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Debug)]
pub struct PressCycle {
    physical_key: Physical,
    down: AtomicBool,
}
impl PressCycle {
    pub fn new(physical_key: Physical) -> Arc<Self> {
        Arc::new(Self {
            physical_key,
            down: AtomicBool::new(true),
        })
    }
    pub fn physical_key(&self) -> Physical {
        self.physical_key
    }
    pub fn is_down(&self) -> bool {
        self.down.load(Ordering::Acquire)
    }
    pub fn release(&self) {
        self.down.store(false, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transition {
    Press,
    Release,
}

#[derive(Clone, Debug, Default)]
pub struct Source {
    cycle: Option<Arc<PressCycle>>,
    transition: Option<Transition>,
}
impl Source {
    pub fn tracked(cycle: Arc<PressCycle>, transition: Transition) -> Self {
        Self {
            cycle: Some(cycle),
            transition: Some(transition),
        }
    }
    pub fn cycle(&self) -> Option<&Arc<PressCycle>> {
        self.cycle.as_ref()
    }
    pub fn transition(&self) -> Option<Transition> {
        self.transition
    }
}
impl PartialEq for Source {
    fn eq(&self, other: &Self) -> bool {
        self.transition == other.transition
            && match (&self.cycle, &other.cycle) {
                (Some(left), Some(right)) => Arc::ptr_eq(left, right),
                (None, None) => true,
                _ => false,
            }
    }
}
impl Eq for Source {}

thread_local! { static CURRENT: RefCell<Source> = RefCell::new(Source::default()); }
pub fn current_source() -> Source {
    CURRENT.with(|slot| slot.borrow().clone())
}

/// A synchronous native dispatch scope. Must not cross an await or thread.
#[derive(Debug)]
pub struct Scope {
    previous: Source,
    _not_send: PhantomData<Rc<()>>,
}
pub fn scope(source: Source) -> Scope {
    let previous = CURRENT.with(|slot| slot.replace(source));
    Scope {
        previous,
        _not_send: PhantomData,
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|slot| {
            slot.replace(std::mem::take(&mut self.previous));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyboard::key;

    fn source(code: key::Code) -> Source {
        Source::tracked(
            PressCycle::new(Physical::Code(code)),
            Transition::Press,
        )
    }

    #[test]
    fn nested_scopes_restore_the_original_event_source() {
        assert_eq!(current_source(), Source::default());
        let first = source(key::Code::Enter);
        let second = source(key::Code::Escape);
        {
            let _first = scope(first.clone());
            assert_eq!(current_source(), first);
            {
                let _second = scope(second.clone());
                assert_eq!(current_source(), second);
                let _untagged = scope(Source::default());
                assert_eq!(current_source(), Source::default());
            }
            assert_eq!(current_source(), first);
        }
        assert_eq!(current_source(), Source::default());
    }

    #[test]
    fn unwinding_a_dispatch_restores_the_outer_source() {
        let outer = source(key::Code::Enter);
        let _outer = scope(outer.clone());
        let result = std::panic::catch_unwind(|| {
            let _inner = scope(source(key::Code::Escape));
            panic!("simulated widget dispatch failure");
        });
        assert!(result.is_err());
        assert_eq!(current_source(), outer);
    }
}
