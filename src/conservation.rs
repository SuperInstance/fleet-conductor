//! Conservation-aware action guard.
//!
//! The README specifies a conservation invariant $C = \gamma + \eta$ that the
//! conductor must keep inside a stable range. A planned action (e.g.
//! terminating an agent, which lowers $\eta$) is checked *before* execution:
//! if applying its deltas would push the fleet outside the stable region the
//! action is **deferred**, not executed.
//!
//! ## What the bounds mean
//!
//! [`ConservationConfig`] carries three scalar bounds, exactly as named in the
//! README:
//!
//! - `gamma_min` / `gamma_max`: $\gamma$ must remain in $[\text{gamma\_min},
//!   \text{gamma\_max}]$.
//! - `eta_floor`: $\eta$ must remain $\ge \text{eta\_floor}$.
//!
//! Together these define the stable region. The implied floor on $C$ is
//! `c_min = gamma_min + eta_floor` (since both terms are at their minima
//! simultaneously); `eta` has a floor but no configured ceiling, so the only
//! upper bound on $C$ is the `gamma_max` gate on $\gamma$ itself. This module
//! is intentionally a literal bounds-check -- it does not model the deeper
//! $\gamma/\eta$ dynamics, which live in the external conservation framework.

use std::fmt;

/// Conservation bounds for the $\gamma + \eta = C$ invariant.
///
/// Matches the field names given in the README:
/// `ConservationConfig { gamma_min, gamma_max, eta_floor }`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConservationConfig {
    /// Minimum allowed value of $\gamma$.
    pub gamma_min: f64,
    /// Maximum allowed value of $\gamma$.
    pub gamma_max: f64,
    /// Minimum allowed value of $\eta$ (a floor; $\eta$ has no configured ceiling).
    pub eta_floor: f64,
}

impl ConservationConfig {
    /// Construct a config. `gamma_min` must be `<= gamma_max`; an inverted or
    /// NaN range yields a config for which every [`Self::check`] defers (there
    /// is no stable region), which is the safe failure mode.
    pub const fn new(gamma_min: f64, gamma_max: f64, eta_floor: f64) -> Self {
        Self {
            gamma_min,
            gamma_max,
            eta_floor,
        }
    }

    /// Implied lower bound on $C = \gamma + \eta$: both terms at their minima.
    pub fn c_min(&self) -> f64 {
        self.gamma_min + self.eta_floor
    }

    /// `true` if the configured bounds describe a non-empty stable region.
    /// A NaN bound or an inverted gamma range makes the config unsound.
    pub fn is_sound(&self) -> bool {
        let finite =
            self.gamma_min.is_finite() && self.gamma_max.is_finite() && self.eta_floor.is_finite();
        finite && self.gamma_min <= self.gamma_max
    }

    /// Check whether a planned action is safe to execute right now.
    ///
    /// Given the *current* `(gamma, eta)` and the *planned deltas*
    /// `(delta_gamma, delta_eta)` the action would apply, returns a
    /// [`ConservationVerdict`]. The action is safe iff, after applying the
    /// deltas, $\gamma$ stays in `[gamma_min, gamma_max]` **and** $\eta$ stays
    /// `>= eta_floor`. Any NaN input or unsound config always defers -- when
    /// safety cannot be proven, the action is not taken.
    pub fn check(
        &self,
        gamma: f64,
        eta: f64,
        delta_gamma: f64,
        delta_eta: f64,
    ) -> ConservationVerdict {
        // Unsound config or NaN inputs: cannot prove safety, so defer.
        if !self.is_sound()
            || gamma.is_nan()
            || eta.is_nan()
            || delta_gamma.is_nan()
            || delta_eta.is_nan()
        {
            return ConservationVerdict::defer_unsound(self, gamma, eta);
        }

        let new_gamma = gamma + delta_gamma;
        let new_eta = eta + delta_eta;
        let new_c = new_gamma + new_eta;

        // Check each configured bound. Eta has a floor only; gamma has both.
        if new_gamma < self.gamma_min {
            return ConservationVerdict::Deferred {
                reason: DeferredReason::GammaBelowMin,
                would_be_gamma: new_gamma,
                would_be_eta: new_eta,
                would_be_c: new_c,
                limit: self.gamma_min,
            };
        }
        if new_gamma > self.gamma_max {
            return ConservationVerdict::Deferred {
                reason: DeferredReason::GammaAboveMax,
                would_be_gamma: new_gamma,
                would_be_eta: new_eta,
                would_be_c: new_c,
                limit: self.gamma_max,
            };
        }
        if new_eta < self.eta_floor {
            return ConservationVerdict::Deferred {
                reason: DeferredReason::EtaBelowFloor,
                would_be_gamma: new_gamma,
                would_be_eta: new_eta,
                would_be_c: new_c,
                limit: self.eta_floor,
            };
        }

        ConservationVerdict::Safe {
            gamma: new_gamma,
            eta: new_eta,
            c: new_c,
        }
    }

    /// Convenience: `true` iff [`Self::check`] returns [`ConservationVerdict::Safe`].
    pub fn is_safe(&self, gamma: f64, eta: f64, delta_gamma: f64, delta_eta: f64) -> bool {
        matches!(
            self.check(gamma, eta, delta_gamma, delta_eta),
            ConservationVerdict::Safe { .. }
        )
    }
}

impl Default for ConservationConfig {
    /// A permissive-but-real default: $\gamma \in [-1, 1]$, $\eta \ge 0$.
    fn default() -> Self {
        Self::new(-1.0, 1.0, 0.0)
    }
}

/// Result of a conservation check.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConservationVerdict {
    /// The action may execute; these are the resulting $(\gamma, \eta, C)$.
    Safe { gamma: f64, eta: f64, c: f64 },
    /// The action must be deferred; applying it would breach a bound.
    Deferred {
        reason: DeferredReason,
        would_be_gamma: f64,
        would_be_eta: f64,
        would_be_c: f64,
        /// The bound that would be breached (gamma_min, gamma_max, or eta_floor).
        limit: f64,
    },
}

impl ConservationVerdict {
    /// `true` if the verdict says the action is safe to execute.
    pub fn is_safe(&self) -> bool {
        matches!(self, ConservationVerdict::Safe { .. })
    }

    /// `true` if the action should be deferred.
    pub fn is_deferred(&self) -> bool {
        matches!(self, ConservationVerdict::Deferred { .. })
    }

    fn defer_unsound(cfg: &ConservationConfig, gamma: f64, eta: f64) -> Self {
        ConservationVerdict::Deferred {
            reason: DeferredReason::UnsoundConfigOrInput,
            would_be_gamma: gamma,
            would_be_eta: eta,
            would_be_c: gamma + eta,
            limit: cfg.c_min(),
        }
    }
}

/// Why a planned action was deferred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeferredReason {
    /// $\gamma$ would drop below `gamma_min`.
    GammaBelowMin,
    /// $\gamma$ would exceed `gamma_max`.
    GammaAboveMax,
    /// $\eta$ would drop below `eta_floor` (e.g. terminating a high-$\eta$ agent).
    EtaBelowFloor,
    /// The config is unsound or an input was NaN; safety could not be proven.
    UnsoundConfigOrInput,
}

impl fmt::Display for DeferredReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeferredReason::GammaBelowMin => write!(f, "gamma below gamma_min"),
            DeferredReason::GammaAboveMax => write!(f, "gamma above gamma_max"),
            DeferredReason::EtaBelowFloor => write!(f, "eta below eta_floor"),
            DeferredReason::UnsoundConfigOrInput => {
                write!(f, "unsound config or NaN input")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The example config straight from the README Quick Start.
    fn readme_config() -> ConservationConfig {
        ConservationConfig::new(-0.5, 0.5, 0.3)
    }

    #[test]
    fn c_min_is_gamma_min_plus_eta_floor() {
        let cfg = readme_config();
        // README: gamma_min=-0.5, eta_floor=0.3 -> C_min = -0.2
        assert!((cfg.c_min() - (-0.2)).abs() < 1e-12);
    }

    #[test]
    fn readme_config_is_sound() {
        assert!(readme_config().is_sound());
    }

    #[test]
    fn action_inside_safe_range_is_allowed() {
        // Current state well inside the region; a small drain that keeps both
        // gamma and eta within bounds is safe.
        let cfg = readme_config();
        // gamma=0.0 in [-0.5,0.5], eta=0.5 >= 0.3.
        let v = cfg.check(0.0, 0.5, -0.1, -0.1);
        // new gamma=-0.1 (in range), new eta=0.4 (>= 0.3) -> safe.
        assert!(v.is_safe());
        match v {
            ConservationVerdict::Safe { gamma, eta, c } => {
                assert!((gamma - (-0.1)).abs() < 1e-12);
                assert!((eta - 0.4).abs() < 1e-12);
                assert!((c - 0.3).abs() < 1e-12);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn zero_delta_on_safe_state_is_safe() {
        // No-op action on an already-safe state must be safe.
        let cfg = readme_config();
        assert!(cfg.is_safe(0.0, 0.4, 0.0, 0.0));
    }

    #[test]
    fn drain_that_breaches_eta_floor_is_deferred() {
        // The README's canonical example: terminating an agent drops eta below
        // the floor -> defer.
        let cfg = readme_config();
        // eta=0.35, drain contributes delta_eta=-0.2 -> new eta=0.15 < 0.3.
        let v = cfg.check(0.0, 0.35, 0.0, -0.2);
        assert!(v.is_deferred());
        match v {
            ConservationVerdict::Deferred {
                reason,
                would_be_eta,
                limit,
                ..
            } => {
                assert_eq!(reason, DeferredReason::EtaBelowFloor);
                assert!((would_be_eta - 0.15).abs() < 1e-12);
                assert!((limit - 0.3).abs() < 1e-12);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn drain_exactly_to_eta_floor_is_safe() {
        // Boundary is inclusive: eta exactly at the floor is allowed.
        let cfg = readme_config();
        // eta=0.5, delta_eta=-0.2 -> new eta=0.3 == eta_floor.
        assert!(cfg.is_safe(0.0, 0.5, 0.0, -0.2));
    }

    #[test]
    fn gamma_below_min_is_deferred() {
        let cfg = readme_config();
        // gamma=-0.4, delta=-0.2 -> new gamma=-0.6 < -0.5.
        let v = cfg.check(-0.4, 0.5, -0.2, 0.0);
        assert!(v.is_deferred());
        match v {
            ConservationVerdict::Deferred {
                reason,
                would_be_gamma,
                would_be_eta,
                would_be_c,
                limit,
            } => {
                assert_eq!(reason, DeferredReason::GammaBelowMin);
                assert!((would_be_gamma - (-0.6)).abs() < 1e-12);
                assert!((would_be_eta - 0.5).abs() < 1e-12);
                assert!((would_be_c - (-0.1)).abs() < 1e-12);
                assert!((limit - (-0.5)).abs() < 1e-12);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn gamma_above_max_is_deferred() {
        let cfg = readme_config();
        // gamma=0.4, delta=+0.2 -> new gamma=0.6 > 0.5.
        let v = cfg.check(0.4, 0.5, 0.2, 0.0);
        assert!(v.is_deferred());
        match v {
            ConservationVerdict::Deferred { reason, .. } => {
                assert_eq!(reason, DeferredReason::GammaAboveMax);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn gamma_boundaries_are_inclusive() {
        let cfg = readme_config();
        // gamma exactly at min and at max are both safe (eta well above floor).
        assert!(cfg.is_safe(-0.5, 0.5, 0.0, 0.0));
        assert!(cfg.is_safe(0.5, 0.5, 0.0, 0.0));
    }

    #[test]
    fn nan_input_is_deferred() {
        let cfg = readme_config();
        // A NaN reading from a broken sensor must never be treated as safe.
        assert!(cfg.check(f64::NAN, 0.5, 0.0, 0.0).is_deferred());
        assert!(cfg.check(0.0, 0.5, f64::NAN, 0.0).is_deferred());
    }

    #[test]
    fn inverted_gamma_range_is_unsound_and_defers_everything() {
        let cfg = ConservationConfig::new(0.5, -0.5, 0.3); // min > max
        assert!(!cfg.is_sound());
        // Even a no-op on a "central" state defers, because there is no stable region.
        assert!(cfg.check(0.0, 0.5, 0.0, 0.0).is_deferred());
    }

    #[test]
    fn replacing_agent_before_drain_can_make_it_safe() {
        // README option (a): spawn a replacement first to lift eta, *then* drain.
        let cfg = readme_config();
        // eta=0.35, draining alone (-0.2) breaches the floor.
        assert!(cfg.check(0.0, 0.35, 0.0, -0.2).is_deferred());
        // But first spawn (+0.2 eta) -> eta=0.55, then drain (-0.2) -> 0.35 >= 0.3.
        let after_spawn = cfg.check(0.0, 0.35, 0.0, 0.2);
        assert!(after_spawn.is_safe());
        match after_spawn {
            ConservationVerdict::Safe { eta, .. } => assert!((eta - 0.55).abs() < 1e-12),
            _ => unreachable!(),
        }
        // Now the drain is safe from the post-spawn state.
        assert!(cfg.is_safe(0.0, 0.55, 0.0, -0.2));
    }

    #[test]
    fn deferred_reason_display_is_human_readable() {
        assert!(DeferredReason::EtaBelowFloor.to_string().contains("eta"));
        assert!(DeferredReason::GammaBelowMin.to_string().contains("gamma"));
    }

    #[test]
    fn default_config_is_sound_and_permissive() {
        let cfg = ConservationConfig::default();
        assert!(cfg.is_sound());
        // gamma=0 in [-1,1], eta=0 >= 0 -> safe.
        assert!(cfg.is_safe(0.0, 0.0, 0.0, 0.0));
    }
}
