"""Offline actual-Claude fixture runner, executed ONLY inside the isolated container.

All HTTP requests terminate here. It never connects to another server. Captures
are synthetic and intentionally remain in the designated /out artifact mount.
"""
import argparse
import concurrent.futures
import copy
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import queue
import signal
import socket
import subprocess
import sys
import threading
import time
import uuid

from codec import (PREFIX, ReplayError, canonical, compact, decode_turn, decode_history, mint_session_id,
                   project_turn, response_json, response_sse)

LOW = "antigravity/gemini-4-argon-low"
HIGH = "antigravity/gemini-4-argon-high"
BINDING = {"profile_id": "synthetic-profile-stagea", "project_id": "synthetic-project-stagea",
           "native_model": "gemini-4-argon-low"}
CLI_TIMEOUT = 90
FIXTURE_MARKER = "SYNTHETIC_READ_FIXTURE_STAGE_A_20261018"
COMPLETION_NATIVE = [{"role": "model", "parts": [{"text": "SYNTHETIC_STAGE_A_DONE", "thoughtSignature": "fake-native-completion-signature"}]}]


def dump(path, value):
    Path(path).write_text(json.dumps(value, ensure_ascii=False, indent=2, allow_nan=False) + "\n")


def clean_env(out, port):
    return {"PATH": "/usr/local/bin:/usr/bin:/bin", "HOME": str(out / "home"),
            "CLAUDE_CONFIG_DIR": str(out / "config"), "TMPDIR": str(out / "tmp"),
            "LANG": "C.UTF-8", "TERM": "dumb", "NO_COLOR": "1",
            "ANTHROPIC_API_KEY": "stage-a-fake-key-not-a-credential",
            "ANTHROPIC_BASE_URL": "http://127.0.0.1:%d" % port,
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
            "DISABLE_AUTOUPDATER": "1", "PYTHONDONTWRITEBYTECODE": "1"}


def cli_command(model, settings, prompt, session_id=None, resume=None, effort=None, bare=True, tools="Read"):
    if not bare and settings.get("disableAllHooks") is not True:
        raise ValueError("Nonbare fixture requires session-only disableAllHooks=true")
    cmd = ["/opt/fixture-cli"] + (["--bare"] if bare else []) + ["--print", "--output-format", "json",
           "--model", model, "--tools", tools, "--allowedTools", "Read(//fixture/read.txt)",
           "--permission-mode", "dontAsk", "--permission-prompts", "none",
           "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}',
           "--no-chrome", "--setting-sources", "",
           "--settings", compact(settings)]
    if bare:
        cmd.append("--disable-slash-commands")
    if session_id:
        cmd += ["--session-id", session_id]
    if resume:
        cmd += ["--resume", resume]
    if effort:
        cmd += ["--effort", effort]
    return cmd + ["--", prompt]


def stream_cli_command(model, settings, session_id):
    command = cli_command(model, settings, "unused", session_id=session_id)[:-2]
    command[command.index("--output-format") + 1] = "stream-json"
    return command + ["--input-format", "stream-json", "--verbose"]


def wire_fields(body):
    result = {"top_level_keys": sorted(body)}
    for key in ("model", "stream", "thinking", "output_config", "tool_choice", "system",
                "max_tokens", "betas", "context_management", "temperature", "metadata"):
        result[key] = {"present": True, "value": body[key]} if key in body else {"present": False}
    output_config = body.get("output_config", {})
    result["output_config.effort"] = ({"present": True, "value": output_config["effort"]}
                                      if "effort" in output_config else {"present": False})
    result["output_config.format"] = ({"present": True, "value": output_config["format"]}
                                      if "format" in output_config else {"present": False})
    result["tool_names"] = [tool.get("name", tool.get("type")) for tool in body.get("tools", [])]
    return result


def assistant_turns(messages):
    # Boundaries come from the original sequence: removing a real system message
    # must not manufacture adjacency. Ordinary user text is never system input.
    if not isinstance(messages, list) or len(messages) > 512:
        raise ReplayError("message count/type limit")
    turns, current = [], []
    for message in messages:
        if message.get("role") != "assistant":
            if current:
                turns.append(current)
                current = []
            continue
        content = message.get("content", [])
        if isinstance(content, str):
            content = [{"type": "text", "text": content}]
        if not isinstance(content, list):
            raise ReplayError("invalid assistant content")
        # Separate complete turns keep separate authoritative carriers; unsigned
        # adjacent fragments still have to pass the preceding turn's digest.
        if content and isinstance(content[0], dict) and content[0].get("type") == "thinking" and current:
            turns.append(current)
            current = []
        if len(current) + len(content) > 4097:
            raise ReplayError("assistant projection block limit")
        current.extend(copy.deepcopy(content))
    if current:
        turns.append(current)
    return turns


def first_assistant_turn(messages):
    turns = assistant_turns(messages)
    return turns[0] if turns else []


def numeric_json(value):
    """Equivalent JSON with explicit exponent spelling, for synthetic input deltas."""
    canonical(value)  # Reject unsafe numbers before rendering.
    def render(v):
        if isinstance(v, bool) or v is None or isinstance(v, str):
            return compact(v)
        if isinstance(v, (int, float)):
            return ("-0e0" if v == 0 else format(float(v), ".17e"))
        if isinstance(v, list):
            return "[" + ",".join(render(x) for x in v) + "]"
        return "{" + ",".join(compact(k) + ":" + render(v[k]) for k in v) + "}"
    return render(value)


def reserialize_row(row):
    """Only benign tool-input representation edits; never change any carrier."""
    result = copy.deepcopy(row)
    def reorder(v):
        if isinstance(v, dict):
            return {key: reorder(v[key]) for key in reversed(list(v))}
        if isinstance(v, list):
            return [reorder(x) for x in v]
        if isinstance(v, int) and not isinstance(v, bool):
            return float(v)
        return v
    message = result.get("message", {})
    if message.get("role") == "assistant" and isinstance(message.get("content"), list):
        for block in message["content"]:
            if block.get("type") == "tool_use":
                block["input"] = reorder(block["input"])
    if canonical(row) != canonical(result):
        raise ValueError("Synthetic transcript reserialization changed semantics")
    return result


def reserialize_transcript(out):
    """Runs after the original CLI exits, only on this case's new synthetic files."""
    import hashlib
    changes = []
    for path in (out / "config" / "projects").rglob("*.jsonl"):
        original = path.read_text()
        rows = [json.loads(line) for line in original.splitlines() if line]
        normalized = [reserialize_row(row) for row in rows]
        # Preserve before/after evidence, without copying in any host transcript.
        (out / "transcript-before-reserialization.jsonl").write_text(original)
        serialized = "\n".join(compact(row) for row in normalized) + "\n"
        (out / "transcript-after-reserialization.jsonl").write_text(serialized)
        path.write_text(serialized)
        changes.append({"path": str(path.relative_to(out)), "changed": original != serialized,
                        "canonical_equal": canonical(rows) == canonical(normalized),
                        "before_sha256": hashlib.sha256(original.encode()).hexdigest(),
                        "after_sha256": hashlib.sha256(serialized.encode()).hexdigest()})
    dump(out / "reserialization.json", changes)
    if not changes:
        raise ValueError("No synthetic transcript found for reserialization")


def fixture(kind):
    call = {"functionCall": {"name": "Read", "args": {"file_path": "/fixture/read.txt"}},
            "thoughtSignature": "fake-native-call-signature"}
    if kind in ("codec_leaf", "codec_root"):
        args = ({"json": compact({"file_path": "/fixture/read.txt", "offset": 1.0})} if kind == "codec_root"
                else {"file_path": compact("/fixture/read.txt"), "offset": 1.0})
        return [{"role": "model", "parts": [
            {"thought": True, "text": "Genuine synthetic native thought 雪 retained even when display omitted.", "thoughtSignature": "fake-v1-native-thought"},
            {"functionCall": {"name": "native_read_alias", "id": "synthetic-reusable-native-id", "args": args}, "thoughtSignature": "fake-v1-native-call"}]}]
    if kind == "tool_only":
        return [{"role": "model", "parts": [call]}]
    if kind == "signed_text":
        return [{"role": "model", "parts": [{"text": "Synthetic signed ordinary text 雪.", "thoughtSignature": "fake-native-text-signature"}]}]
    if kind == "signature_only":
        return [{"role": "model", "parts": [{"thoughtSignature": "fake-native-signature-only"}, call]}]
    if kind == "genuine":
        return [{"role": "model", "parts": [{"thought": True, "text": "Synthetic visible thought α.", "thoughtSignature": "fake-thought"}, call]}]
    if kind == "interleaved":
        second = {"functionCall": {"id": "synthetic-native-second", "name": "Read",
                                   "args": {"file_path": "/fixture/read.txt", "offset": 1.0, "limit": 2}}}
        return [{"role": "model", "parts": [
            {"thought": True, "text": "Synthetic thought one 雪.", "thoughtSignature": "fake-first-thought"},
            {"text": "Synthetic split "}, {"text": "ordinary text."},
            {"thought": True, "text": " Synthetic thought two é."}, call, second],
                 "synthetic_unknown": {"unicode": "雪é", "nested": [1, 1.0, {"z": 0, "a": -0.0}]}},
                {"role": "model", "parts": [{"thoughtSignature": "fake-late-native-signature"}, {"text": ""}]}]
    if kind == "signed_id":
        call["functionCall"]["id"] = "synthetic-native-signed"
        return [{"role": "model", "parts": [call]}]
    if kind == "numeric_valid":
        call["functionCall"]["args"] = {"file_path": "/fixture/read.txt", "limit": 2.0, "offset": 1.0}
        return [{"role": "model", "parts": [{"thought": True, "text": "Synthetic numeric valid thought 雪é é."}, call]}]
    if kind == "numeric":
        call["functionCall"]["args"] = {"file_path": "/fixture/read.txt", "limit": 2.0, "offset": 1.0,
                                           "synthetic_extra": {"z": [1, 1.0, 1e0, -0.0, 1e-3, {"雪": "éé"}], "a": True}}
        return [{"role": "model", "parts": [{"thought": True, "text": "Synthetic numeric codec thought."}, call]}]
    raise ValueError(kind)


def expected_tool_error_retained(histories):
    """Schema-stress oracle: an expected error is not a successful Read."""
    return bool(histories) and all(
        o.get("decoded") is True and o.get("native_exact") is True
        and bool(o.get("tool_results"))
        and all(t.get("is_error") is True for t in o["tool_results"])
        for o in histories)


def schema_inventory(tools):
    """JSON-Schema-aware recursion, not a count of user property/example keys."""
    keywords, types, occurrences, top_fields = {}, set(), [], {}
    maps = {"properties", "patternProperties", "$defs", "definitions", "dependentSchemas"}
    arrays = {"allOf", "anyOf", "oneOf", "prefixItems"}
    singles = {"items", "additionalProperties", "additionalItems", "contains", "not", "if", "then", "else",
               "propertyNames", "unevaluatedProperties", "unevaluatedItems"}
    boolean_nodes = []
    def visit(schema, path, name):
        if isinstance(schema, bool):
            boolean_nodes.append({"tool": name, "path": path, "value": schema})
            return
        if not isinstance(schema, dict):
            return
        for key, value in schema.items():
            keywords[key] = keywords.get(key, 0) + 1
            child_path = path + "/" + key
            occurrences.append({"tool": name, "path": child_path, "keyword": key, "value_type": type(value).__name__})
            if key == "type":
                types.update(value if isinstance(value, list) else [value])
            if key in maps and isinstance(value, dict):
                for prop, sub in value.items():
                    visit(sub, child_path + "/" + prop.replace("~", "~0").replace("/", "~1"), name)
            elif key in arrays and isinstance(value, list):
                for index, sub in enumerate(value):
                    visit(sub, child_path + "/" + str(index), name)
            elif key in singles:
                if isinstance(value, list):
                    for index, sub in enumerate(value):
                        visit(sub, child_path + "/" + str(index), name)
                else:
                    visit(value, child_path, name)
    for index, tool in enumerate(tools):
        for key in tool:
            top_fields[key] = top_fields.get(key, 0) + 1
        visit(tool.get("input_schema"), "/tools/%d/input_schema" % index, tool.get("name"))
    return {"tool_names": [t.get("name") for t in tools],
            "tool_type_fields": [{"name": t.get("name"), "present": "type" in t, **({"value": t["type"]} if "type" in t else {})} for t in tools],
            "tool_top_level_fields": dict(sorted(top_fields.items())), "schema_keywords": dict(sorted(keywords.items())),
            "schema_types": sorted(types), "keyword_occurrences": occurrences, "boolean_schemas": boolean_nodes}


def compaction_status(boundary_count, post_observations, signed_completions):
    if boundary_count and signed_completions and any(o.get("history_present") and not o.get("decoded") for o in post_observations):
        return "UNSUPPORTED"
    if boundary_count and signed_completions and post_observations and all(
            o.get("decoded") and o.get("matches_signed_completion") for o in post_observations):
        return "PASS"
    return "UNVERIFIED"


def case_specs():
    cases = {}
    for kind in ("tool_only", "signed_text", "signature_only", "genuine", "interleaved", "numeric", "numeric_valid", "signed_id"):
        for thinking in ("default", "on", "off"):
            cases[kind + "-" + thinking] = {"kind": kind, "thinking": thinking}
    cases["minimal"] = {"kind": "tool_only", "thinking": "default"}
    for negative in ("missing", "mutated", "arguments", "order", "profile", "project", "model"):
        cases["negative-" + negative] = {"kind": "interleaved", "thinking": "default", "negative": negative}
    for mode in ("default", "on", "off"):
        cases["signed-text-conversation-" + mode] = {"kind": "signed_text", "thinking": mode, "same_process_followup": True}
    cases["numeric-reserialize"] = {"kind": "numeric_valid", "thinking": "default", "reserialize": True}
    cases["numeric-nested-reserialize"] = {"kind": "numeric", "thinking": "default", "reserialize": True}
    cases["switch-low-high"] = {"kind": "interleaved", "thinking": "default", "switch": HIGH}
    cases["switch-low-high-on"] = {"kind": "interleaved", "thinking": "on", "switch": HIGH}
    cases["switch-low-high-off"] = {"kind": "interleaved", "thinking": "off", "switch": HIGH}
    cases["suffix-provider-1m"] = {"kind": "genuine", "thinking": "default", "model": LOW + "[1m]"}
    cases["suffix-gateway-1m"] = {"kind": "genuine", "thinking": "default", "model": "claude-antigravity-gemini-4-argon-low[1m]"}
    cases["effort-low"] = {"kind": "genuine", "thinking": "on", "effort": "low"}
    cases["effort-high"] = {"kind": "genuine", "thinking": "on", "effort": "high"}
    cases["compact"] = {"kind": "interleaved", "thinking": "default", "compact": True}
    cases["compact-nonbare"] = {"kind": "interleaved", "thinking": "default", "compact": True, "bare": False, "signed_completions": True}
    cases["enforcement-valid"] = {"kind": "interleaved", "thinking": "default", "enforce_replay": True}
    cases["enforcement-mismatch"] = {"kind": "interleaved", "thinking": "default", "negative": "model", "enforce_replay": True}
    cases["v1-codec-omitted"] = {"kind": "codec_leaf", "thinking": "default", "final_codec": True, "signed_completions": True, "enforce_replay": True}
    cases["v1-codec-full"] = {"kind": "codec_leaf", "thinking": "on", "final_codec": True, "signed_completions": True, "enforce_replay": True, "same_process_followup": True}
    cases["v1-codec-root"] = {"kind": "codec_root", "thinking": "default", "final_codec": True, "signed_completions": True, "enforce_replay": True}
    cases["v1-codec-compact"] = {"kind": "codec_leaf", "thinking": "default", "final_codec": True, "signed_completions": True, "enforce_replay": True, "compact": True, "bare": False}
    for spec in cases.values():
        if spec["kind"] == "numeric":
            spec["expected_tool_error"] = True
        if spec.get("switch"):
            spec["enforce_replay"] = True
    return cases


class FixtureServer(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = False

    def __init__(self, out, blocks, numeric_spelling=False, enforce_replay=False, signed_completions=False, catalog_only=False,
                 initial_native=None, tool_projections=None, native_session_id=None):
        super().__init__(("127.0.0.1", 0), Handler)
        self.out, self.blocks = out, blocks
        self.numeric_spelling = numeric_spelling
        self.enforce_replay = enforce_replay
        self.signed_completions = signed_completions
        self.catalog_only = catalog_only
        self.initial_native = copy.deepcopy(initial_native)
        self.tool_projections = copy.deepcopy(tool_projections)
        self.native_session_id = native_session_id or mint_session_id()
        self.native_generation_events = []
        self.replay_rejections = []
        self.phase = "initial"
        self.records = []
        self.lock = threading.Lock()
        self.sent_initial = False

    def synthetic_native_generation(self, record, initial):
        """Instrumented LOCAL callback, never a connection to a native backend."""
        self.native_generation_events.append({"request": record["index"], "phase": record["phase"]})
        if self.catalog_only:
            return response_json([{"type": "text", "text": "SYNTHETIC_CATALOG_CAPTURE_DONE"}], record["body"].get("model", LOW))
        blocks = self.blocks if initial else [{"type": "text", "text": "SYNTHETIC_STAGE_A_DONE"}]
        session_id = record.get("validated_session_id") or self.native_session_id
        display = "omitted" if record["body"].get("thinking", {}).get("display") == "omitted" else "full"
        if initial and self.initial_native is not None:
            blocks = project_turn(self.initial_native, BINDING, session_id=session_id, thought_display=display,
                                  tool_projections=self.tool_projections)
        if not initial and self.signed_completions:
            blocks = project_turn(COMPLETION_NATIVE, BINDING, session_id=session_id, thought_display=display)
        return response_json(blocks, record["body"].get("model", LOW))


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def setup(self):
        super().setup()
        self.connection.settimeout(10)

    def do_GET(self):
        self.send_payload(404, b'{"error":{"type":"not_found_error","message":"local fixture only"}}', "application/json")

    def send_payload(self, code, data, content_type):
        self.send_response(code)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(data)
        self.wfile.flush()

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        if not 0 < length <= 16 * 1024 * 1024:
            return self.send_payload(413, b"{}", "application/json")
        raw = self.rfile.read(length)
        try:
            body = json.loads(raw)
        except ValueError:
            return self.send_payload(400, b"{}", "application/json")
        with self.server.lock:
            index = len(self.server.records)
            record = {"index": index, "phase": self.server.phase, "path": self.path,
                      "headers": dict(self.headers), "body": body, "wire": wire_fields(body)}
            self.server.records.append(record)
            dump(self.server.out / ("request-%02d.json" % index), record)
            (self.server.out / ("request-%02d.raw.json" % index)).write_bytes(raw)
            if index >= 16:
                return self.send_payload(400, b'{"error":{"type":"invalid_request_error","message":"fixture request bound"}}', "application/json")
            if self.path.split("?", 1)[0] == "/v1/messages/count_tokens":
                return self.send_payload(200, b'{"input_tokens":64}', "application/json")
            if self.path.split("?", 1)[0] != "/v1/messages":
                return self.send_payload(404, b"{}", "application/json")
            if self.server.enforce_replay:
                history = first_assistant_turn(body.get("messages", []))
                if history:
                    model = body.get("model", "")
                    binding = dict(BINDING, native_model=model[len("antigravity/"):] if model.startswith("antigravity/") else model)
                    try:
                        validated = decode_history(assistant_turns(body.get("messages", [])), binding)
                        record["validated_session_id"] = validated["session_id"]
                    except ReplayError as exc:
                        error = {"type": "error", "error": {"type": "invalid_request_error",
                            "message": "Stage A replay rejected: " + str(exc) + ". Model/tier changes require a new session."}}
                        rejection = {"request": index, "phase": record["phase"], "status": 400,
                                     "error": error["error"], "native_generation_callbacks_for_request": 0}
                        self.server.replay_rejections.append(rejection)
                        record["response_status"] = 400
                        dump(self.server.out / ("request-%02d.json" % index), record)
                        dump(self.server.out / ("response-%02d.json" % index), error)
                        return self.send_payload(400, compact(error).encode(), "application/json")
            initial = not self.server.sent_initial
            self.server.sent_initial = True
            message = self.server.synthetic_native_generation(record, initial)
            record["response_status"] = 200
            dump(self.server.out / ("request-%02d.json" % index), record)
        dump(self.server.out / ("response-%02d.json" % index), message)
        if body.get("stream") is True:
            data = response_sse(message)
            if initial and self.server.numeric_spelling:
                # Change only JSON lexemes in tool argument deltas, not semantic input.
                frames = []
                for frame in data.decode().split("\n\n"):
                    if not frame:
                        continue
                    event = json.loads(frame.split("data: ", 1)[1])
                    if event.get("delta", {}).get("type") == "input_json_delta":
                        event["delta"]["partial_json"] = numeric_json(json.loads(event["delta"]["partial_json"]))
                    frames.append("event: " + event["type"] + "\ndata: " + compact(event) + "\n\n")
                data = "".join(frames).encode()
            (self.server.out / ("response-%02d.sse" % index)).write_bytes(data)
            self.send_payload(200, data, "text/event-stream")
        else:
            self.send_payload(200, compact(message).encode(), "application/json")


def run_cli(out, phase, command, env):
    dump(out / (phase + "-command.json"), {"argv": command, "env": env, "cwd": "/fixture", "timeout_seconds": CLI_TIMEOUT})
    started = time.monotonic()
    with (out / (phase + "-stdout.json")).open("wb") as stdout, (out / (phase + "-stderr.log")).open("wb") as stderr:
        process = subprocess.Popen(command, cwd="/fixture", env=env, stdin=subprocess.DEVNULL,
                                   stdout=stdout, stderr=stderr, start_new_session=True)
        timed_out = False
        try:
            process.wait(timeout=CLI_TIMEOUT)
        except subprocess.TimeoutExpired:
            timed_out = True
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)
    result = {"returncode": process.returncode, "timeout": timed_out, "pid": process.pid,
              "seconds": round(time.monotonic() - started, 3)}
    try:
        result["cli_result_is_error"] = json.loads((out / (phase + "-stdout.json")).read_text()).get("is_error")
    except (ValueError, AttributeError):
        result["cli_result_is_error"] = None
    dump(out / (phase + "-process.json"), result)
    return result


def run_cli_conversation(out, command, env):
    """Two real user turns in ONE CLI process, then normal fresh-process resume.

Wait for the first result before submitting the second prompt; submitting both
at once can batch user input and would not establish signed-text history echo.
"""
    dump(out / "initial-command.json", {"argv": command, "env": env, "cwd": "/fixture", "timeout_seconds": CLI_TIMEOUT})
    started = time.monotonic()
    results = queue.Queue()
    with (out / "initial-stdout.jsonl").open("w") as stdout, (out / "initial-stderr.log").open("w") as stderr:
        process = subprocess.Popen(command, cwd="/fixture", env=env, stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=stderr, text=True, start_new_session=True)
        def collect():
            for line in process.stdout:
                stdout.write(line)
                stdout.flush()
                try:
                    event = json.loads(line)
                    if event.get("type") == "result":
                        results.put(event)
                except ValueError:
                    pass
            results.put(None)
        reader = threading.Thread(target=collect, daemon=True)
        reader.start()
        completed = 0
        timed_out = False
        try:
            for prompt in ("Synthetic signed-text Stage A fixture; respond with the prepared synthetic text.",
                           "Continue this same-process synthetic signed-text fixture without using any tool."):
                process.stdin.write(compact({"type": "user", "message": {"role": "user", "content": prompt}}) + "\n")
                process.stdin.flush()
                event = results.get(timeout=CLI_TIMEOUT)
                if event is None:
                    raise ValueError("CLI ended before same-process result")
                if event.get("is_error"):
                    raise ValueError("Same-process CLI returned error")
                completed += 1
            process.stdin.close()
            process.wait(timeout=10)
        except (queue.Empty, subprocess.TimeoutExpired):
            timed_out = True
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=5)
            reader.join(timeout=5)
            process.stdout.close()
    result = {"returncode": process.returncode, "timeout": timed_out, "pid": process.pid,
              "completed_same_process_user_turns": completed, "seconds": round(time.monotonic() - started, 3)}
    dump(out / "initial-process.json", result)
    return result


def fresh_decode(out, name, blocks, binding):
    inp = {"blocks": blocks, "binding": binding}
    dump(out / (name + "-decode-input.json"), inp)
    process = subprocess.run([sys.executable, "-B", "/fixture/codec.py"], input=compact(inp),
                             text=True, capture_output=True, timeout=10,
                             env={"PATH": "/usr/bin:/bin", "PYTHONDONTWRITEBYTECODE": "1"})
    result = json.loads(process.stdout)
    dump(out / (name + "-decoded.json"), result)
    return result


def fresh_history_decode(out, name, messages, binding):
    value = {"turns": assistant_turns(messages), "binding": binding}
    dump(out / (name + "-history-input.json"), value)
    process = subprocess.run([sys.executable, "-B", "/fixture/codec.py"], input=compact(value),
                             text=True, capture_output=True, timeout=10,
                             env={"PATH": "/usr/bin:/bin", "PYTHONDONTWRITEBYTECODE": "1"})
    result = json.loads(process.stdout)
    dump(out / (name + "-history-decoded.json"), result)
    return result


def observe_record(out, record, native, binding, all_turns=False):
    blocks = first_assistant_turn(record["body"].get("messages", []))
    if not blocks:
        return {"index": record["index"], "phase": record["phase"], "history_present": False}
    decoded = fresh_decode(out, "request-%02d" % record["index"], blocks, binding)
    results = [b for m in record["body"].get("messages", []) if m.get("role") == "user"
               for b in (m.get("content", []) if isinstance(m.get("content"), list) else [])
               if b.get("type") == "tool_result"]
    observation = {"index": record["index"], "phase": record["phase"], "history_present": True,
            "carrier_count": sum(b.get("type") == "thinking" and b.get("signature", "").startswith(PREFIX) for b in blocks),
            "thinking_texts": [b.get("thinking") for b in blocks if b.get("type") == "thinking"],
            "decoded": decoded["ok"], "error": decoded.get("error"),
            "native_exact": decoded["ok"] and compact(decoded["envelope"]["contents"]) == compact(native),
            "matches_signed_completion": decoded["ok"] and compact(decoded["envelope"]["contents"]) == compact(COMPLETION_NATIVE),
            "tool_results": [{"id": b.get("tool_use_id"), "is_error": b.get("is_error", False),
                              "fixture_read": FIXTURE_MARKER in compact(b.get("content"))} for b in results]}
    if decoded["ok"]:
        observation["session_id"] = decoded["envelope"]["session_id"]
        observation["thought_display"] = decoded["envelope"]["thought_display"]
    if all_turns:
        history = fresh_history_decode(out, "request-%02d" % record["index"], record["body"].get("messages", []), binding)
        observation["history_decoder_ok"] = history["ok"]
        observation["history_session_id"] = history.get("history", {}).get("session_id")
        observation["history_turns"] = len(history.get("history", {}).get("turns", []))
        observation["history_error"] = history.get("error")
    return observation


def run_case(name, spec, root):
    out = root / name
    out.mkdir(parents=True, exist_ok=False)
    for directory in ("home", "config", "tmp"):
        (out / directory).mkdir()
    native = fixture(spec["kind"])
    native_session_id = mint_session_id()
    tool_projections = None
    if spec["kind"] in ("codec_leaf", "codec_root"):
        tool_projections = [{"caller_name": "Read", "codec_paths": [] if spec["kind"] == "codec_root" else ["/file_path"],
                             "root_wrapper": "json" if spec["kind"] == "codec_root" else None}]
    blocks = project_turn(native, BINDING, session_id=native_session_id, tool_projections=tool_projections)
    negative = spec.get("negative")
    if negative == "missing":
        blocks = blocks[1:]
    elif negative == "mutated":
        blocks[0]["signature"] = PREFIX + "mutated"
    elif negative == "arguments":
        next(b for b in blocks if b["type"] == "tool_use")["input"]["offset"] = 2
    elif negative == "order":
        indices = [i for i, b in enumerate(blocks) if b["type"] == "tool_use"]
        blocks[indices[0]], blocks[indices[1]] = blocks[indices[1]], blocks[indices[0]]
    elif negative in ("profile", "project", "model"):
        key = {"profile": "profile_id", "project": "project_id", "model": "native_model"}[negative]
        blocks = project_turn(native, dict(BINDING, **{key: "synthetic-foreign-" + negative}))
    dump(out / "fixture.json", {"spec": spec, "binding": BINDING, "contents": native, "projected": blocks})
    server = FixtureServer(out, blocks, numeric_spelling=spec["kind"].startswith("numeric"),
                           enforce_replay=spec.get("enforce_replay", False), signed_completions=spec.get("signed_completions", False),
                           initial_native=native if spec.get("final_codec") else None,
                           tool_projections=tool_projections, native_session_id=native_session_id)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    env = clean_env(out, server.server_port)
    settings = {"env": {"ANTHROPIC_API_KEY": env["ANTHROPIC_API_KEY"], "ANTHROPIC_BASE_URL": env["ANTHROPIC_BASE_URL"]}}
    bare = spec.get("bare", True)
    if not bare:
        settings["disableAllHooks"] = True
    if spec["thinking"] != "default":
        settings["alwaysThinkingEnabled"] = spec["thinking"] == "on"
    session = str(uuid.uuid4())
    model = spec.get("model", LOW)
    runs = {}
    try:
        if spec.get("same_process_followup"):
            runs["initial"] = run_cli_conversation(out, stream_cli_command(model, settings, session), env)
        else:
            runs["initial"] = run_cli(out, "initial", cli_command(model, settings,
                "Synthetic offline Stage A fixture. Use only Read on /fixture/read.txt if requested, then finish.",
                session_id=session, effort=spec.get("effort"), bare=bare), env)
        if spec.get("reserialize"):
            reserialize_transcript(out)
        server.phase = "resume"
        runs["resume"] = run_cli(out, "resume", cli_command(model, settings,
            "Continue the synthetic fixture and acknowledge its previous result.", resume=session, effort=spec.get("effort"), bare=bare), env)
        if spec.get("switch"):
            server.phase = "switch"
            runs["switch"] = run_cli(out, "switch", cli_command(spec["switch"], settings,
                "Continue the same synthetic fixture after this explicit model switch.", resume=session, bare=bare), env)
        if spec.get("compact"):
            server.phase = "compact"
            runs["compact"] = run_cli(out, "compact", cli_command(model, settings, "/compact", resume=session, bare=bare), env)
            server.phase = "post-compact"
            runs["post-compact"] = run_cli(out, "post-compact", cli_command(model, settings,
                "Continue the synthetic fixture after attempted compaction.", resume=session, bare=bare), env)
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)
    dump(out / "endpoint-enforcement.json", {"enabled": server.enforce_replay,
         "native_generation_events": server.native_generation_events, "replay_rejections": server.replay_rejections})
    if spec.get("final_codec"):
        actual_initial = json.loads((out / "response-00.json").read_text())["content"]
        dump(out / "fixture.json", {"spec": spec, "binding": BINDING, "session_id": native_session_id,
             "contents": native, "tool_projections": tool_projections, "projected": actual_initial,
             "completion_native": COMPLETION_NATIVE})
    records = [r for r in server.records if r["path"].split("?", 1)[0] == "/v1/messages"]
    observations = []
    golden = []
    for record in records:
        binding = dict(BINDING)
        if record["phase"] == "switch":
            binding["native_model"] = "gemini-4-argon-high"
        observation = observe_record(out, record, native, binding, all_turns=spec.get("final_codec", False))
        if record["phase"] == "switch" and observation.get("history_present"):
            original_decode = fresh_decode(out, "switch-origin-binding", first_assistant_turn(record["body"]["messages"]), BINDING)
            observation["origin_binding_decodes"] = original_decode["ok"]
            # Separate negative control: do NOT inject these old blocks into the
            # actual switch request. This proves strict rejection independently
            # of any carrier stripping by the real client.
            prior = next(r for r in reversed(records[:records.index(record)]) if r["phase"] == "resume")
            control = fresh_decode(out, "switch-strict-binding-control", first_assistant_turn(prior["body"]["messages"]), binding)
            observation["strict_binding_control_rejected"] = not control["ok"] and "binding" in control.get("error", "")
        observations.append(observation)
        golden.append({"index": record["index"], "phase": record["phase"], "wire": record["wire"],
                       "headers": record["headers"], "messages": record["body"].get("messages", [])})
    dump(out / "golden-request-vectors.json", golden)
    histories = [o for o in observations if o.get("history_present") and o["phase"] in ("initial", "resume")]
    resume_seen = any(o["phase"] == "resume" for o in histories)
    expected_error_phases = {r["phase"] for r in server.replay_rejections}
    processes_ok = all(not r["timeout"] and (
        (r["returncode"] != 0 or r.get("cli_result_is_error") is True) if phase in expected_error_phases
        else r["returncode"] == 0) for phase, r in runs.items())
    retention_passed = bool(histories) and resume_seen and all(o["decoded"] and o["native_exact"] for o in histories)
    if negative:
        passed = bool(histories) and resume_seen and all(not o["decoded"] for o in histories) and processes_ok
    else:
        passed = retention_passed and processes_ok
    if spec.get("same_process_followup"):
        passed = passed and runs["initial"].get("completed_same_process_user_turns") == 2 and any(o["phase"] == "initial" for o in histories)
    if any("functionCall" in p for c in native for p in c["parts"]):
        read_observed = any(t["fixture_read"] and not t["is_error"] for o in histories for t in o["tool_results"])
        if spec.get("expected_tool_error"):
            passed = passed and expected_tool_error_retained(histories)
        else:
            passed = passed and read_observed
    else:
        read_observed = None
    rejection_indices = {r["request"] for r in server.replay_rejections}
    rejected_callback_count = sum(e["request"] in rejection_indices for e in server.native_generation_events)
    if spec.get("enforce_replay") and negative:
        passed = passed and bool(server.replay_rejections) and rejected_callback_count == 0
    if spec.get("switch"):
        switches = [o for o in observations if o["phase"] == "switch" and o.get("history_present")]
        switch_requests = [r for r in records if r["phase"] == "switch"]
        passed = (passed and bool(switches) and all(not o["decoded"] and o.get("strict_binding_control_rejected") for o in switches)
                  and bool(switch_requests) and all(r.get("response_status") == 400 for r in switch_requests)
                  and not any(e["phase"] == "switch" for e in server.native_generation_events))
    if spec.get("final_codec"):
        all_histories = [o for o in observations if o.get("history_present")]
        passed = passed and bool(all_histories) and all(o.get("history_decoder_ok") and o.get("history_session_id") == native_session_id for o in all_histories)
    expected_rejection = bool(spec.get("expected_tool_error") or negative or spec.get("switch"))
    summary = {"case": name, "spec": spec, "status": ("EXPECTED_REJECTION" if expected_rejection else "PASS") if passed else "FAIL", "session_id": session,
               "same_model_retention": "PASS" if retention_passed else ("EXPECTED_REJECTION" if negative and passed else "FAIL"),
               "processes": runs, "requests": len(records), "stream_values": sorted(set(str(r["body"].get("stream")) for r in records)),
               "observations": observations, "fixture_read_observed": read_observed,
               "expected_tool_error_with_exact_replay": expected_tool_error_retained(histories) if spec.get("expected_tool_error") else None,
               "endpoint_enforcement": {"enabled": server.enforce_replay, "rejections": server.replay_rejections,
                    "generation_callbacks_on_rejected_requests": rejected_callback_count,
                    "generation_callbacks_by_phase": {phase: sum(e["phase"] == phase for e in server.native_generation_events) for phase in runs}},
               "compaction": "UNVERIFIED" if spec.get("compact") else "not-attempted"}
    if spec.get("compact"):
        # A successful process exit or post-command replay does not prove compaction.
        compact_output = json.loads((out / "compact-stdout.json").read_text())
        summary["compaction_command_result"] = compact_output.get("result")
        boundaries = []
        for transcript in (out / "config" / "projects").rglob("*.jsonl"):
            for line in transcript.read_text().splitlines():
                value = json.loads(line)
                if value.get("subtype") == "compact_boundary":
                    boundaries.append(value)
        post_observations = [o for o in observations if o["phase"] == "post-compact"]
        summary["compaction"] = compaction_status(len(boundaries), post_observations, spec.get("signed_completions", False))
        summary["status"] = summary["compaction"] if passed else "FAIL"
        summary["compaction_boundary_count"] = len(boundaries)
        dump(out / "compaction-evidence.json", {"boundaries": boundaries,
             "signed_completions": server.signed_completions, "post_observations": post_observations,
             "note": "Fixture summary is synthetic, not a quality test of compaction or Google native compatibility."})
    dump(out / "summary.json", summary)
    return summary


def run_catalog(root):
    """Separate catalog-only session: advertise tools, NEVER emit tool calls."""
    out = root / "catalog-default"
    out.mkdir(parents=True, exist_ok=False)
    for directory in ("home", "config", "tmp"):
        (out / directory).mkdir()
    server = FixtureServer(out, [], catalog_only=True)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    env = clean_env(out, server.server_port)
    settings = {"disableAllHooks": True, "env": {"ANTHROPIC_API_KEY": env["ANTHROPIC_API_KEY"], "ANTHROPIC_BASE_URL": env["ANTHROPIC_BASE_URL"]}}
    command = cli_command(LOW, settings, "Synthetic catalog-only probe. Respond with the prepared text and do not call any tool.",
                          session_id=str(uuid.uuid4()), bare=False, tools="default")
    try:
        process = run_cli(out, "catalog", command, env)
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)
    records = [r for r in server.records if r["path"].split("?", 1)[0] == "/v1/messages"]
    catalogs = []
    no_calls = True
    for record in records:
        tools = record["body"].get("tools", [])
        catalogs.append({"request": record["index"], "complete_tool_definitions": tools, "inventory": schema_inventory(tools), "wire": record["wire"]})
        response = json.loads((out / ("response-%02d.json" % record["index"])).read_text())
        no_calls = no_calls and response["stop_reason"] == "end_turn" and all(b["type"] == "text" for b in response["content"])
    dump(out / "tool-catalog.json", catalogs)
    inventory = schema_inventory([t for r in records for t in r["body"].get("tools", [])])
    passed = bool(records) and len(inventory["tool_names"]) > 1 and no_calls and process["returncode"] == 0 and not process["timeout"]
    summary = {"case": "catalog-default", "status": "PASS_CATALOG_ONLY" if passed else "FAIL",
               "separate_from_replay_matrix": True, "process": process, "requests": len(records),
               "server_tool_calls_emitted": 0 if no_calls else "unexpected", "tool_names": inventory["tool_names"],
               "schema_keywords": inventory["schema_keywords"], "schema_types": inventory["schema_types"],
               "tool_type_fields": inventory["tool_type_fields"], "tool_top_level_fields": inventory["tool_top_level_fields"]}
    dump(out / "summary.json", summary)
    return summary


def safe_identity():
    """Admit only a positive, matching real/effective caller identity."""
    if sys.platform != "linux":
        return False
    values = [getattr(os, name)() for name in ("getuid", "geteuid", "getgid", "getegid")]
    return (all(type(value) is int and value > 0 for value in values)
            and values[0] == values[1] and values[2] == values[3])


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("cases", nargs="+", choices=list(case_specs()) + ["matrix", "catalog-default"])
    parser.add_argument("--workers", type=int, default=6)
    args = parser.parse_args()
    if not safe_identity() or not Path("/opt/fixture-cli").is_file() or not Path("/out").is_dir():
        raise SystemExit("Run only via run_container.py in the isolated fixture container")
    dump(Path("/out/isolation.json"), {"uid": os.getuid(), "gid": os.getgid(), "pid": os.getpid(),
          "network_interfaces": socket.if_nameindex(),
          "net_namespace": os.readlink("/proc/self/ns/net"), "pid_namespace": os.readlink("/proc/self/ns/pid"),
          "container_env_keys": sorted(os.environ),
          "routes": Path("/proc/net/route").read_text()})
    if args.cases == ["catalog-default"]:
        result = run_catalog(Path("/out"))
        dump(Path("/out/catalog-result.json"), result)
        print(compact(result), flush=True)
        return 0 if result["status"] == "PASS_CATALOG_ONLY" else 1
    if "catalog-default" in args.cases:
        raise SystemExit("Run catalog-default separately from the replay matrix")
    cases = case_specs()
    names = [n for n in cases if n != "minimal"] if "matrix" in args.cases else args.cases
    summaries = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as executor:
        futures = {executor.submit(run_case, name, cases[name], Path("/out")): name for name in names}
        for future in concurrent.futures.as_completed(futures):
            name = futures[future]
            try:
                summary = future.result()
            except Exception as exc:
                summary = {"case": name, "status": "HARNESS_ERROR", "error": repr(exc)}
            summaries.append(summary)
            print(compact({"case": name, "status": summary["status"], "error": summary.get("error")}), flush=True)
    dump(Path("/out/matrix.json"), sorted(summaries, key=lambda s: s["case"]))
    return 0 if all(s["status"] in ("PASS", "EXPECTED_REJECTION") for s in summaries) else 1


if __name__ == "__main__":
    sys.exit(main())
