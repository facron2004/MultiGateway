"""MiniMax's native Anthropic transport to the gateway's chat stream."""
import json
import time

from .transport import ProviderError, sse_events


def content_blocks(content):
    if isinstance(content, str):
        return [{"type": "text", "text": content}] if content else []
    if content is not None and (not isinstance(content, list) or not all(isinstance(part, dict) for part in content)):
        raise ProviderError("message content must be text or an array of content blocks", 400)
    blocks = []
    for part in content or []:
        if part.get("type") in ("text", "input_text"):
            blocks.append({"type": "text", "text": part.get("text", "")})
        elif part.get("type") == "image_url":
            image = part["image_url"]
            url = image if isinstance(image, str) else image.get("url", "")
            if url.startswith("data:"):
                mime, encoded = url[5:].split(";base64,", 1)
                source = {"type": "base64", "media_type": mime, "data": encoded}
            else:
                source = {"type": "url", "url": url}
            blocks.append({"type": "image", "source": source})
        else:
            raise ProviderError("MiniMax cannot translate content type: " + str(part.get("type")), 400)
    return blocks


def chat_to_anthropic(payload):
    if payload.get("n", 1) != 1:
        raise ProviderError("MiniMax supports one completion per request", 400)
    incoming = payload.get("messages")
    if not isinstance(incoming, list) or not all(isinstance(message, dict) for message in incoming):
        raise ProviderError("messages must be an array of message objects", 400)
    system, messages = [], []
    for message in incoming:
        role = message.get("role")
        if role in ("system", "developer"):
            system.extend(content_blocks(message.get("content")))
            continue
        if role == "tool":
            if not message.get("tool_call_id"):
                raise ProviderError("tool result requires tool_call_id", 400)
            role = "user"
            blocks = [{"type": "tool_result", "tool_use_id": message.get("tool_call_id"),
                       "content": content_blocks(message.get("content"))}]
        elif role in ("user", "assistant"):
            blocks = content_blocks(message.get("content"))
            for tool in message.get("tool_calls") or []:
                function = tool.get("function") or {}
                if not tool.get("id") or not function.get("name"):
                    raise ProviderError("tool call requires id and function name", 400)
                try:
                    arguments = json.loads(function.get("arguments") or "{}")
                except (ValueError, TypeError) as exc:
                    raise ProviderError("tool arguments must be a JSON object", 400) from exc
                if not isinstance(arguments, dict):
                    raise ProviderError("tool arguments must be a JSON object", 400)
                blocks.append({"type": "tool_use", "id": tool["id"],
                               "name": function["name"], "input": arguments})
        else:
            raise ProviderError("unsupported message role: " + str(role), 400)
        if not blocks:
            continue
        if messages and messages[-1]["role"] == role:
            messages[-1]["content"].extend(blocks)
        else:
            messages.append({"role": role, "content": blocks})
    if not messages:
        raise ProviderError("messages must contain at least one conversation message", 400)
    maximum = payload.get("max_completion_tokens") or payload.get("max_tokens") or 8192
    if not isinstance(maximum, int) or isinstance(maximum, bool) or maximum <= 0:
        raise ProviderError("max_tokens must be a positive integer", 400)
    result = {"model": payload["model"], "messages": messages, "max_tokens": maximum, "stream": True}
    if system:
        result["system"] = system
    for key in ("temperature", "top_p"):
        if key in payload:
            result[key] = payload[key]
    if payload.get("stop"):
        stop = payload["stop"]
        result["stop_sequences"] = [stop] if isinstance(stop, str) else stop
    tools = []
    for tool in payload.get("tools") or []:
        if tool.get("type") != "function":
            raise ProviderError("MiniMax supports function tools on this endpoint", 400)
        function = tool.get("function") or {}
        if not function.get("name"):
            raise ProviderError("function tool requires a name", 400)
        tools.append({"name": function["name"], "description": function.get("description", ""),
                      "input_schema": function.get("parameters") or {"type": "object", "properties": {}}})
    choice = payload.get("tool_choice", "auto")
    if tools and choice != "none":
        result["tools"] = tools
        if isinstance(choice, dict):
            result["tool_choice"] = {"type": "tool", "name": choice["function"]["name"]}
        else:
            result["tool_choice"] = {"type": "any" if choice == "required" else "auto"}
        if payload.get("parallel_tool_calls") is False:
            result["tool_choice"]["disable_parallel_tool_use"] = True
    return result


def chat_usage(usage):
    cached = int(usage.get("cache_read_input_tokens") or 0)
    created = int(usage.get("cache_creation_input_tokens") or 0)
    prompt = int(usage.get("input_tokens") or 0) + cached + created
    output = int(usage.get("output_tokens") or 0)
    return {"prompt_tokens": prompt, "completion_tokens": output, "total_tokens": prompt + output,
            "prompt_tokens_details": {"cached_tokens": cached, "cache_creation_tokens": created}}


def minimax_chat_stream(response, public_model):
    identifier, created, usage, tools = "", int(time.time()), {}, {}
    tool_arguments = set()
    stopped = False

    def frame(delta=None, finish=None, include_usage=False):
        chunk = {"id": identifier, "object": "chat.completion.chunk", "created": created,
                 "model": public_model, "choices": [{"index": 0, "delta": delta or {}, "finish_reason": finish}]}
        if include_usage:
            chunk["usage"] = chat_usage(usage)
        return ("data: " + json.dumps(chunk, ensure_ascii=False) + "\n\n").encode("utf-8")

    for event in sse_events(response):
        kind = event.get("type")
        if kind == "error":
            error = event.get("error") or {}
            raise ProviderError(error.get("message", "MiniMax stream failed"))
        if kind == "message_start":
            message = event.get("message") or {}
            identifier = message.get("id") or "chatcmpl-minimax"
            usage.update(message.get("usage") or {})
            yield frame({"role": "assistant"})
        elif kind == "content_block_start":
            block = event.get("content_block") or {}
            if block.get("type") == "tool_use":
                index = len(tools)
                tools[event["index"]] = index
                arguments = json.dumps(block["input"]) if block.get("input") else ""
                if arguments:
                    tool_arguments.add(event["index"])
                yield frame({"tool_calls": [{"index": index, "id": block["id"], "type": "function",
                                             "function": {"name": block["name"], "arguments": arguments}}]})
            elif block.get("text"):
                yield frame({"content": block["text"]})
            elif block.get("thinking"):
                yield frame({"reasoning_content": block["thinking"]})
        elif kind == "content_block_delta":
            delta = event.get("delta") or {}
            if delta.get("type") == "text_delta":
                yield frame({"content": delta.get("text", "")})
            elif delta.get("type") == "thinking_delta":
                yield frame({"reasoning_content": delta.get("thinking", "")})
            elif delta.get("type") == "input_json_delta":
                index = tools[event["index"]]
                if delta.get("partial_json"):
                    tool_arguments.add(event["index"])
                yield frame({"tool_calls": [{"index": index, "function": {"arguments": delta.get("partial_json", "")}}]})
        elif kind == "content_block_stop" and event.get("index") in tools:
            if event["index"] not in tool_arguments:
                yield frame({"tool_calls": [{"index": tools[event["index"]], "function": {"arguments": "{}"}}]})
        elif kind == "message_delta":
            usage.update(event.get("usage") or {})
            reason = (event.get("delta") or {}).get("stop_reason")
            finish = {"tool_use": "tool_calls", "max_tokens": "length"}.get(reason, "stop")
            yield frame(finish=finish, include_usage=True)
        elif kind == "message_stop":
            stopped = True
            yield b"data: [DONE]\n\n"
            break
    if not stopped:
        raise ProviderError("MiniMax stream ended before message_stop")
