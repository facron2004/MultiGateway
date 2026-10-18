"""Anthropic Messages API (/v1/messages) over the relay's Chat Completions core.

Claude Code and other Anthropic clients send ``system`` + ``messages`` with
``tool_use`` / ``tool_result`` content blocks and expect ``content_block`` SSE
events.  This module only translates the wire format; the caller still owns and
executes every tool.
"""

from __future__ import annotations

import json
import re
from typing import Any, AsyncIterator, Mapping, Optional

from .reasoning_effort import normalize_level

_DEFAULT_MAX_TOKENS = 32000
_ANTHROPIC_STOP_REASONS = {
    "length": "max_tokens",
    "max_tokens": "max_tokens",
    "content_filter": "refusal",
    "stop": "end_turn",
    "tool_calls": "tool_use",
    "tool_use": "tool_use",
}


class AnthropicRequestError(ValueError):
    """An Anthropic request could not be translated to Chat Completions."""

    def __init__(self, message: str, param: Optional[str] = None):
        super().__init__(message)
        self.param = param


def make_id(prefix: str) -> str:
    from .sse import make_id as _make_id

    return _make_id(prefix)


def _json_text(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), default=str)


def _text_from_blocks(value: Any) -> str:
    if isinstance(value, str):
        return value
    if not isinstance(value, list):
        return ""
    parts: list[str] = []
    for block in value:
        if isinstance(block, str):
            parts.append(block)
            continue
        if not isinstance(block, Mapping):
            continue
        if str(block.get("type") or "text") in ("text", "input_text"):
            text = block.get("text")
            if isinstance(text, str):
                parts.append(text)
    return "\n".join(part for part in parts if part)


def _tool_result_text(content: Any) -> str:
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return _json_text(content) if content not in (None, "") else ""
    parts: list[str] = []
    for block in content:
        if isinstance(block, str):
            parts.append(block)
            continue
        if not isinstance(block, Mapping):
            parts.append(str(block))
            continue
        if str(block.get("type") or "text") in ("text", "input_text"):
            text = block.get("text")
            if isinstance(text, str):
                parts.append(text)
        elif block.get("type") == "image":
            parts.append("[image tool result omitted]")
        else:
            parts.append(_json_text(block))
    return "\n".join(part for part in parts if part)


def _image_data_url(source: Any) -> str:
    if not isinstance(source, Mapping):
        return ""
    source_type = str(source.get("type") or "").strip().lower()
    if source_type == "base64":
        media_type = str(source.get("media_type") or "image/png")
        data = str(source.get("data") or "")
        if data:
            return f"data:{media_type};base64,{data}"
    if source_type == "url":
        url = source.get("url")
        if isinstance(url, str) and url:
            return url
    return ""


def _content_blocks_to_chat(content: Any) -> list[dict[str, Any]]:
    """Convert one Anthropic message's content into OpenAI content parts."""

    if isinstance(content, str):
        return [{"type": "text", "text": content}]
    if not isinstance(content, list):
        return []
    parts: list[dict[str, Any]] = []
    for block in content:
        if isinstance(block, str):
            parts.append({"type": "text", "text": block})
            continue
        if not isinstance(block, Mapping):
            continue
        block_type = str(block.get("type") or "text").strip().lower()
        if block_type in ("text", "input_text"):
            text = block.get("text")
            if isinstance(text, str) and text:
                parts.append({"type": "text", "text": text})
        elif block_type == "image":
            url = _image_data_url(block.get("source"))
            if url:
                parts.append({"type": "image_url", "image_url": {"url": url}})
    return parts


def normalize_request(body: Mapping[str, Any]) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    """Translate an Anthropic Messages request into Chat Completions inputs."""

    if not isinstance(body, Mapping):
        raise AnthropicRequestError("JSON body must be an object")
    messages_value = body.get("messages")
    if not isinstance(messages_value, list) or not messages_value:
        raise AnthropicRequestError("messages is required", "messages")

    messages: list[dict[str, Any]] = []
    system_text = _text_from_blocks(body.get("system"))
    if system_text:
        messages.append({"role": "system", "content": system_text})

    for index, raw in enumerate(messages_value):
        if not isinstance(raw, Mapping):
            raise AnthropicRequestError(f"messages.{index} must be an object", "messages")
        role = str(raw.get("role") or "").strip().lower()
        if role not in ("user", "assistant"):
            raise AnthropicRequestError(
                f"messages.{index}.role must be user or assistant", "messages"
            )
        content = raw.get("content")
        blocks = _content_blocks_to_chat(content)
        raw_blocks = content if isinstance(content, list) else []

        if role == "assistant":
            tool_calls: list[dict[str, Any]] = []
            text_parts: list[dict[str, Any]] = []
            for block in raw_blocks:
                if not isinstance(block, Mapping):
                    continue
                block_type = str(block.get("type") or "").strip().lower()
                if block_type == "tool_use":
                    name = str(block.get("name") or "")
                    if not name:
                        continue
                    tool_input = block.get("input")
                    tool_calls.append(
                        {
                            "id": str(block.get("id") or make_id("call")),
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": _json_text(
                                    tool_input if tool_input is not None else {}
                                ),
                            },
                        }
                    )
            text_parts = [part for part in blocks if part.get("type") == "text"]
            text = "\n".join(part["text"] for part in text_parts)
            message: dict[str, Any] = {"role": "assistant"}
            if text:
                message["content"] = text
            elif not tool_calls:
                message["content"] = ""
            if tool_calls:
                message["tool_calls"] = tool_calls
            if text or tool_calls:
                messages.append(message)
            continue

        # user role: text/image parts stay in the user turn, tool_result blocks
        # become OpenAI ``tool`` messages so the upstream keeps call correlation.
        text_parts = [part for part in blocks if part.get("type") == "text"]
        image_parts = [part for part in blocks if part.get("type") == "image_url"]
        for block in raw_blocks:
            if not isinstance(block, Mapping):
                continue
            if str(block.get("type") or "").strip().lower() != "tool_result":
                continue
            tool_call_id = str(block.get("tool_use_id") or "")
            result_text = _tool_result_text(block.get("content"))
            if block.get("is_error"):
                result_text = "Client tool result status=failed:\n" + result_text
            messages.append(
                {
                    "role": "tool",
                    "tool_call_id": tool_call_id or "unknown",
                    "content": result_text,
                }
            )
        if text_parts or image_parts:
            if image_parts:
                messages.append({"role": "user", "content": [*text_parts, *image_parts]})
            else:
                messages.append(
                    {"role": "user", "content": "\n".join(p["text"] for p in text_parts)}
                )

    if not any(str(m.get("role")) != "system" for m in messages):
        raise AnthropicRequestError("messages must contain a user turn", "messages")

    options: dict[str, Any] = {}
    max_tokens = body.get("max_tokens")
    if isinstance(max_tokens, bool) or not isinstance(max_tokens, (int, float)):
        max_tokens = _DEFAULT_MAX_TOKENS
    options["max_tokens"] = int(max_tokens) if max_tokens > 0 else _DEFAULT_MAX_TOKENS
    for key in ("temperature", "top_p", "stop_sequences", "metadata"):
        if key in body and body[key] is not None:
            options["stop" if key == "stop_sequences" else key] = body[key]

    tools = body.get("tools")
    if isinstance(tools, list) and tools:
        converted: list[dict[str, Any]] = []
        for tool in tools:
            if not isinstance(tool, Mapping):
                continue
            name = str(tool.get("name") or "")
            if not name:
                continue
            converted.append(
                {
                    "type": "function",
                    "function": {
                        "name": name,
                        "description": str(tool.get("description") or ""),
                        "parameters": tool.get("input_schema")
                        or {"type": "object", "properties": {}},
                    },
                }
            )
        if converted:
            options["tools"] = converted

    choice = body.get("tool_choice")
    if isinstance(choice, Mapping):
        choice_type = str(choice.get("type") or "").strip().lower()
        if choice_type == "any":
            options["tool_choice"] = "required"
        elif choice_type == "tool":
            name = str(choice.get("name") or "")
            if name:
                options["tool_choice"] = {"type": "function", "function": {"name": name}}
        elif choice_type == "none":
            options["tool_choice"] = "none"
        else:
            options["tool_choice"] = "auto"

    thinking = body.get("thinking")
    if isinstance(thinking, Mapping):
        thinking_type = str(thinking.get("type") or "").strip().lower()
        budget = thinking.get("budget_tokens")
        if thinking_type != "disabled":
            request_thinking: dict[str, Any] = {"type": "enabled"}
            if isinstance(budget, (int, float)) and not isinstance(budget, bool):
                request_thinking["budget_tokens"] = int(budget)
            request_thinking["effort"] = _budget_effort(budget)
            options["thinking"] = request_thinking
    return messages, options


def _budget_effort(budget: Any) -> str:
    if not isinstance(budget, (int, float)) or isinstance(budget, bool):
        return "high"
    if budget < 4096:
        return "light"
    if budget < 16384:
        return "high"
    return "extra_high"


def _usage(usage: Any, *, input_fallback: int = 0) -> dict[str, Any]:
    values = usage if isinstance(usage, Mapping) else {}

    def number(*keys: str) -> int:
        for key in keys:
            value = values.get(key)
            if isinstance(value, (int, float)) and not isinstance(value, bool):
                return max(0, int(value))
        return 0

    input_tokens = number("prompt_tokens", "input_tokens") or input_fallback
    output_tokens = number("completion_tokens", "output_tokens")
    return {"input_tokens": input_tokens, "output_tokens": output_tokens}


def _stop_reason(finish_reason: Any, *, has_tool_use: bool) -> str:
    if has_tool_use:
        return "tool_use"
    return _ANTHROPIC_STOP_REASONS.get(str(finish_reason or "stop").strip().lower(), "end_turn")


def _tool_use_block(call: Mapping[str, Any]) -> dict[str, Any]:
    function = call.get("function")
    function = function if isinstance(function, Mapping) else {}
    arguments = function.get("arguments")
    if isinstance(arguments, str):
        try:
            parsed: Any = json.loads(arguments or "{}")
        except json.JSONDecodeError:
            parsed = {"_raw": arguments}
    else:
        parsed = arguments if arguments is not None else {}
    if not isinstance(parsed, Mapping):
        parsed = {"value": parsed}
    return {
        "type": "tool_use",
        "id": str(call.get("id") or make_id("toolu")),
        "name": str(function.get("name") or call.get("name") or "tool"),
        "input": dict(parsed),
    }


def completion_to_message(
    completion: Mapping[str, Any],
    model: str,
    *,
    request_input_tokens: int = 0,
) -> dict[str, Any]:
    """Convert one Chat Completions object into an Anthropic message."""

    choices = completion.get("choices")
    choice = choices[0] if isinstance(choices, list) and choices else {}
    choice = choice if isinstance(choice, Mapping) else {}
    message = choice.get("message")
    message = message if isinstance(message, Mapping) else {}
    blocks: list[dict[str, Any]] = []
    reasoning = message.get("reasoning_content")
    if isinstance(reasoning, str) and reasoning.strip():
        blocks.append({"type": "thinking", "thinking": reasoning, "signature": ""})
    content = message.get("content")
    text = content if isinstance(content, str) else _text_from_blocks(content)
    if text:
        blocks.append({"type": "text", "text": text})
    tool_calls = message.get("tool_calls")
    tool_calls = tool_calls if isinstance(tool_calls, list) else []
    for call in tool_calls:
        if isinstance(call, Mapping):
            blocks.append(_tool_use_block(call))
    if not blocks:
        blocks.append({"type": "text", "text": ""})
    return {
        "id": make_id("msg"),
        "type": "message",
        "role": "assistant",
        "model": str(completion.get("model") or model),
        "content": blocks,
        "stop_reason": _stop_reason(
            choice.get("finish_reason"), has_tool_use=bool(tool_calls)
        ),
        "stop_sequence": None,
        "usage": _usage(completion.get("usage"), input_fallback=request_input_tokens),
    }


def _event(event_type: str, payload: Mapping[str, Any]) -> str:
    body = {"type": event_type, **payload}
    return f"event: {event_type}\ndata: {_json_text(body)}\n\n"


def ping_event() -> str:
    return _event("ping", {})


def error_event(message: str, error_type: str = "api_error") -> str:
    return _event(
        "error", {"error": {"type": error_type, "message": str(message or "error")}}
    )


async def translate_chat_stream(
    body_iterator: AsyncIterator[Any],
    model: str,
    *,
    request_input_tokens: int = 0,
) -> AsyncIterator[str]:
    """Translate the relay's OpenAI SSE stream into Anthropic SSE events.

    Thinking deltas open a ``thinking`` block, text deltas open a ``text``
    block, and tool calls open one ``tool_use`` block each with
    ``input_json_delta`` fragments.
    """

    message_id = make_id("msg")
    started = False
    text_index: Optional[int] = None
    thinking_index: Optional[int] = None
    tool_indexes: dict[int, int] = {}
    tool_names: dict[int, str] = {}
    tool_ids: dict[int, str] = {}
    next_index = 0
    finish_reason = "stop"
    usage_values: dict[str, Any] = {}
    saw_tool_use = False
    saw_finish_reason = False
    buffer = ""

    def open_message() -> list[str]:
        nonlocal started, next_index
        if started:
            return []
        started = True
        events = [
            _event(
                "message_start",
                {
                    "message": {
                        "id": message_id,
                        "type": "message",
                        "role": "assistant",
                        "model": model,
                        "content": [],
                        "stop_reason": None,
                        "stop_sequence": None,
                        "usage": {
                            "input_tokens": int(request_input_tokens or 0),
                            "output_tokens": 0,
                        },
                    }
                },
            )
        ]
        return events

    def start_text() -> list[str]:
        nonlocal text_index, next_index
        events = open_message()
        if text_index is not None:
            return events
        text_index = next_index
        next_index += 1
        events.append(
            _event(
                "content_block_start",
                {"index": text_index, "content_block": {"type": "text", "text": ""}},
            )
        )
        return events

    def start_thinking() -> list[str]:
        nonlocal thinking_index, next_index
        events = open_message()
        if thinking_index is not None:
            return events
        thinking_index = next_index
        next_index += 1
        events.append(
            _event(
                "content_block_start",
                {
                    "index": thinking_index,
                    "content_block": {"type": "thinking", "thinking": "", "signature": ""},
                },
            )
        )
        return events

    def start_tool(call: Mapping[str, Any]) -> list[str]:
        nonlocal next_index, saw_tool_use
        events = open_message()
        raw_index = call.get("index")
        try:
            key = int(raw_index)
        except (TypeError, ValueError):
            key = 0
        if key in tool_indexes:
            return events
        block_index = next_index
        next_index += 1
        tool_indexes[key] = block_index
        function = call.get("function")
        function = function if isinstance(function, Mapping) else {}
        name = str(function.get("name") or call.get("name") or "tool")
        call_id = str(call.get("id") or make_id("toolu"))
        tool_names[key] = name
        tool_ids[key] = call_id
        saw_tool_use = True
        events.append(
            _event(
                "content_block_start",
                {
                    "index": block_index,
                    "content_block": {
                        "type": "tool_use",
                        "id": call_id,
                        "name": name,
                        "input": {},
                    },
                },
            )
        )
        return events

    def close_blocks() -> list[str]:
        events: list[str] = []
        for index in (
            [thinking_index] if thinking_index is not None else []
        ) + ([text_index] if text_index is not None else []) + list(tool_indexes.values()):
            events.append(_event("content_block_stop", {"index": index}))
        return events

    def handle_chunk(chunk: Mapping[str, Any]) -> list[str]:
        nonlocal finish_reason, saw_finish_reason
        events: list[str] = []
        if isinstance(chunk.get("usage"), Mapping):
            usage_values.update(chunk["usage"])
        for key in ("reasoning_content", "reasoning", "thinking"):
            value = chunk.get(key)
            if isinstance(value, str) and value:
                events.extend(start_thinking())
                events.append(
                    _event(
                        "content_block_delta",
                        {
                            "index": thinking_index,
                            "delta": {"type": "thinking_delta", "thinking": value},
                        },
                    )
                )
                break
        choices = chunk.get("choices")
        if not isinstance(choices, list):
            return events
        for choice in choices:
            if not isinstance(choice, Mapping):
                continue
            if choice.get("finish_reason") not in (None, ""):
                finish_reason = str(choice.get("finish_reason"))
                saw_finish_reason = True
            delta = choice.get("delta")
            delta = delta if isinstance(delta, Mapping) else {}
            reasoning = delta.get("reasoning_content")
            if isinstance(reasoning, str) and reasoning:
                events.extend(start_thinking())
                events.append(
                    _event(
                        "content_block_delta",
                        {
                            "index": thinking_index,
                            "delta": {"type": "thinking_delta", "thinking": reasoning},
                        },
                    )
                )
            content = delta.get("content")
            if isinstance(content, str) and content:
                events.extend(start_text())
                events.append(
                    _event(
                        "content_block_delta",
                        {
                            "index": text_index,
                            "delta": {"type": "text_delta", "text": content},
                        },
                    )
                )
            calls = delta.get("tool_calls")
            if isinstance(calls, list):
                for call in calls:
                    if not isinstance(call, Mapping):
                        continue
                    events.extend(start_tool(call))
                    raw_index = call.get("index")
                    try:
                        key = int(raw_index)
                    except (TypeError, ValueError):
                        key = 0
                    function = call.get("function")
                    function = function if isinstance(function, Mapping) else {}
                    arguments = function.get("arguments")
                    if isinstance(arguments, str) and arguments:
                        events.append(
                            _event(
                                "content_block_delta",
                                {
                                    "index": tool_indexes.get(key, 0),
                                    "delta": {
                                        "type": "input_json_delta",
                                        "partial_json": arguments,
                                    },
                                },
                            )
                        )
        return events

    def terminal_events() -> list[str]:
        """Build the Anthropic terminal envelope exactly once."""

        events = close_blocks()
        if not started:
            events.extend(open_message())
            events.extend(close_blocks())
        events.append(
            _event(
                "message_delta",
                {
                    "delta": {
                        "stop_reason": _stop_reason(
                            finish_reason, has_tool_use=saw_tool_use
                        ),
                        "stop_sequence": None,
                    },
                    "usage": _usage(
                        usage_values, input_fallback=request_input_tokens
                    ),
                },
            )
        )
        events.append(_event("message_stop", {}))
        return events

    # Emit the envelope before the first upstream token, but only after all
    # local event helpers have been defined (nested defs are runtime bindings).
    for event in open_message():
        yield event
    yield ping_event()

    async for raw in body_iterator:
        if isinstance(raw, (bytes, bytearray)):
            text = bytes(raw).decode("utf-8", errors="replace")
        else:
            text = str(raw or "")
        if not text:
            continue
        buffer += text.replace("\r\n", "\n")
        while "\n\n" in buffer:
            block, buffer = buffer.split("\n\n", 1)
            if block.lstrip().startswith(":"):
                # Claude Code treats a missing ``message_start`` as a stalled
                # request and aborts after ~60s, so a keepalive that arrives
                # before the first token must still open the envelope.  ZedeX
                # sends ``message_start`` early for exactly this reason.
                for event in open_message():
                    yield event
                yield ping_event()
                continue
            data_lines = [
                line[5:].lstrip()
                for line in block.split("\n")
                if line.startswith("data:")
            ]
            if not data_lines:
                continue
            payload = "\n".join(data_lines).strip()
            if payload == "[DONE]":
                for event in terminal_events():
                    yield event
                return
            try:
                parsed = json.loads(payload)
            except json.JSONDecodeError:
                continue
            if not isinstance(parsed, Mapping):
                continue
            error = parsed.get("error")
            if isinstance(error, Mapping) and error:
                yield error_event(str(error.get("message") or "upstream error"))
                return
            for event in handle_chunk(parsed):
                yield event

    # A few proxies close an SSE response after writing the final ``data:``
    # line without the usual blank-line delimiter.  Process that buffered
    # block before deciding whether the stream ended cleanly; otherwise the
    # final text/finish reason would be silently discarded.
    if buffer.strip():
        block = buffer.strip()
        if not block.startswith(":"):
            data_lines = [
                line[5:].lstrip()
                for line in block.split("\n")
                if line.startswith("data:")
            ]
            payload = "\n".join(data_lines).strip()
            if payload == "[DONE]":
                for event in terminal_events():
                    yield event
                return
            if payload:
                try:
                    parsed = json.loads(payload)
                except json.JSONDecodeError:
                    parsed = None
                if isinstance(parsed, Mapping):
                    error = parsed.get("error")
                    if isinstance(error, Mapping) and error:
                        yield error_event(
                            str(error.get("message") or "upstream error")
                        )
                        return
                    for event in handle_chunk(parsed):
                        yield event

    # A final finish-reason frame is sufficient evidence of completion if a
    # proxy drops only the trailing [DONE]. Without it, report a broken stream
    # instead of presenting a partial response as a successful end_turn.
    for event in close_blocks():
        yield event
    if not started:
        for event in open_message():
            yield event
    if not saw_finish_reason:
        yield error_event("Upstream stream ended before completion")
        return
    yield _event(
        "message_delta",
        {
            "delta": {
                "stop_reason": _stop_reason(finish_reason, has_tool_use=saw_tool_use),
                "stop_sequence": None,
            },
            "usage": _usage(usage_values, input_fallback=request_input_tokens),
        },
    )
    yield _event("message_stop", {})


def estimate_input_tokens(
    messages: list[Mapping[str, Any]],
    system: Any = "",
    tools: Any = None,
) -> int:
    """Estimate input tokens without counting image base64 as text."""

    from .sse import estimate_tokens

    has_system_message = any(
        isinstance(message, Mapping) and message.get("role") == "system"
        for message in messages
    )
    total = 0
    if system and not has_system_message:
        total += estimate_tokens(_text_from_blocks(system))
    for message in messages:
        if not isinstance(message, Mapping):
            continue
        content = message.get("content")
        if isinstance(content, str):
            total += estimate_tokens(content)
        elif isinstance(content, list):
            for block in content:
                if isinstance(block, Mapping):
                    block_type = str(block.get("type") or "").strip().lower()
                    if block_type in ("text", "input_text"):
                        total += estimate_tokens(str(block.get("text") or ""))
                    elif block_type in ("image", "image_url", "input_image"):
                        # Anthropic image token use depends on dimensions and
                        # model-specific tiling; use a bounded estimate rather
                        # than charging the entire base64 payload as text.
                        total += 1000
                    else:
                        total += estimate_tokens(_json_text(block))
                elif isinstance(block, str):
                    total += estimate_tokens(block)
        elif content not in (None, ""):
            total += estimate_tokens(_json_text(content))
        tool_calls = message.get("tool_calls")
        if tool_calls:
            total += estimate_tokens(_json_text(tool_calls))
    if tools:
        total += estimate_tokens(_json_text(tools))
    return total


_LEVEL_RE = re.compile(r"^(light|high|extra_high)$")


def requested_effort(body: Mapping[str, Any]) -> str:
    """Native reasoning level implied by an Anthropic thinking budget."""

    thinking = body.get("thinking")
    if not isinstance(thinking, Mapping):
        return ""
    budget = thinking.get("budget_tokens")
    level = _budget_effort(budget)
    return level if _LEVEL_RE.match(level or "") else ""


__all__ = [
    "AnthropicRequestError",
    "normalize_request",
    "completion_to_message",
    "translate_chat_stream",
    "estimate_input_tokens",
    "error_event",
    "ping_event",
    "requested_effort",
    "normalize_level",
]
