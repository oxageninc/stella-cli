//! Context-receipts emission (spec §4/§5), kept out of the driver body so the
//! step loop calls one helper and the emission logic stays independently
//! testable. Produces, per step: a `BlockRegistered` event for each context
//! block the model has not seen before (content-addressed, emitted once), then
//! a `StepManifest` naming the blocks it saw this step in wire order.
//!
//! Content-free by construction: blocks carry a `content_digest`, never the
//! payload bytes — the preimage already lives in the originating event in the
//! journal, so this is an index over the fold, not a second content store.
//!
//! Granularity is event-level (increment 1): the engine decomposes what it can
//! see in the message vec — the system prefix, each assistant text and
//! tool-call, and each tool result by `call_id`. Splitting the recalled user
//! message into per-frame `RecalledFrame` blocks (with `memory_id`) is the
//! memory-join increment (spec §9), where the pipeline participates.

use std::collections::HashMap;

use sha2::{Digest, Sha256};
use stella_protocol::{
    AgentEvent, BlockKind, BlockOrigin, CacheZone, CompletionMessage, ManifestEntry, MessageRole,
    ModelCallRole,
};

use crate::estimator::{CHARS_PER_TOKEN, estimate_conversation_tokens};
use crate::event_sender::EventSender;

/// `sha256` hex of a string. Byte-wise hex (the sha2 0.11 output type does not
/// implement `LowerHex` directly), matching the context store's `to_hex`.
fn sha256_hex(s: &str) -> String {
    use std::fmt::Write as _;
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let mut hex = String::with_capacity(64);
    for byte in h.finalize() {
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

/// The content-addressed id of a block: `blk_` + the first 24 hex chars of
/// `sha256(kind_tag \0 content)`. Mirrors the context store's `nod_…` shape.
/// Byte-identical blocks of the same kind share an id — the property that makes
/// dedup/supersession identities and lets residency track a block across steps.
fn block_id(kind: BlockKind, content: &str) -> String {
    format!(
        "blk_{}",
        &sha256_hex(&format!("{}\0{}", kind_tag(kind), content))[..24]
    )
}

/// The stable snake_case tag for a block kind — kept in lockstep with the
/// protocol enum's `rename_all = "snake_case"` so a block's id and its stored
/// `kind` string agree.
fn kind_tag(kind: BlockKind) -> &'static str {
    match kind {
        BlockKind::SystemPrefix => "system_prefix",
        BlockKind::UserGoal => "user_goal",
        BlockKind::RecalledFrame => "recalled_frame",
        BlockKind::AssistantText => "assistant_text",
        BlockKind::ToolCall => "tool_call",
        BlockKind::ToolResult => "tool_result",
        BlockKind::Steered => "steered",
        BlockKind::Summary => "summary",
        BlockKind::Attachment => "attachment",
        BlockKind::Other => "other",
    }
}

/// The engine's raw per-block token estimate — the same char heuristic the
/// conversation estimator uses, applied to one block's content, so a block's
/// cost is on the same scale as `StepUsage.estimated_input_tokens`.
fn estimate_tokens(content: &str) -> u32 {
    (content.chars().count() as f64 / CHARS_PER_TOKEN).ceil() as u32
}

/// Whether a block kind's preimage lives in the event journal (resolved at
/// reconstruction time) or must be captured locally at emission. Tool I/O and
/// assistant text ride the journal (`ToolStart`/`ToolResult`/`Text`); the
/// system prefix and the assembled user/recall/steer/summary messages do not,
/// so their bytes are carried as local-only block content (spec §5.3).
fn is_gap_kind(kind: BlockKind) -> bool {
    matches!(
        kind,
        BlockKind::SystemPrefix
            | BlockKind::UserGoal
            | BlockKind::RecalledFrame
            | BlockKind::Steered
            | BlockKind::Summary
    )
}

/// One decomposed context block, before it becomes a manifest entry.
struct BlockDraft {
    block_id: String,
    kind: BlockKind,
    content_digest: String,
    token_cost: u32,
    call_id: Option<String>,
    cache_zone: CacheZone,
    /// Which sent `CompletionMessage` this block belonged to — its position in
    /// the message vector — so reconstruction regroups exactly (spec §5.1).
    message_index: usize,
    /// Local-only preimage for gap kinds; `None` for journal-resolvable kinds.
    content: Option<String>,
}

impl BlockDraft {
    fn new(kind: BlockKind, content: &str, call_id: Option<String>, message_index: usize) -> Self {
        BlockDraft {
            block_id: block_id(kind, content),
            kind,
            content_digest: format!("sha256:{}", sha256_hex(content)),
            token_cost: estimate_tokens(content),
            call_id,
            cache_zone: CacheZone::Cacheable,
            message_index,
            content: is_gap_kind(kind).then(|| content.to_string()),
        }
    }
}

/// Decompose the live message vector into event-granular blocks, in wire order,
/// tagging each with its source message index (for reconstruction regrouping)
/// and a structural cache zone. The zone is a hint at emission — the system
/// prefix is the stable-cached head (L-E8) and the final message is the live
/// tail that is recomputed each step; cache attribution (spec §7, A3) refines
/// these against reported usage.
fn decompose(messages: &[CompletionMessage]) -> Vec<BlockDraft> {
    let mut drafts = Vec::new();
    for (message_index, message) in messages.iter().enumerate() {
        match message.role {
            MessageRole::System => {
                drafts.push(BlockDraft::new(
                    BlockKind::SystemPrefix,
                    &message.content,
                    None,
                    message_index,
                ));
            }
            MessageRole::User => {
                // The recalled frames live inside this message; splitting them
                // into per-frame blocks is the memory-join increment (§9).
                drafts.push(BlockDraft::new(
                    BlockKind::UserGoal,
                    &message.content,
                    None,
                    message_index,
                ));
            }
            MessageRole::Assistant => {
                if !message.content.is_empty() {
                    drafts.push(BlockDraft::new(
                        BlockKind::AssistantText,
                        &message.content,
                        None,
                        message_index,
                    ));
                }
                for call in &message.tool_calls {
                    let content = serde_json::to_string(call).unwrap_or_default();
                    drafts.push(BlockDraft::new(
                        BlockKind::ToolCall,
                        &content,
                        Some(call.call_id.clone()),
                        message_index,
                    ));
                }
            }
            MessageRole::Tool => {
                for result in &message.tool_results {
                    let content = serde_json::to_string(&result.output).unwrap_or_default();
                    drafts.push(BlockDraft::new(
                        BlockKind::ToolResult,
                        &content,
                        Some(result.call_id.clone()),
                        message_index,
                    ));
                }
            }
        }
    }
    // Structural zones: the head is the stable prefix, the tail is volatile.
    // Set the tail first so a single-block conversation ends StablePrefix.
    if let Some(last) = drafts.last_mut() {
        last.cache_zone = CacheZone::Volatile;
    }
    if let Some(first) = drafts.first_mut()
        && first.kind == BlockKind::SystemPrefix
    {
        first.cache_zone = CacheZone::StablePrefix;
    }
    drafts
}

/// Per-turn receipt state: which blocks have been registered (and at which
/// step they first appeared, for residency), plus the effective compaction
/// budget the driver computed for the step about to run. Lives on the stack in
/// `run_turn`, threaded into the model call by reference — the engine holds no
/// receipt state of its own.
///
/// Public so reconstruction tests and inspect tooling can drive receipt
/// emission directly against a message vector without standing up a full turn.
pub struct ReceiptLedger {
    turn_instance: u32,
    first_seen_step: HashMap<String, usize>,
    effective_budget_tokens: u64,
    calibration_factor: f64,
}

impl ReceiptLedger {
    pub fn new(turn_instance: u32) -> Self {
        ReceiptLedger {
            turn_instance,
            first_seen_step: HashMap::new(),
            effective_budget_tokens: 0,
            calibration_factor: 1.0,
        }
    }

    /// Record the effective compaction budget and calibration factor the
    /// driver computed for the next step — the values the compaction pass
    /// actually compared against, carried onto the manifest so the receipt's
    /// numbers line up with the decision that was made (#364 item 1).
    pub fn set_effective_budget(&mut self, budget_tokens: u64, factor: f64) {
        self.effective_budget_tokens = budget_tokens;
        self.calibration_factor = factor;
    }

    /// Emit the receipt for one committed step: a `BlockRegistered` for every
    /// block first seen this step, then the ordered `StepManifest`. Called at
    /// the settled boundary where the served model/provider are known.
    pub fn emit_step_receipt(
        &mut self,
        messages: &[CompletionMessage],
        step: usize,
        role: ModelCallRole,
        provider: &str,
        model: &str,
        events: &EventSender,
    ) {
        // The manifest's estimate is the same conversation estimate the driver
        // pairs with `StepUsage` (a drift sample), computed here from the same
        // messages so the two events always agree.
        let estimated_input_tokens = estimate_conversation_tokens(messages);
        let drafts = decompose(messages);
        let mut blocks = Vec::with_capacity(drafts.len());
        for draft in &drafts {
            let resident_since_step = match self.first_seen_step.get(&draft.block_id) {
                Some(first) => *first,
                None => {
                    self.first_seen_step.insert(draft.block_id.clone(), step);
                    // Durable-before-visible: register the block before the
                    // manifest cites it. Gap kinds carry their local-only bytes
                    // (spec §5.3); journal-resolvable kinds carry only a digest.
                    let _ = events.send(AgentEvent::BlockRegistered {
                        block_id: draft.block_id.clone(),
                        kind: draft.kind,
                        origin: BlockOrigin {
                            turn_instance: self.turn_instance,
                            step,
                            call_id: draft.call_id.clone(),
                            memory_id: None,
                        },
                        token_cost: draft.token_cost,
                        content_digest: draft.content_digest.clone(),
                        citation_label: None,
                        content: draft.content.clone(),
                    });
                    step
                }
            };
            blocks.push(ManifestEntry {
                block_id: draft.block_id.clone(),
                cache_zone: draft.cache_zone,
                token_cost: draft.token_cost,
                resident_since_step,
                message_index: draft.message_index,
            });
        }
        let _ = events.send(AgentEvent::StepManifest {
            turn_instance: self.turn_instance,
            step,
            role,
            provider: provider.to_string(),
            model: model.to_string(),
            blocks,
            effective_budget_tokens: self.effective_budget_tokens,
            calibration_factor: self.calibration_factor,
            estimated_input_tokens,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stella_protocol::{ToolCall, ToolOutput, ToolResult};
    use tokio::sync::mpsc::unbounded_channel;

    fn drain(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    fn convo() -> Vec<CompletionMessage> {
        vec![
            CompletionMessage::system("you are a careful engineer"),
            CompletionMessage::user("fix the failing test"),
            CompletionMessage {
                role: MessageRole::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCall {
                    call_id: "c1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "a.rs"}),
                }],
                tool_results: vec![],
                attachments: vec![],
            },
            CompletionMessage {
                role: MessageRole::Tool,
                content: String::new(),
                tool_calls: vec![],
                tool_results: vec![ToolResult {
                    call_id: "c1".into(),
                    output: ToolOutput::Ok {
                        content: "fn a() {}".into(),
                    },
                }],
                attachments: vec![],
            },
        ]
    }

    #[test]
    fn manifest_names_every_block_in_wire_order_with_structural_zones() {
        let (tx, mut rx) = unbounded_channel();
        let events = EventSender::new(tx);
        let mut ledger = ReceiptLedger::new(0);
        ledger.set_effective_budget(136_363, 1.1);
        let messages = convo();
        ledger.emit_step_receipt(
            &messages,
            0,
            ModelCallRole::Worker,
            "anthropic",
            "opus",
            &events,
        );

        let evts = drain(&mut rx);
        let manifest = evts
            .iter()
            .find_map(|e| match e {
                AgentEvent::StepManifest { blocks, .. } => Some(blocks.clone()),
                _ => None,
            })
            .expect("a manifest was emitted");
        // system prefix, user goal, one tool_call, one tool_result = 4 blocks.
        assert_eq!(manifest.len(), 4);
        assert_eq!(manifest[0].cache_zone, CacheZone::StablePrefix);
        assert_eq!(manifest[3].cache_zone, CacheZone::Volatile);
        // A BlockRegistered was emitted for each, before the manifest.
        let registered = evts
            .iter()
            .filter(|e| matches!(e, AgentEvent::BlockRegistered { .. }))
            .count();
        assert_eq!(registered, 4);
    }

    #[test]
    fn a_block_carried_across_steps_registers_once_and_ages_its_residency() {
        let (tx, mut rx) = unbounded_channel();
        let events = EventSender::new(tx);
        let mut ledger = ReceiptLedger::new(0);
        let messages = convo();

        ledger.emit_step_receipt(&messages, 0, ModelCallRole::Worker, "p", "m", &events);
        let _ = drain(&mut rx);
        // Same conversation on the next step: no NEW registrations, and the
        // blocks report resident_since_step == 0 (they arrived on step 0).
        ledger.emit_step_receipt(&messages, 1, ModelCallRole::Worker, "p", "m", &events);
        let evts = drain(&mut rx);

        let new_registrations = evts
            .iter()
            .filter(|e| matches!(e, AgentEvent::BlockRegistered { .. }))
            .count();
        assert_eq!(new_registrations, 0, "carried blocks re-register 0 times");
        let manifest = evts
            .iter()
            .find_map(|e| match e {
                AgentEvent::StepManifest { blocks, .. } => Some(blocks.clone()),
                _ => None,
            })
            .expect("manifest");
        assert!(
            manifest.iter().all(|b| b.resident_since_step == 0),
            "every block has been resident since step 0"
        );
    }

    #[test]
    fn identical_tool_output_resolves_to_the_same_block_id() {
        // Two tool results with byte-identical output share a content-addressed
        // id — the property dedup/supersession identities rely on.
        let a = serde_json::to_string(&ToolOutput::Ok {
            content: "same".into(),
        })
        .unwrap();
        let id1 = block_id(BlockKind::ToolResult, &a);
        let id2 = block_id(BlockKind::ToolResult, &a);
        assert_eq!(id1, id2);
        assert!(id1.starts_with("blk_"));
        assert_eq!(id1.len(), 4 + 24);
    }
}
