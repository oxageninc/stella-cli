# Extension hook bus

The hook bus (`stella_core::bus`) is the typed, in-process event system that
lets extensions observe agent activity and, on an explicit allowlist of
policy-sensitive actions, intercept it. It is independent of UI rendering:
the TUI and the `stream-json` serializer consume `AgentEvent`s; extensions
consume `HookEvent`s. Nothing here depends on either renderer.

> **Not** the settings-declared shell-command engine. That is
> `stella_core::hooks` (`HookEvent::PreToolUse` etc., Claude Code parity),
> which spawns commands from `settings.json`. This module is the
> *programmatic* seam: in-process handlers registered in code against a
> catalog of dotted event names. The two share a name (`HookEvent`) but are
> distinct types; `bus::HookEvent` stays module-qualified to keep them
> apart.

## Envelope

Every event is delivered in one uniform envelope:

```rust
struct HookEvent {
    id: String,          // "evt_<session_id>_<sequence>"
    name: String,        // dotted, e.g. "file.updated"
    timestamp: String,   // ISO 8601 UTC, millisecond precision, ...Z
    session_id: String,
    turn_id: Option<String>,
    agent_id: Option<String>,
    sequence: u64,       // monotonic within a session, from 1
    payload: serde_json::Value,
}
```

`sequence` is assigned under an atomic counter, so it is dense and unique
even under concurrent emitters. Events from one thread are delivered in
emission order; a consumer needing a total order across threads sorts by
`sequence`. The timestamp is fixed-width and zero-padded, so lexicographic
order equals time order.

## Two hook kinds

### Observers — `on` / `emit`

```rust
let sub = bus.on("file.*", |event| {
    log_it(event);
    Ok(())            // Err(msg) reports a handled failure — isolated
});
bus.emit_named("file.read", json!({ "path": "src/main.rs" }));
```

Observers run inline in registration order. They **cannot** block, delay, or
fail the primary operation: a handler that returns `Err` or panics is logged
to a bounded in-memory ring (`recent_failures`), surfaced as an
`extension.error` event, and skipped — the next handler and the emitting
operation both continue. `extension.error` delivery does not recurse into a
globally-broken handler.

`emit` runs handlers synchronously on the emitting thread — there is no
queue to overflow. For genuinely async processing, forward into a channel:

```rust
let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
bus.on("*", bus::forward_to(tx)).detach();
// ... consume rx on your own task
```

### Policy hooks — `on_blocking` / `emit_blocking`

Only the events in `names::BLOCKING` are interceptable:

```text
tool.call.requested   file.created   file.updated   file.deleted
command.started       git.commit.requested          git.push.requested
pull_request.requested                               deployment.requested
```

```rust
bus.on_blocking("deployment.*", |_event| {
    HookDecision::RequireApproval { reason: "prod deploys need a human".into() }
});
let outcome = bus.emit_blocking(HookEventDraft::new("deployment.requested", payload));
if outcome.allowed() { /* proceed */ }
```

Blocking handlers run **sequentially in registration order**. Each returns a
`HookDecision`:

| decision                       | effect                                              |
| ------------------------------ | --------------------------------------------------- |
| `Allow`                        | continue to the next handler                        |
| `Modify { payload }`           | replace the event payload, continue (next handler sees it) |
| `Deny { reason }`              | stop; the operation is refused                      |
| `RequireApproval { reason }`   | stop; the operation needs approval                  |

The chain folds `Modify` into the payload and **stops at the first `Deny` or
`RequireApproval`**. A chain that only ever `Modify`/`Allow`s ends `Allow`
with `modified == true`. A panicking policy handler **fails closed** — it
denies, so a broken policy extension can never wave an operation through.

Every `emit_blocking` records its final decision as a `policy.evaluated`
event, plus exactly one of `policy.allowed` / `policy.blocked` /
`approval.requested`. Those records carry only the decision, never the
payload.

## Payload hygiene

Payloads must not carry secrets or full file contents by default:

- **Observable** tool events (`tool.call.started`, …) run the input through
  `sanitize_tool_input`, which replaces content-bearing fields (`content`,
  `new_string`, `old_string`) with `"<omitted: N bytes, M lines>"`. Only
  blocking policy handlers — the privileged interception point — and the
  `emit_blocking` caller see the raw input.
- `scan_for_secrets` recognizes high-precision secret shapes (PEM private
  keys, AWS/GitHub/Slack/Google/`sk-` tokens) and drives `secret.detected`,
  which names only the *kind*, never the matched text.
- `is_sensitive_path` flags credential-shaped paths (`.env*`, `*.pem`,
  `id_rsa`, …) and drives `sensitive_operation.detected` (path only).

## Disposal

`on` / `on_blocking` return a `HookSubscription`. **Dropping it
unsubscribes** — cleanup-safe by construction. Call `.detach()` to keep the
handler for the bus's lifetime, or `bus.off(sub)` / `sub.unsubscribe()` to
remove it explicitly. Removal is idempotent and safe after the bus is gone.

## Matching

`on`/`on_blocking` patterns match by:

- exact name — `"file.read"`
- namespace wildcard — `"file.*"` matches `file.read`, `file.diff.computed`;
  `"tool.*"` matches `tool.call.requested`. Never matches a bare `file` or a
  prefix-confused `filesystem.read`.
- global — `"*"` matches every event.

## Host wiring

`ToolRegistry::attach_bus(bus)` connects the tool loop to a session bus.
From then on, every `execute`:

1. runs the `tool.call.requested` blocking chain (a `modify` rewrites the
   input the tool actually runs);
2. emits `sensitive_operation.detected` / `secret.detected` for a
   classified file op;
3. runs the per-side-effect blocking chain (`file.created` / `file.updated`
   / `file.deleted`, or `command.started` for `bash`);
4. emits `tool.call.started` (sanitized), runs the tool, then
   `tool.call.completed` / `tool.call.failed` (and `command.completed` /
   `command.failed` for `bash`);
5. on success, emits the `file.*` fact event and `files_touched.updated`,
   carrying the ledger `revision` assigned under the touch lock.

A registry with no bus attached behaves exactly as it did before hooks
existed — every emission is `None`-guarded.
