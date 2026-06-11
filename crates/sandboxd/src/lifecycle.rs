//! Sandbox lifecycle state machine (spec §13.1).
//!
//! ```text
//! creating -> running -> stopping -> stopped -> resuming -> running
//!          -> deleting -> deleted
//! creating -> failed
//! running  -> failed
//! ```

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Creating,
    Running,
    Stopping,
    Stopped,
    Resuming,
    Deleting,
    Deleted,
    Failed,
}

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Creating => "creating",
            State::Running => "running",
            State::Stopping => "stopping",
            State::Stopped => "stopped",
            State::Resuming => "resuming",
            State::Deleting => "deleting",
            State::Deleted => "deleted",
            State::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<State> {
        Some(match s {
            "creating" => State::Creating,
            "running" => State::Running,
            "stopping" => State::Stopping,
            "stopped" => State::Stopped,
            "resuming" => State::Resuming,
            "deleting" => State::Deleting,
            "deleted" => State::Deleted,
            "failed" => State::Failed,
            _ => return None,
        })
    }

    /// Is the sandbox consuming CPU/memory on a node right now? Used by billing
    /// (per-second running compute) and by capacity admission.
    pub fn is_active(&self) -> bool {
        matches!(self, State::Creating | State::Running | State::Resuming)
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, State::Deleted | State::Failed)
    }

    /// Whether a transition is permitted by the state machine.
    pub fn can_transition_to(&self, next: State) -> bool {
        use State::*;
        matches!(
            (self, next),
            (Creating, Running)
                | (Creating, Failed)
                | (Running, Stopping)
                | (Running, Deleting)
                | (Running, Failed)
                | (Stopping, Stopped)
                | (Stopped, Resuming)
                | (Stopped, Deleting)
                | (Resuming, Running)
                | (Resuming, Failed)
                | (Deleting, Deleted)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::State::*;

    #[test]
    fn happy_path() {
        assert!(Creating.can_transition_to(Running));
        assert!(Running.can_transition_to(Stopping));
        assert!(Stopping.can_transition_to(Stopped));
        assert!(Stopped.can_transition_to(Resuming));
        assert!(Resuming.can_transition_to(Running));
        assert!(Running.can_transition_to(Deleting));
        assert!(Deleting.can_transition_to(Deleted));
    }

    #[test]
    fn illegal_transitions_rejected() {
        assert!(!Stopped.can_transition_to(Running)); // must go via resuming
        assert!(!Deleted.can_transition_to(Running));
        assert!(!Running.can_transition_to(Stopped)); // must go via stopping
    }
}
