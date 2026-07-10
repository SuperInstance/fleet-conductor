//! Runnable quick-start for fleet-conductor's in-memory orchestration core.
//!
//! Demonstrates the real, tested API: reconcile an empty fleet to a desired
//! spec, observe the result, advance the lifecycle, and drain an agent under
//! the conservation guard. Run with `cargo run --example quickstart`.

use fleet_conductor::{AgentSpec, AgentState, Conductor, ConservationConfig, FleetSpec};

fn main() {
    // An in-memory conductor with the README's example conservation bounds:
    // gamma in [-0.5, 0.5], eta >= 0.3, and each active agent contributes 0.1
    // to fleet eta.
    let mut conductor = Conductor::new()
        .with_conservation(ConservationConfig::new(-0.5, 0.5, 0.3))
        .with_eta_per_agent(0.1);

    let spec = FleetSpec {
        agents: vec![AgentSpec {
            kind: "inference".to_string(),
            count: 4,
            layer: 0,
        }],
        conservation: ConservationConfig::new(-0.5, 0.5, 0.3),
    };

    // Reconcile from an empty fleet: spawns 4 Pending agents to match the spec.
    conductor.reconcile(spec);
    let observed = conductor.observe();
    assert_eq!(observed.by_kind["inference"].observed, 4);
    assert_eq!(
        observed.by_kind["inference"].by_state[&AgentState::Pending],
        4
    );
    println!(
        "after reconcile:  {} agents, eta = {:.2}",
        observed.live, observed.eta
    );

    // Running reconcile again is idempotent (matches the README's claim).
    conductor.reconcile(FleetSpec {
        agents: vec![AgentSpec {
            kind: "inference".to_string(),
            count: 4,
            layer: 0,
        }],
        conservation: ConservationConfig::new(-0.5, 0.5, 0.3),
    });
    assert_eq!(conductor.observe().total, 4);

    // Advance agents through Pending -> Starting -> Healthy.
    conductor.advance_lifecycle();
    conductor.advance_lifecycle();
    let healthy = conductor.observe();
    assert_eq!(
        healthy.by_kind["inference"].by_state[&AgentState::Healthy],
        4
    );
    // 4 active agents * 0.1 = 0.4 eta.
    assert!((healthy.eta - 0.4).abs() < 1e-12);
    println!("after advance:    4 healthy, eta = {:.2}", healthy.eta);

    // Drain an agent. With eta = 0.4, draining one drops it to 0.3 (== floor),
    // which the conservation guard allows.
    let first = conductor.observe().agents[0].id;
    let outcome = conductor.drain_agent(first);
    assert!(outcome.is_drained());
    println!("drain {:?}: {:?}", first, outcome);

    // A second drain would drop eta to 0.2 < 0.3, so the guard defers it.
    let second = conductor.observe().agents[1].id;
    let deferred = conductor.drain_agent(second);
    assert!(deferred.is_deferred());
    println!("drain {:?}: DEFERRED (eta floor)", second);

    println!("\nall good: in-memory orchestration core behaves as documented.");
}
