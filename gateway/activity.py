"""Provider-scoped request information, without recording prompts or credentials."""
import collections
import hashlib
import json
import os
import threading
import time
import uuid

CACHED_KEYS = ("cache_read_input_tokens", "prompt_cache_hit_tokens", "cached_tokens")
CACHE_GROUPS = ("prompt_tokens_details", "input_tokens_details")
# How many finished rows stay in memory. The log on disk is the record of
# truth and holds everything, so this is a memory guard, not a visibility cap:
# records() reads the tail back from disk when a caller asks for more.
MEMORY_ROWS = 2000


def _number(value):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return value


def _usage_number(usage, keys, groups=()):
    """Read the first numeric key, looking inside detail objects as a fallback."""
    for key in keys:
        value = _number(usage.get(key))
        if value is not None:
            return value
    for name in groups:
        details = usage.get(name)
        if isinstance(details, dict):
            value = _usage_number(details, keys)
            if value is not None:
                return value
    return None


class Activity:
    def __init__(self, directory):
        self.path = directory / "requests.jsonl"
        self.lock = threading.RLock()
        self.rows = collections.deque(maxlen=MEMORY_ROWS)
        self.active = {}
        self.daily = {}
        # How many rows the log has ever held, kept in step with every append
        # instead of re-counting the file per read: the usage board asks for
        # the full history every poll, and a line-by-line walk of a long log
        # on each 10s poll is the cost that grows forever.
        self.written = 0
        # Bumped whenever the observable request set changes (one starts or
        # one finishes); the board cache keys on it so an idle dashboard
        # stops rebuilding the whole board on every poll.
        self.revision = 0
        self._handle = None
        if self.path.exists():
            with self.path.open(encoding="utf-8") as handle:
                for line in handle:
                    try:
                        row = json.loads(line)
                    except ValueError:
                        continue
                    self.rows.append(row)
                    self._count(row)
                    self.written += 1

    def close(self):
        """Release the kept-open log handle.

        The handle is cheap to reopen (the next `_write` opens it again), so
        closing is about lifecycle, not speed: on Windows an open handle
        blocks the data directory from ever being removed, which kept test
        workspaces and uninstalled installs from cleaning up after themselves.
        """
        with self.lock:
            if self._handle is None:
                return
            handle, self._handle = self._handle, None
            try:
                handle.close()
            except Exception:
                pass

    def begin(self, provider, model, protocol, port, session=None):
        row = {"id": uuid.uuid4().hex, "provider": provider, "model": model,
               "protocol": protocol, "port": port, "timestamp": time.time(),
               "status": "processing", "http_status": None, "duration_ms": None,
               "input_tokens": None, "output_tokens": None, "cached_tokens": None,
               "reasoning_tokens": None, "ttft_ms": None, "account": "", "error": ""}
        row["session_id"] = ("ses_" + hashlib.sha256(str(session).encode("utf-8")).hexdigest()[:16]
                             if session else "req_" + row["id"][:16])
        row["session_source"] = "client" if session else "request"
        with self.lock:
            self.active[row["id"]] = row
            self.revision += 1
        return row

    def finish(self, row, status, disconnected=False):
        row.update(http_status=status, duration_ms=round((time.time() - row["timestamp"]) * 1000))
        row["status"] = "cancelled" if disconnected else (
            "success" if status and status < 400 and not row["error"] else "failed")
        with self.lock:
            self.active.pop(row["id"], None)
            self.rows.append(row)
            self._count(row)
            self._append(row)
            self.written += 1
            self.revision += 1

    def _append(self, row):
        """One row to the log, through a handle kept open between requests.

        Opening and closing per row was the per-request price of durability;
        the same guarantee (every write leaves the Python buffer) holds with
        a flush, minus the open/close syscalls. A vanished or recreated
        data directory reopens instead of failing the request.
        """
        text = json.dumps(row, ensure_ascii=False) + "\n"
        try:
            self._write(text)
            return
        except OSError:
            pass
        handle, self._handle = self._handle, None
        try:
            handle.close()
        except Exception:
            pass
        try:
            self._write(text)
            return
        except OSError:
            pass
        self.written = max(self.written - 1, 0)

    def _write(self, text):
        if self._handle is None:
            self._handle = self.path.open("a", encoding="utf-8")
        handle = self._handle
        try:
            handle.write(text)
            handle.flush()
        except ValueError:
            raise OSError("activity log closed")

    def _count(self, row):
        day = time.strftime("%Y-%m-%d", time.localtime(row["timestamp"]))
        bucket = self.daily.setdefault((day, row["provider"]), {"requests": 0, "failed": 0,
                                                                  "tokens": 0, "metered": 0})
        bucket["requests"] += 1
        bucket["failed"] += row["status"] == "failed"
        bucket["tokens"] += (row["input_tokens"] or 0) + (row["output_tokens"] or 0)
        bucket["metered"] += row["input_tokens"] is not None or row["output_tokens"] is not None

    def view(self, provider):
        with self.lock:
            rows = [dict(row) for row in self.rows if row["provider"] == provider]
            active = [dict(row) for row in self.active.values() if row["provider"] == provider]
            bucket = self.daily.get((time.strftime("%Y-%m-%d"), provider),
                                    {"requests": 0, "failed": 0, "tokens": 0, "metered": 0}).copy()
        return {"records": list(reversed(active)) + list(reversed(rows))[:200],
                "summary": {**bucket, "in_flight": len(active)}}

    def records(self, limit=None):
        """Every provider's requests, newest first, with in-flight calls on top.

        `limit` of None means no cap. What is held in memory is bounded by the
        deque (see MEMORY_ROWS), so an uncapped caller - or one asking for more
        than the deque still holds - gets the difference read back from the
        log, which is the record of truth. The in-memory half exists so the
        common case never touches the disk.
        """
        with self.lock:
            rows = [dict(row) for row in self.rows]
            active = [dict(row) for row in self.active.values()]
        merged = list(reversed(active)) + list(reversed(rows))
        # "No cap" means every row the log still holds, so it counts the log; a
        # finite limit is answered from memory when that is enough, and only
        # reaches for the disk when the memory half cannot cover it.
        wanted = self._log_total() if limit is None else limit
        if len(merged) >= wanted:
            return merged if limit is None else merged[:wanted]
        known = {row["id"] for row in merged if row.get("id")}
        return merged + self._older_rows(wanted - len(merged), known)

    def _log_total(self):
        """How many rows the log holds, from the append counter; 0 when empty.

        The old answer re-read the whole file line by line, which the board
        (uncapped, polled every 10s) paid forever as the log grew. The
        counter is bumped by every `finish`, and `finish` is the only writer.
        """
        return self.written

    def _older_rows(self, count, known):
        """The `count` rows just older than the ones already in `known`.

        Returned newest-first like the caller's own rows, so the result can be
        appended straight after them. The log is append-only and the memory
        half is its tail, so walking it backwards and dropping every id
        already collected reproduces the rows the deque has evicted - with no
        risk of repeating one because the file was rotated or trimmed
        underneath us.
        """
        if count <= 0 or not self.path.exists():
            return []
        found = []
        try:
            with self.path.open("rb") as handle:
                handle.seek(0, os.SEEK_END)
                position = handle.tell()
                buffer = b""
                while position > 0 and len(found) < count:
                    step = min(64 * 1024, position)
                    position -= step
                    handle.seek(position)
                    buffer = handle.read(step) + buffer
                    parts = buffer.split(b"\n")
                    buffer = parts[0]
                    for raw in reversed(parts[1:]):
                        row = self._decode(raw)
                        if row is None or row.get("id") in known:
                            continue
                        # Only a row that carries an id can be recognised as a
                        # duplicate later; a row without one is always kept,
                        # exactly as the old slice kept whatever it read.
                        if row.get("id"):
                            known.add(row["id"])
                        found.append(row)
                        if len(found) >= count:
                            break
                if position == 0 and buffer.strip() and len(found) < count:
                    row = self._decode(buffer)
                    if row is not None and row.get("id") not in known:
                        if row.get("id"):
                            known.add(row["id"])
                        found.append(row)
        except OSError:
            return []
        # `found` was collected newest-first; the caller's list is newest-first
        # too, so it is already in the right order.
        return found

    @staticmethod
    def _decode(raw):
        """One log line as a dict; None when it is blank or not valid JSON."""
        text = raw.strip()
        if not text:
            return None
        try:
            row = json.loads(text.decode("utf-8", "replace"))
        except (ValueError, UnicodeDecodeError):
            return None
        return row if isinstance(row, dict) else None


class ObservedWriter:
    def __init__(self, writer, row):
        self.writer, self.row = writer, row
        self.buffer = b""
        self.disconnected = False

    def __getattr__(self, name):
        return getattr(self.writer, name)

    def write(self, data):
        try:
            result = self.writer.write(data)
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
            self.disconnected = True
            raise
        if self.row.get("ttft_ms") is None and b"data:" in data:
            self.row["ttft_ms"] = round((time.time() - self.row["timestamp"]) * 1000)
        self.buffer += data
        while b"\n" in self.buffer:
            line, self.buffer = self.buffer.split(b"\n", 1)
            if line.startswith(b"data:"):
                try:
                    self.observe(json.loads(line[5:].strip()))
                except (ValueError, UnicodeDecodeError):
                    pass
        if len(self.buffer) > 1024 * 1024:
            self.buffer = b""
        return result

    def observe(self, value):
        if not isinstance(value, dict):
            return
        error = value.get("error")
        if error:
            self.row["error"] = str(error.get("message", "请求失败") if isinstance(error, dict) else error)[:300]
        usage = value.get("usage") or (value.get("message") or {}).get("usage") or (value.get("response") or {}).get("usage")
        if not isinstance(usage, dict):
            return
        cached = _usage_number(usage, CACHED_KEYS, CACHE_GROUPS)
        written = _usage_number(usage, ("cache_creation_input_tokens", "prompt_cache_write_tokens"))
        prompt = _usage_number(usage, ("prompt_tokens",))
        if prompt is None:
            fresh = _usage_number(usage, ("input_tokens",))
            # Anthropic reports the uncached remainder in input_tokens, so the
            # billable prompt is fresh + cache read + cache write. OpenAI's
            # prompt_tokens/input_tokens already contain the cached part.
            if fresh is not None and any(key in usage for key in
                                         ("cache_read_input_tokens", "cache_creation_input_tokens")):
                fresh += (cached or 0) + (written or 0)
            prompt = fresh
        counts = {"input_tokens": prompt,
                  "output_tokens": _usage_number(usage, ("completion_tokens", "output_tokens")),
                  "cached_tokens": cached,
                  "reasoning_tokens": _usage_number(usage, ("reasoning_tokens",), ("completion_tokens_details",))}
        for target, count in counts.items():
            if count is not None:
                self.row[target] = count

    def complete(self):
        if self.buffer.strip().startswith(b"{"):
            try:
                self.observe(json.loads(self.buffer))
            except (ValueError, UnicodeDecodeError):
                pass
