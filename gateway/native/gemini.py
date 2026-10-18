"""Translation between OpenAI ``chat.completions`` bodies and Gemini requests.

Antigravity's content API is Gemini ``generateContent`` wrapped in a Google
Cloud Code Assist envelope (see :mod:`gateway.native.antigravity`), so the
protocol conversion lives here:

* :func:`build_request` turns an OpenAI chat body into the inner
  ``request`` object (``contents`` / ``systemInstruction`` / ``tools`` /
  ``generationConfig``).
* :func:`usage` turns Gemini ``usageMetadata`` into an OpenAI ``usage`` object.

The rules encoded below come from docs/research/antigravity.md §5 and are there
because each one fixes a real upstream 400:

* ``assistant`` is not a valid role — it must be sent as ``model``.
* A tool-result turn is a **model**-role turn carrying ``functionResponse``
  parts, and consecutive tool results are merged into one turn.
* ``systemInstruction`` must be an object with ``parts``, never a string.
* Tool schemas must be sent as ``parameters`` (not ``parametersJsonSchema``)
  and must not contain ``$ref`` / ``$defs`` / ``const`` / ``default`` /
  ``examples`` / ``$schema`` / ``$id``.
* ``maxOutputTokens`` must be strictly greater than ``thinkingBudget``; the
  budget is raised/lowered together with the output cap so the pair stays valid.
* Thinking blocks from a previous turn are dropped on the way in: replaying
  ``thought`` parts without their ``thoughtSignature`` is rejected with
  "Invalid signature in thinking block" (§7.8).
"""
import json

from ..transport import ProviderError

#: Gemini/Vertex finish reasons mapped onto the OpenAI vocabulary.
FINISH_REASONS = {
    "STOP": "stop",
    "MAX_TOKENS": "length",
    "SAFETY": "content_filter",
    "RECITATION": "content_filter",
    "BLOCKLIST": "content_filter",
    "PROHIBITED_CONTENT": "content_filter",
    "SPII": "content_filter",
    "IMAGE_SAFETY": "content_filter",
    "MALFORMED_FUNCTION_CALL": "tool_calls",
    "UNEXPECTED_TOOL_CALL": "tool_calls",
    "OTHER": "stop",
}

#: Output ceilings the official client uses; the server rejects more (§5.6).
MAX_OUTPUT_TOKENS = 65536
CLAUDE_MAX_OUTPUT_TOKENS = 64000

#: Room kept between ``thinkingBudget`` and ``maxOutputTokens`` when the caller's
#: ``max_tokens`` is smaller than the thinking budget it asked for.
OUTPUT_HEADROOM = 1024

#: ``reasoning_effort`` labels -> thinking token budget.
REASONING_BUDGETS = {"minimal": 512, "low": 2048, "medium": 8192, "high": 16384}

#: Roles that become ``systemInstruction`` rather than a content turn.
SYSTEM_ROLES = ("system", "developer")

#: JSON Schema keywords the Gemini schema subset rejects; ``const`` is rewritten
#: to ``enum`` and ``$ref`` is dropped because it cannot be inlined here.
DROPPED_SCHEMA_KEYS = ("$schema", "$id", "$defs", "definitions", "default", "examples",
                       "$comment", "strict", "$ref")


def build_request(payload):
    """Return the inner ``request`` object for one OpenAI chat body."""
    messages = payload.get("messages")
    if not isinstance(messages, list) or not messages:
        raise ProviderError("messages 必须为非空数组", 400)
    system_instruction, contents = contents_of(messages)
    request = {"contents": contents}
    if system_instruction:
        request["systemInstruction"] = system_instruction
    tools = tools_of(payload.get("tools"))
    if tools:
        request["tools"] = tools
    request["generationConfig"] = generation_config(payload, str(payload.get("model") or ""))
    return request


def contents_of(messages):
    """Return ``(systemInstruction, contents)`` for the message list."""
    system_parts = []
    contents = []
    tool_names = {}
    for message in messages:
        if not isinstance(message, dict):
            continue
        role = message.get("role")
        if role in SYSTEM_ROLES:
            text = text_of(message.get("content"))
            if text:
                system_parts.append({"text": text})
            continue
        if role == "assistant":
            parts = []
            text = text_of(message.get("content"))
            if text:
                parts.append({"text": text})
            for call in message.get("tool_calls") or []:
                if not isinstance(call, dict):
                    continue
                function = call.get("function") if isinstance(call.get("function"), dict) else {}
                name = str(function.get("name") or "")
                if not name:
                    continue
                identifier = str(call.get("id") or "")
                if identifier:
                    tool_names[identifier] = name
                invocation = {"name": name, "args": arguments_of(function.get("arguments"))}
                if identifier:
                    invocation["id"] = identifier
                parts.append({"functionCall": invocation})
            if parts:
                contents.append({"role": "model", "parts": parts})
            continue
        if role == "tool":
            name = str(message.get("name") or tool_names.get(message.get("tool_call_id") or "")
                       or "tool")
            append_tool_result(contents, name, message.get("content"))
            continue
        contents.append({"role": "user", "parts": user_parts(message.get("content"))})
    if not contents:
        raise ProviderError("messages 至少需要一条 user 消息", 400)
    # Gemini expects the conversation to open on a user turn and to end on one;
    # a trailing assistant text turn is a prefill the upstream rejects.
    if contents[0]["role"] != "user":
        contents.insert(0, {"role": "user", "parts": [{"text": ""}]})
    if contents[-1]["role"] == "model" and all("text" in part for part in contents[-1]["parts"]):
        contents.pop()
    if not contents:
        raise ProviderError("messages 至少需要一条 user 消息", 400)
    system_instruction = {"parts": system_parts} if system_parts else None
    return system_instruction, contents


def append_tool_result(contents, name, content):
    """Append one ``functionResponse`` part, merging adjacent tool results."""
    part = {"functionResponse": {"name": name, "response": response_of(content)}}
    if contents and contents[-1]["role"] == "model" and all(
            "functionResponse" in item for item in contents[-1]["parts"]):
        contents[-1]["parts"].append(part)
    else:
        contents.append({"role": "model", "parts": [part]})


def text_of(content):
    """Flatten an OpenAI message content value to plain text."""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(str(part.get("text") or "") for part in content
                       if isinstance(part, dict)
                       and str(part.get("type") or "") in ("text", "input_text"))
    return ""


def arguments_of(value):
    """Return a dict of tool-call arguments from an OpenAI ``arguments`` value."""
    if isinstance(value, dict):
        return value
    if isinstance(value, str) and value.strip():
        try:
            parsed = json.loads(value)
        except ValueError:
            return {"input": value}
        return parsed if isinstance(parsed, dict) else {"input": parsed}
    return {}


def response_of(content):
    """Return the ``functionResponse.response`` struct for a tool result."""
    text = text_of(content)
    stripped = text.strip()
    if stripped[:1] in ("{", "["):
        try:
            value = json.loads(stripped)
        except ValueError:
            value = None
        if isinstance(value, dict):
            return value
    return {"result": text}


def user_parts(content):
    """Return Gemini parts for one user message (text and images)."""
    if isinstance(content, str):
        return [{"text": content}]
    parts = []
    for item in content or []:
        if not isinstance(item, dict):
            continue
        kind = item.get("type")
        if kind in ("text", "input_text"):
            parts.append({"text": str(item.get("text") or "")})
        elif kind in ("image_url", "input_image"):
            image = image_part(item)
            if image:
                parts.append(image)
    return parts or [{"text": ""}]


def image_part(item):
    """Convert one OpenAI image block to ``inlineData`` or ``fileData``."""
    url = item.get("image_url")
    if isinstance(url, dict):
        url = url.get("url")
    if not isinstance(url, str) or not url:
        return None
    if url.startswith("data:"):
        header, _, data = url.partition(",")
        mime = header[5:].split(";")[0] or "image/png"
        return {"inlineData": {"mimeType": mime, "data": data}}
    # Remote URLs cannot be fetched while streaming; pass them through and let
    # the upstream resolve them (best effort, matching the research note).
    return {"fileData": {"fileUri": url}}


def tools_of(tools):
    """Convert OpenAI tool declarations into Gemini ``functionDeclarations``."""
    declarations = []
    for tool in tools or []:
        if not isinstance(tool, dict):
            continue
        function = tool.get("function") if isinstance(tool.get("function"), dict) else tool
        name = str(function.get("name") or "")
        if not name:
            continue
        declaration = {"name": name}
        description = function.get("description")
        if description:
            declaration["description"] = str(description)
        schema = (function.get("parameters") or function.get("parametersJsonSchema")
                  or function.get("parameters_json_schema"))
        declaration["parameters"] = (clean_schema(schema) if isinstance(schema, dict)
                                     else {"type": "object", "properties": {}})
        declarations.append(declaration)
    return [{"functionDeclarations": declarations}] if declarations else []


def clean_schema(schema):
    """Strip JSON Schema keywords the Gemini schema subset rejects."""
    if isinstance(schema, list):
        return [clean_schema(item) for item in schema]
    if not isinstance(schema, dict):
        return schema
    cleaned = {}
    for key, value in schema.items():
        if key in DROPPED_SCHEMA_KEYS:
            continue
        if key == "const":
            cleaned.setdefault("enum", [value])
            continue
        if key == "properties" and isinstance(value, dict):
            cleaned[key] = {str(name): clean_schema(item) for name, item in value.items()}
        elif key in ("items", "anyOf", "oneOf", "allOf", "not", "additionalProperties"):
            cleaned[key] = clean_schema(value)
        else:
            cleaned[key] = value
    return cleaned


def generation_config(payload, model):
    """Return ``generationConfig``; keeps ``maxOutputTokens > thinkingBudget``."""
    limit = CLAUDE_MAX_OUTPUT_TOKENS if "claude" in model.lower() else MAX_OUTPUT_TOKENS
    output = _positive_int(payload.get("max_tokens") or payload.get("max_completion_tokens"),
                           limit)
    output = max(1, min(output, limit))
    budget = thinking_budget(payload, model)
    if budget is not None and budget > 0 and output <= budget:
        output = min(budget + OUTPUT_HEADROOM, limit)
        if output <= budget:
            budget = max(output - OUTPUT_HEADROOM, 1)
    config = {"maxOutputTokens": output}
    temperature = payload.get("temperature")
    if isinstance(temperature, (int, float)) and not isinstance(temperature, bool):
        config["temperature"] = float(temperature)
    top_p = payload.get("top_p")
    if isinstance(top_p, (int, float)) and not isinstance(top_p, bool):
        config["topP"] = float(top_p)
    stop = payload.get("stop")
    if isinstance(stop, str) and stop:
        config["stopSequences"] = [stop]
    elif isinstance(stop, list):
        sequences = [str(value) for value in stop if isinstance(value, str) and value]
        if sequences:
            config["stopSequences"] = sequences
    if budget is not None:
        # -1 is a meaningful sentinel ("model default/dynamic"); never normalize it.
        config["thinkingConfig"] = {"thinkingBudget": budget, "includeThoughts": True}
    return config


def thinking_budget(payload, model):
    """Return the ``thinkingBudget`` to request, or ``None`` to leave thinking off."""
    explicit = payload.get("thinking_budget")
    if isinstance(explicit, int) and not isinstance(explicit, bool):
        return explicit
    effort = payload.get("reasoning_effort")
    if isinstance(effort, dict):
        effort = effort.get("effort")
    if not effort:
        reasoning = payload.get("reasoning")
        if isinstance(reasoning, dict):
            effort = reasoning.get("effort")
    if isinstance(effort, str) and effort.strip():
        label = effort.strip().lower()
        if label in ("none", "off", "disabled"):
            return None
        if label in REASONING_BUDGETS:
            return REASONING_BUDGETS[label]
    return -1 if _thinks_by_default(model) else None


def usage(metadata):
    """Map Gemini ``usageMetadata`` onto an OpenAI ``usage`` object."""
    if not isinstance(metadata, dict):
        return None
    prompt = _tokens(metadata.get("promptTokenCount"))
    completion = _tokens(metadata.get("candidatesTokenCount"))
    thoughts = _tokens(metadata.get("thoughtsTokenCount"))
    cached = _tokens(metadata.get("cachedContentTokenCount"))
    total = _tokens(metadata.get("totalTokenCount")) or prompt + completion + thoughts
    result = {"prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": total}
    if cached:
        result["prompt_tokens_details"] = {"cached_tokens": cached}
    if thoughts:
        result["completion_tokens_details"] = {"reasoning_tokens": thoughts}
    return result


def _thinks_by_default(model):
    """Pro / ``-high`` / ``-thinking`` ids reason unless the caller says otherwise."""
    name = model.lower()
    return "thinking" in name or name.endswith("-high") or "-pro-" in name


def _positive_int(value, default):
    if isinstance(value, bool):
        return default
    try:
        number = int(value)
    except (TypeError, ValueError):
        return default
    return number if number > 0 else default


def _tokens(value):
    try:
        return max(int(value or 0), 0)
    except (TypeError, ValueError):
        return 0
