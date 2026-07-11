//! In-memory fleet conductor: desired-state reconciliation over an
//! in-memory fleet model.
//!
//! This is the **orchestration core** described in the README, scoped to what
//! is real and testable without network or external dependencies. The
//! [`Conductor`] holds a `HashMap<AgentId, Agent>` in memory and reconciles a
//! desired [`FleetSpec`] against the observed state by spawning `Pending`
//! agents to cover deficits and draining excess agents -- with every drain
//! gated by the [`ConservationConfig`](crate::ConservationConfig) guard.
//!
//! ## What is real here
//! - Agent spawning, draining, and lifecycle advancement operate on a real
//!   in-memory map and go through the [`AgentState`](crate::AgentState) state
//!   machine.
//! - Reconciliation is real count-correction logic and is idempotent.
//! - Drains are gated by the conservation guard; a drain that would breach
//!   `eta_floor` is deferred, not executed.
//!
//! ## What is a stand-in (not real)
//! - $\gamma$ is held at a nominal target value; real $\gamma$ computation
//!   lives in the external conservation framework this crate does not depend
//!   on. The guard still enforces the $\gamma$ bounds.
//! - $\eta$ is modeled as `active_agent_count * eta_per_agent`. Real
//!   per-agent $\eta$ contributions would come from that same framework.
//! - There is no node scheduling, network I/O, or live health-checking; agent
//!   readiness is advanced by an explicit [`Conductor::advance_lifecycle`]
//!   tick.

use std::collections::HashMap;
use std::fmt;

use crate::{AgentState, ConservationConfig, ConservationVerdict, DeferredReason, TransitionError};

/// Stable identifier for an agent within a conductor's fleet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct AgentId(pub u64);

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "agent-{}", self.0)
    }
}

/// The kind/role of an agent, e.g. `"inference"` or `"coordinator"`.
pub type AgentKind = String;

/// Desired configuration for a class of agents within a fleet.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AgentSpec {
    /// Role of the agent (e.g. `"inference"`).
    pub kind: AgentKind,
    /// Desired number of live agents of this kind.
    pub count: usize,
    /// Logical layer / tier the agent belongs to.
    pub layer: u32,
}

/// The desired fleet state handed to [`Conductor::reconcile`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FleetSpec {
    /// Desired agent populations, one entry per kind.
    pub agents: Vec<AgentSpec>,
    /// Conservation bounds enforcing the $\gamma + \eta = C$ invariant.
    pub conservation: ConservationConfig,
}

/// A single agent in the fleet.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Agent {
    pub id: AgentId,
    pub kind: AgentKind,
    pub layer: u32,
    pub state: AgentState,
}

/// States that count as "live" (present in the fleet, serving or soon to).
fn is_live(state: AgentState) -> bool {
    matches!(
        state,
        AgentState::Pending | AgentState::Starting | AgentState::Healthy | AgentState::Degraded
    )
}

/// States that contribute to fleet $\eta$ (diversity/capacity).
fn contributes_eta(state: AgentState) -> bool {
    matches!(state, AgentState::Healthy | AgentState::Degraded)
}

/// Per-kind summary returned by [`Conductor::observe`].
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct KindSummary {
    /// Desired live count from the last [`FleetSpec`], if a spec was given.
    pub desired: Option<usize>,
    /// Observed live count (agents in Pending/Starting/Healthy/Degraded).
    pub observed: usize,
    /// Total agents of this kind (including draining/terminated).
    pub total: usize,
    /// Count of agents of this kind in each state.
    pub by_state: HashMap<AgentState, usize>,
}

/// A snapshot of the fleet at a point in time, returned by
/// [`Conductor::observe`].
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FleetState {
    /// All agents, sorted by id for stable output.
    pub agents: Vec<Agent>,
    /// Total number of agents (all states).
    pub total: usize,
    /// Number of live agents.
    pub live: usize,
    /// Current fleet $\gamma$.
    pub gamma: f64,
    /// Current fleet $\eta$.
    pub eta: f64,
    /// Per-kind summaries.
    pub by_kind: HashMap<AgentKind, KindSummary>,
}

/// Outcome of a [`Conductor::drain_agent`] call.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum DrainOutcome {
    /// The agent was moved to [`AgentState::Draining`].
    Drained {
        id: AgentId,
        new_eta: f64,
        new_c: f64,
    },
    /// The drain was deferred by the conservation guard; the agent is
    /// unchanged. Spawn a replacement or wait for $\eta$ to recover.
    Deferred {
        id: AgentId,
        reason: DeferredReason,
        would_be_eta: f64,
        would_be_c: f64,
    },
    /// The agent exists but is not in a drainable state
    /// ([`AgentState::Healthy`] or [`AgentState::Degraded`]).
    NotDrainable { id: AgentId, state: AgentState },
    /// No agent with that id.
    NotFound(AgentId),
}

impl DrainOutcome {
    /// `true` if the drain was executed.
    pub fn is_drained(&self) -> bool {
        matches!(self, DrainOutcome::Drained { .. })
    }

    /// `true` if the drain was deferred by the conservation guard.
    pub fn is_deferred(&self) -> bool {
        matches!(self, DrainOutcome::Deferred { .. })
    }
}

/// The in-memory fleet conductor.
pub struct Conductor {
    agents: HashMap<AgentId, Agent>,
    next_id: u64,
    conservation: ConservationConfig,
    /// Last desired counts per kind, recorded by `reconcile`.
    desired: HashMap<AgentKind, usize>,
    /// Nominal fleet $\gamma$ held by this in-memory core.
    gamma: f64,
    /// $\eta$ contributed by each active (Healthy/Degraded) agent.
    eta_per_agent: f64,
}

impl Default for Conductor {
    fn default() -> Self {
        Self::new()
    }
}

impl Conductor {
    /// Create an empty conductor with a permissive default conservation
    /// config (`$\gamma \in [-1, 1]$`, `$\eta \ge 0$`) and `$\eta$` per agent
    /// of `0.1`.
    pub fn new() -> Self {
        Self {
            agents: HashMap::new(),
            next_id: 0,
            conservation: ConservationConfig::default(),
            desired: HashMap::new(),
            gamma: 0.0,
            eta_per_agent: 0.1,
        }
    }

    /// Set the conservation bounds to enforce on drains.
    pub fn with_conservation(mut self, cfg: ConservationConfig) -> Self {
        self.conservation = cfg;
        self
    }

    /// Set the nominal fleet $\gamma$ (a stand-in; real $\gamma$ comes from
    /// the external conservation framework).
    pub fn with_gamma(mut self, gamma: f64) -> Self {
        self.gamma = gamma;
        self
    }

    /// Set the per-agent $\eta$ contribution used by the in-memory model.
    pub fn with_eta_per_agent(mut self, eta_per_agent: f64) -> Self {
        self.eta_per_agent = eta_per_agent;
        self
    }

    /// The conservation config currently in effect.
    pub fn conservation(&self) -> ConservationConfig {
        self.conservation
    }

    /// Current fleet $\gamma$ (nominal).
    pub fn current_gamma(&self) -> f64 {
        self.gamma
    }

    /// Current fleet $\eta$: sum of per-agent contributions from active
    /// (Healthy/Degraded) agents.
    pub fn current_eta(&self) -> f64 {
        self.eta_per_agent
            * self
                .agents
                .values()
                .filter(|a| contributes_eta(a.state))
                .count() as f64
    }

    /// Number of agents in the fleet (all states).
    pub fn agent_count(&self) -> usize {
        self.agents.len()
    }

    /// Look up an agent's state by id.
    pub fn agent_state(&self, id: AgentId) -> Option<AgentState> {
        self.agents.get(&id).map(|a| a.state)
    }

    /// Observe the current in-memory fleet state.
    pub fn observe(&self) -> FleetState {
        let mut agents: Vec<Agent> = self.agents.values().cloned().collect();
        agents.sort_by_key(|a| a.id);

        let mut by_kind: HashMap<AgentKind, KindSummary> = HashMap::new();
        for a in &agents {
            let summary = by_kind.entry(a.kind.clone()).or_default();
            summary.total += 1;
            if is_live(a.state) {
                summary.observed += 1;
            }
            *summary.by_state.entry(a.state).or_insert(0) += 1;
        }
        for (kind, desired) in &self.desired {
            by_kind.entry(kind.clone()).or_default().desired = Some(*desired);
        }

        let live = agents.iter().filter(|a| is_live(a.state)).count();
        FleetState {
            agents,
            total: self.agents.len(),
            live,
            gamma: self.current_gamma(),
            eta: self.current_eta(),
            by_kind,
        }
    }

    /// Spawn a single new agent as [`AgentState::Pending`].
    ///
    /// `spec.count` is ignored here (it is a reconcile-level concern); the
    /// agent's `kind` and `layer` are taken from `spec`. Returns the new id.
    pub fn spawn_agent(&mut self, spec: AgentSpec) -> AgentId {
        let id = AgentId(self.next_id);
        self.next_id += 1;
        self.agents.insert(
            id,
            Agent {
                id,
                kind: spec.kind,
                layer: spec.layer,
                state: AgentState::Pending,
            },
        );
        id
    }

    /// Attempt to drain an agent, gated by the conservation guard.
    ///
    /// Only [`AgentState::Healthy`] and [`AgentState::Degraded`] agents may be
    /// drained. The drain removes the agent's $\eta$ contribution; if that
    /// would breach `eta_floor` (or a $\gamma$ bound), the drain is deferred
    /// and the agent is left unchanged.
    pub fn drain_agent(&mut self, id: AgentId) -> DrainOutcome {
        // Read the agent's current state without holding a borrow across the
        // conservation check (which only reads self fields).
        let state = match self.agents.get(&id) {
            Some(a) => a.state,
            None => return DrainOutcome::NotFound(id),
        };

        if !matches!(state, AgentState::Healthy | AgentState::Degraded) {
            return DrainOutcome::NotDrainable { id, state };
        }

        let gamma = self.current_gamma();
        let eta = self.current_eta();
        // Draining this agent removes its eta contribution; gamma is nominal.
        let delta_eta = -self.eta_per_agent;
        match self.conservation.check(gamma, eta, 0.0, delta_eta) {
            ConservationVerdict::Safe {
                eta: new_eta,
                c: new_c,
                ..
            } => {
                // Perform the legal transition Healthy|Degraded -> Draining.
                let next = state
                    .transition(AgentState::Draining)
                    .expect("Healthy/Degraded -> Draining is a legal edge");
                if let Some(a) = self.agents.get_mut(&id) {
                    a.state = next;
                }
                DrainOutcome::Drained { id, new_eta, new_c }
            }
            ConservationVerdict::Deferred {
                reason,
                would_be_eta,
                would_be_c,
                ..
            } => DrainOutcome::Deferred {
                id,
                reason,
                would_be_eta,
                would_be_c,
            },
        }
    }

    /// Reconcile the fleet toward the desired `spec`.
    ///
    /// For each kind: if observed live agents are below the desired count,
    /// spawn `Pending` agents to cover the deficit; if above, drain the excess
    /// (preferring [`AgentState::Degraded`] agents, then newest-first), with
    /// each drain gated by the conservation guard. Agents of a kind absent
    /// from the spec are drained toward zero. The call is idempotent: running
    /// it again with the same spec performs no further changes.
    pub fn reconcile(&mut self, spec: FleetSpec) {
        self.conservation = spec.conservation;

        // Record desired counts.
        self.desired.clear();
        for a in &spec.agents {
            self.desired.insert(a.kind.clone(), a.count);
        }

        // All kinds known to the fleet or the spec.
        let mut kinds: std::collections::BTreeSet<AgentKind> =
            self.desired.keys().cloned().collect();
        for a in self.agents.values() {
            kinds.insert(a.kind.clone());
        }

        for kind in kinds {
            let desired = self.desired.get(&kind).copied().unwrap_or(0);
            let layer = spec
                .agents
                .iter()
                .find(|a| a.kind == kind)
                .map(|a| a.layer)
                .unwrap_or(0);

            // Live agents of this kind, sorted for deterministic drain order:
            // Degraded before Healthy, then newest (highest id) first.
            let mut live: Vec<AgentId> = self
                .agents
                .values()
                .filter(|a| a.kind == kind && is_live(a.state))
                .map(|a| a.id)
                .collect();
            live.sort_by(|&x, &y| {
                let sx = self.agents[&x].state;
                let sy = self.agents[&y].state;
                // Degraded (true) should sort before Healthy (false).
                let dx = matches!(sx, AgentState::Degraded);
                let dy = matches!(sy, AgentState::Degraded);
                dy.cmp(&dx).then_with(|| y.cmp(&x))
            });

            let live_count = live.len();
            if live_count < desired {
                let deficit = desired - live_count;
                for _ in 0..deficit {
                    self.spawn_agent(AgentSpec {
                        kind: kind.clone(),
                        count: 1,
                        layer,
                    });
                }
            } else if live_count > desired {
                let excess = live_count - desired;
                for &id in live.iter().take(excess) {
                    // Drain, respecting the conservation guard. A deferred
                    // drain is left for a subsequent cycle (real reconcile
                    // behavior -- we don't force a breach).
                    let _ = self.drain_agent(id);
                }
            }
        }
    }

    /// Advance every agent one legal step toward its steady state:
    /// - `Pending -> Starting`
    /// - `Starting -> Healthy`
    /// - `Draining -> Terminated`
    ///
    /// `Healthy`, `Degraded`, and `Terminated` are unchanged. Returns the
    /// number of agents that advanced.
    pub fn advance_lifecycle(&mut self) -> usize {
        let mut advanced = 0usize;
        for a in self.agents.values_mut() {
            let next = match a.state {
                AgentState::Pending => Some(AgentState::Starting),
                AgentState::Starting => Some(AgentState::Healthy),
                AgentState::Draining => Some(AgentState::Terminated),
                _ => None,
            };
            if let Some(next) = next {
                // The edges above are all legal; transition() documents that.
                match a.state.transition(next) {
                    Ok(s) => {
                        a.state = s;
                        advanced += 1;
                    }
                    Err(TransitionError { from, to }) => {
                        // Should be unreachable given the match above; keep
                        // the agent unchanged rather than panicking.
                        debug_assert!(
                            false,
                            "advance_lifecycle produced illegal edge {from} -> {to}"
                        );
                    }
                }
            }
        }
        advanced
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn readme_conservation() -> ConservationConfig {
        ConservationConfig::new(-0.5, 0.5, 0.3)
    }

    /// A permissive config (eta_floor = 0) for tests that exercise the
    /// drain-down / count logic in isolation from the conservation guard.
    fn permissive_conservation() -> ConservationConfig {
        ConservationConfig::new(-1.0, 1.0, 0.0)
    }

    /// A conductor wired with the README's conservation config and a generous
    /// eta_per_agent so the guard is easy to exercise.
    fn conductor() -> Conductor {
        Conductor::new()
            .with_conservation(readme_conservation())
            .with_eta_per_agent(0.1)
    }

    /// Build a spec.
    fn spec(kind: &str, count: usize, layer: u32) -> FleetSpec {
        FleetSpec {
            agents: vec![AgentSpec {
                kind: kind.to_string(),
                count,
                layer,
            }],
            conservation: readme_conservation(),
        }
    }

    /// Build a spec with a permissive conservation config.
    fn permissive_spec(kind: &str, count: usize, layer: u32) -> FleetSpec {
        FleetSpec {
            agents: vec![AgentSpec {
                kind: kind.to_string(),
                count,
                layer,
            }],
            conservation: permissive_conservation(),
        }
    }

    #[test]
    fn observe_empty_fleet() {
        let c = conductor();
        let s = c.observe();
        assert_eq!(s.total, 0);
        assert_eq!(s.live, 0);
        assert!(s.agents.is_empty());
        assert!(s.by_kind.is_empty());
    }

    #[test]
    fn spawn_agent_creates_pending_agent() {
        let mut c = conductor();
        let id = c.spawn_agent(AgentSpec {
            kind: "inference".to_string(),
            count: 1,
            layer: 0,
        });
        assert_eq!(c.agent_state(id), Some(AgentState::Pending));
        let s = c.observe();
        assert_eq!(s.total, 1);
        assert_eq!(s.live, 1);
        assert_eq!(s.by_kind["inference"].observed, 1);
        assert_eq!(s.by_kind["inference"].by_state[&AgentState::Pending], 1);
    }

    #[test]
    fn reconcile_from_empty_spawns_pending_to_target_count() {
        let mut c = conductor();
        c.reconcile(spec("inference", 3, 0));

        let s = c.observe();
        assert_eq!(s.total, 3);
        assert_eq!(s.live, 3);
        let inf = &s.by_kind["inference"];
        assert_eq!(inf.observed, 3);
        assert_eq!(inf.desired, Some(3));
        // Spawned agents start Pending.
        assert_eq!(inf.by_state[&AgentState::Pending], 3);
        assert_eq!(inf.by_state.get(&AgentState::Healthy), None);
    }

    #[test]
    fn reconcile_is_idempotent_running_twice_changes_nothing() {
        // README explicitly claims reconciliation is idempotent.
        let mut c = conductor();
        c.reconcile(spec("inference", 3, 0));
        let after_first = c.observe();

        c.reconcile(spec("inference", 3, 0));
        let after_second = c.observe();

        assert_eq!(after_first.total, after_second.total);
        assert_eq!(after_first.live, after_second.live);
        let a = &after_first.by_kind["inference"];
        let b = &after_second.by_kind["inference"];
        assert_eq!(a.observed, b.observed);
        assert_eq!(a.by_state, b.by_state);
        // No new agents were spawned on the second pass.
        assert_eq!(after_second.total, 3);
    }

    #[test]
    fn reconcile_drains_excess_down_to_target() {
        let mut c = Conductor::new()
            .with_conservation(permissive_conservation())
            .with_eta_per_agent(0.1);
        // Start with 5 Healthy inference agents.
        c.reconcile(permissive_spec("inference", 5, 0));
        c.advance_lifecycle(); // Pending -> Starting
        c.advance_lifecycle(); // Starting -> Healthy
        assert_eq!(
            c.observe().by_kind["inference"].by_state[&AgentState::Healthy],
            5
        );

        // Now lower the desired count to 2 and reconcile.
        c.reconcile(permissive_spec("inference", 2, 0));
        let s = c.observe();
        let inf = &s.by_kind["inference"];
        // 2 remain Healthy, 3 are Draining (excess).
        assert_eq!(inf.by_state[&AgentState::Healthy], 2);
        assert_eq!(inf.by_state[&AgentState::Draining], 3);
        // Observed live (Pending/Starting/Healthy/Degraded) == desired 2.
        assert_eq!(inf.observed, 2);
    }

    #[test]
    fn reconcile_drains_kinds_absent_from_spec() {
        let mut c = Conductor::new()
            .with_conservation(permissive_conservation())
            .with_eta_per_agent(0.1);
        // Fleet has coordinator agents...
        c.reconcile(permissive_spec("coordinator", 2, 1));
        c.advance_lifecycle();
        c.advance_lifecycle();
        assert_eq!(c.observe().by_kind["coordinator"].observed, 2);

        // ...but the new spec only wants inference. Coordinators should drain.
        c.reconcile(FleetSpec {
            agents: vec![AgentSpec {
                kind: "inference".to_string(),
                count: 1,
                layer: 0,
            }],
            conservation: permissive_conservation(),
        });
        let s = c.observe();
        assert_eq!(s.by_kind["coordinator"].observed, 0);
        assert_eq!(s.by_kind["coordinator"].by_state[&AgentState::Draining], 2);
        assert_eq!(s.by_kind["inference"].by_state[&AgentState::Pending], 1);
    }

    #[test]
    fn advance_lifecycle_progresses_pending_to_healthy_one_step_at_a_time() {
        let mut c = conductor();
        c.spawn_agent(AgentSpec {
            kind: "inference".to_string(),
            count: 1,
            layer: 0,
        });
        let id = AgentId(0);

        assert_eq!(c.agent_state(id), Some(AgentState::Pending));
        assert_eq!(c.advance_lifecycle(), 1);
        assert_eq!(c.agent_state(id), Some(AgentState::Starting));
        assert_eq!(c.advance_lifecycle(), 1);
        assert_eq!(c.agent_state(id), Some(AgentState::Healthy));
        // At steady state, advance does nothing.
        assert_eq!(c.advance_lifecycle(), 0);
        assert_eq!(c.agent_state(id), Some(AgentState::Healthy));
    }

    #[test]
    fn advance_lifecycle_completes_draining_to_terminated() {
        let mut c = conductor();
        c.reconcile(spec("inference", 1, 0));
        c.advance_lifecycle();
        c.advance_lifecycle();
        let id = AgentId(0);
        assert_eq!(c.agent_state(id), Some(AgentState::Healthy));
        // Drain (eta floor not binding with only the default generous config?
        // -- with eta_per_agent=0.1 and one agent eta=0.1 < floor 0.3, this
        // drain would be deferred. So use a permissive config here.)
        let mut c = Conductor::new().with_eta_per_agent(0.1);
        c.reconcile(FleetSpec {
            agents: vec![AgentSpec {
                kind: "inference".to_string(),
                count: 1,
                layer: 0,
            }],
            conservation: ConservationConfig::new(-1.0, 1.0, 0.0),
        });
        c.advance_lifecycle();
        c.advance_lifecycle();
        let id = AgentId(0);
        assert_eq!(c.agent_state(id), Some(AgentState::Healthy));
        assert!(c.drain_agent(id).is_drained());
        assert_eq!(c.agent_state(id), Some(AgentState::Draining));
        assert_eq!(c.advance_lifecycle(), 1);
        assert_eq!(c.agent_state(id), Some(AgentState::Terminated));
    }

    #[test]
    fn drain_deferred_when_it_would_breach_eta_floor() {
        // The README's canonical scenario: draining would collapse eta below
        // the floor, so the action is deferred, not executed.
        let mut c = conductor(); // eta_per_agent=0.1, eta_floor=0.3
        c.reconcile(spec("inference", 3, 0));
        c.advance_lifecycle();
        c.advance_lifecycle();
        // 3 Healthy agents -> eta = 0.3, exactly at the floor.
        assert!((c.current_eta() - 0.3).abs() < 1e-12);

        // Draining one would drop eta to 0.2 < 0.3 -> deferred.
        let outcome = c.drain_agent(AgentId(0));
        assert!(outcome.is_deferred());
        match outcome {
            DrainOutcome::Deferred {
                reason,
                would_be_eta,
                ..
            } => {
                assert_eq!(reason, DeferredReason::EtaBelowFloor);
                assert!((would_be_eta - 0.2).abs() < 1e-12);
            }
            _ => unreachable!("expected deferred"),
        }
        // The agent is unchanged (still Healthy).
        assert_eq!(c.agent_state(AgentId(0)), Some(AgentState::Healthy));
        // eta unchanged.
        assert!((c.current_eta() - 0.3).abs() < 1e-12);
    }

    #[test]
    fn drain_executes_when_eta_stays_above_floor() {
        let mut c = conductor();
        c.reconcile(spec("inference", 4, 0));
        c.advance_lifecycle();
        c.advance_lifecycle();
        // 4 Healthy -> eta = 0.4. Draining one -> 0.3 == floor (allowed).
        assert!((c.current_eta() - 0.4).abs() < 1e-12);
        let outcome = c.drain_agent(AgentId(0));
        assert!(outcome.is_drained());
        assert_eq!(c.agent_state(AgentId(0)), Some(AgentState::Draining));
        assert!((c.current_eta() - 0.3).abs() < 1e-12);
    }

    #[test]
    fn drain_unknown_agent_returns_not_found() {
        let mut c = conductor();
        assert_eq!(
            c.drain_agent(AgentId(99)),
            DrainOutcome::NotFound(AgentId(99))
        );
    }

    #[test]
    fn drain_pending_agent_is_not_drainable() {
        let mut c = conductor();
        let id = c.spawn_agent(AgentSpec {
            kind: "inference".to_string(),
            count: 1,
            layer: 0,
        });
        // Pending -> Draining is not a legal edge.
        assert_eq!(
            c.drain_agent(id),
            DrainOutcome::NotDrainable {
                id,
                state: AgentState::Pending
            }
        );
    }

    #[test]
    fn reconcile_excess_drain_respects_conservation_guard() {
        // 3 Healthy agents, eta=0.3 at floor. Spec wants 0 -> reconcile tries
        // to drain all 3, but every drain would breach the floor, so all are
        // deferred and the agents stay Healthy.
        let mut c = conductor();
        c.reconcile(spec("inference", 3, 0));
        c.advance_lifecycle();
        c.advance_lifecycle();
        assert!((c.current_eta() - 0.3).abs() < 1e-12);

        // Lower desired to 0 (drain everything). eta_floor=0.3 means draining
        // from 0.3 -> 0.2 breaches, so the first drain is deferred; with no
        // drain succeeding, eta never drops and all three stay.
        c.reconcile(spec("inference", 0, 0));
        let s = c.observe();
        let inf = &s.by_kind["inference"];
        assert_eq!(inf.by_state[&AgentState::Healthy], 3);
        assert_eq!(inf.by_state.get(&AgentState::Draining), None);
        assert_eq!(inf.observed, 3); // still all live
    }

    #[test]
    fn reconcile_excess_drain_partially_succeeds_then_defers() {
        // 4 Healthy, eta=0.4. Spec wants 0. First drain 0.4->0.3 (ok), second
        // 0.3->0.2 (deferred). So exactly one agent drains.
        let mut c = conductor();
        c.reconcile(spec("inference", 4, 0));
        c.advance_lifecycle();
        c.advance_lifecycle();

        c.reconcile(spec("inference", 0, 0));
        let s = c.observe();
        let inf = &s.by_kind["inference"];
        assert_eq!(inf.by_state[&AgentState::Draining], 1);
        assert_eq!(inf.by_state[&AgentState::Healthy], 3);
        assert_eq!(inf.observed, 3);
    }

    #[test]
    fn full_reconcile_advance_drain_terminate_lifecycle() {
        // End-to-end: empty -> reconcile -> advance -> drain -> terminate.
        let mut c = Conductor::new()
            .with_eta_per_agent(0.5)
            .with_conservation(ConservationConfig::new(-1.0, 1.0, 0.0));
        c.reconcile(spec("inference", 2, 0));
        // Both Pending.
        assert_eq!(
            c.observe().by_kind["inference"].by_state[&AgentState::Pending],
            2
        );
        c.advance_lifecycle();
        c.advance_lifecycle();
        // Both Healthy.
        assert_eq!(
            c.observe().by_kind["inference"].by_state[&AgentState::Healthy],
            2
        );
        // Drain one.
        assert!(c.drain_agent(AgentId(0)).is_drained());
        assert_eq!(c.agent_state(AgentId(0)), Some(AgentState::Draining));
        // Complete the drain.
        c.advance_lifecycle();
        assert_eq!(c.agent_state(AgentId(0)), Some(AgentState::Terminated));
        // The other is still Healthy.
        assert_eq!(c.agent_state(AgentId(1)), Some(AgentState::Healthy));
    }

    #[test]
    fn observe_reports_gamma_and_eta() {
        let mut c = conductor();
        c.reconcile(spec("inference", 3, 0));
        c.advance_lifecycle();
        c.advance_lifecycle();
        let s = c.observe();
        assert_eq!(s.gamma, 0.0);
        assert!((s.eta - 0.3).abs() < 1e-12); // 3 active * 0.1
    }

    #[test]
    fn multi_kind_reconcile() {
        let mut c = conductor();
        c.reconcile(FleetSpec {
            agents: vec![
                AgentSpec {
                    kind: "inference".to_string(),
                    count: 3,
                    layer: 0,
                },
                AgentSpec {
                    kind: "coordinator".to_string(),
                    count: 1,
                    layer: 1,
                },
            ],
            conservation: readme_conservation(),
        });
        let s = c.observe();
        assert_eq!(s.by_kind["inference"].observed, 3);
        assert_eq!(s.by_kind["coordinator"].observed, 1);
        assert_eq!(s.total, 4);
        // Layers recorded on the agents.
        assert_eq!(s.agents.iter().filter(|a| a.layer == 1).count(), 1);
    }

    #[test]
    fn agent_id_display_is_stable() {
        let mut c = conductor();
        let id = c.spawn_agent(AgentSpec {
            kind: "inference".to_string(),
            count: 1,
            layer: 0,
        });
        assert_eq!(id.to_string(), "agent-0");
    }
}
