"""Protocol translation between the gateway's two upstream dialects.

Every native provider speaks one of:

* OpenAI ``chat.completions`` (Grok, Copilot, Qwen, iFlow, …)
* Anthropic ``messages`` (Claude Code, MiniMax)

The gateway's unified surface is OpenAI chat + OpenAI Responses + Anthropic
Messages. Chat and Responses are produced by :mod:`gateway.handler` from OpenAI
stream frames, so a provider only has to expose OpenAI frames for ``chat``.
This module translates an Anthropic request into an OpenAI request (for
``/v1/messages``) and OpenAI stream frames back into Anthropic SSE events.
"""
import json

from ..transport import ProviderError

STOP_REASONS = {"stop": "end_turn", "length": "max_tokens", "tool_calls": "tool_use",
                "function_call": "tool_use", "content_filter": "end_turn"}


def anthropic_event(kind, data):
    return ("event: " + kind + "\ndata: " + json.dumps(data, ensure_ascii=False) + "\n\n").encode("utf-8")


def anthropic_usage(usage):
    """Map an OpenAI ``usage`` object onto Anthropic's token accounting."""
    usage = usage or {}
    cached = 0
    details = usage.get("prompt_tokens_details") or usage.get("input_tokens_details") or {}
    if isinstance(details, dict):
        cached = int(details.get("cached_tokens") or details.get("cache_read") or 0)
    prompt = int(usage.get("prompt_tokens") or usage.get("input_tokens") or 0)
    output = int(usage.get("completion_tokens") or usage.get("output_tokens") or 0)
    result = {"input_tokens": max(prompt - cached, 0), "output_tokens": output}
    if cached:
        result["cache_read_input_tokens"] = cached
    return result


def _blocks(content):
    if isinstance(content, str):
        return [{"type": "text", "text": content}] if content else []
    if content is None:
        return []
    if not isinstance(content, list) or not all(isinstance(part, dict) for part in content):
        raise ProviderError("message content must be text or an array of content blocks", 400)
    return content


def _image_part(block):
    source = block.get("source") or {}
    if source.get("type") == "base64":
        media = source.get("media_type") or "image/png"
        return {"type": "image_url", "image_url": {
            "url": "data:" + media + ";base64," + str(source.get("data") or "")}}
    if source.get("type") == "url" and source.get("url"):
        return {"type": "image_url", "image_url": {"url": source["url"]}}
    raise ProviderError("经网关转发时暂不支持该图片格式", 400)


def _text_of(content):
    if isinstance(content, str):
        return content
    return "".join(block.get("text", "") for block in _blocks(content)
                   if block.get("type") == "text")


def _tool_result_text(block):
    content = block.get("content")
    if isinstance(content, str):
        return content
    parts = []
    for part in _blocks(content):
        if part.get("type") == "text":
            parts.append(part.get("text", ""))
        else:
            raise ProviderError("工具返回内容仅支持文本，暂不支持图片", 400)
    return "".join(parts)


def anthropic_to_chat(payload):
    """Translate an Anthropic Messages request into an OpenAI chat request."""
    messages = []
    if payload.get("system"):
        messages.append({"role": "system", "content": _text_of(payload["system"])})
    incoming = payload.get("messages")
    if not isinstance(incoming, list) or not incoming:
        raise ProviderError("messages 必须为非空数组", 400)
    for message in incoming:
        role = message.get("role")
        content = message.get("content")
        if role == "assistant":
            texts, calls = [], []
            for block in _blocks(content):
                kind = block.get("type")
                if kind == "text":
                    texts.append(block.get("text", ""))
                elif kind == "tool_use":
                    if not block.get("id") or not block.get("name"):
                        raise ProviderError("工具调用缺少 id 或名称", 400)
                    calls.append({"id": block["id"], "type": "function", "function": {
                        "name": block["name"],
                        "arguments": json.dumps(block.get("input") or {}, ensure_ascii=False)}})
                elif kind in ("thinking", "redacted_thinking"):
                    continue
                else:
                    raise ProviderError("暂不支持的内容块类型: " + str(kind), 400)
            entry = {"role": "assistant"}
            joined = "".join(texts)
            if joined:
                entry["content"] = joined
            if calls:
                entry["tool_calls"] = calls
            if "content" not in entry and not calls:
                continue
            messages.append(entry)
            continue
        if role != "user":
            raise ProviderError("暂不支持的消息角色: " + str(role), 400)
        pending = []
        for block in _blocks(content):
            kind = block.get("type")
            if kind == "text":
                pending.append({"type": "text", "text": block.get("text", "")})
            elif kind == "image":
                pending.append(_image_part(block))
            elif kind == "tool_result":
                if pending:
                    messages.append({"role": "user", "content": _merge(pending)})
                    pending = []
                if not block.get("tool_use_id"):
                    raise ProviderError("工具结果缺少 tool_use_id", 400)
                messages.append({"role": "tool", "tool_call_id": block["tool_use_id"],
                                 "content": _tool_result_text(block)})
            elif kind in ("thinking", "redacted_thinking"):
                continue
            else:
                raise ProviderError("暂不支持的内容块类型: " + str(kind), 400)
        if pending:
            messages.append({"role": "user", "content": _merge(pending)})
    result = {"model": payload["model"], "messages": messages, "stream": True}
    maximum = payload.get("max_tokens") or payload.get("max_completion_tokens")
    if maximum:
        result["max_tokens"] = maximum
    for key in ("temperature", "top_p", "presence_penalty", "frequency_penalty"):
        if payload.get(key) is not None:
            result[key] = payload[key]
    if payload.get("stop_sequences"):
        stop = payload["stop_sequences"]
        result["stop"] = stop[0] if isinstance(stop, list) and len(stop) == 1 else stop
    tools = []
    for tool in payload.get("tools") or []:
        function = tool.get("function") or tool
        if not function.get("name"):
            raise ProviderError("工具缺少名称", 400)
        tools.append({"type": "function", "function": {
            "name": function["name"],
            "description": function.get("description", ""),
            "parameters": function.get("parameters") or function.get("input_schema")
            or {"type": "object", "properties": {}}}})
    if tools:
        result["tools"] = tools
        choice = payload.get("tool_choice")
        if isinstance(choice, dict):
            kind = choice.get("type")
            name = (choice.get("function") or {}).get("name") or choice.get("name")
            if kind == "tool" and name:
                result["tool_choice"] = {"type": "function", "function": {"name": name}}
            elif kind == "any":
                result["tool_choice"] = "required"
            elif kind == "none":
                result["tool_choice"] = "none"
            else:
                result["tool_choice"] = "auto"
            if choice.get("disable_parallel_tool_use"):
                result["parallel_tool_calls"] = False
        elif choice in ("any", "required"):
            result["tool_choice"] = "required"
        elif choice in ("auto", "none"):
            result["tool_choice"] = choice
    elif payload.get("tool_choice") == "none":
        result["tool_choice"] = "none"
    return result


def _merge(parts):
    if all(part.get("type") == "text" for part in parts):
        return "".join(part.get("text", "") for part in parts)
    return parts


def anthropic_aggregate(frames, model):
    """Fold Anthropic SSE frames into a non-streaming Messages response."""
    message = {"id": "msg_gateway", "type": "message", "role": "assistant", "model": model,
               "content": [], "stop_reason": "end_turn", "stop_sequence": None,
               "usage": {"input_tokens": 0, "output_tokens": 0}}
    blocks = {}
    stop_reason = "end_turn"
    for frame in frames:
        if isinstance(frame, dict):
            event = frame
        else:
            text = (bytes(frame).decode("utf-8", "replace")
                    if isinstance(frame, (bytes, bytearray)) else str(frame))
            data = None
            for line in text.splitlines():
                if line.startswith("data:"):
                    data = line[5:].strip()
            if data is None:
                continue
            try:
                event = json.loads(data)
            except ValueError:
                continue
        if not isinstance(event, dict):
            continue
        kind = event.get("type")
        if kind == "message_start":
            inner = event.get("message") or {}
            message["id"] = inner.get("id") or message["id"]
            message["usage"].update(inner.get("usage") or {})
        elif kind == "content_block_start":
            index = event.get("index", len(blocks))
            blocks[index] = dict(event.get("content_block") or {})
            if blocks[index].get("type") == "tool_use":
                blocks[index]["_json"] = ""
        elif kind == "content_block_delta":
            index = event.get("index", 0)
            block = blocks.setdefault(index, {"type": "text", "text": ""})
            delta = event.get("delta") or {}
            if delta.get("type") == "text_delta":
                block["text"] = (block.get("text") or "") + delta.get("text", "")
            elif delta.get("type") == "thinking_delta":
                block["thinking"] = (block.get("thinking") or "") + delta.get("thinking", "")
            elif delta.get("type") == "input_json_delta":
                block["_json"] = (block.get("_json") or "") + delta.get("partial_json", "")
        elif kind == "message_delta":
            message["usage"].update(event.get("usage") or {})
            stop_reason = (event.get("delta") or {}).get("stop_reason") or stop_reason
        elif kind == "message_stop":
            break
    content = []
    for index in sorted(blocks):
        block = blocks[index]
        if block.get("type") == "tool_use":
            raw = block.pop("_json", "")
            if raw:
                try:
                    block["input"] = json.loads(raw)
                except ValueError:
                    block["input"] = {}
            content.append(block)
        elif block.get("type") in ("text", "thinking"):
            block.pop("_json", None)
            content.append(block)
    message["content"] = content or [{"type": "text", "text": ""}]
    message["stop_reason"] = stop_reason
    return message


def _parse_frame(frame):
    if isinstance(frame, dict):
        return frame
    if isinstance(frame, str):
        frame = frame.encode("utf-8")
    if not isinstance(frame, (bytes, bytearray)):
        return None
    text = bytes(frame).decode("utf-8", "replace").strip()
    if not text.startswith("data:"):
        return None
    data = text[5:].strip()
    if data in ("[DONE]", ""):
        return None
    try:
        return json.loads(data)
    except ValueError:
        return None


def chat_frames_to_anthropic(frames, model, message_id=None, input_usage=None):
    """Convert OpenAI chat stream frames into Anthropic SSE events.

    The generator is lazy so a client disconnect stops reading upstream.
    """
    identifier = message_id or "msg_gateway"
    started = False
    active = None
    next_index = 0
    tools = {}
    usage = dict(input_usage or {})
    stop_reason = "end_turn"

    def start_message(usage_hint):
        nonlocal started, identifier
        if started:
            return None
        started = True
        return anthropic_event("message_start", {"type": "message_start", "message": {
            "id": identifier, "type": "message", "role": "assistant", "model": model,
            "content": [], "stop_reason": None, "stop_sequence": None,
            "usage": {"input_tokens": usage_hint.get("input_tokens", 0), "output_tokens": 0,
                      **({"cache_read_input_tokens": usage_hint["cache_read_input_tokens"]}
                         if usage_hint.get("cache_read_input_tokens") else {})}}})

    def close_active():
        nonlocal active
        if active is None:
            return None
        frame = anthropic_event("content_block_stop", {"type": "content_block_stop",
                                                      "index": active["index"]})
        active = None
        return frame

    def open_block(kind, **fields):
        nonlocal active, next_index
        active = {"type": kind, "index": next_index, **fields}
        next_index += 1
        if kind == "tool":
            block = {"type": "tool_use", "id": fields.get("id") or "call_gateway",
                     "name": fields.get("name") or "tool", "input": {}}
        elif kind == "thinking":
            block = {"type": "thinking", "thinking": "", "signature": ""}
        else:
            block = {"type": kind, "text": ""}
        return anthropic_event("content_block_start", {"type": "content_block_start",
                                                       "index": active["index"],
                                                       "content_block": block})

    for frame in frames:
        chunk = _parse_frame(frame)
        if not chunk:
            continue
        if chunk.get("error"):
            error = chunk["error"]
            message = error.get("message") if isinstance(error, dict) else str(error)
            raise ProviderError(message or "上游返回错误")
        if chunk.get("id"):
            identifier = chunk["id"] if str(chunk["id"]).startswith("msg_") else "msg_" + str(chunk["id"])
        usage_hint = anthropic_usage(chunk.get("usage"))
        if usage_hint.get("input_tokens"):
            usage.update(usage_hint)
        head = start_message(usage)
        if head:
            yield head
        choices = chunk.get("choices") or []
        if not choices:
            continue
        choice = choices[0] or {}
        delta = choice.get("delta") or {}
        reasoning = delta.get("reasoning_content") or delta.get("reasoning")
        if reasoning:
            if not active or active["type"] != "thinking":
                close = close_active()
                if close:
                    yield close
                yield open_block("thinking")
            yield anthropic_event("content_block_delta", {
                "type": "content_block_delta", "index": active["index"],
                "delta": {"type": "thinking_delta", "thinking": reasoning}})
        text = delta.get("content")
        if text:
            if not active or active["type"] != "text":
                close = close_active()
                if close:
                    yield close
                yield open_block("text")
            yield anthropic_event("content_block_delta", {
                "type": "content_block_delta", "index": active["index"],
                "delta": {"type": "text_delta", "text": text}})
        for call in delta.get("tool_calls") or []:
            key = call.get("index", 0)
            function = call.get("function") or {}
            state = tools.get(key)
            if state is None:
                state = {"id": call.get("id") or "call_%s" % key,
                         "name": function.get("name") or ""}
                tools[key] = state
            elif function.get("name") and not state["name"]:
                state["name"] = function["name"]
            arguments = function.get("arguments") or ""
            if active is None or active["type"] != "tool" or active["tool"] != key:
                close = close_active()
                if close:
                    yield close
                yield open_block("tool", id=state["id"], name=state["name"] or "tool", tool=key)
            if arguments:
                yield anthropic_event("content_block_delta", {
                    "type": "content_block_delta", "index": active["index"],
                    "delta": {"type": "input_json_delta", "partial_json": arguments}})
        finish = choice.get("finish_reason")
        if finish:
            stop_reason = STOP_REASONS.get(finish, "end_turn")
    close = close_active()
    if close:
        yield close
    # Some providers report usage on the last separate chunk (no choices).
    final_usage = {"input_tokens": usage.get("input_tokens", 0),
                   "output_tokens": usage.get("output_tokens", 0)}
    if usage.get("cache_read_input_tokens"):
        final_usage["cache_read_input_tokens"] = usage["cache_read_input_tokens"]
    if not started:
        yield start_message(usage)
    yield anthropic_event("message_delta", {"type": "message_delta",
                                            "delta": {"stop_reason": stop_reason,
                                                      "stop_sequence": None},
                                            "usage": final_usage})
    yield anthropic_event("message_stop", {"type": "message_stop"})
