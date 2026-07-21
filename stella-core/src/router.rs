//! Role → model resolver + per-provider circuit breaker
//! (roles, provider adapters, and default role
//! assignments by scenario — the authoritative precedence this module
//! implements).
//!
//! `stella-core` cannot see the concrete model catalog — that's
//! `stella-model`, a downstream crate that depends on `stella-protocol`
//! (dependency direction: adapters depend on the engine's ports, never the
//! reverse). The router therefore resolves roles
//! over a caller-supplied abstraction (`ProviderProfile`) instead of any
//! concrete catalog type, and has no I/O of its own: `resolve` is a plain
//! synchronous function over owned data. It returns *data* describing what
//! happened (a `ModelRef` plus an optional `FallbackInfo`); the pipeline layer
//! (`stella-pipeline`) is what turns a `FallbackInfo` into an
//! `AgentEvent::ProviderFallback` and pushes it onto the event channel —
//! there is no event channel here.
//!
//! Binding lessons this module exists to satisfy: L-M3 (no `"auto"` string
//! sentinel anywhere — absence of a pin is `Option::None`), L-M6 (role-based
//! routing with per-role pins is the core abstraction), L-M7 (every adapter
//! carries a breaker; fallback is always visible, never a silent mid-turn
//! family switch), L-M8 (a single configured provider must still yield a
//! fully working set of role resolutions).

use std::collections::HashMap;

use stella_protocol::{ModelRef, Role};

use crate::ports::Clock;

// ---------------------------------------------------------------------
// Provider profiles
// ---------------------------------------------------------------------

/// A caller-supplied description of one configured (BYOK key present)
/// provider. `stella-core` never sees the real
/// model catalog (`stella-model`, downstream); this is the entire interface
/// it needs to route roles to models.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderProfile {
    /// Stable provider id, e.g. `"zai"`, `"anthropic"` — matches
    /// `stella_protocol::Provider::id()` and is the key `CircuitBreaker`
    /// tracks state under.
    pub id: String,
    /// Cross-family grouping key for judge selection (§1: "prefer a
    /// different family when ≥2 profiles with distinct family... are
    /// available"). Two profiles sharing a `family` are treated as the
    /// SAME family even if their `id`s differ — e.g. two OpenRouter-routed
    /// slugs of the same underlying model. Defaults to `id` via
    /// `ProviderProfile::new` when the caller has no finer concept.
    pub family: String,
    /// Strongest available model for main coding/execution steps — the
    /// `worker`/`plan` default.
    pub worker_model: ModelRef,
    /// Cheap/fast tier used for triage classification. May equal
    /// `worker_model` when the provider has no separate fast tier — that's
    /// the caller's choice when constructing the profile, not the router's
    /// concern.
    pub triage_model: ModelRef,
    /// Model used when this provider is chosen as judge: "never the same
    /// instance as worker" — a fresh, separate call even when it
    /// shares a slug with `worker_model`.
    pub judge_model: ModelRef,
}

impl ProviderProfile {
    /// Construct a profile with `family` defaulting to `id` (the common
    /// case: one key = one family). Call `.with_family(..)` to override for
    /// providers that front more than one underlying model family, or that
    /// should be grouped with another profile for judge cross-family
    /// selection.
    pub fn new(
        id: impl Into<String>,
        worker_model: ModelRef,
        triage_model: ModelRef,
        judge_model: ModelRef,
    ) -> Self {
        let id = id.into();
        let family = id.clone();
        Self {
            id,
            family,
            worker_model,
            triage_model,
            judge_model,
        }
    }

    /// Override the cross-family grouping key. See the `family` field doc.
    pub fn with_family(mut self, family: impl Into<String>) -> Self {
        self.family = family.into();
        self
    }
}

// ---------------------------------------------------------------------
// Role table (explicit pins)
// ---------------------------------------------------------------------

/// Explicit per-role pins (L-M6: per-function model overrides are the core
/// abstraction, not an afterthought). The CLI populates pins from the
/// settings' `agent_engine_config` (per-agent provider/model sections,
/// edited on the deck's SETTINGS tab) on the production wiring path.
/// Absence of a pin for a role is `None`, never a magic sentinel
/// (L-M3) — the router falls through to scenario defaults when a role
/// isn't in this table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoleTable {
    pins: HashMap<Role, ModelRef>,
}

impl RoleTable {
    /// An empty table — every role is auto (no pins).
    pub fn new() -> Self {
        Self::default()
    }

    /// Set (or replace) an explicit pin for `role`. Returns `&mut Self` for
    /// chaining multiple pins during construction.
    pub fn pin(&mut self, role: Role, model: ModelRef) -> &mut Self {
        self.pins.insert(role, model);
        self
    }

    /// Clear an explicit pin, reverting `role` to auto.
    pub fn unpin(&mut self, role: Role) -> &mut Self {
        self.pins.remove(&role);
        self
    }

    /// The explicit pin for `role`, if any.
    pub fn get(&self, role: Role) -> Option<&ModelRef> {
        self.pins.get(&role)
    }

    /// Whether `role` currently carries an explicit pin.
    pub fn is_pinned(&self, role: Role) -> bool {
        self.pins.contains_key(&role)
    }
}

// ---------------------------------------------------------------------
// Circuit breaker
// ---------------------------------------------------------------------

/// A provider's current disposition, always *derived* from the last
/// recorded outcome plus the clock — never stored directly, so time only
/// moves when the caller's injected `Clock` says it does (this crate never
/// sleeps for real; `ports::Clock` exists precisely so breaker cooldowns
/// are testable without it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerStatus {
    /// Healthy — routes normally.
    Closed,
    /// Broken — the router must skip this provider entirely.
    Open,
    /// Cooldown elapsed since the breaker opened — the next call through is
    /// the trial: success closes the breaker, failure reopens it
    /// immediately (without needing to re-accumulate the full failure
    /// threshold).
    HalfOpen,
}

#[derive(Debug, Clone, Copy, Default)]
struct BreakerEntry {
    consecutive_failures: u32,
    /// `Some(t)` once the breaker has tripped open at clock time `t`;
    /// `None` means closed (never tripped, or reset by a success).
    opened_at_ms: Option<u64>,
}

/// Per-provider circuit breaker ("a per-provider
/// circuit breaker", §7 reliability rules; L-M7: "every adapter carries a
/// breaker … no silent mid-turn family switches"). One `CircuitBreaker`
/// instance tracks every provider's state, keyed by provider id.
///
/// The caller feeds outcomes in via `record_success`/`record_failure`;
/// `Router` consults `is_available`/`status` before resolving a role to a
/// provider and routes around anything open. No I/O: cooldown timing runs
/// entirely off the injected `Clock`.
pub struct CircuitBreaker {
    clock: Box<dyn Clock>,
    failure_threshold: u32,
    cooldown_ms: u64,
    entries: HashMap<String, BreakerEntry>,
}

impl CircuitBreaker {
    /// Consecutive transport-class failures before the breaker opens, used
    /// by `CircuitBreaker::new`.
    pub const DEFAULT_FAILURE_THRESHOLD: u32 = 3;
    /// Cooldown before an open breaker allows a half-open trial call, used
    /// by `CircuitBreaker::new`.
    pub const DEFAULT_COOLDOWN_MS: u64 = 30_000;

    /// A breaker with the default threshold (3 consecutive failures) and
    /// cooldown (30s), timed by `clock`.
    pub fn new(clock: Box<dyn Clock>) -> Self {
        Self::with_thresholds(
            clock,
            Self::DEFAULT_FAILURE_THRESHOLD,
            Self::DEFAULT_COOLDOWN_MS,
        )
    }

    /// A breaker with caller-chosen thresholds. `failure_threshold` is
    /// clamped to at least 1 (a breaker that opens on 0 failures is never
    /// closed).
    pub fn with_thresholds(
        clock: Box<dyn Clock>,
        failure_threshold: u32,
        cooldown_ms: u64,
    ) -> Self {
        Self {
            clock,
            failure_threshold: failure_threshold.max(1),
            cooldown_ms,
            entries: HashMap::new(),
        }
    }

    /// The failure threshold this breaker was constructed with — used by
    /// `Router` to build human-readable fallback reasons.
    pub fn failure_threshold(&self) -> u32 {
        self.failure_threshold
    }

    /// `provider_id`'s current disposition, derived from the last recorded
    /// outcome and the clock.
    pub fn status(&self, provider_id: &str) -> BreakerStatus {
        match self.entries.get(provider_id).and_then(|e| e.opened_at_ms) {
            None => BreakerStatus::Closed,
            Some(opened_at) => {
                let elapsed = self.clock.now_ms().saturating_sub(opened_at);
                if elapsed >= self.cooldown_ms {
                    BreakerStatus::HalfOpen
                } else {
                    BreakerStatus::Open
                }
            }
        }
    }

    /// Whether the router may currently route to `provider_id` — true for
    /// `Closed` and `HalfOpen` (a half-open breaker allows exactly the next
    /// call through as the trial), false for `Open`.
    pub fn is_available(&self, provider_id: &str) -> bool {
        !matches!(self.status(provider_id), BreakerStatus::Open)
    }

    /// Record a successful call. Fully closes the breaker (resets the
    /// consecutive-failure counter and clears the open timestamp)
    /// regardless of prior state — this is how a `HalfOpen` trial call
    /// succeeding closes the breaker again.
    pub fn record_success(&mut self, provider_id: &str) {
        self.entries
            .insert(provider_id.to_string(), BreakerEntry::default());
    }

    /// Record a transport-class failure. Opens the breaker when either (a)
    /// this failure was the half-open trial call (one failure reopens
    /// immediately — classic half-open semantics, no need to re-accumulate
    /// the full threshold), or (b) consecutive failures have now reached
    /// `failure_threshold`.
    pub fn record_failure(&mut self, provider_id: &str) {
        let now = self.clock.now_ms();
        let was_half_open = self.status(provider_id) == BreakerStatus::HalfOpen;
        let threshold = self.failure_threshold;

        let entry = self.entries.entry(provider_id.to_string()).or_default();
        entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
        if was_half_open || entry.consecutive_failures >= threshold {
            entry.opened_at_ms = Some(now);
        }
    }
}

// ---------------------------------------------------------------------
// Resolution result
// ---------------------------------------------------------------------

/// A breaker-forced provider substitution — maps directly onto
/// `AgentEvent::ProviderFallback`'s fields. `stella-core` has no event
/// channel; `driver.rs` is the one place that turns this into the real
/// event. Never constructed for an intentional routing choice (e.g. judge's
/// cross-family preference) — only when the originally preferred provider
/// was unavailable (L-M7: fallback is always visible, never silent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackInfo {
    pub from: String,
    pub to: String,
    pub reason: String,
}

/// The result of resolving one `Role`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterDecision {
    /// The model to call.
    pub model_ref: ModelRef,
    /// Set when a circuit breaker forced a provider substitution away from
    /// the most-preferred configured provider.
    pub fallback: Option<FallbackInfo>,
    /// A human-readable caveat about resolution quality that isn't a
    /// `fallback` (nothing was skipped due to a breaker) but is still worth
    /// surfacing — e.g. judge degrading to a same-family instance because
    /// no second family is currently healthy (§5, L-M8: single-key BYOK
    /// must still fully work, just with this documented caveat).
    pub caveat: Option<String>,
}

impl RouterDecision {
    fn plain(model_ref: ModelRef) -> Self {
        Self {
            model_ref,
            fallback: None,
            caveat: None,
        }
    }
}

/// Router resolution failures — always a loud, named error, never a silent
/// guess (mirrors the catalog's "unknown slug is a hard error" rule,
///, at the routing layer).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RouterError {
    /// No `ProviderProfile`s were supplied at all — the router-level
    /// equivalent of `stella-cli::config::Config::load`'s "no API key
    /// found" error.
    #[error(
        "no provider configured for role `{role:?}` — configure at least one provider (BYOK key) before resolving models"
    )]
    NoProvidersConfigured { role: Role },

    /// Every configured provider's breaker is currently open for this
    /// role's resolution — routing around them all is impossible.
    #[error(
        "all configured providers are circuit-broken for role `{role:?}` — every breaker is open; wait for a cooldown or configure a healthy provider"
    )]
    AllProvidersUnavailable { role: Role },

    /// `Embed`/`Vision`/`Image`/`Video` have no default resolution in this
    /// router — those roles are served by their own crates (`stella-context`
    /// embeds locally, `stella-media` routes by media kind), not by the
    /// chat-model scenario defaults. Never invent a fake default; pin
    /// explicitly via `RoleTable::pin` if you need one.
    #[error(
        "no default model available for role `{role:?}` — pin one explicitly with RoleTable::pin"
    )]
    NoDefaultForRole { role: Role },
}

// ---------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------

/// Resolves a `Role` to a `ModelRef`, applying (in order, per
/// "Resolution: explicit pin > per-role config >
/// scenario defaults"):
///
/// 1. An explicit `RoleTable` pin — wins unconditionally, no breaker check.
/// 2. Scenario defaults over the configured `ProviderProfile`s, in
///    preference order, skipping any whose circuit breaker is open.
///
/// Construction is the only mutation-free step; `resolve` takes `&self` and
/// is a pure query. Only `record_success`/`record_failure` mutate breaker
/// state.
pub struct Router {
    role_table: RoleTable,
    /// Configured providers in preference order — first = most preferred,
    /// mirroring `stella-cli::config::PROVIDERS`'s existing preference
    /// ordering ( Phase 2 item 2).
    profiles: Vec<ProviderProfile>,
    breaker: CircuitBreaker,
}

impl Router {
    pub fn new(
        role_table: RoleTable,
        profiles: Vec<ProviderProfile>,
        breaker: CircuitBreaker,
    ) -> Self {
        Self {
            role_table,
            profiles,
            breaker,
        }
    }

    /// Read access to the explicit pins, e.g. for `stella config model get`.
    pub fn role_table(&self) -> &RoleTable {
        &self.role_table
    }

    /// The configured providers, in preference order.
    pub fn providers(&self) -> &[ProviderProfile] {
        &self.profiles
    }

    /// Feed a successful call outcome into the breaker for `provider_id`.
    pub fn record_success(&mut self, provider_id: &str) {
        self.breaker.record_success(provider_id);
    }

    /// Feed a failed (transport-class) call outcome into the breaker for
    /// `provider_id`; may trip it open (§2, §7; L-M7).
    pub fn record_failure(&mut self, provider_id: &str) {
        self.breaker.record_failure(provider_id);
    }

    /// Resolve `role` to a model per the precedence documented on `Router`.
    pub fn resolve(&self, role: Role) -> Result<RouterDecision, RouterError> {
        if let Some(pinned) = self.role_table.get(role) {
            return Ok(RouterDecision::plain(pinned.clone()));
        }

        match role {
            Role::Worker | Role::Plan => self.resolve_tier(role, |p| &p.worker_model),
            Role::Triage => self.resolve_tier(role, |p| &p.triage_model),
            Role::Judge => self.resolve_judge(),
            Role::Embed | Role::Vision | Role::Image | Role::Video => {
                Err(RouterError::NoDefaultForRole { role })
            }
        }
    }

    /// Shared logic for `Worker`/`Plan`/`Triage`: pick the most-preferred
    /// available provider's tier model, falling back to the next available
    /// one (and reporting it) when the preferred provider is breaker-open.
    fn resolve_tier(
        &self,
        role: Role,
        pick: impl Fn(&ProviderProfile) -> &ModelRef,
    ) -> Result<RouterDecision, RouterError> {
        if self.profiles.is_empty() {
            return Err(RouterError::NoProvidersConfigured { role });
        }

        let preferred = &self.profiles[0];
        if self.breaker.is_available(&preferred.id) {
            return Ok(RouterDecision::plain(pick(preferred).clone()));
        }

        for candidate in &self.profiles[1..] {
            if self.breaker.is_available(&candidate.id) {
                return Ok(RouterDecision {
                    model_ref: pick(candidate).clone(),
                    fallback: Some(self.fallback_from(preferred, candidate)),
                    caveat: None,
                });
            }
        }

        Err(RouterError::AllProvidersUnavailable { role })
    }

    /// Judge resolution (§1, §5, L-E11 "always on a different model than
    /// the worker"): never the same model as Worker's *actual* current
    /// resolution; prefer a healthy provider from a different `family`;
    /// degrade — with a caveat, never an error — to the same
    /// provider/family when it's the only healthy option (L-M8: a single
    /// configured provider must still yield a working judge).
    fn resolve_judge(&self) -> Result<RouterDecision, RouterError> {
        if self.profiles.is_empty() {
            return Err(RouterError::NoProvidersConfigured { role: Role::Judge });
        }

        // Availability is checked FIRST so an all-breakers-open state reports a
        // `Judge` failure — resolving Worker up front would propagate its `?`
        // as `AllProvidersUnavailable { role: Worker }`, mislabeling a Judge
        // resolution with the wrong role.
        let available: Vec<&ProviderProfile> = self
            .profiles
            .iter()
            .filter(|p| self.breaker.is_available(&p.id))
            .collect();
        let Some(&first_healthy) = available.first() else {
            return Err(RouterError::AllProvidersUnavailable { role: Role::Judge });
        };

        // What Worker actually resolves to right now (pin included) — the
        // instance Judge must never repeat.
        let worker_decision = self.resolve(Role::Worker)?;
        let worker_family = self
            .profiles
            .iter()
            .find(|p| p.worker_model == worker_decision.model_ref)
            .map(|p| p.family.as_str());

        // Prefer a healthy provider whose family differs from worker's. If
        // worker's family is unknown (e.g. an explicit pin to a model no
        // profile advertises), any healthy provider counts as "different".
        let cross_family = available
            .iter()
            .find(|p| Some(p.family.as_str()) != worker_family)
            .copied();

        let (chosen, degraded_same_family) = match cross_family {
            Some(candidate) => (candidate, false),
            None => (first_healthy, true),
        };

        // A fallback (vs. an intentional cross-family choice) is only
        // reported when the most-preferred provider overall was skipped
        // because its breaker is open (L-M7: fallback events are for
        // breaker-forced substitutions, never for ordinary judge routing).
        let preferred = &self.profiles[0];
        let fallback = if !self.breaker.is_available(&preferred.id) && chosen.id != preferred.id {
            Some(self.fallback_from(preferred, chosen))
        } else {
            None
        };

        let caveat = degraded_same_family.then(|| {
            format!(
                "judge running on the same provider/family as worker (`{}`) — no distinct-family \
                 provider is currently healthy; cross-family bias-resistant judging is degraded \
                 until a second family is configured or its breaker recovers (L-M8)",
                chosen.id
            )
        });

        Ok(RouterDecision {
            model_ref: chosen.judge_model.clone(),
            fallback,
            caveat,
        })
    }

    fn fallback_from(&self, from: &ProviderProfile, to: &ProviderProfile) -> FallbackInfo {
        FallbackInfo {
            from: from.id.clone(),
            to: to.id.clone(),
            reason: format!(
                "circuit breaker open for `{}` after {} consecutive transport failures",
                from.id,
                self.breaker.failure_threshold()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A `Clock` test double whose time only advances when the test tells
    /// it to — cooldown behavior is proven without a real sleep.
    #[derive(Clone)]
    struct ManualClock(Arc<AtomicU64>);

    impl ManualClock {
        fn new(start_ms: u64) -> Self {
            Self(Arc::new(AtomicU64::new(start_ms)))
        }

        fn advance(&self, delta_ms: u64) {
            self.0.fetch_add(delta_ms, Ordering::SeqCst);
        }
    }

    impl Clock for ManualClock {
        fn now_ms(&self) -> u64 {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn model(provider: &str, id: &str) -> ModelRef {
        ModelRef::new(provider, id)
    }

    fn zai_profile() -> ProviderProfile {
        ProviderProfile::new(
            "zai",
            model("zai", "glm-5.2"),
            model("zai", "glm-5.2-air"),
            model("zai", "glm-5.2-judge"),
        )
    }

    fn anthropic_profile() -> ProviderProfile {
        ProviderProfile::new(
            "anthropic",
            model("anthropic", "claude-fable-5"),
            model("anthropic", "claude-haiku"),
            model("anthropic", "claude-fable-5"),
        )
    }

    fn breaker_with_clock(clock: ManualClock) -> CircuitBreaker {
        CircuitBreaker::new(Box::new(clock))
    }

    // -- ProviderProfile ---------------------------------------------

    #[test]
    fn provider_profile_family_defaults_to_id_and_is_overridable() {
        let p = zai_profile();
        assert_eq!(p.family, "zai");
        let renamed = p.with_family("glm");
        assert_eq!(renamed.family, "glm");
        assert_eq!(renamed.id, "zai");
    }

    // -- RoleTable ------------------------------------------------------

    #[test]
    fn role_table_starts_empty_and_pins_unpin_round_trip() {
        let mut table = RoleTable::new();
        assert!(!table.is_pinned(Role::Worker));
        table.pin(Role::Worker, model("zai", "glm-5.2"));
        assert!(table.is_pinned(Role::Worker));
        assert_eq!(table.get(Role::Worker), Some(&model("zai", "glm-5.2")));
        table.unpin(Role::Worker);
        assert!(!table.is_pinned(Role::Worker));
        assert_eq!(table.get(Role::Worker), None);
    }

    // -- Explicit pin precedence -----------------------------------------

    #[test]
    fn explicit_pin_wins_unconditionally_even_with_providers_and_open_breakers() {
        let mut table = RoleTable::new();
        let pinned = model("openrouter", "some/pinned-slug");
        table.pin(Role::Judge, pinned.clone());

        let clock = ManualClock::new(0);
        let mut breaker = breaker_with_clock(clock);
        // Break every configured provider — the pin must still win.
        for _ in 0..CircuitBreaker::DEFAULT_FAILURE_THRESHOLD {
            breaker.record_failure("zai");
            breaker.record_failure("anthropic");
        }

        let router = Router::new(table, vec![zai_profile(), anthropic_profile()], breaker);
        let decision = router.resolve(Role::Judge).expect("pin always resolves");
        assert_eq!(decision.model_ref, pinned);
        assert_eq!(decision.fallback, None);
    }

    #[test]
    fn pin_resolves_even_for_roles_with_no_default_yet() {
        let mut table = RoleTable::new();
        let pinned = model("local", "onnx-embed");
        table.pin(Role::Embed, pinned.clone());
        let router = Router::new(table, vec![], breaker_with_clock(ManualClock::new(0)));
        let decision = router.resolve(Role::Embed).expect("pin honored");
        assert_eq!(decision.model_ref, pinned);
    }

    // -- Single-provider (BYOK-with-one-key, L-M8) ------------------------

    #[test]
    fn single_provider_yields_a_fully_working_worker_triage_judge_set() {
        let router = Router::new(
            RoleTable::new(),
            vec![zai_profile()],
            breaker_with_clock(ManualClock::new(0)),
        );

        let worker = router.resolve(Role::Worker).expect("worker resolves");
        assert_eq!(worker.model_ref, model("zai", "glm-5.2"));
        assert_eq!(worker.fallback, None);

        let triage = router.resolve(Role::Triage).expect("triage resolves");
        assert_eq!(triage.model_ref, model("zai", "glm-5.2-air"));

        let judge = router
            .resolve(Role::Judge)
            .expect("judge must still resolve, not error, with only one provider");
        assert_eq!(judge.model_ref, model("zai", "glm-5.2-judge"));
        assert_eq!(judge.fallback, None);
        assert!(
            judge.caveat.is_some(),
            "single-provider judge must flag the same-family degradation"
        );
    }

    #[test]
    fn plan_defaults_to_the_same_tier_as_worker() {
        let router = Router::new(
            RoleTable::new(),
            vec![zai_profile()],
            breaker_with_clock(ManualClock::new(0)),
        );
        let worker = router.resolve(Role::Worker).unwrap();
        let plan = router.resolve(Role::Plan).unwrap();
        assert_eq!(worker.model_ref, plan.model_ref);
    }

    // -- Multi-provider cross-family judge --------------------------------

    #[test]
    fn multi_provider_judge_picks_a_different_family_than_worker() {
        let router = Router::new(
            RoleTable::new(),
            vec![zai_profile(), anthropic_profile()],
            breaker_with_clock(ManualClock::new(0)),
        );

        let worker = router.resolve(Role::Worker).unwrap();
        assert_eq!(worker.model_ref, model("zai", "glm-5.2"));

        let judge = router.resolve(Role::Judge).unwrap();
        assert_eq!(judge.model_ref, model("anthropic", "claude-fable-5"));
        assert_eq!(judge.fallback, None);
        assert_eq!(
            judge.caveat, None,
            "cross-family judge selection needs no degradation caveat"
        );
    }

    // -- Zero providers configured -----------------------------------------

    #[test]
    fn zero_providers_configured_is_a_named_error_not_a_panic() {
        let router = Router::new(
            RoleTable::new(),
            vec![],
            breaker_with_clock(ManualClock::new(0)),
        );

        assert_eq!(
            router.resolve(Role::Worker).unwrap_err(),
            RouterError::NoProvidersConfigured { role: Role::Worker }
        );
        assert_eq!(
            router.resolve(Role::Triage).unwrap_err(),
            RouterError::NoProvidersConfigured { role: Role::Triage }
        );
        assert_eq!(
            router.resolve(Role::Judge).unwrap_err(),
            RouterError::NoProvidersConfigured { role: Role::Judge }
        );
    }

    // -- Embed/Vision/Image/Video: no default yet ------------------------

    #[test]
    fn unpinned_media_and_embed_roles_return_named_not_available_outcome() {
        let router = Router::new(
            RoleTable::new(),
            vec![zai_profile(), anthropic_profile()],
            breaker_with_clock(ManualClock::new(0)),
        );
        for role in [Role::Embed, Role::Vision, Role::Image, Role::Video] {
            assert_eq!(
                router.resolve(role).unwrap_err(),
                RouterError::NoDefaultForRole { role },
                "role {role:?} must not silently guess a default"
            );
        }
    }

    // -- Circuit breaker: opens, falls back, closes, half-opens ----------

    #[test]
    fn breaker_is_closed_and_available_before_any_failures() {
        let breaker = breaker_with_clock(ManualClock::new(0));
        assert_eq!(breaker.status("zai"), BreakerStatus::Closed);
        assert!(breaker.is_available("zai"));
    }

    #[test]
    fn breaker_opens_after_default_threshold_consecutive_failures() {
        let mut breaker = breaker_with_clock(ManualClock::new(0));
        for _ in 0..CircuitBreaker::DEFAULT_FAILURE_THRESHOLD - 1 {
            breaker.record_failure("zai");
            assert_eq!(
                breaker.status("zai"),
                BreakerStatus::Closed,
                "must stay closed before the threshold is reached"
            );
        }
        breaker.record_failure("zai");
        assert_eq!(breaker.status("zai"), BreakerStatus::Open);
        assert!(!breaker.is_available("zai"));
    }

    #[test]
    fn router_falls_back_to_the_next_provider_when_preferred_is_broken() {
        let clock = ManualClock::new(0);
        let mut breaker = breaker_with_clock(clock);
        for _ in 0..CircuitBreaker::DEFAULT_FAILURE_THRESHOLD {
            breaker.record_failure("zai");
        }

        let router = Router::new(
            RoleTable::new(),
            vec![zai_profile(), anthropic_profile()],
            breaker,
        );

        let decision = router
            .resolve(Role::Worker)
            .expect("falls back to anthropic");
        assert_eq!(decision.model_ref, model("anthropic", "claude-fable-5"));
        let fallback = decision
            .fallback
            .expect("fallback must be reported, not silent");
        assert_eq!(fallback.from, "zai");
        assert_eq!(fallback.to, "anthropic");
        assert!(
            fallback.reason.contains("circuit breaker"),
            "{}",
            fallback.reason
        );
    }

    #[test]
    fn breaker_closes_again_after_a_recorded_success() {
        let clock = ManualClock::new(0);
        let mut breaker = breaker_with_clock(clock);
        for _ in 0..CircuitBreaker::DEFAULT_FAILURE_THRESHOLD {
            breaker.record_failure("zai");
        }
        assert_eq!(breaker.status("zai"), BreakerStatus::Open);

        breaker.record_success("zai");
        assert_eq!(breaker.status("zai"), BreakerStatus::Closed);
        assert!(breaker.is_available("zai"));
    }

    #[test]
    fn router_routes_back_to_the_preferred_provider_once_its_breaker_closes() {
        let clock = ManualClock::new(0);
        let mut breaker = breaker_with_clock(clock);
        for _ in 0..CircuitBreaker::DEFAULT_FAILURE_THRESHOLD {
            breaker.record_failure("zai");
        }
        breaker.record_success("zai");

        let router = Router::new(
            RoleTable::new(),
            vec![zai_profile(), anthropic_profile()],
            breaker,
        );
        let decision = router.resolve(Role::Worker).unwrap();
        assert_eq!(decision.model_ref, model("zai", "glm-5.2"));
        assert_eq!(decision.fallback, None);
    }

    #[test]
    fn breaker_half_opens_after_cooldown_elapses_via_injected_clock() {
        let clock = ManualClock::new(0);
        let mut breaker = breaker_with_clock(clock.clone());
        for _ in 0..CircuitBreaker::DEFAULT_FAILURE_THRESHOLD {
            breaker.record_failure("zai");
        }
        assert_eq!(breaker.status("zai"), BreakerStatus::Open);

        // Not yet at the cooldown boundary: still open.
        clock.advance(CircuitBreaker::DEFAULT_COOLDOWN_MS - 1);
        assert_eq!(breaker.status("zai"), BreakerStatus::Open);

        // Cooldown elapsed: half-open, and the trial call is allowed.
        clock.advance(1);
        assert_eq!(breaker.status("zai"), BreakerStatus::HalfOpen);
        assert!(breaker.is_available("zai"));
    }

    #[test]
    fn a_failed_half_open_trial_reopens_immediately() {
        let clock = ManualClock::new(0);
        let mut breaker = breaker_with_clock(clock.clone());
        for _ in 0..CircuitBreaker::DEFAULT_FAILURE_THRESHOLD {
            breaker.record_failure("zai");
        }
        clock.advance(CircuitBreaker::DEFAULT_COOLDOWN_MS);
        assert_eq!(breaker.status("zai"), BreakerStatus::HalfOpen);

        // The trial call fails — reopen without needing 3 more failures.
        breaker.record_failure("zai");
        assert_eq!(breaker.status("zai"), BreakerStatus::Open);
    }

    #[test]
    fn all_providers_broken_is_a_named_error_not_a_silent_pick() {
        let clock = ManualClock::new(0);
        let mut breaker = breaker_with_clock(clock);
        for _ in 0..CircuitBreaker::DEFAULT_FAILURE_THRESHOLD {
            breaker.record_failure("zai");
            breaker.record_failure("anthropic");
        }
        let router = Router::new(
            RoleTable::new(),
            vec![zai_profile(), anthropic_profile()],
            breaker,
        );
        assert_eq!(
            router.resolve(Role::Worker).unwrap_err(),
            RouterError::AllProvidersUnavailable { role: Role::Worker }
        );
    }

    #[test]
    fn judge_reports_both_fallback_and_same_family_caveat_when_only_the_worker_family_is_healthy() {
        // zai is broken; anthropic is the only healthy provider, and it's
        // also the family worker ends up resolving to (since zai is down).
        // Judge must report the breaker fallback (zai -> anthropic) AND
        // flag that it's degraded to worker's own family.
        let clock = ManualClock::new(0);
        let mut breaker = breaker_with_clock(clock);
        for _ in 0..CircuitBreaker::DEFAULT_FAILURE_THRESHOLD {
            breaker.record_failure("zai");
        }
        let router = Router::new(
            RoleTable::new(),
            vec![zai_profile(), anthropic_profile()],
            breaker,
        );

        let judge = router.resolve(Role::Judge).unwrap();
        assert_eq!(judge.model_ref, model("anthropic", "claude-fable-5"));
        let fallback = judge.fallback.expect("breaker fallback must be reported");
        assert_eq!(fallback.from, "zai");
        assert_eq!(fallback.to, "anthropic");
        assert!(
            judge.caveat.is_some(),
            "same-family degradation must be flagged"
        );
    }
}
