//! fleet-conductor - Fleet orchestration conductor for distributed agent coordination.
//!
//! This crate implements the **in-memory orchestration core** of the
//! fleet conductor described in the project README: the agent lifecycle
//! state machine, the conservation-aware action guard, and a desired-state
//! reconciliation loop operating over an in-memory fleet model.
//!
//! It is deliberately scoped to logic that is real and testable without
//! any network or external dependencies. Real node scheduling, circuit
//! breakers that talk to live nodes, and integration with the external
//! `construct` / `avoidance-cascade-c` repositories are out of scope for
//! this crate and remain as planned work.

pub mod state;

pub use state::{AgentState, TransitionError};

/// Stub module retained for backward compatibility with the original
/// scaffold. Real functionality lives in the typed modules above.
pub mod stub {
    /// Placeholder function returning a greeting.
    pub fn hello() -> &'static str {
        "hello from fleet-conductor"
    }
}

#[cfg(test)]
mod tests {
    use super::stub;

    #[test]
    fn it_works() {
        assert_eq!(stub::hello(), "hello from fleet-conductor");
    }
}
