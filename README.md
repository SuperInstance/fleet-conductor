# fleet-conductor — Distributed Agent Fleet Orchestration

**fleet-conductor** is a Rust library that orchestrates distributed agent fleets — coordinating the lifecycle (spawn, health-check, scale, terminate) of agents running across heterogeneous nodes. It provides the conductor primitive that sits above `construct` (topology definition) and below the fleet management UI, issuing commands like "deploy 10 inference agents to node-A" or "drain agent-7 for maintenance" while respecting conservation constraints.

> ## Scope of this crate (read this first)
>
> This crate implements the **real, tested in-memory orchestration core** of the
> conductor: the agent lifecycle state machine, the conservation-aware action
> guard, and a desired-state reconciliation loop operating over an in-memory
> fleet model. Everything in this crate is real Rust that compiles and is
> covered by `cargo test` (45 tests).
>
> It now includes a **real HTTP server** (`ConductorServer`) that exposes the
> in-memory core over a TCP socket — two separate OS processes can coordinate
> fleet state through genuine network I/O. This is verified by integration
> tests that start the server on a real OS-assigned port and exercise it as a
> TCP client, including one that spawns the server as an actual subprocess.
>
> The following remain genuinely out of scope and planned work (🔮):
>
> - 🔮 **Real node scheduling** — agents are spawned into an in-memory map, not
>   scheduled onto real nodes. The HTTP API exposes the conductor over the
>   network, but agents are still in-memory entities, not processes on real
>   machines.
> - 🔮 **Live circuit breakers** — there is no real node error rate to break
>   on.
> - 🔮 **Integration with the external `construct` / `avoidance-cascade-c`
>   repositories** — topology definitions and the deeper conservation dynamics
>   are consumed from those crates, which are not available here.
> - 🔮 **Real γ / η computation** — the conservation *guard* is real and fully
>   tested, but the metrics that feed it are simple stand-ins (η is modeled as
>   `active_agent_count * eta_per_agent`; γ is held at a nominal target). The
>   real γ/η computation lives in the external conservation framework.
>
> This mirrors the honest scoping used by sibling crates in this pass (e.g.
> `vessel-bridge`, scoped as a real data model and validation layer rather
> than a working hardware bridge): a smaller, real, tested core is better
> than an ambitious, partially-fake one.

## Why It Matters

Running a fleet of AI agents across multiple machines is an operations problem with real consequences: agents hold conversational state, have active sessions with users, and must be migrated without dropping connections. A conductor provides the **control plane** — a single authority that knows the desired state ("20 agents across 5 nodes with γ ≈ 0.3") and reconciles it against the actual state ("18 agents, one node at γ = 0.8"). Without centralized coordination, agents can form split-brain scenarios, oversubscribe resources, or create avoidance cascades when multiple nodes independently decide to shed load. In the full system design, fleet-conductor is intended to pair with `construct` for topology and `avoidance-cascade-c` for safety to provide a fleet management primitive (these cross-repo integrations are 🔮 planned, not yet present in this crate).

## How It Works

### Desired-State Reconciliation

The conductor follows the **reconciliation loop** pattern (used by Kubernetes controllers):

```text
loop:
    observed = observe_current_state()
    desired  = get_desired_state()
    diff     = compute_diff(observed, desired)
    for action in diff:
        execute(action)
    sleep(reconcile_interval)
```

Each iteration observes the fleet state (agent counts, health, distribution), compares against the desired specification, and generates corrective actions (spawn, migrate, terminate). The reconciliation is **idempotent** — running it multiple times produces the same result, making it safe under network partitions.

**Status: implemented in-memory.** [`Conductor::reconcile`] compares desired vs. observed counts per agent kind and performs real, observable state changes on the in-memory map (spawning `Pending` agents to reach the desired count, or marking excess agents for `Draining`). [`Conductor::observe`] returns the current in-memory state. Idempotency is asserted by tests.

### Agent Lifecycle Management

The conductor manages each agent through a state machine:

```text
Pending → Starting → Healthy ↔ Degraded → Draining → Terminated
```

- **Pending**: Allocation requested, not yet scheduled.
- **Starting**: Scheduled to a node, initializing.
- **Healthy**: Running and serving requests.
- **Degraded**: Running but failing health checks (high latency, error rate > threshold).
- **Draining**: Stop accepting new requests; finish in-flight work before termination.
- **Terminated**: Fully stopped; resources released.

State transitions are guarded by predicates — e.g. Healthy → Draining requires the conservation check (does removing this agent destabilize γ + η = C?).

**Status: implemented.** [`AgentState::transition`] only allows the documented edges and rejects invalid jumps (e.g. Pending → Healthy directly, or anything → Pending). [`Conductor::advance_lifecycle`] advances agents one legal step toward their steady state. Every legal edge, several illegal ones, and a full end-to-end lifecycle walk are covered by tests.

### Conservation-Aware Scheduling

The conductor integrates with the conservation framework. Before scaling up or down, it checks:

$$C_{\text{current}} = \gamma + \eta$$

If a planned action would push $C$ outside the stable range $[C_{\min}, C_{\max}]$, the action is deferred. For example, terminating an agent that contributes high entropy (η) might collapse the remaining population's diversity below the floor — the conductor detects this and either (a) spawns a replacement agent first, or (b) delays termination until entropy recovers.

**Status: the guard is implemented and tested.** [`ConservationConfig::check`] takes a current (γ, η) pair, a planned (Δγ, Δη), and the config, and returns a [`ConservationVerdict`] (`Safe` or `Deferred` with the breached bound). [`Conductor::drain_agent`] routes every drain through this guard; a drain that would breach `eta_floor` is deferred, not executed. Tests cover the safe range, the eta-floor drain case, both γ bounds (inclusive), NaN inputs, an inverted config, and the "spawn a replacement first" recovery path.

### Networked Operation (HTTP API) ✅

The `ConductorServer` wraps the in-memory `Conductor` behind a minimal
HTTP/1.1 server built on `std::net::TcpListener`. Two separate OS processes
can coordinate fleet state through real TCP connections — no shared memory,
no mocks.

**Status: ✅ implemented and tested over real sockets.** The integration test
in `tests/network_integration.rs` starts the server on a real OS-assigned
port and exercises the full register → advance → drain → terminate lifecycle
as a genuine TCP client, including conservation-guard enforcement (a drain
that would breach `eta_floor` is correctly deferred over the network). A
second test spawns the server as an actual subprocess (separate PID) and
coordinates with it over a real socket.

| Method | Path         | Body             | Action                          |
|--------|--------------|------------------|---------------------------------|
| GET    | `/health`    | —                | Liveness probe.                 |
| GET    | `/fleet`     | —                | Observe current fleet state.    |
| POST   | `/reconcile` | `FleetSpec` JSON | Reconcile toward desired state. |
| POST   | `/advance`   | —                | Advance all agent lifecycles.   |
| POST   | `/drain/:id` | —                | Drain an agent (guard-gated).   |
| GET    | `/agent/:id` | —                | Get an agent's current state.   |

```bash
# Run the server
cargo run --bin conductor-server -- 127.0.0.1:7878

# In another terminal:
curl http://127.0.0.1:7878/health
curl -X POST http://127.0.0.1:7878/reconcile \
  -H 'Content-Type: application/json' \
  -d '{"agents":[{"kind":"inference","count":4,"layer":0}],"conservation":{"gamma_min":-0.5,"gamma_max":0.5,"eta_floor":0.3}}'
curl http://127.0.0.1:7878/fleet
```

### Load Balancing & Circuit Breaking 🔮

Agents are distributed across nodes using a **weighted round-robin** policy, where weights are inversely proportional to node load. Circuit breakers (from the `construct-coordination` dependency chain) prevent cascading failures: if a node's error rate exceeds threshold, the conductor stops routing new agents there and initiates drain procedures for existing ones.

**Status: 🔮 planned.** The HTTP API exposes the conductor over the network, but agents are still in-memory entities — there are no real compute nodes to schedule onto and no live error rates to break on. Weighted round-robin placement and live circuit breaking remain out of scope.

### Complexity

| Operation | Cost |
|-----------|------|
| Reconciliation cycle | $O(N + E)$ where $N$ = agents, $E$ = edges |
| Agent scheduling decision | $O(k)$ where $k$ = candidate nodes |
| Health check fan-out | $O(n)$ parallel, $O(n/c)$ rounds with $c$ concurrency |
| Conservation check | $O(n)$ to compute γ, $O(3)$ for η (3 action types) |

## Quick Start

This is real, runnable code — see [`examples/quickstart.rs`](examples/quickstart.rs), run with `cargo run --example quickstart`:

```rust
use fleet_conductor::{
    AgentSpec, AgentState, Conductor, ConservationConfig, FleetSpec,
};

// An in-memory conductor with the README's example conservation bounds:
// gamma in [-0.5, 0.5], eta >= 0.3, each active agent contributes 0.1 to eta.
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
assert_eq!(observed.by_kind["inference"].by_state[&AgentState::Pending], 4);

// Running reconcile again is idempotent.
// (see examples/quickstart.rs for the full run, including a conservation-
//  gated drain that executes and one that is deferred.)
```

```bash
# Build
git clone https://github.com/SuperInstance/fleet-conductor.git
cd fleet-conductor
cargo build --release

# Run the example
cargo run --example quickstart

# Run the HTTP server (networked mode)
cargo run --release --bin conductor-server -- 127.0.0.1:7878

# Test (includes real TCP socket integration tests)
cargo test
```

## API

### Implemented (in-memory orchestration core)

```rust
// The agent lifecycle state machine (src/state.rs).
pub enum AgentState { Pending, Starting, Healthy, Degraded, Draining, Terminated }

impl AgentState {
    pub fn can_transition(&self, to: AgentState) -> bool;
    pub fn transition(&self, to: AgentState) -> Result<AgentState, TransitionError>;
}

// The conservation guard (src/conservation.rs).
pub struct ConservationConfig {
    pub gamma_min: f64,
    pub gamma_max: f64,
    pub eta_floor: f64,
}

impl ConservationConfig {
    pub const fn new(gamma_min: f64, gamma_max: f64, eta_floor: f64) -> Self;
    pub fn c_min(&self) -> f64;                       // gamma_min + eta_floor
    pub fn is_sound(&self) -> bool;
    pub fn check(&self, gamma: f64, eta: f64,
                 delta_gamma: f64, delta_eta: f64) -> ConservationVerdict;
    pub fn is_safe(&self, gamma: f64, eta: f64,
                   delta_gamma: f64, delta_eta: f64) -> bool;
}

pub enum ConservationVerdict {
    Safe { gamma: f64, eta: f64, c: f64 },
    Deferred { reason: DeferredReason, would_be_gamma: f64,
               would_be_eta: f64, would_be_c: f64, limit: f64 },
}

// The in-memory conductor (src/conductor.rs).
pub struct Conductor { /* HashMap<AgentId, Agent> in memory */ }

impl Conductor {
    pub fn new() -> Self;
    pub fn with_conservation(self, cfg: ConservationConfig) -> Self;
    pub fn with_gamma(self, gamma: f64) -> Self;          // nominal gamma stand-in
    pub fn with_eta_per_agent(self, eta_per_agent: f64) -> Self;
    pub fn observe(&self) -> FleetState;
    pub fn reconcile(&mut self, spec: FleetSpec);          // idempotent
    pub fn spawn_agent(&mut self, spec: AgentSpec) -> AgentId;
    pub fn drain_agent(&mut self, id: AgentId) -> DrainOutcome;  // guard-gated
    pub fn advance_lifecycle(&mut self) -> usize;          // Pending->Starting->Healthy, Draining->Terminated
    pub fn current_gamma(&self) -> f64;
    pub fn current_eta(&self) -> f64;
}

pub struct FleetSpec  { pub agents: Vec<AgentSpec>, pub conservation: ConservationConfig }
pub struct AgentSpec  { pub kind: String, pub count: usize, pub layer: u32 }
pub struct Agent      { pub id: AgentId, pub kind: String, pub layer: u32, pub state: AgentState }
pub struct FleetState { pub agents: Vec<Agent>, pub total: usize, pub live: usize,
                        pub gamma: f64, pub eta: f64, pub by_kind: HashMap<String, KindSummary> }
pub enum  DrainOutcome { Drained { .. }, Deferred { .. }, NotDrainable { .. }, NotFound(..) }
pub struct AgentId(pub u64);
```

### Planned 🔮 (out of scope for this crate)

These remain framed as planned because they require infrastructure this crate
does not own (real compute nodes, the external `construct` /
`avoidance-cascade-c` repositories):

- 🔮 Real **node scheduling** — weighted round-robin placement onto live
  compute nodes. The HTTP API exposes the conductor over the network, but
  agents are still in-memory entities, not processes on real machines.
- 🔮 Live **circuit breakers** that read real node error rates and trip.
- 🔮 **Integration with `construct`** for topology definitions and
  `avoidance-cascade-c` for the deeper γ/η dynamics that feed the
  conservation guard.
- 🔮 Real **γ / η computation** from fleet telemetry (the guard itself is
  real; the metrics fed to it are stand-ins here).

## Architecture Notes

fleet-conductor is the **control plane** of the SuperInstance fleet. It consumes topology definitions from `construct`, runs coordination experiments from `construct-coordination` to validate configuration changes before deployment, and enforces the γ + η = C conservation invariant at the fleet level. The conductor is the only component authorized to spawn or terminate agents — this single-writer design prevents race conditions where multiple controllers create conflicting agent populations. In the conservation framework, the conductor's job is to keep $C$ stable while the underlying agent population evolves continuously.

> **In this crate**, the single-writer property is realized by the in-memory
> `Conductor` owning its `HashMap<AgentId, Agent>`; all spawns, drains, and
> lifecycle advances go through `&mut self`. The HTTP server (`ConductorServer`)
> wraps the conductor in `Arc<Mutex<Conductor>>`, preserving single-writer
> semantics across concurrent network clients. The cross-repo topology and
> conservation-framework integration described above are 🔮 planned, not present.

See: [SuperInstance Architecture](https://github.com/SuperInstance/SuperInstance/blob/main/ARCHITECTURE.md)

## Module Layout

| Module | What's real |
|--------|-------------|
| `src/state.rs` | `AgentState` enum + `transition()` guard (the full state machine). |
| `src/conservation.rs` | `ConservationConfig` + `check()` bounds guard (`Safe` / `Deferred`). |
| `src/conductor.rs` | `Conductor` with `observe` / `reconcile` / `spawn_agent` / `drain_agent` / `advance_lifecycle`. |
| `src/server.rs` | `ConductorServer` — HTTP/1.1 server over real TCP (`std::net::TcpListener`), thread-per-connection. |
| `src/bin/conductor-server.rs` | Standalone server binary (`cargo run --bin conductor-server`). |
| `tests/network_integration.rs` | Real integration tests over TCP sockets (background thread + subprocess). |
| `examples/quickstart.rs` | Runnable end-to-end example. |

## Related Repos

- [fleet-midi](https://github.com/SuperInstance/fleet-midi) — fleet-level event-bus and binary codec; provides the messaging transport layer that could carry conductor commands between nodes.
- [nexus-edge-runtime](https://github.com/SuperInstance/nexus-edge-runtime) — edge runtime with fleet coordination, self-healing, and trust-engine modules; overlapping domain of distributed agent orchestration.
- [vessel-bridge](https://github.com/SuperInstance/vessel-bridge) — hardware command/sensor bridge; a conductor would eventually issue drain and deploy commands through bridges of this kind.
- [superinstance-architecture](https://github.com/SuperInstance/superinstance-architecture) — overarching architecture spec defining the fleet topology, conservation framework, and conductor's role in the system.

## References

1. Burns, B. et al. (2015). "Borg, Omega, and Kubernetes." *ACM Queue* 14(1) — The reconciliation-loop pattern and desired-state convergence used by the conductor.
2. Oppenheimer, D. et al. (2016). "Designing for Failures: Lessons from Large-Scale Cluster Management." — Circuit breaking, draining, and graceful degradation in fleet systems.

## License

MIT OR Apache-2.0
