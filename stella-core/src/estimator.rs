//! Token estimation for compaction decisions. The estimator returns a
//! deliberately conservative (over-)estimate — over-estimating triggers
//! compaction earlier, the safe direction. Provider-reported usage flows
//! through `AgentEvent::StepUsage` for telemetry AND back into the
//! estimator: [`Calibration`] tracks the observed actual/estimated
//! input-token ratio per model (`07-model-matrix.md` §4.3) and corrects the
//! heuristic with a bounded factor, so the estimate converges on what the
//! provider's tokenizer actually reports over a session. The uncalibrated
//! functions remain the char-heuristic baseline; the correction is applied
//! on top, never by mutating the heuristic's constants.

use std::collections::HashMap;
use std::sync::Mutex;

use stella_protocol::CompletionMessage;

/// Chars-per-token divisor. 4 is the classic English-prose heuristic; code
/// and JSON run denser (more tokens per char), so we use 3.5 to bias the
/// estimate high — over-estimating triggers compaction *earlier*, which is
/// the safe direction (silent truncation by the provider is the failure
/// mode this exists to prevent).
pub(crate) const CHARS_PER_TOKEN: f64 = 3.5;

/// Fixed per-message framing overhead (role tags, separators) in tokens.
const PER_MESSAGE_OVERHEAD: u64 = 4;

/// Estimate the token cost of one message, including any tool calls and
/// tool results it carries.
pub fn estimate_message_tokens(message: &CompletionMessage) -> u64 {
    let mut chars = message.content.len();
    for call in &message.tool_calls {
        chars += call.name.len();
        chars += call.input.to_string().len();
    }
    for result in &message.tool_results {
        chars += result.call_id.len();
        chars += match &result.output {
            stella_protocol::ToolOutput::Ok { content } => content.len(),
            stella_protocol::ToolOutput::Error { message } => message.len(),
        };
    }
    (chars as f64 / CHARS_PER_TOKEN).ceil() as u64 + PER_MESSAGE_OVERHEAD
}

/// Estimate the total token cost of a conversation.
pub fn estimate_conversation_tokens(messages: &[CompletionMessage]) -> u64 {
    messages.iter().map(estimate_message_tokens).sum()
}

/// EWMA smoothing weight for new drift samples. 0.3 converges within a
/// handful of steps (visible improvement inside one session) while a single
/// noisy sample moves the state by at most 30%.
const CALIBRATION_ALPHA: f64 = 0.3;

/// Samples required before a correction is applied at all — below this the
/// factor is exactly 1.0, so one or two flukes can never steer budgeting.
const CALIBRATION_MIN_SAMPLES: u32 = 3;

/// Bounds on the applied correction factor. The lower bound matters most:
/// the raw estimator is deliberately biased HIGH (see [`CHARS_PER_TOKEN`]),
/// and a factor below 1.0 spends that safety margin — capping it at 0.5
/// means calibration can at most halve the conservative estimate, so the
/// calibrated number stays an early-compaction bias, never an invitation to
/// silent provider truncation. The upper bound keeps a run of pathological
/// samples (a tokenizer bug, a provider mis-reporting usage) from doubling
/// every estimate and thrashing compaction.
const CALIBRATION_MIN_FACTOR: f64 = 0.5;
const CALIBRATION_MAX_FACTOR: f64 = 2.0;

/// One raw sample may not move the EWMA state by more than this ratio band
/// (2× the applied-factor bounds): a single absurd sample (estimated 10,
/// actual 50 000) is truncated before it enters the average, so recovery
/// takes a few ordinary samples instead of dozens.
const CALIBRATION_SAMPLE_MIN_RATIO: f64 = CALIBRATION_MIN_FACTOR / 2.0;
const CALIBRATION_SAMPLE_MAX_RATIO: f64 = CALIBRATION_MAX_FACTOR * 2.0;

/// Drift correction for one provider/model: an EWMA of the observed
/// actual/estimated input-token ratio, fed by `(estimated, actual)` pairs
/// from committed steps (`AgentEvent::StepUsage`) and applied as a bounded
/// multiplicative factor on the char-heuristic estimate.
///
/// The ratio also absorbs systematic per-request overhead the heuristic
/// cannot see (tool schemas, provider framing): those inflate `actual`
/// uniformly, so they surface as a stable ratio rather than noise.
///
/// Pure state — no I/O, no clock. Persistence of samples across sessions is
/// `stella-store`'s job; replay them through [`Calibration::record`] (oldest
/// first, so the EWMA weights the most recent session highest) to rebuild
/// the state.
#[derive(Debug, Clone)]
pub struct Calibration {
    ratio_ewma: f64,
    samples: u32,
}

impl Default for Calibration {
    fn default() -> Self {
        Self::new()
    }
}

impl Calibration {
    /// Fresh, uncorrected state: factor 1.0 until enough samples arrive.
    pub fn new() -> Self {
        Self {
            ratio_ewma: 1.0,
            samples: 0,
        }
    }

    /// Feed one observed pair: the raw (uncalibrated) pre-call estimate and
    /// the provider-reported actual input tokens (total, cached included).
    /// Zero on either side carries no signal (scripted tests, providers that
    /// omit usage) and is ignored rather than recorded as ratio 0/∞.
    pub fn record(&mut self, estimated: u64, actual: u64) {
        if estimated == 0 || actual == 0 {
            return;
        }
        let ratio = (actual as f64 / estimated as f64)
            .clamp(CALIBRATION_SAMPLE_MIN_RATIO, CALIBRATION_SAMPLE_MAX_RATIO);
        if self.samples == 0 {
            self.ratio_ewma = ratio;
        } else {
            self.ratio_ewma =
                CALIBRATION_ALPHA * ratio + (1.0 - CALIBRATION_ALPHA) * self.ratio_ewma;
        }
        self.samples += 1;
    }

    /// How many samples have been recorded.
    pub fn samples(&self) -> u32 {
        self.samples
    }

    /// The correction factor to multiply a raw estimate by: exactly 1.0
    /// below [`CALIBRATION_MIN_SAMPLES`], otherwise the EWMA ratio clamped
    /// to [[`CALIBRATION_MIN_FACTOR`], [`CALIBRATION_MAX_FACTOR`]] — see the
    /// bounds' doc for why that keeps the safe over-estimate direction.
    pub fn factor(&self) -> f64 {
        if self.samples < CALIBRATION_MIN_SAMPLES {
            return 1.0;
        }
        self.ratio_ewma
            .clamp(CALIBRATION_MIN_FACTOR, CALIBRATION_MAX_FACTOR)
    }

    /// [`estimate_message_tokens`] corrected by the current factor.
    pub fn calibrated_message_tokens(&self, message: &CompletionMessage) -> u64 {
        (estimate_message_tokens(message) as f64 * self.factor()).ceil() as u64
    }

    /// [`estimate_conversation_tokens`] corrected by the current factor.
    pub fn calibrated_conversation_tokens(&self, messages: &[CompletionMessage]) -> u64 {
        (estimate_conversation_tokens(messages) as f64 * self.factor()).ceil() as u64
    }
}

/// Per-model calibration state, keyed by the model string the provider
/// reports on each result (tokenizers differ per model, so their drift must
/// never blend). The provider dimension is handled where samples are
/// persisted and re-loaded: `stella-store` keys telemetry by
/// `(provider, model)` and a session seeds only its own resolved pair, so
/// one map never mixes same-named models from different providers.
///
/// Interior-mutable (a `Mutex` never held across an await) because the
/// engine drives turns through `&self` — the caller owns the map across
/// turns and hands the engine a shared reference, mirroring how
/// `BudgetGuard` outlives individual turns.
#[derive(Debug, Default)]
pub struct CalibrationMap {
    inner: Mutex<HashMap<String, Calibration>>,
}

impl CalibrationMap {
    /// An empty map: every model reads factor 1.0 until samples arrive.
    pub fn new() -> Self {
        Self::default()
    }

    /// Replay persisted `(estimated, actual)` pairs — oldest first — into
    /// `model`'s state, so a new session starts where the last one left off.
    pub fn seed(&self, model: &str, samples: &[(u64, u64)]) {
        let mut inner = self.lock();
        let calibration = inner.entry(model.to_string()).or_default();
        for &(estimated, actual) in samples {
            calibration.record(estimated, actual);
        }
    }

    /// Feed one observed pair into `model`'s state.
    pub fn record(&self, model: &str, estimated: u64, actual: u64) {
        self.lock()
            .entry(model.to_string())
            .or_default()
            .record(estimated, actual);
    }

    /// The correction factor for `model`. `None` (the driver hasn't seen a
    /// result yet, so no model string exists) falls back to the map's single
    /// entry when there is exactly one — the session-seeded state — and to
    /// 1.0 otherwise; an ambiguous multi-model map never guesses.
    pub fn factor(&self, model: Option<&str>) -> f64 {
        let inner = self.lock();
        match model {
            Some(model) => inner.get(model).map(Calibration::factor).unwrap_or(1.0),
            None if inner.len() == 1 => inner
                .values()
                .next()
                .map(Calibration::factor)
                .unwrap_or(1.0),
            None => 1.0,
        }
    }

    /// [`estimate_conversation_tokens`] corrected by `model`'s factor (same
    /// `None` fallback as [`CalibrationMap::factor`]).
    pub fn calibrated_conversation_tokens(
        &self,
        model: Option<&str>,
        messages: &[CompletionMessage],
    ) -> u64 {
        (estimate_conversation_tokens(messages) as f64 * self.factor(model)).ceil() as u64
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Calibration>> {
        // Poisoning means a panic mid-record; the state is a plain f64+u32
        // pair that cannot be left torn — keep calibrating.
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stella_protocol::{MessageRole, ToolOutput, ToolResult};

    #[test]
    fn empty_message_costs_only_overhead() {
        let m = CompletionMessage {
            role: MessageRole::User,
            content: String::new(),
            tool_calls: vec![],
            tool_results: vec![],
        };
        assert_eq!(estimate_message_tokens(&m), PER_MESSAGE_OVERHEAD);
    }

    #[test]
    fn estimate_grows_with_content_length() {
        let short = CompletionMessage::user("hi");
        let long = CompletionMessage::user("a".repeat(3500));
        assert!(estimate_message_tokens(&long) > estimate_message_tokens(&short));
        // 3500 chars / 3.5 = 1000 tokens + overhead
        assert_eq!(estimate_message_tokens(&long), 1000 + PER_MESSAGE_OVERHEAD);
    }

    #[test]
    fn tool_results_count_toward_the_estimate() {
        let bare = CompletionMessage {
            role: MessageRole::Tool,
            content: String::new(),
            tool_calls: vec![],
            tool_results: vec![],
        };
        let loaded = CompletionMessage {
            role: MessageRole::Tool,
            content: String::new(),
            tool_calls: vec![],
            tool_results: vec![ToolResult {
                call_id: "c1".into(),
                output: ToolOutput::Ok {
                    content: "x".repeat(7000),
                },
            }],
        };
        assert!(estimate_message_tokens(&loaded) > estimate_message_tokens(&bare) + 1900);
    }

    #[test]
    fn conversation_estimate_is_sum_of_messages() {
        let messages = vec![
            CompletionMessage::system("sys"),
            CompletionMessage::user("hello"),
        ];
        let total = estimate_conversation_tokens(&messages);
        let sum: u64 = messages.iter().map(estimate_message_tokens).sum();
        assert_eq!(total, sum);
    }

    // ---- Calibration -------------------------------------------------------

    #[test]
    fn factor_is_identity_below_the_minimum_sample_count() {
        let mut cal = Calibration::new();
        assert_eq!(cal.factor(), 1.0);
        cal.record(1000, 1500);
        cal.record(1000, 1500);
        assert_eq!(
            cal.factor(),
            1.0,
            "two samples must not steer budgeting yet (min is {CALIBRATION_MIN_SAMPLES})"
        );
        cal.record(1000, 1500);
        assert!(
            (cal.factor() - 1.5).abs() < 1e-9,
            "at the minimum count the correction applies: {}",
            cal.factor()
        );
    }

    #[test]
    fn calibration_converges_and_the_estimate_error_shrinks() {
        // A provider whose tokenizer consistently runs 40% over the char
        // heuristic: the calibrated estimate's error against `actual` must
        // shrink monotonically-ish over a session and land near zero.
        let mut cal = Calibration::new();
        let estimated = 10_000u64;
        let actual = 14_000u64;
        let initial_error = estimated.abs_diff(actual);
        let mut last_error = u64::MAX;
        for step in 0..10 {
            let calibrated = (estimated as f64 * cal.factor()).ceil() as u64;
            let error = calibrated.abs_diff(actual);
            if step >= 3 {
                assert!(
                    error <= last_error,
                    "error must not grow once corrections apply: step {step}, {error} > {last_error}"
                );
                last_error = error;
            }
            cal.record(estimated, actual);
        }
        let final_error = ((estimated as f64 * cal.factor()).ceil() as u64).abs_diff(actual);
        assert!(
            final_error < initial_error / 10,
            "after 10 steady samples the error must be <10% of the raw drift: \
             {final_error} vs initial {initial_error}"
        );
    }

    #[test]
    fn factor_is_clamped_so_wild_ratios_cannot_destabilize_budgets() {
        let mut low = Calibration::new();
        let mut high = Calibration::new();
        for _ in 0..20 {
            low.record(100_000, 1); // absurd under-run
            high.record(1, 100_000); // absurd over-run
        }
        assert_eq!(
            low.factor(),
            CALIBRATION_MIN_FACTOR,
            "the correction may at most halve the conservative estimate"
        );
        assert_eq!(
            high.factor(),
            CALIBRATION_MAX_FACTOR,
            "the correction may at most double the estimate"
        );
    }

    #[test]
    fn one_noisy_sample_is_bounded_before_it_enters_the_average() {
        let mut cal = Calibration::new();
        for _ in 0..5 {
            cal.record(1000, 1000); // steady, perfectly calibrated
        }
        cal.record(1, 1_000_000); // one absurd spike
        // The spike enters as at most CALIBRATION_SAMPLE_MAX_RATIO with
        // weight alpha: 0.3·4.0 + 0.7·1.0 = 1.9 — never the raw 1e6 ratio.
        assert!(
            cal.factor() <= 1.9 + 1e-9,
            "a single spike must move the factor by a bounded step, got {}",
            cal.factor()
        );
        // …and a couple of ordinary samples pull it back down.
        cal.record(1000, 1000);
        cal.record(1000, 1000);
        cal.record(1000, 1000);
        assert!(
            cal.factor() < 1.35,
            "recovery must be quick: {}",
            cal.factor()
        );
    }

    #[test]
    fn zero_sided_samples_carry_no_signal_and_are_ignored() {
        let mut cal = Calibration::new();
        for _ in 0..10 {
            cal.record(0, 5_000); // no estimate recorded (legacy rows)
            cal.record(5_000, 0); // provider omitted usage
        }
        assert_eq!(cal.samples(), 0);
        assert_eq!(cal.factor(), 1.0);
    }

    #[test]
    fn calibrated_estimates_scale_the_raw_heuristic() {
        let mut cal = Calibration::new();
        for _ in 0..5 {
            cal.record(1000, 2000);
        }
        let msg = CompletionMessage::user("a".repeat(3500));
        let raw = estimate_message_tokens(&msg);
        assert_eq!(cal.calibrated_message_tokens(&msg), raw * 2);
        let convo = vec![CompletionMessage::system("sys"), msg];
        assert_eq!(
            cal.calibrated_conversation_tokens(&convo),
            (estimate_conversation_tokens(&convo) as f64 * 2.0).ceil() as u64
        );
    }

    #[test]
    fn map_keys_models_independently_and_seeds_replay_history() {
        let map = CalibrationMap::new();
        map.seed("glm-5.2", &[(1000, 1500), (1000, 1500), (1000, 1500)]);
        map.record("claude-fable-5", 1000, 800);
        assert!((map.factor(Some("glm-5.2")) - 1.5).abs() < 1e-9);
        assert_eq!(
            map.factor(Some("claude-fable-5")),
            1.0,
            "one sample is below the minimum — and glm's drift must not leak in"
        );
        assert_eq!(map.factor(Some("never-seen")), 1.0);
    }

    #[test]
    fn map_falls_back_to_its_single_entry_before_any_model_is_known() {
        let map = CalibrationMap::new();
        assert_eq!(map.factor(None), 1.0, "empty map: no correction");
        map.seed("glm-5.2", &[(1000, 1400), (1000, 1400), (1000, 1400)]);
        assert!(
            (map.factor(None) - 1.4).abs() < 1e-9,
            "a single seeded entry serves the first pre-call read of a session"
        );
        map.seed("other-model", &[(1000, 1400), (1000, 1400), (1000, 1400)]);
        assert_eq!(
            map.factor(None),
            1.0,
            "two entries and no model named: never guess"
        );
    }
}
