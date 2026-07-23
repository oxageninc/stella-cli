//! Opt-in live smoke suite (issue #274) — one minimal REAL call per provider
//! adapter, against the provider's actual endpoint. Every other test in this
//! crate runs against `wiremock`, which proves "we parse the shape we
//! expect" but can never prove "the live API accepts the shape we send" —
//! exactly the gap that let the Anthropic adapter's legacy `thinking` shape
//! ship for months before a live call surfaced its 400 (see #240).
//!
//! **Gate**: every test here is a no-op unless `STELLA_LIVE_SMOKE=1` is set
//! (see [`live_smoke_enabled`]) AND that provider's own credential resolves
//! (env var, or a `~/.stella/credentials.toml` entry — the same two
//! non-interactive steps `stella-cli`'s chain uses, see [`resolve_key`]).
//! Neither condition met -> the test prints why to stderr and returns
//! `Ok` (a clean skip, never a failure) — so `cargo test`/`make gate`/CI's
//! default `cargo test --workspace` never makes a network call or spends
//! real money. Wire it up locally with e.g.
//! `STELLA_LIVE_SMOKE=1 ANTHROPIC_API_KEY=sk-… cargo test -p stella-model \
//! --test live_smoke -- --nocapture`.
//!
//! **What each test asserts**: wire-shape ACCEPTANCE — the live endpoint
//! returns 200 and stella's own parser reassembles a `CompletionResult` from
//! it — never model quality (no assertion on what the model actually says).
//! A wire-shape regression (a field the adapter sends that the live API now
//! rejects, or a response shape it no longer parses) fails the relevant
//! test with the real `ProviderError`, which already carries the
//! provider's own rejection reason (see `http::classify_http_status`) —
//! never a raw credential.
//!
//! **The Anthropic `cache_control` question** (the other half of #274):
//! `anthropic.rs`'s `stamp_tail_cache_breakpoint` places `cache_control`
//! only on content BLOCKS (the system block, and the last block of the
//! final message) — never as a top-level request field, which the assumption
//! holds would 400. That placement is asserted against a mock in
//! `anthropic::tests::request_serializes_both_cache_breakpoints`;
//! `anthropic_smoke` below checks it against the real API by sending a system
//! prompt long enough to clear Anthropic's minimum cacheable-prefix floor (so
//! a real cache write is exercised, not just accepted-but-inert) and asserting
//! the call succeeds.

use stella_model::anthropic::AnthropicProvider;
use stella_model::bedrock::BedrockProvider;
use stella_model::credential::{ApiKey, CredentialsFile};
use stella_model::gemini::GeminiProvider;
use stella_model::openai::OpenAiProvider;
use stella_model::provider::Provider;
use stella_model::vertex::VertexProvider;
use stella_model::zai::ZaiProvider;
use stella_protocol::{CompletionMessage, CompletionRequest};

/// Serializes the tests in this file that mutate `STELLA_LIVE_SMOKE` (the
/// gate-behavior witnesses below) against every test that reads it
/// (`armed_key`, called at the top of every live-* test) — `setenv`/`getenv`
/// racing across threads is documented UB on POSIX regardless of which side
/// mutates, and libtest runs this binary's tests on parallel threads.
/// Mirrors `stella-cli`'s `test_env` convention. Held only across the
/// synchronous gate-check + key-resolve step, never across a network await.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Whether the live smoke suite is armed at all. Requires the LITERAL value
/// `1` (not `true`/`yes`/any other truthy-looking string) — a narrow,
/// unambiguous switch for something that spends real money and makes real
/// network calls, deliberately stricter than the repo's other `STELLA_*`
/// boolean gates (`STELLA_NO_ENV_FILE`, `STELLA_ENV_DEBUG`), which accept
/// any non-empty non-`"0"` value.
fn live_smoke_enabled() -> bool {
    std::env::var("STELLA_LIVE_SMOKE").is_ok_and(|v| v == "1")
}

/// Resolve `env_var`'s (and, in order, each of `aliases`') credential from
/// the process environment, then `~/.stella/credentials.toml` —
/// the same two non-interactive steps of `stella-cli`'s chain
/// (`ApiKey::resolve` + alias fallback), minus the CLI-flag and
/// interactive-prompt steps, neither of which make sense for an
/// unattended test. Never panics; `None` means "nothing resolves."
fn resolve_key(provider_id: &str, env_var: &str, aliases: &[&str]) -> Option<ApiKey> {
    if let Ok(key) = ApiKey::from_env(env_var) {
        return Some(key);
    }
    for alias in aliases {
        if let Ok(key) = ApiKey::from_env(alias) {
            return Some(key);
        }
    }
    CredentialsFile::load_default()
        .ok()?
        .get(provider_id)
        .map(ApiKey::new)
}

/// Core of [`armed_key`], assuming the caller already holds `env_lock()`.
/// Split out so the gate-behavior witness tests below — which need their
/// own guard held across a full mutate-assert-restore window — can call
/// this directly instead of re-entering `env_lock()` through `armed_key`:
/// `std::sync::Mutex` is not reentrant, so a witness test that already
/// holds the guard and then called `armed_key` (which locks again on the
/// same thread) would deadlock rather than assert. Always logs WHY to
/// stderr (visible with `cargo test -- --nocapture`) so a run makes it
/// obvious which providers were actually exercised versus skipped.
fn armed_key_locked(provider_id: &str, env_var: &str, aliases: &[&str]) -> Option<ApiKey> {
    if !live_smoke_enabled() {
        eprintln!("[live_smoke] {provider_id}: skipped — set STELLA_LIVE_SMOKE=1 to run");
        return None;
    }
    match resolve_key(provider_id, env_var, aliases) {
        Some(key) => Some(key),
        None => {
            let alias_note = if aliases.is_empty() {
                String::new()
            } else {
                format!(" (or {})", aliases.join("/"))
            };
            eprintln!(
                "[live_smoke] {provider_id}: skipped — no {env_var}{alias_note} env var and no \
                 `{provider_id}` entry in ~/.stella/credentials.toml"
            );
            None
        }
    }
}

/// The credential to run `provider_id`'s smoke test with, or `None` to skip
/// cleanly — never a failure — when the suite isn't armed or the
/// credential isn't resolvable. Takes `env_lock()` for the synchronous
/// gate-check + resolve step only, released well before any caller's
/// network `.await` (see `env_lock`'s doc). Callers that already hold the
/// guard themselves must call [`armed_key_locked`] instead, not this.
fn armed_key(provider_id: &str, env_var: &str, aliases: &[&str]) -> Option<ApiKey> {
    let _guard = env_lock();
    armed_key_locked(provider_id, env_var, aliases)
}

/// A minimal completion request: one short user turn, output capped tiny.
/// The point is wire-shape acceptance, never model quality, so content is
/// deliberately trivial — `system` is the only part callers vary (the
/// Anthropic cache-control probe needs a long one; everyone else gets a
/// short one).
fn tiny_request(system: impl Into<String>) -> CompletionRequest {
    CompletionRequest {
        messages: vec![
            CompletionMessage::system(system),
            CompletionMessage::user("Reply with the single word OK."),
        ],
        max_output_tokens: Some(16),
        temperature: None,
        effort: None,
        tools: vec![],
        reasoning: None,
        params: None,
    }
}

const TINY_SYSTEM_PROMPT: &str = "You are a terse smoke-test assistant.";

/// A system prompt long enough to clear Anthropic's minimum cacheable-block
/// size (documented as ~1024 tokens for Sonnet/Opus-tier models, ~2048 for
/// Haiku-tier) — deterministic, cheap-to-generate filler repeated well past
/// either floor (~150 repeats of an ~80-byte sentence is ~3,000 tokens), so
/// the smoke call can tell "cache_control accepted" (any success) apart
/// from "cache_control accepted AND actually engaged" (a real
/// `cache_creation_input_tokens`/`cache_read_input_tokens` count > 0). Only
/// `anthropic_smoke` needs this; every other provider's system prompt stays
/// genuinely tiny.
fn anthropic_cache_probe_system_prompt() -> String {
    "You are a terse smoke-test assistant. Only ever reply with the single word OK. ".repeat(150)
}

// ---- gate-behavior witnesses (always run; never touch the network) ------

#[test]
fn live_smoke_is_disabled_by_default() {
    let _guard = env_lock();
    // SAFETY: guarded by `env_lock`; this test owns the mutate-assert-
    // restore window.
    unsafe {
        std::env::remove_var("STELLA_LIVE_SMOKE");
    }
    assert!(
        !live_smoke_enabled(),
        "the live smoke suite must be OFF unless STELLA_LIVE_SMOKE=1 is explicitly set"
    );
}

#[test]
fn live_smoke_requires_the_exact_value_one() {
    let _guard = env_lock();
    // SAFETY: guarded by `env_lock`.
    unsafe {
        std::env::set_var("STELLA_LIVE_SMOKE", "true");
    }
    assert!(
        !live_smoke_enabled(),
        "STELLA_LIVE_SMOKE must require the literal value `1`, not any truthy-looking string \
         (this suite spends real money — the gate should never arm by accident)"
    );
    unsafe {
        std::env::remove_var("STELLA_LIVE_SMOKE");
    }
}

#[test]
fn armed_key_skips_cleanly_when_the_gate_is_off_even_with_a_key_present() {
    let _guard = env_lock();
    // SAFETY: guarded by `env_lock`; unique var name this test owns.
    unsafe {
        std::env::remove_var("STELLA_LIVE_SMOKE");
        std::env::set_var("STELLA_LIVE_SMOKE_WITNESS_KEY", "sk-present-but-unarmed");
    }
    assert!(
        armed_key_locked("witness-provider", "STELLA_LIVE_SMOKE_WITNESS_KEY", &[]).is_none(),
        "a resolvable key must not be enough on its own — the STELLA_LIVE_SMOKE gate must \
         also be set"
    );
    unsafe {
        std::env::remove_var("STELLA_LIVE_SMOKE_WITNESS_KEY");
    }
}

#[test]
fn armed_key_skips_cleanly_when_the_gate_is_on_but_no_key_resolves() {
    let _guard = env_lock();
    // SAFETY: guarded by `env_lock`; unique var name, deliberately left
    // unset.
    unsafe {
        std::env::set_var("STELLA_LIVE_SMOKE", "1");
        std::env::remove_var("STELLA_LIVE_SMOKE_WITNESS_UNSET_KEY");
    }
    assert!(
        armed_key_locked(
            "witness-provider",
            "STELLA_LIVE_SMOKE_WITNESS_UNSET_KEY",
            &[]
        )
        .is_none(),
        "the gate alone must not be enough — a resolvable credential is also required"
    );
    unsafe {
        std::env::remove_var("STELLA_LIVE_SMOKE");
    }
}

// ---- live smoke: one minimal real call per adapter -----------------------

/// Anthropic Messages API. Also the `cache_control` settlement: every
/// Anthropic request `stella` builds carries `cache_control` on the system
/// block and the final message's tail block (never top-level — see
/// `anthropic::stamp_tail_cache_breakpoint`'s doc). This test's long system
/// prompt ([`anthropic_cache_probe_system_prompt`]) exists specifically so
/// that placement gets genuinely exercised, not just accepted-but-inert.
///
/// **Verdict**: a successful `Ok(_)` here proves the live Messages API
/// accepts today's block-level-only `cache_control` shape; the printed
/// `cache_write_tokens`/`cached_input_tokens` additionally show whether
/// caching actually engaged for this call (0 on a cold cache is normal on
/// the FIRST call — a second run within the ~5-minute TTL should show a
/// `cached_input_tokens` read instead of a `cache_write_tokens` write).
///
/// A billing/quota 400 (`"credit balance is too low"`) is NOT a wire-shape
/// verdict: Anthropic names the offending field in `invalid_request_error`
/// when a request is actually malformed, so a balance rejection never
/// exercises the request shape at all. Re-run with
/// `STELLA_LIVE_SMOKE=1 cargo test -p stella-model --test live_smoke \
/// anthropic_smoke -- --nocapture`.
#[tokio::test]
async fn anthropic_smoke() {
    let Some(key) = armed_key("anthropic", "ANTHROPIC_API_KEY", &[]) else {
        return;
    };
    let provider = AnthropicProvider::new(key, "claude-fable-5");
    let req = tiny_request(anthropic_cache_probe_system_prompt());
    match provider.complete(req).await {
        Ok(r) => eprintln!(
            "[live_smoke] anthropic: OK — model={} finish={:?} usage={:?} \
             cache_write_tokens={} cached_input_tokens={} text={:?}",
            r.model,
            r.finish_reason,
            r.usage,
            r.usage.cache_write_tokens,
            r.usage.cached_input_tokens,
            r.text
        ),
        // Deliberately does NOT say "rejected the request shape": a failure
        // here can be an account-billing 400 that never reached shape
        // validation, so pre-judging the cause would print something false on
        // the exact failure this suite exists to distinguish from a real
        // regression. `e` already carries the provider's own status + body
        // (never a raw credential), so a human reading it tells wire-shape
        // apart from auth/quota/billing.
        Err(e) => panic!(
            "anthropic live smoke did not return a parseable 200 — could be a genuine \
             wire-shape regression OR an unrelated account/auth/quota/billing problem; \
             read the status and body below to tell which: {e}"
        ),
    }
}

/// OpenAI Responses API.
#[tokio::test]
async fn openai_smoke() {
    let Some(key) = armed_key("openai", "OPENAI_API_KEY", &[]) else {
        return;
    };
    let provider = OpenAiProvider::new(key, "gpt-5.5");
    let req = tiny_request(TINY_SYSTEM_PROMPT);
    match provider.complete(req).await {
        Ok(r) => eprintln!(
            "[live_smoke] openai: OK — model={} finish={:?} usage={:?} text={:?}",
            r.model, r.finish_reason, r.usage, r.text
        ),
        Err(e) => panic!("openai live smoke failed: {e}"),
    }
}

/// Gemini direct `generateContent`. `GEMINI_API_KEY` (spec-documented
/// alias `GOOGLE_API_KEY`, same as `stella-cli`'s `config::PROVIDERS` row).
#[tokio::test]
async fn gemini_smoke() {
    let Some(key) = armed_key("gemini", "GEMINI_API_KEY", &["GOOGLE_API_KEY"]) else {
        return;
    };
    let provider = GeminiProvider::new(key, "gemini-3-pro");
    let req = tiny_request(TINY_SYSTEM_PROMPT);
    match provider.complete(req).await {
        Ok(r) => eprintln!(
            "[live_smoke] gemini: OK — model={} finish={:?} usage={:?} text={:?}",
            r.model, r.finish_reason, r.usage, r.text
        ),
        Err(e) => panic!("gemini live smoke failed: {e}"),
    }
}

/// Vertex AI `generateContent`. Needs the OAuth2 bearer
/// (`VERTEX_ACCESS_TOKEN`, e.g. `$(gcloud auth print-access-token)`) AND a
/// project id (`VERTEX_PROJECT_ID` or `GOOGLE_CLOUD_PROJECT`) — mirrors
/// `stella-cli::agent::engine::build_provider_parts`'s Vertex arm exactly,
/// so this test skips the same way a real `stella --model vertex/…` run
/// would fail with a named error, rather than sending a doomed request.
#[tokio::test]
async fn vertex_smoke() {
    let Some(access_token) = armed_key("vertex", "VERTEX_ACCESS_TOKEN", &[]) else {
        return;
    };
    let Some(project) = std::env::var("VERTEX_PROJECT_ID")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| {
            std::env::var("GOOGLE_CLOUD_PROJECT")
                .ok()
                .filter(|v| !v.is_empty())
        })
    else {
        eprintln!(
            "[live_smoke] vertex: skipped — VERTEX_ACCESS_TOKEN is set but no \
             VERTEX_PROJECT_ID/GOOGLE_CLOUD_PROJECT"
        );
        return;
    };
    let location = std::env::var("VERTEX_LOCATION")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "global".to_string());
    let provider = VertexProvider::new(access_token, "gemini-3-pro", project, location);
    let req = tiny_request(TINY_SYSTEM_PROMPT);
    match provider.complete(req).await {
        Ok(r) => eprintln!(
            "[live_smoke] vertex: OK — model={} finish={:?} usage={:?} text={:?}",
            r.model, r.finish_reason, r.usage, r.text
        ),
        Err(e) => panic!("vertex live smoke failed: {e}"),
    }
}

/// Amazon Bedrock Converse. Needs the standard AWS chain
/// (`AWS_ACCESS_KEY_ID` + `AWS_SECRET_ACCESS_KEY`, optional
/// `AWS_SESSION_TOKEN`/`AWS_REGION`) — mirrors
/// `build_provider_parts`'s Bedrock arm exactly.
#[tokio::test]
async fn bedrock_smoke() {
    let Some(access_key) = armed_key("bedrock", "AWS_ACCESS_KEY_ID", &[]) else {
        return;
    };
    let Some(secret) = std::env::var("AWS_SECRET_ACCESS_KEY")
        .ok()
        .filter(|v| !v.is_empty())
    else {
        eprintln!(
            "[live_smoke] bedrock: skipped — AWS_ACCESS_KEY_ID is set but no \
             AWS_SECRET_ACCESS_KEY"
        );
        return;
    };
    let session_token = std::env::var("AWS_SESSION_TOKEN")
        .ok()
        .filter(|v| !v.is_empty())
        .map(ApiKey::new);
    let region = std::env::var("AWS_REGION")
        .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "us-east-1".to_string());
    let provider = BedrockProvider::new(
        access_key,
        ApiKey::new(secret),
        session_token,
        region,
        "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
    );
    let req = tiny_request(TINY_SYSTEM_PROMPT);
    match provider.complete(req).await {
        Ok(r) => eprintln!(
            "[live_smoke] bedrock: OK — model={} finish={:?} usage={:?} text={:?}",
            r.model, r.finish_reason, r.usage, r.text
        ),
        Err(e) => panic!("bedrock live smoke failed: {e}"),
    }
}

/// OpenRouter, via the shared OpenAI-compatible `ZaiProvider` re-identified
/// (`with_identity`) exactly like `build_provider_parts`'s
/// `Dialect::OpenaiCompatible` arm — including app attribution and
/// `usage: {"include": true}` accounting, so this test also exercises
/// OpenRouter's own per-call cost-reporting frame. `openrouter/auto` is the
/// real vendor-namespaced slug for OpenRouter's own auto-router model (its
/// catalog is namespaced `vendor/model`; this is genuinely how
/// `stella-cli`'s auto-detected default reaches it — see
/// `config::PROVIDERS`'s `openrouter` row).
#[tokio::test]
async fn openrouter_smoke() {
    let Some(key) = armed_key("openrouter", "OPENROUTER_API_KEY", &[]) else {
        return;
    };
    let provider = ZaiProvider::new(key, "openrouter/auto")
        .with_base_url("https://openrouter.ai/api/v1")
        .with_identity("openrouter", "OpenRouter")
        .with_attribution("https://stella.oxagen.sh", "Stella")
        .with_usage_accounting();
    let req = tiny_request(TINY_SYSTEM_PROMPT);
    match provider.complete(req).await {
        Ok(r) => eprintln!(
            "[live_smoke] openrouter: OK — model={} finish={:?} usage={:?} cost_usd={} text={:?}",
            r.model, r.finish_reason, r.usage, r.cost_usd, r.text
        ),
        Err(e) => panic!("openrouter live smoke failed: {e}"),
    }
}

/// Z.ai (GLM), via the OpenAI-compatible adapter under its own identity.
#[tokio::test]
async fn zai_smoke() {
    let Some(key) = armed_key("zai", "ZAI_API_KEY", &[]) else {
        return;
    };
    let provider = ZaiProvider::new(key, "glm-5.2")
        .with_base_url("https://api.z.ai/api/paas/v4")
        .with_identity("zai", "Z.ai");
    let req = tiny_request(TINY_SYSTEM_PROMPT);
    match provider.complete(req).await {
        Ok(r) => eprintln!(
            "[live_smoke] zai: OK — model={} finish={:?} usage={:?} text={:?}",
            r.model, r.finish_reason, r.usage, r.text
        ),
        Err(e) => panic!("zai live smoke failed: {e}"),
    }
}

/// DeepSeek, via the OpenAI-compatible adapter under its own identity.
#[tokio::test]
async fn deepseek_smoke() {
    let Some(key) = armed_key("deepseek", "DEEPSEEK_API_KEY", &[]) else {
        return;
    };
    let provider = ZaiProvider::new(key, "deepseek-chat")
        .with_base_url("https://api.deepseek.com/v1")
        .with_identity("deepseek", "DeepSeek");
    let req = tiny_request(TINY_SYSTEM_PROMPT);
    match provider.complete(req).await {
        Ok(r) => eprintln!(
            "[live_smoke] deepseek: OK — model={} finish={:?} usage={:?} text={:?}",
            r.model, r.finish_reason, r.usage, r.text
        ),
        Err(e) => panic!("deepseek live smoke failed: {e}"),
    }
}

/// xAI (Grok), via the OpenAI-compatible adapter under its own identity.
#[tokio::test]
async fn xai_smoke() {
    let Some(key) = armed_key("xai", "XAI_API_KEY", &[]) else {
        return;
    };
    let provider = ZaiProvider::new(key, "grok-4")
        .with_base_url("https://api.x.ai/v1")
        .with_identity("xai", "xAI");
    let req = tiny_request(TINY_SYSTEM_PROMPT);
    match provider.complete(req).await {
        Ok(r) => eprintln!(
            "[live_smoke] xai: OK — model={} finish={:?} usage={:?} text={:?}",
            r.model, r.finish_reason, r.usage, r.text
        ),
        Err(e) => panic!("xai live smoke failed: {e}"),
    }
}
