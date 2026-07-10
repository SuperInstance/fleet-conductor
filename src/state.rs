//! Agent lifecycle state machine.
//!
//! Implements the state machine described in the crate-level README:
//!
//! ```text
//! Pending -> Starting -> Healthy <-> Degraded -> Draining -> Terminated
//! ```
//!
//! Only the edges shown above are legal. Self-transitions and any edge not
//! explicitly listed are rejected.

use std::fmt;

/// The lifecycle state of a single agent in the fleet.
///
/// The canonical progression is
/// `Pending -> Starting -> Healthy <-> Degraded -> Draining -> Terminated`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentState {
    /// Allocation requested, not yet scheduled.
    Pending,
    /// Scheduled to a node, initializing.
    Starting,
    /// Running and serving requests.
    Healthy,
    /// Running but failing health checks (high latency, error rate).
    Degraded,
    /// Stop accepting new requests; finish in-flight work before termination.
    Draining,
    /// Fully stopped; resources released. Terminal state.
    Terminated,
}

impl AgentState {
    /// Returns `true` if `self` may transition to `to` under the documented
    /// state machine.
    ///
    /// Legal edges:
    /// - `Pending -> Starting`
    /// - `Starting -> Healthy`
    /// - `Healthy -> Degraded` and `Degraded -> Healthy`
    /// - `Healthy -> Draining` and `Degraded -> Draining`
    /// - `Draining -> Terminated`
    ///
    /// Everything else -- including self-transitions and any move back to
    /// `Pending` -- is illegal. `Terminated` is terminal.
    pub fn can_transition(&self, to: AgentState) -> bool {
        matches!(
            (self, to),
            (AgentState::Pending, AgentState::Starting)
                | (AgentState::Starting, AgentState::Healthy)
                | (AgentState::Healthy, AgentState::Degraded)
                | (AgentState::Degraded, AgentState::Healthy)
                | (AgentState::Healthy, AgentState::Draining)
                | (AgentState::Degraded, AgentState::Draining)
                | (AgentState::Draining, AgentState::Terminated)
        )
    }

    /// Attempts to transition from `self` to `to`.
    ///
    /// Returns `Ok(to)` if the edge is legal, otherwise an
    /// [`InvalidTransition`] describing the rejected move. `self` is consumed
    /// only on success; on error the original state is preserved inside the
    /// error so callers can recover it.
    pub fn transition(&self, to: AgentState) -> Result<AgentState, TransitionError> {
        if self.can_transition(to) {
            Ok(to)
        } else {
            Err(TransitionError::new(*self, to))
        }
    }
}

impl fmt::Display for AgentState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentState::Pending => write!(f, "Pending"),
            AgentState::Starting => write!(f, "Starting"),
            AgentState::Healthy => write!(f, "Healthy"),
            AgentState::Degraded => write!(f, "Degraded"),
            AgentState::Draining => write!(f, "Draining"),
            AgentState::Terminated => write!(f, "Terminated"),
        }
    }
}

/// Error returned when a requested [`AgentState`] transition is not a legal
/// edge of the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransitionError {
    /// The state the agent was in when the illegal transition was requested.
    pub from: AgentState,
    /// The state that was requested.
    pub to: AgentState,
}

impl TransitionError {
    pub fn new(from: AgentState, to: AgentState) -> Self {
        Self { from, to }
    }

    /// `true` if the failure was because `from` is the terminal state.
    pub fn was_terminal(&self) -> bool {
        self.from == AgentState::Terminated
    }
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.was_terminal() {
            write!(
                f,
                "illegal transition: {} is terminal and cannot move to {}",
                self.from, self.to
            )
        } else {
            write!(
                f,
                "illegal transition: {} -> {} is not a valid edge of the agent state machine",
                self.from, self.to
            )
        }
    }
}

impl std::error::Error for TransitionError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every edge explicitly listed in the README must succeed.
    #[test]
    fn all_documented_edges_succeed() {
        let cases = [
            (AgentState::Pending, AgentState::Starting),
            (AgentState::Starting, AgentState::Healthy),
            (AgentState::Healthy, AgentState::Degraded),
            (AgentState::Degraded, AgentState::Healthy),
            (AgentState::Healthy, AgentState::Draining),
            (AgentState::Degraded, AgentState::Draining),
            (AgentState::Draining, AgentState::Terminated),
        ];
        for (from, to) in cases {
            assert!(
                from.can_transition(to),
                "can_transition({from} -> {to}) should be true"
            );
            assert_eq!(
                from.transition(to),
                Ok(to),
                "transition({from} -> {to}) should succeed"
            );
        }
    }

    #[test]
    fn pending_to_healthy_is_rejected() {
        // README explicitly calls out: must not skip Starting.
        let err = AgentState::Pending
            .transition(AgentState::Healthy)
            .expect_err("Pending -> Healthy must be rejected");
        assert_eq!(err.from, AgentState::Pending);
        assert_eq!(err.to, AgentState::Healthy);
        assert!(!err.was_terminal());
        assert!(err.to_string().contains("Pending"));
        assert!(err.to_string().contains("Healthy"));
    }

    #[test]
    fn nothing_can_return_to_pending() {
        // README explicitly calls out: anything -> Pending is illegal.
        for from in [
            AgentState::Starting,
            AgentState::Healthy,
            AgentState::Degraded,
            AgentState::Draining,
            AgentState::Terminated,
        ] {
            assert!(
                !from.can_transition(AgentState::Pending),
                "{from} -> Pending should be rejected"
            );
        }
    }

    #[test]
    fn terminated_is_terminal() {
        // No legal edge leaves Terminated.
        for to in [
            AgentState::Pending,
            AgentState::Starting,
            AgentState::Healthy,
            AgentState::Degraded,
            AgentState::Draining,
            AgentState::Terminated,
        ] {
            let err = AgentState::Terminated
                .transition(to)
                .expect_err("Terminated should reject every outgoing edge");
            assert!(err.was_terminal(), "Terminated -> {to} should be terminal");
        }
    }

    #[test]
    fn self_transitions_are_rejected() {
        // The documented graph has no self-loops; the state machine is strict.
        for s in [
            AgentState::Pending,
            AgentState::Starting,
            AgentState::Healthy,
            AgentState::Degraded,
            AgentState::Draining,
            AgentState::Terminated,
        ] {
            assert!(
                !s.can_transition(s),
                "{s} -> {s} (self-transition) should be rejected"
            );
        }
    }

    #[test]
    fn skipped_states_are_rejected() {
        // Pending -> Draining, Pending -> Terminated, Starting -> Draining, etc.
        let invalid = [
            (AgentState::Pending, AgentState::Degraded),
            (AgentState::Pending, AgentState::Draining),
            (AgentState::Pending, AgentState::Terminated),
            (AgentState::Starting, AgentState::Degraded),
            (AgentState::Starting, AgentState::Draining),
            (AgentState::Starting, AgentState::Terminated),
            (AgentState::Healthy, AgentState::Terminated),
            (AgentState::Degraded, AgentState::Terminated),
        ];
        for (from, to) in invalid {
            assert!(
                !from.can_transition(to),
                "{from} -> {to} should be rejected"
            );
        }
    }

    #[test]
    fn full_lifecycle_walk_end_to_end() {
        // The canonical happy path from the README, walked step by step.
        let mut s = AgentState::Pending;
        s = s.transition(AgentState::Starting).unwrap();
        assert_eq!(s, AgentState::Starting);
        s = s.transition(AgentState::Healthy).unwrap();
        assert_eq!(s, AgentState::Healthy);
        // A health blip and recovery along the way.
        s = s.transition(AgentState::Degraded).unwrap();
        assert_eq!(s, AgentState::Degraded);
        s = s.transition(AgentState::Healthy).unwrap();
        assert_eq!(s, AgentState::Healthy);
        // Graceful drain then termination.
        s = s.transition(AgentState::Draining).unwrap();
        assert_eq!(s, AgentState::Draining);
        s = s.transition(AgentState::Terminated).unwrap();
        assert_eq!(s, AgentState::Terminated);
        // Terminal: further moves impossible.
        assert!(s.transition(AgentState::Healthy).is_err());
    }

    #[test]
    fn degraded_can_drain_directly() {
        // Degraded -> Draining is a documented edge (don't require recovery first).
        assert_eq!(
            AgentState::Degraded.transition(AgentState::Draining),
            Ok(AgentState::Draining)
        );
    }

    #[test]
    fn transition_error_is_std_error() {
        // The error type should be usable with `?` and the std Error trait.
        fn do_it(s: AgentState) -> Result<AgentState, Box<dyn std::error::Error>> {
            Ok(s.transition(AgentState::Healthy)?)
        }
        let err = do_it(AgentState::Pending).unwrap_err();
        assert!(err.to_string().contains("Pending"));
    }
}
