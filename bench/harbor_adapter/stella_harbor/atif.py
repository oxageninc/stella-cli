"""Convert Stella's stable JSON event envelope to ATIF v1.7.

Stella emits one ``step_usage`` event for every committed model call.  Execute
calls carry their authoritative output in a following ``text`` event; pipeline
management and compaction calls embed ``output_text`` in the usage record
because they are not user-facing.  This module folds both shapes into one ATIF
agent step per call and keeps the original instruction as the first user step.
"""

from __future__ import annotations

import json
import math
from dataclasses import dataclass, field
from typing import Any

from harbor.models.trajectories import (
    Agent,
    FinalMetrics,
    Metrics,
    Observation,
    ObservationResult,
    Step,
    ToolCall,
    Trajectory,
)


def _stringify(value: Any) -> str:
    """Return a lossless-enough display representation for JSON-like values."""
    if isinstance(value, str):
        return value
    try:
        return json.dumps(value, ensure_ascii=False, sort_keys=True)
    except (TypeError, ValueError):
        return str(value)


def _nonnegative_int(value: Any) -> int | None:
    """Normalize a non-negative numeric telemetry value, excluding booleans."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    number = float(value)
    if not math.isfinite(number) or number < 0 or not number.is_integer():
        return None
    return int(number)


def _nonnegative_float(value: Any) -> float | None:
    """Normalize a finite non-negative cost value, excluding booleans."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    number = float(value)
    if not math.isfinite(number) or number < 0:
        return None
    return number


def envelope_accounting(envelope: dict[str, Any]) -> dict[str, Any]:
    """Audit envelope totals against every reported ``step_usage`` record.

    Each field reports how many usage records contained a valid value and its
    sum. Missing values are explicit and make that field incomplete; they are
    never replaced with zero. The envelope-vs-step cost check is only declared
    consistent or mismatched when both sides are complete and known.
    """
    raw_events = envelope.get("events")
    events = raw_events if isinstance(raw_events, list) else []
    usage_events = [
        event
        for event in events
        if isinstance(event, dict) and event.get("type") == "step_usage"
    ]
    record_count = len(usage_events)
    usage_models = [
        model.strip()
        for event in usage_events
        if isinstance((model := event.get("model")), str) and model.strip()
    ]
    if not record_count:
        model_state = "unknown"
    elif len(usage_models) == record_count:
        model_state = "complete"
    else:
        model_state = "incomplete"

    field_specs = {
        "input_tokens": _nonnegative_int,
        "output_tokens": _nonnegative_int,
        "cached_input_tokens": _nonnegative_int,
        "cache_write_tokens": _nonnegative_int,
        "estimated_input_tokens": _nonnegative_int,
        "cost_usd": _nonnegative_float,
        "duration_ms": _nonnegative_int,
        "retries": _nonnegative_int,
        "tool_calls": _nonnegative_int,
    }
    fields: dict[str, dict[str, Any]] = {}
    for field_name, normalize in field_specs.items():
        values = [
            normalized
            for event in usage_events
            if (normalized := normalize(event.get(field_name))) is not None
        ]
        if not record_count:
            state = "unknown"
        elif len(values) == record_count:
            state = "complete"
        else:
            state = "incomplete"
        total: int | float | None
        if not values:
            total = None
        elif field_name == "cost_usd":
            total = math.fsum(values)
        else:
            total = sum(values)
        fields[field_name] = {
            "state": state,
            "reported_records": len(values),
            "total": total,
        }

    envelope_cost = _nonnegative_float(envelope.get("cost_usd"))
    step_cost = fields["cost_usd"]
    step_cost_total = step_cost["total"]
    cost_difference: float | None = None
    if (
        envelope_cost is None
        or step_cost["state"] != "complete"
        or not isinstance(step_cost_total, (int, float))
    ):
        cost_consistency = "unknown"
    else:
        cost_difference = float(envelope_cost) - float(step_cost_total)
        tolerance = max(1e-9, abs(envelope_cost) * 1e-6)
        cost_consistency = (
            "consistent" if abs(cost_difference) <= tolerance else "mismatch"
        )

    stream = envelope.get("_stella_stream")
    stream_complete = (
        stream.get("stream_complete") if isinstance(stream, dict) else None
    )
    cost_source = stream.get("cost_source") if isinstance(stream, dict) else None
    if cost_consistency != "unknown" and cost_source == "summed_step_usage":
        # The synthetic envelope total is useful, but it is the same evidence,
        # not an independent cross-check against a terminal Complete event.
        cost_consistency = "derived_from_step_usage"
    required_fields = (
        "input_tokens",
        "output_tokens",
        "cached_input_tokens",
        "cost_usd",
    )
    required_complete = all(
        fields[field_name]["state"] == "complete" for field_name in required_fields
    )
    if stream_complete is False or envelope.get("status") == "interrupted":
        accounting_state = "incomplete"
    elif not record_count or envelope_cost is None:
        accounting_state = "unknown"
    elif required_complete:
        accounting_state = "complete"
    else:
        accounting_state = "incomplete"

    return {
        "state": accounting_state,
        "step_usage_records": record_count,
        "fields": fields,
        "envelope_total_cost_usd": envelope_cost,
        "step_usage_total_cost_usd": step_cost_total,
        "cost_consistency": cost_consistency,
        "cost_difference_usd": cost_difference,
        "model_state": model_state,
        "model_records": len(usage_models),
        "models": sorted(set(usage_models)),
    }


def _event_fragment(event: dict[str, Any]) -> str:
    """Read a text/reasoning fragment across old and current field names."""
    value = event.get("delta")
    if value is None:
        value = event.get("text")
    return value if isinstance(value, str) else _stringify(value or "")


@dataclass
class _ModelCall:
    """Intermediate fold for one metered or best-effort unmetered model call."""

    usage: dict[str, Any] | None
    reasoning: list[str] = field(default_factory=list)
    events: list[dict[str, Any]] = field(default_factory=list)


def _fold_model_calls(events: list[Any]) -> list[_ModelCall]:
    """Partition Stella events without treating preview text as authoritative.

    Reasoning is streamed before the corresponding ``step_usage`` record.
    Authoritative text and tools follow that record.  Tool execution can be
    followed by reasoning for the *next* call, so reasoning fragments remain
    pending until the next usage record arrives.
    """
    calls: list[_ModelCall] = []
    current: _ModelCall | None = None
    pending_reasoning: list[str] = []
    pre_usage_events: list[dict[str, Any]] = []

    for raw_event in events:
        if not isinstance(raw_event, dict):
            continue
        event_type = raw_event.get("type")

        if event_type == "reasoning":
            pending_reasoning.append(_event_fragment(raw_event))
            continue

        if event_type == "step_usage":
            # Preserve any authoritative events that unexpectedly preceded
            # the first usage record as their own unmetered call.
            if pre_usage_events:
                calls.append(
                    _ModelCall(
                        usage=None,
                        reasoning=[],
                        events=pre_usage_events,
                    )
                )
                pre_usage_events = []
            current = _ModelCall(
                usage=raw_event,
                reasoning=pending_reasoning,
            )
            pending_reasoning = []
            calls.append(current)
            continue

        if event_type not in {"text", "tool_start", "tool_result"}:
            # ``text_delta`` is explicitly a best-effort preview and must not
            # be merged with the following authoritative ``text`` event.
            continue

        if current is None:
            pre_usage_events.append(raw_event)
        else:
            current.events.append(raw_event)

    if pre_usage_events:
        calls.append(_ModelCall(usage=None, events=pre_usage_events))
    if pending_reasoning:
        # A call can be interrupted after streaming reasoning but before a
        # usage record.  Keep the evidence while making its unmetered nature
        # explicit rather than silently attributing it to the previous call.
        calls.append(_ModelCall(usage=None, reasoning=pending_reasoning))

    return calls


def _tool_arguments(call: dict[str, Any]) -> tuple[dict[str, Any], dict[str, Any]]:
    """Return ATIF-valid arguments plus metadata for a non-object input."""
    raw_input = call.get("input")
    if isinstance(raw_input, dict):
        return raw_input, {}
    return {"value": raw_input}, {"stella_raw_input_type": type(raw_input).__name__}


def _tool_result_content(
    output: Any,
) -> tuple[str | None, str, dict[str, Any] | None]:
    """Decode Stella's externally-tagged ``ToolOutput`` representation."""
    if isinstance(output, dict):
        ok_value = output.get("ok")
        if isinstance(ok_value, dict) and "content" in ok_value:
            return _stringify(ok_value.get("content")), "ok", None

        error_value = output.get("error")
        if isinstance(error_value, dict) and "message" in error_value:
            return _stringify(error_value.get("message")), "error", None

    if output is None:
        return None, "unknown", None
    return (
        _stringify(output),
        "unknown",
        {"unrecognized_stella_output": output},
    )


def _call_to_step(call: _ModelCall, step_id: int, default_model: str | None) -> Step:
    """Convert one folded Stella call into a validated ATIF agent step."""
    usage = call.usage
    embedded_output = usage.get("output_text") if usage is not None else None
    text_parts: list[str] = (
        [embedded_output] if isinstance(embedded_output, str) else []
    )
    tool_specs: list[dict[str, Any]] = []
    tool_spec_by_id: dict[str, dict[str, Any]] = {}
    result_specs: list[dict[str, Any]] = []

    for event in call.events:
        event_type = event.get("type")
        if event_type == "text":
            text_parts.append(_event_fragment(event))
            continue

        if event_type == "tool_start":
            raw_call = event.get("call")
            if not isinstance(raw_call, dict):
                raw_call = {}
            raw_id = raw_call.get("call_id")
            call_id = raw_id if isinstance(raw_id, str) and raw_id else "unknown-call"
            arguments, extra = _tool_arguments(raw_call)
            spec = {
                "call_id": call_id,
                "name": raw_call.get("name") or "unknown",
                "arguments": arguments,
                "extra": extra,
                "has_result": False,
            }
            tool_specs.append(spec)
            tool_spec_by_id.setdefault(call_id, spec)
            continue

        if event_type != "tool_result":
            continue

        raw_id = event.get("call_id")
        call_id = raw_id if isinstance(raw_id, str) and raw_id else "unknown-call"
        tool_spec = tool_spec_by_id.get(call_id)
        if tool_spec is None:
            # ATIF requires every observation source id to reference a tool
            # call in the same step.  Synthesize the missing start while
            # retaining the original result and call id for auditability.
            tool_spec = {
                "call_id": call_id,
                "name": "unknown",
                "arguments": {},
                "extra": {"synthetic_from_orphan_result": True},
                "has_result": False,
            }
            tool_specs.append(tool_spec)
            tool_spec_by_id[call_id] = tool_spec
        tool_spec["has_result"] = True

        content, status, output_extra = _tool_result_content(event.get("output"))
        result_extra: dict[str, Any] = {"status": status}
        duration_ms = _nonnegative_int(event.get("duration_ms"))
        if duration_ms is not None:
            result_extra["duration_ms"] = duration_ms
        if isinstance(event.get("speculated"), bool):
            result_extra["speculated"] = event["speculated"]
        if output_extra:
            result_extra.update(output_extra)
        result_specs.append(
            {
                "source_call_id": call_id,
                "content": content,
                "extra": result_extra,
            }
        )

    tool_calls: list[ToolCall] = []
    for spec in tool_specs:
        extra = dict(spec["extra"])
        if not spec["has_result"]:
            extra["result_missing"] = True
        tool_calls.append(
            ToolCall(
                tool_call_id=spec["call_id"],
                function_name=_stringify(spec["name"]),
                arguments=spec["arguments"],
                extra=extra or None,
            )
        )

    observations = [ObservationResult(**spec) for spec in result_specs]

    metrics: Metrics | None = None
    model_name = default_model
    step_extra: dict[str, Any] = {}
    if usage is not None:
        raw_model = usage.get("model")
        if isinstance(raw_model, str) and raw_model:
            model_name = raw_model

        metrics_extra: dict[str, Any] = {}
        for key in (
            "cache_write_tokens",
            "estimated_input_tokens",
            "duration_ms",
            "retries",
            "tool_calls",
        ):
            value = _nonnegative_int(usage.get(key))
            if value is not None:
                metrics_extra[key] = value

        metrics = Metrics(
            prompt_tokens=_nonnegative_int(usage.get("input_tokens")),
            completion_tokens=_nonnegative_int(usage.get("output_tokens")),
            cached_tokens=_nonnegative_int(usage.get("cached_input_tokens")),
            cost_usd=_nonnegative_float(usage.get("cost_usd")),
            extra=metrics_extra or None,
        )
        stella_step = _nonnegative_int(usage.get("step"))
        if stella_step is not None:
            step_extra["stella_step"] = stella_step
        purpose = usage.get("purpose")
        if isinstance(purpose, str) and purpose:
            step_extra["stella_purpose"] = purpose
    else:
        step_extra["usage_missing"] = True

    return Step(
        step_id=step_id,
        source="agent",
        model_name=model_name,
        message="".join(text_parts),
        reasoning_content="".join(call.reasoning) or None,
        tool_calls=tool_calls or None,
        observation=Observation(results=observations) if observations else None,
        metrics=metrics,
        llm_call_count=1,
        extra=step_extra or None,
    )


def envelope_to_trajectory(
    envelope: dict[str, Any],
    *,
    instruction: str,
    session_id: str,
    agent_version: str,
    default_model: str | None,
    return_code: int | None,
    binary_sha256: str | None = None,
    binary_sha256_verified: bool | None = None,
    source_commit: str | None = None,
    disable_reflection: str | None = None,
    adapter_version: str | None = None,
    adapter_sha256: str | None = None,
    harbor_version: str | None = None,
    harbor_sha256: str | None = None,
    source_commit_verified: bool | None = None,
    base_url: str | None = None,
    provider_route_policy: str | None = None,
    budget_usd: str | None = None,
    credential_handoff: str | None = None,
    host_credential_source: str | None = None,
    host_credential_name: str | None = None,
    host_credential_bundle_count: int | None = None,
    container_credential_absence_verified: bool | None = None,
    launcher_controls: dict[str, str] | None = None,
    engine_posture_version: str | None = None,
    engine_posture: dict[str, Any] | None = None,
    engine_posture_json: str | None = None,
    engine_posture_sha256: str | None = None,
) -> Trajectory:
    """Build and validate an ATIF-v1.7 trajectory from a Stella envelope."""
    events = envelope.get("events")
    raw_events = events if isinstance(events, list) else []
    model = envelope.get("model")
    if not isinstance(model, str) or not model:
        model = default_model

    steps: list[Step] = [Step(step_id=1, source="user", message=instruction)]
    calls = _fold_model_calls(raw_events)
    for call in calls:
        steps.append(_call_to_step(call, len(steps) + 1, model))

    # If an old/malformed envelope omitted events, retain its final answer as
    # an explicitly unmetered call instead of producing an instruction-only
    # trajectory or silently dropping agent output.
    final_text = envelope.get("text")
    if len(steps) == 1 and isinstance(final_text, str) and final_text:
        steps.append(
            Step(
                step_id=2,
                source="agent",
                model_name=model,
                message=final_text,
                llm_call_count=1,
                extra={"usage_missing": True, "source": "envelope_final_text"},
            )
        )

    metric_steps = [step for step in steps if step.metrics is not None]

    def _sum_metric(name: str) -> int | None:
        values = [
            value
            for step in metric_steps
            if (value := getattr(step.metrics, name)) is not None
        ]
        return sum(values) if values and len(values) == len(metric_steps) else None

    step_costs = [
        step.metrics.cost_usd
        for step in metric_steps
        if step.metrics.cost_usd is not None
    ]
    total_cost = _nonnegative_float(envelope.get("cost_usd"))
    if total_cost is None and step_costs and len(step_costs) == len(metric_steps):
        total_cost = sum(step_costs)

    total_model_duration_ms = 0
    model_duration_count = 0
    total_cache_write_tokens = 0
    cache_write_count = 0
    for step in metric_steps:
        extra = step.metrics.extra or {}
        duration = _nonnegative_int(extra.get("duration_ms"))
        if duration is not None:
            total_model_duration_ms += duration
            model_duration_count += 1
        cache_write = _nonnegative_int(extra.get("cache_write_tokens"))
        if cache_write is not None:
            total_cache_write_tokens += cache_write
            cache_write_count += 1

    total_tool_duration_ms = 0
    tool_duration_seen = False
    for step in steps:
        if step.observation is None:
            continue
        for result in step.observation.results:
            duration = _nonnegative_int((result.extra or {}).get("duration_ms"))
            if duration is not None:
                total_tool_duration_ms += duration
                tool_duration_seen = True

    final_extra: dict[str, Any] = {}
    if metric_steps and model_duration_count == len(metric_steps):
        final_extra["total_model_duration_ms"] = total_model_duration_ms
    if tool_duration_seen:
        final_extra["total_tool_duration_ms"] = total_tool_duration_ms
    if metric_steps and cache_write_count == len(metric_steps):
        final_extra["total_cache_write_tokens"] = total_cache_write_tokens
    accounting = envelope_accounting(envelope)
    final_extra["stella_accounting"] = accounting

    root_extra: dict[str, Any] = {"stella_event_count": len(raw_events)}
    for key in (
        "status",
        "reason",
        "task_class",
        "verdict",
        "revisions",
        "candidates_run",
        "files_touched",
    ):
        value = envelope.get(key)
        if value is not None:
            root_extra[key] = value
    if return_code is not None:
        root_extra["stella_return_code"] = return_code
    stream_metadata = envelope.get("_stella_stream")
    if isinstance(stream_metadata, dict):
        root_extra["stella_stream"] = stream_metadata

    agent_extra: dict[str, Any] = {"output_format": "stream-json"}
    if binary_sha256:
        agent_extra["binary_sha256"] = binary_sha256
    if binary_sha256_verified is not None:
        agent_extra["binary_sha256_verified_in_container"] = binary_sha256_verified
    if source_commit:
        agent_extra["source_commit"] = source_commit
    if source_commit_verified is not None:
        agent_extra["source_commit_verified_in_binary"] = source_commit_verified
    if disable_reflection is not None:
        agent_extra["disable_reflection"] = disable_reflection
        agent_extra["reflection_policy"] = (
            "disabled_for_ephemeral_benchmark"
            if disable_reflection.strip().lower() in {"1", "true", "yes", "on"}
            else "explicitly_enabled"
        )
    if adapter_version:
        agent_extra["adapter_version"] = adapter_version
    if adapter_sha256:
        agent_extra["adapter_sha256"] = adapter_sha256
    if harbor_version:
        agent_extra["harbor_version"] = harbor_version
    if harbor_sha256:
        agent_extra["harbor_sha256"] = harbor_sha256
    if base_url:
        agent_extra["base_url"] = base_url
    if provider_route_policy:
        agent_extra["provider_route_policy"] = provider_route_policy
    if budget_usd is not None:
        agent_extra["budget_usd"] = budget_usd
    if credential_handoff:
        agent_extra["credential_handoff"] = credential_handoff
    if host_credential_source:
        agent_extra["host_credential_source"] = host_credential_source
    if host_credential_name:
        agent_extra["host_credential_name"] = host_credential_name
    if host_credential_bundle_count is not None:
        agent_extra["host_credential_bundle_count"] = host_credential_bundle_count
    if container_credential_absence_verified is not None:
        agent_extra["container_credential_absence_verified"] = (
            container_credential_absence_verified
        )
    if launcher_controls:
        agent_extra["launcher_controls"] = dict(launcher_controls)
    if engine_posture_version:
        agent_extra["engine_posture_version"] = engine_posture_version
    if engine_posture:
        agent_extra["engine_posture"] = dict(engine_posture)
    if engine_posture_json:
        agent_extra["engine_posture_json"] = engine_posture_json
    if engine_posture_sha256:
        agent_extra["engine_posture_sha256"] = engine_posture_sha256

    return Trajectory(
        schema_version="ATIF-v1.7",
        session_id=session_id,
        agent=Agent(
            name="stella",
            version=agent_version,
            model_name=model,
            extra=agent_extra,
        ),
        steps=steps,
        notes=(
            "Each agent step represents one Stella model call. Text events or "
            "usage.output_text are authoritative; text_delta previews are excluded. "
            "The source is Stella's durable stream-json event log. "
            "The initial user instruction is included in total_steps."
        ),
        final_metrics=FinalMetrics(
            total_prompt_tokens=_sum_metric("prompt_tokens"),
            total_completion_tokens=_sum_metric("completion_tokens"),
            total_cached_tokens=_sum_metric("cached_tokens"),
            total_cost_usd=total_cost,
            total_steps=len(steps),
            extra=final_extra or None,
        ),
        extra=root_extra,
    )
