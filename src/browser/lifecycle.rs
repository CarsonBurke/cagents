use super::{BrowserError, BrowserLifecycle};

#[derive(Debug, Clone)]
pub(crate) struct LifecycleMachine {
    state: BrowserLifecycle,
}

impl LifecycleMachine {
    pub(crate) fn new(visible: bool) -> Self {
        Self {
            state: if visible {
                BrowserLifecycle::Restoring
            } else {
                BrowserLifecycle::Hibernated
            },
        }
    }

    pub(crate) fn state(&self) -> BrowserLifecycle {
        self.state
    }

    pub(crate) fn resources_created(&mut self, visible: bool) -> Result<(), BrowserError> {
        self.require(&[BrowserLifecycle::Restoring, BrowserLifecycle::Hibernated])?;
        self.state = if visible {
            BrowserLifecycle::Visible
        } else {
            BrowserLifecycle::OffscreenHot
        };
        Ok(())
    }

    pub(crate) fn set_visible(&mut self, visible: bool) -> Result<(), BrowserError> {
        self.require(&[BrowserLifecycle::Visible, BrowserLifecycle::OffscreenHot])?;
        self.state = if visible {
            BrowserLifecycle::Visible
        } else {
            BrowserLifecycle::OffscreenHot
        };
        Ok(())
    }

    pub(crate) fn begin_hibernation(&mut self) -> Result<(), BrowserError> {
        self.require(&[BrowserLifecycle::Visible, BrowserLifecycle::OffscreenHot])?;
        self.state = BrowserLifecycle::Hibernating;
        Ok(())
    }

    pub(crate) fn resources_destroyed(&mut self) -> Result<(), BrowserError> {
        self.require(&[BrowserLifecycle::Hibernating])?;
        self.state = BrowserLifecycle::Hibernated;
        Ok(())
    }

    pub(crate) fn begin_restore(&mut self) -> Result<(), BrowserError> {
        self.require(&[BrowserLifecycle::Hibernated])?;
        self.state = BrowserLifecycle::Restoring;
        Ok(())
    }

    pub(crate) fn close(&mut self) {
        self.state = BrowserLifecycle::Closed;
    }

    fn require(&self, allowed: &[BrowserLifecycle]) -> Result<(), BrowserError> {
        if allowed.contains(&self.state) {
            Ok(())
        } else {
            Err(BrowserError::Servo(format!(
                "invalid browser lifecycle transition from {:?}",
                self.state
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hibernation_releases_before_restore() {
        let mut machine = LifecycleMachine::new(true);
        machine.resources_created(true).unwrap();
        assert_eq!(machine.state(), BrowserLifecycle::Visible);
        machine.set_visible(false).unwrap();
        assert_eq!(machine.state(), BrowserLifecycle::OffscreenHot);
        machine.begin_hibernation().unwrap();
        assert_eq!(machine.state(), BrowserLifecycle::Hibernating);
        machine.resources_destroyed().unwrap();
        assert_eq!(machine.state(), BrowserLifecycle::Hibernated);
        machine.begin_restore().unwrap();
        assert_eq!(machine.state(), BrowserLifecycle::Restoring);
        machine.resources_created(true).unwrap();
        assert_eq!(machine.state(), BrowserLifecycle::Visible);
    }

    #[test]
    fn invalid_transition_is_rejected() {
        let mut machine = LifecycleMachine::new(false);
        assert!(machine.begin_hibernation().is_err());
        assert_eq!(machine.state(), BrowserLifecycle::Hibernated);
    }
}
