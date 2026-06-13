# fleet-conductor — Distributed Agent Fleet Orchestration

**fleet-conductor** is a Rust library that orchestrates distributed agent fleets — coordinating the lifecycle (spawn, health-check, scale, terminate) of agents running across heterogeneous nodes. It provides the conductor primitive that sits above `construct` (topology definition) and below the fleet management UI, issuing commands like "deploy 10 inference agents to node-A" or "drain agent-7 for maintenance" while respecting conservation constraints.

## Why It Matters

Running a fleet of AI agents across multiple machines is an operations problem with real consequences: agents hold conversational state, have active sessions with users, and must be migrated without dropping connections. A conductor provides the **control plane** — a single authority that knows the desired state ("20 agents across 5 nodes with γ ≈ 0.3") and reconciles it against the actual state ("18 agents, one node at γ = 0.8"). Without centralized coordination, agents can form split-brain scenarios, oversubscribe resources, or create avoidance cascades when multiple nodes independently decide to shed load. fleet-conductor pairs with `construct` for topology and `avoidance-cascade-c` for safety to provide a complete fleet management primitive.

## How It Works

### Desired-State Reconciliation

The conductor follows the **reconciliation loop** pattern (used by Kubernetes controllers):

```
loop:
    observed = observe_current_state()
    desired  = get_desired_state()
    diff     = compute_diff(observed, desired)
    for action in diff:
        execute(action)
    sleep(reconcile_interval)
```

Each iteration observes the fleet state (agent counts, health, distribution), compares against the desired specification, and generates corrective actions (spawn, migrate, terminate). The reconciliation is **idempotent** — running it multiple times produces the same result, making it safe under network partitions.

### Agent Lifecycle Management

The conductor manages each agent through a state machine:

```
Pending → Starting → Healthy ↔ Degraded → Draining → Terminated
```

- **Pending**: Allocation requested, not yet scheduled.
- **Starting**: Scheduled to a node, initializing.
- **Healthy**: Running and serving requests.
- **Degraded**: Running but failing health checks (high latency, error rate > threshold).
- **Draining**: Stop accepting new requests; finish in-flight work before termination.
- **Terminated**: Fully stopped; resources released.

State transitions are guarded by predicates — e.g., Healthy → Draining requires the conservation check (does removing this agent destabilize γ + η = C?).

### Conservation-Aware Scheduling

The conductor integrates with the conservation framework. Before scaling up or down, it checks:

$$C_{\text{current}} = \gamma + \eta$$

If a planned action would push $C$ outside the stable range $[C_{\min}, C_{\max}]$, the action is deferred. For example, terminating an agent that contributes high entropy (η) might collapse the remaining population's diversity below the floor — the conductor detects this and either (a) spawns a replacement agent first, or (b) delays termination until entropy recovers.

### Load Balancing & Circuit Breaking

Agents are distributed across nodes using a **weighted round-robin** policy, where weights are inversely proportional to node load. Circuit breakers (from the `construct-coordination` dependency chain) prevent cascading failures: if a node's error rate exceeds threshold, the conductor stops routing new agents there and initiates drain procedures for existing ones.

### Complexity

| Operation | Cost |
|-----------|------|
| Reconciliation cycle | $O(N + E)$ where $N$ = agents, $E$ = edges |
| Agent scheduling decision | $O(k)$ where $k$ = candidate nodes |
| Health check fan-out | $O(n)$ parallel, $O(n/c)$ rounds with $c$ concurrency |
| Conservation check | $O(n)$ to compute γ, $O(3)$ for η (3 action types) |

## Quick Start

```rust
use fleet_conductor::stub;

fn main() {
    println!("{}", stub::hello());
    // "hello from fleet-conductor"
}

// When fully implemented:
use fleet_conductor::{Conductor, FleetSpec, NodeSpec};

let conductor = Conductor::new();

let spec = FleetSpec {
    agents: vec![
        AgentSpec { kind: "inference", count: 10, layer: 0 },
        AgentSpec { kind: "coordinator", count: 2, layer: 1 },
    ],
    conservation: ConservationConfig {
        gamma_min: -0.5,
        gamma_max: 0.5,
        eta_floor: 0.3,
    },
};

conductor.reconcile(spec);
```

```bash
# Build
git clone https://github.com/SuperInstance/fleet-conductor.git
cd fleet-conductor
cargo build --release

# Test
cargo test
```

## API

```rust
// Current (stub) implementation:
pub mod stub {
    pub fn hello() -> &'static str;
}

// Planned API (per description):
pub struct Conductor { /* ... */ }

impl Conductor {
    pub fn new() -> Self;
    pub fn reconcile(&mut self, spec: FleetSpec);
    pub fn observe(&self) -> FleetState;
    pub fn drain_agent(&mut self, agent_id: &str);
    pub fn spawn_agent(&mut self, spec: AgentSpec) -> AgentId;
}

pub struct FleetSpec {
    pub agents: Vec<AgentSpec>,
    pub conservation: ConservationConfig,
}

pub struct ConservationConfig {
    pub gamma_min: f64,
    pub gamma_max: f64,
    pub eta_floor: f64,
}
```

## Architecture Notes

fleet-conductor is the **control plane** of the SuperInstance fleet. It consumes topology definitions from `construct`, runs coordination experiments from `construct-coordination` to validate configuration changes before deployment, and enforces the γ + η = C conservation invariant at the fleet level. The conductor is the only component authorized to spawn or terminate agents — this single-writer design prevents race conditions where multiple controllers create conflicting agent populations. In the conservation framework, the conductor's job is to keep $C$ stable while the underlying agent population evolves continuously.

See: [SuperInstance Architecture](https://github.com/SuperInstance/SuperInstance/blob/main/ARCHITECTURE.md)

## References

1. Burns, B. et al. (2015). "Borg, Omega, and Kubernetes." *ACM Queue* 14(1) — The reconciliation-loop pattern and desired-state convergence used by the conductor.
2. Oppenheimer, D. et al. (2016). "Designing for Failures: Lessons from Large-Scale Cluster Management." — Circuit breaking, draining, and graceful degradation in fleet systems.

## License

MIT
