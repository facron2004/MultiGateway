"""OpenAI chat.completions <-> xAI Responses API translation for the Grok provider.

The Grok OAuth grant is only usable on ``POST {base}/responses``: xAI marks
``/v1/chat/completions`` deprecated and the Grok CLI proxy does not expose it at
all (docs/research/grok.md §3.1, §6). The gateway's unified surface is OpenAI
chat, so every request is translated *up* into a Responses body and the Responses
SSE event stream is translated *back* into ``chat.completion.chunk`` frames.

Both directions are pure functions over dicts (no sockets, no provider state), so
the unit tests exercise every branch without starting an upstream server.
"""
import json
import time
import uuid

from ..transport import ProviderError

# The CLI proxy only streams visible reasoning when encrypted reasoning content is
# requested, and it expects ``store=false`` unless a client pins a response id.
REASONING_INCLUDE = "reasoning.encrypted_content"
# The CLI proxy rejects requests carrying more than 200 tools (HTTP 400).
MAX_TOOLS = 200
# The CLI proxy accepts exactly these effort levels; ``xhigh`` and friends are
# stripped instead of being forwarded as an invalid enum value.
REASONING_EFFORTS = ("low", "medium", "high")
# Parameters that only exist in Chat Completions are dropped here: the Responses
# surface has no equivalent, and forwarding ``presence_penalty``,
# ``frequency_penalty``, ``logprobs``, ``top_logprobs``, ``stop``, ``seed`` or
# ``stream_options`` makes the CLI proxy answer 400 (research §3.4).


def _text_of(content, what="消息内容"):
    """Flatten an OpenAI text/parts content value into a plain string."""
    if content is None:
        return ""
    if isinstance(content, str):
        return content
    if isinstance(content, (int, float, bool)):
        return str(content)
    if isinstance(content, list):
        parts = []
        for part in content:
            if isinstance(part, str):
                parts.append(part)
            elif isinstance(part, dict) and "text" in part:
                parts.append(str(part.get("text") or ""))
            else:
                raise ProviderError("%s仅支持文本内容块" % what, 400)
        return "".join(parts)
    raise ProviderError("%s必须为字符串或内容块数组" % what, 400)


def _input_image(part):
    url = part.get("image_url")
    if isinstance(url, dict):
        url = url.get("url")
    url = url or part.get("url")
    if not isinstance(url, str) or not url:
        raise ProviderError("图片内容块缺少 URL", 400)
    return {"type": "input_image", "image_url": url}


def _user_content(content):
    """Translate OpenAI user content into a Responses input value."""
    if content is None:
        return ""
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        raise ProviderError("用户消息内容必须为字符串或内容块数组", 400)
    parts = []
    for part in content:
        if not isinstance(part, dict):
            raise ProviderError("用户消息内容块必须为对象", 400)
        kind = part.get("type")
        if kind in ("text", "input_text"):
            parts.append({"type": "input_text", "text": str(part.get("text") or "")})
        elif kind in ("image_url", "input_image"):
            parts.append(_input_image(part))
        else:
            raise ProviderError("暂不支持的用户消息内容块: " + str(kind), 400)
    if parts and all(part["type"] == "input_text" for part in parts):
        return "".join(part["text"] for part in parts)
    return parts


def _function_call_items(message):
    calls = []
    for call in message.get("tool_calls") or []:
        function = call.get("function") or {}
        name = function.get("name")
        if not name:
            raise ProviderError("工具调用缺少函数名称", 400)
        arguments = function.get("arguments")
        if arguments is None:
            arguments = ""
        elif not isinstance(arguments, str):
            arguments = json.dumps(arguments, ensure_ascii=False)
        calls.append({"type": "function_call",
                      "call_id": call.get("id") or "call_" + uuid.uuid4().hex,
                      "name": str(name), "arguments": arguments})
    return calls


def _input_items(messages):
    """Translate OpenAI chat messages into a Responses ``input`` array."""
    items = []
    for message in messages:
        if not isinstance(message, dict):
            raise ProviderError("messages 中包含无效条目", 400)
        role = message.get("role")
        content = message.get("content")
        if role in ("system", "developer"):
            text = _text_of(content, "system 消息内容")
            if text:
                items.append({"role": "system", "content": text})
            continue
        if role == "user":
            items.append({"role": "user", "content": _user_content(content)})
            continue
        if role == "assistant":
            text = _text_of(content, "assistant 消息内容")
            if text:
                items.append({"role": "assistant", "content": text})
            items.extend(_function_call_items(message))
            if not text and not message.get("tool_calls"):
                raise ProviderError("assistant 消息内容为空", 400)
            continue
        if role == "tool":
            call_id = message.get("tool_call_id")
            if not call_id:
                raise ProviderError("工具结果缺少 tool_call_id", 400)
            items.append({"type": "function_call_output", "call_id": str(call_id),
                          "output": _text_of(content, "工具返回内容")})
            continue
        raise ProviderError("暂不支持的消息角色: " + str(role), 400)
    return items


def _tools(tools):
    if tools is None:
        return []
    if not isinstance(tools, list):
        raise ProviderError("tools 必须为数组", 400)
    rows = []
    for tool in tools[:MAX_TOOLS]:
        if not isinstance(tool, dict):
            raise ProviderError("tools 中包含无效条目", 400)
        # Chat Completions only standardises function tools; server-side tools
        # (web_search, x_search, …) never appear in this payload shape.
        if tool.get("type") not in (None, "function"):
            continue
        function = tool.get("function") or tool
        name = function.get("name")
        if not name:
            raise ProviderError("工具缺少名称", 400)
        entry = {"type": "function", "name": str(name),
                 "parameters": function.get("parameters") or {"type": "object", "properties": {}}}
        if function.get("description") is not None:
            entry["description"] = function["description"]
        if "strict" in function:
            entry["strict"] = bool(function["strict"])
        rows.append(entry)
    return rows


def _tool_choice(choice):
    if choice is None:
        return None
    if isinstance(choice, str):
        return choice if choice in ("auto", "none", "required") else None
    if isinstance(choice, dict):
        function = choice.get("function") or {}
        name = function.get("name") or choice.get("name")
        return {"type": "function", "name": str(name)} if name else None
    return None


def _text_format(response_format):
    if not isinstance(response_format, dict):
        return None
    kind = response_format.get("type")
    if kind == "json_object":
        return {"type": "json_object"}
    if kind == "json_schema":
        schema = response_format.get("json_schema") or {}
        return {"type": "json_schema", "name": str(schema.get("name") or "output"),
                "schema": schema.get("schema") or {"type": "object"},
                **({"strict": bool(schema["strict"])} if "strict" in schema else {})}
    return None


def _reasoning(payload):
    reasoning = payload.get("reasoning")
    effort = reasoning.get("effort") if isinstance(reasoning, dict) else None
    effort = effort or payload.get("reasoning_effort")
    return {"effort": effort} if effort in REASONING_EFFORTS else None


def chat_to_responses(payload):
    """Build a Responses request body from an OpenAI chat.completions payload."""
    if not isinstance(payload, dict):
        raise ProviderError("请求体必须为 JSON 对象", 400)
    model = str(payload.get("model") or "").strip()
    if not model:
        raise ProviderError("缺少 model 字段", 400)
    messages = payload.get("messages")
    if not isinstance(messages, list) or not messages:
        raise ProviderError("messages 必须为非空数组", 400)
    body = {"model": model, "input": _input_items(messages), "stream": True,
            "store": False, "include": [REASONING_INCLUDE]}
    maximum = payload.get("max_completion_tokens") or payload.get("max_tokens")
    if maximum:
        try:
            body["max_output_tokens"] = int(maximum)
        except (TypeError, ValueError) as exc:
            raise ProviderError("max_tokens 必须为整数", 400) from exc
    for key in ("temperature", "top_p"):
        if payload.get(key) is not None:
            body[key] = payload[key]
    tools = _tools(payload.get("tools"))
    if tools:
        body["tools"] = tools
        choice = _tool_choice(payload.get("tool_choice"))
        if choice is not None:
            body["tool_choice"] = choice
        if payload.get("parallel_tool_calls") is not None:
            body["parallel_tool_calls"] = bool(payload["parallel_tool_calls"])
    reasoning = _reasoning(payload)
    if reasoning:
        body["reasoning"] = reasoning
    fmt = _text_format(payload.get("response_format"))
    if fmt:
        body["text"] = {"format": fmt}
    return body


def map_usage(usage):
    """Map Responses token accounting onto the OpenAI ``usage`` object."""
    if not isinstance(usage, dict):
        return None
    prompt = usage.get("input_tokens")
    completion = usage.get("output_tokens")
    if prompt is None and completion is None:
        return None
    prompt = int(prompt or 0)
    completion = int(completion or 0)
    total = usage.get("total_tokens")
    total = total if isinstance(total, (int, float)) else prompt + completion
    result = {"prompt_tokens": prompt, "completion_tokens": completion,
              "total_tokens": int(total)}
    details = usage.get("input_tokens_details")
    cached = details.get("cached_tokens") if isinstance(details, dict) else None
    if cached is None:
        cached = usage.get("cached_tokens")
    result["prompt_tokens_details"] = {"cached_tokens": int(cached or 0)}
    output_details = usage.get("output_tokens_details")
    reasoning = output_details.get("reasoning_tokens") if isinstance(output_details, dict) else None
    if reasoning:
        result["completion_tokens_details"] = {"reasoning_tokens": int(reasoning)}
    return result


def _index_of(event):
    index = event.get("output_index")
    return index if isinstance(index, int) and index >= 0 else 0


def _output_text(item):
    content = item.get("content")
    if isinstance(content, str):
        return content
    parts = []
    for part in content or []:
        if isinstance(part, dict) and part.get("type") in ("output_text", "text"):
            parts.append(str(part.get("text") or ""))
    return "".join(parts)


def _summary_text(item):
    summary = item.get("summary")
    if isinstance(summary, str):
        return summary
    parts = []
    for part in summary or []:
        if isinstance(part, dict) and part.get("type") in ("summary_text", "text"):
            parts.append(str(part.get("text") or ""))
    if parts:
        return "".join(parts)
    content = item.get("content")
    if isinstance(content, str):
        return content
    for part in content or []:
        if isinstance(part, dict) and part.get("type") in ("reasoning_text", "output_text", "text"):
            parts.append(str(part.get("text") or ""))
    return "".join(parts)


def _upstream_error(payload):
    if isinstance(payload, dict):
        message = payload.get("message") or payload.get("error") or payload.get("code")
        code = payload.get("code") or payload.get("type") or "provider_error"
    else:
        message, code = payload, "provider_error"
    text = str(message or "").strip() or "未知错误"
    if not text.startswith("Grok"):
        text = "Grok 上游返回错误：" + text
    return ProviderError(text, 502, str(code))


class _ChatStream:
    """Fold Responses SSE events into OpenAI chat chunk dicts."""

    def __init__(self, model):
        self.model = model or ""
        self.id = "chatcmpl-" + uuid.uuid4().hex
        self.created = int(time.time())
        self.started = False
        self.tool_index = {}
        self.tool_announced = set()
        self.tool_named = set()
        self.next_tool = 0
        self.text_seen = set()
        self.reasoning_seen = set()
        self.args_seen = set()
        self.tools_seen = False

    # ------------------------------------------------------------------ helpers
    def begin(self, response):
        if not isinstance(response, dict):
            return
        if response.get("id"):
            self.id = str(response["id"])
        if response.get("model"):
            self.model = str(response["model"])
        try:
            created = int(response.get("created_at") or 0)
        except (TypeError, ValueError):
            created = 0
        if created:
            self.created = created

    def _frame(self, delta=None, finish=None):
        return {"id": self.id, "object": "chat.completion.chunk", "created": self.created,
                "model": self.model,
                "choices": [{"index": 0, "delta": delta or {}, "finish_reason": finish}]}

    def _usage_frame(self, usage):
        return {"id": self.id, "object": "chat.completion.chunk", "created": self.created,
                "model": self.model, "choices": [], "usage": usage}

    def _start(self):
        if self.started:
            return []
        self.started = True
        return [self._frame({"role": "assistant", "content": ""})]

    def _tool(self, index):
        tool = self.tool_index.get(index)
        if tool is None:
            tool = self.next_tool
            self.next_tool += 1
            self.tool_index[index] = tool
        return tool

    # ------------------------------------------------------------------- events
    def response_created(self, response):
        self.begin(response)
        return self._start()

    def text_delta(self, index, text):
        self.text_seen.add(index)
        return self._start() + [self._frame({"content": text})]

    def reasoning_delta(self, index, text):
        self.reasoning_seen.add(index)
        return self._start() + [self._frame({"reasoning_content": text})]

    def tool_started(self, index, call_id, name):
        """Announce a function call; later calls only fill in a missing name.

        Event ordering is not guaranteed beyond a monotonic sequence number, so an
        arguments delta may arrive before the call's name. A second announcement
        with the missing name keeps the tool call usable for strict clients.
        """
        self.tools_seen = True
        name = str(name or "")
        tool = self.tool_index.get(index)
        if tool is None:
            tool = self._tool(index)
            self.tool_announced.add(index)
            if name:
                self.tool_named.add(index)
            return self._start() + [self._frame({"tool_calls": [{
                "index": tool, "id": str(call_id or "call_" + uuid.uuid4().hex),
                "type": "function", "function": {"name": name, "arguments": ""}}]})]
        if name and index not in self.tool_named:
            self.tool_named.add(index)
            call = {"index": tool, "type": "function", "function": {"name": name}}
            if call_id:
                call["id"] = str(call_id)
            return self._start() + [self._frame({"tool_calls": [call]})]
        return []

    def tool_arguments(self, index, arguments):
        if not arguments:
            return []
        frames = []
        if index not in self.tool_announced:
            frames += self.tool_started(index, "", "")
        self.args_seen.add(index)
        target = self.tool_index[index]
        frames += [self._frame({"tool_calls": [
            {"index": target, "function": {"arguments": arguments}}]})]
        return frames

    def finish(self, response):
        frames = self._start()
        frames.append(self._frame({}, self.finish_reason(response)))
        usage = map_usage(response.get("usage"))
        if usage:
            frames.append(self._usage_frame(usage))
        return frames

    def finish_reason(self, response):
        if self.tools_seen:
            return "tool_calls"
        details = response.get("incomplete_details")
        if response.get("status") == "incomplete" and isinstance(details, dict):
            if details.get("reason") in ("max_output_tokens", "max_tokens"):
                return "length"
        return "stop"

    def rebuild_output(self, output):
        """Replay ``response.output`` items when no incremental event carried them."""
        frames = []
        for index, item in enumerate(output or []):
            if not isinstance(item, dict):
                continue
            kind = item.get("type")
            if kind == "message" and index not in self.text_seen:
                text = _output_text(item)
                if text:
                    frames += self.text_delta(index, text)
            elif kind == "reasoning" and index not in self.reasoning_seen:
                text = _summary_text(item)
                if text:
                    frames += self.reasoning_delta(index, text)
            elif kind == "function_call":
                frames += self.tool_started(index, item.get("call_id") or item.get("id"),
                                            item.get("name"))
                if index not in self.args_seen and item.get("arguments"):
                    frames += self.tool_arguments(index, item["arguments"])
        return frames


def responses_frames(events, model=""):
    """Yield OpenAI ``chat.completion.chunk`` dicts for a Responses event stream."""
    stream = _ChatStream(model)
    completed = False
    for event in events:
        if not isinstance(event, dict):
            continue
        kind = event.get("type") or ""
        if kind == "response.created":
            yield from stream.response_created(event.get("response") or {})
        elif kind == "response.output_text.delta":
            text = event.get("delta") or ""
            if text:
                yield from stream.text_delta(_index_of(event), str(text))
        elif kind in ("response.reasoning_summary_text.delta", "response.reasoning_text.delta"):
            text = event.get("delta") or ""
            if text:
                yield from stream.reasoning_delta(_index_of(event), str(text))
        elif kind == "response.output_item.added":
            item = event.get("item") or {}
            if item.get("type") == "function_call":
                yield from stream.tool_started(_index_of(event),
                                               item.get("call_id") or item.get("id"),
                                               item.get("name"))
        elif kind == "response.function_call_arguments.delta":
            arguments = event.get("delta") or ""
            if arguments:
                yield from stream.tool_arguments(_index_of(event), str(arguments))
        elif kind == "response.function_call_arguments.done":
            index = _index_of(event)
            arguments = event.get("arguments") or ""
            if arguments and index not in stream.args_seen:
                yield from stream.tool_arguments(index, str(arguments))
        elif kind == "response.output_item.done":
            item = event.get("item") or {}
            index = _index_of(event)
            if item.get("type") == "message" and index not in stream.text_seen:
                text = _output_text(item)
                if text:
                    yield from stream.text_delta(index, text)
            elif item.get("type") == "reasoning" and index not in stream.reasoning_seen:
                text = _summary_text(item)
                if text:
                    yield from stream.reasoning_delta(index, text)
            elif item.get("type") == "function_call":
                yield from stream.tool_started(index, item.get("call_id") or item.get("id"),
                                               item.get("name"))
                if index not in stream.args_seen and item.get("arguments"):
                    yield from stream.tool_arguments(index, item["arguments"])
        elif kind in ("response.completed", "response.incomplete"):
            response = event.get("response") or {}
            stream.begin(response)
            frames = []
            # The CLI proxy can send ``response.completed`` with an empty ``output``;
            # in that case the items replayed from ``output_item.done`` are the only
            # content. If neither event carried anything, replay ``response.output``.
            if not (stream.text_seen or stream.reasoning_seen or stream.tool_index):
                frames = stream.rebuild_output(response.get("output"))
            frames += stream.finish(response)
            yield from frames
            completed = True
            return
        elif kind in ("error", "response.failed"):
            payload = event.get("error") or (event.get("response") or {}).get("error") or event
            raise _upstream_error(payload)
    if not completed:
        raise ProviderError("Grok 上游响应意外中断（未收到 response.completed）", 502,
                            "upstream_truncated")
