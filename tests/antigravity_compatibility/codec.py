"""Test-only Stage A replay codec. No network/auth/provider implementation.

Digest specification: sha256(UTF-8 compact JSON of tagged canonical values).
Objects become ["object", sorted [key, value] pairs]; arrays stay ordered;
scalars have explicit tags. Numbers are big-endian IEEE754 binary64 hex, with
negative zero normalized to zero. Unsafe integral numbers/nonfinite values fail.
Only adjacent projected text blocks merge. No Unicode normalization is applied.
Only block-level signature/cache_control fields are excluded, never tool args.
The digest binding contains profile_id/project_id/native_model/session_id,
thought_display and the complete ordered tool_mapping. Each mapping explicitly
records caller_name/local_id, native position/ID presence, codec_paths (strict
RFC6901 pointers into RAW native args) and root_wrapper (null or one exact key).
Root-wrapper and leaf-path modes are mutually exclusive. Native contents are
never rewritten; only the client projection decodes JSON-string transport leaves.
This is consistency checking, NOT authentication of arbitrary API callers.
"""
import base64
import copy
import hashlib
import json
import math
import re
import secrets
import struct
import uuid

PREFIX = "ccp:antigravity:v1:"
MAX_ENCODED = 12 * 1024 * 1024
MAX_DECODED = 8 * 1024 * 1024
MAX_DEPTH = 64
MAX_CONTENTS = 512
MAX_PARTS = 4096
MAX_TOOLS = 128
SAFE_INTEGER = 2**53 - 1
BINDING_KEYS = ("profile_id", "project_id", "native_model")
ENVELOPE_KEYS = {"version", *BINDING_KEYS, "session_id", "thought_display", "contents", "tool_mapping", "projection_digest"}
MAPPING_KEYS = {"local_id", "caller_name", "content_index", "part_index", "native_id_present", "codec_paths", "root_wrapper"}
MAX_CODEC_PATHS = 64
MAX_CODEC_PATH_BYTES = 1024
MAX_ARGUMENT_BYTES = 1024 * 1024


class ReplayError(ValueError):
    pass


def compact(value):
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), allow_nan=False)


def canonical(value, max_depth=MAX_DEPTH):
    def visit(v, depth):
        if depth > max_depth:
            raise ReplayError("JSON depth limit")
        if v is None:
            return ["null"]
        if isinstance(v, bool):
            return ["bool", v]
        if isinstance(v, (int, float)):
            if isinstance(v, int) and abs(v) > SAFE_INTEGER:
                raise ReplayError("unsafe integer")
            if not math.isfinite(v) or (v == int(v) and abs(v) > SAFE_INTEGER):
                raise ReplayError("unsafe/nonfinite number")
            f = float(v)
            return ["number", struct.pack(">d", 0.0 if f == 0 else f).hex()]
        if isinstance(v, str):
            return ["string", v]
        if isinstance(v, list):
            return ["array", [visit(x, depth + 1) for x in v]]
        if isinstance(v, dict) and all(isinstance(k, str) for k in v):
            return ["object", [[k, visit(v[k], depth + 1)] for k in sorted(v)]]
        raise ReplayError("unsupported JSON type")
    return compact(visit(value, 0)).encode("utf-8")


def binding_fields(binding):
    if set(binding) != set(BINDING_KEYS) or not all(isinstance(v, str) and v for v in binding.values()):
        raise ReplayError("invalid binding")
    return dict(binding)


def projection_values(blocks):
    result = []
    for block in blocks:
        kind = block.get("type")
        allowed = {"type", "signature", "cache_control"}
        if kind == "thinking":
            allowed.add("thinking")
            if not isinstance(block.get("thinking"), str):
                raise ReplayError("invalid thinking")
            item = {"type": kind, "thinking": block["thinking"]}
        elif kind == "text":
            allowed.add("text")
            if not isinstance(block.get("text"), str):
                raise ReplayError("invalid text")
            item = {"type": kind, "text": block["text"]}
            if result and result[-1]["type"] == "text":
                if set(block) - allowed:
                    raise ReplayError("unknown projection fields")
                result[-1]["text"] += item["text"]
                continue
        elif kind == "tool_use":
            allowed.update(("id", "name", "input"))
            if not all(isinstance(block.get(k), str) and block[k] for k in ("id", "name")) or not isinstance(block.get("input"), dict):
                raise ReplayError("invalid tool")
            item = {k: copy.deepcopy(block[k]) for k in ("type", "id", "name", "input")}
        else:
            raise ReplayError("unsupported projection block")
        if set(block) - allowed:
            raise ReplayError("unknown projection fields")
        result.append(item)
    return result


def validate_session_id(value):
    if not isinstance(value, str) or len(value) > 20 or not re.fullmatch(r"-?[1-9][0-9]*", value):
        raise ReplayError("session_id must be canonical nonzero i64 decimal string")
    if not -(2**63) <= int(value) <= 2**63 - 1:
        raise ReplayError("session_id outside i64 range")
    return value


def mint_session_id():
    return str(secrets.randbelow(2**63 - 1) + 1)


def validate_display(value):
    if value not in ("full", "omitted"):
        raise ReplayError("invalid thought_display")
    return value


def digest_input(blocks, binding, session_id, thought_display, tool_mapping):
    metadata = {**binding_fields(binding), "session_id": validate_session_id(session_id),
                "thought_display": validate_display(thought_display), "tool_mapping": tool_mapping}
    return canonical({"binding": metadata, "blocks": projection_values(blocks)})


def digest(blocks, binding, session_id, thought_display, tool_mapping):
    return hashlib.sha256(digest_input(blocks, binding, session_id, thought_display, tool_mapping)).hexdigest()


def call_arguments(call):
    """Project absent completed-call args to {}, without mutating native contents."""
    if "partialArgs" in call or "willContinue" in call:
        raise ReplayError("incremental native function calls are unsupported")
    args = call.get("args", {})
    if not isinstance(args, dict):
        raise ReplayError("invalid native call arguments")
    return args


def collect_calls(contents):
    if not isinstance(contents, list) or len(contents) > MAX_CONTENTS:
        raise ReplayError("content limit")
    calls, idless_names, part_count = [], set(), 0
    for ci, content in enumerate(contents):
        if not isinstance(content, dict) or content.get("role") != "model" or not isinstance(content.get("parts"), list):
            raise ReplayError("invalid native content")
        part_count += len(content["parts"])
        if part_count > MAX_PARTS:
            raise ReplayError("part limit")
        for pi, part in enumerate(content["parts"]):
            if not isinstance(part, dict):
                raise ReplayError("invalid native part")
            if "text" in part and not isinstance(part["text"], str):
                raise ReplayError("invalid native text")
            if "functionCall" not in part:
                continue
            call = part["functionCall"]
            if not isinstance(call, dict) or not isinstance(call.get("name"), str) or not call["name"]:
                raise ReplayError("invalid native call")
            call_arguments(call)
            if len(calls) >= MAX_TOOLS:
                raise ReplayError("tool map limit")
            if "id" in call:
                if not isinstance(call["id"], str) or not call["id"]:
                    raise ReplayError("invalid native ID")
            elif call["name"] in idless_names:
                raise ReplayError("ambiguous same-name ID-less parallel calls")
            else:
                idless_names.add(call["name"])
            calls.append((ci, pi, call))
    return calls


def pointer_tokens(path):
    if not isinstance(path, str) or not path.startswith("/") or len(path.encode("utf-8")) > MAX_CODEC_PATH_BYTES:
        raise ReplayError("invalid codec path")
    tokens = []
    for token in path[1:].split("/"):
        if re.search(r"~(?![01])", token):
            raise ReplayError("invalid codec pointer escape")
        tokens.append(token.replace("~1", "/").replace("~0", "~"))
    if len(tokens) > MAX_DEPTH:
        raise ReplayError("codec path depth limit")
    return tokens


def decoded_codec_json(value):
    if not isinstance(value, str):
        raise ReplayError("codec leaf must be a JSON string")
    raw = value.encode("utf-8")
    if len(raw) > MAX_ARGUMENT_BYTES:
        raise ReplayError("codec JSON byte limit")
    try:
        decoded = parse_bounded(raw)
        canonical(decoded)  # finite/safe numbers, depth and JSON types
    except (ValueError, UnicodeError, RecursionError) as exc:
        raise ReplayError("invalid codec JSON") from exc
    return decoded


def decode_arguments(raw_args, paths, root_wrapper):
    if not isinstance(paths, list) or len(paths) > MAX_CODEC_PATHS:
        raise ReplayError("codec paths limit/type")
    if root_wrapper is not None:
        if not isinstance(root_wrapper, str) or not root_wrapper or len(root_wrapper.encode("utf-8")) > 256 or paths:
            raise ReplayError("invalid root wrapper metadata")
        if set(raw_args) != {root_wrapper}:
            raise ReplayError("root wrapper requires exactly one named key")
        result = decoded_codec_json(raw_args[root_wrapper])
        if not isinstance(result, dict):
            raise ReplayError("root codec must decode an object")
    else:
        parsed = [pointer_tokens(path) for path in paths]
        for i, left in enumerate(parsed):
            for right in parsed[i + 1:]:
                if left[:len(right)] == right or right[:len(left)] == left:
                    raise ReplayError("duplicate/overlapping codec paths")
        result = copy.deepcopy(raw_args)
        for tokens in parsed:
            parent = result
            for offset, token in enumerate(tokens):
                if isinstance(parent, dict):
                    if token not in parent:
                        raise ReplayError("codec path missing object key")
                    key = token
                elif isinstance(parent, list):
                    if not re.fullmatch(r"0|[1-9][0-9]*", token) or len(token) > 10:
                        raise ReplayError("invalid codec array index")
                    key = int(token)
                    if key >= len(parent):
                        raise ReplayError("codec path array index out of range")
                else:
                    raise ReplayError("codec path traverses a scalar")
                if offset == len(tokens) - 1:
                    parent[key] = decoded_codec_json(parent[key])
                else:
                    parent = parent[key]
    canonical(result)
    if len(compact(result).encode("utf-8")) > MAX_ARGUMENT_BYTES:
        raise ReplayError("decoded argument byte limit")
    return result


def make_mapping(contents, tool_projections):
    calls = collect_calls(contents)
    if tool_projections is not None and (not isinstance(tool_projections, list) or len(tool_projections) != len(calls)):
        raise ReplayError("tool projection recipe count mismatch")
    turn_id = uuid.uuid4().hex
    mapping = []
    for index, (ci, pi, call) in enumerate(calls):
        recipe = tool_projections[index] if tool_projections is not None else {}
        if not isinstance(recipe, dict) or set(recipe) - {"local_id", "caller_name", "codec_paths", "root_wrapper"}:
            raise ReplayError("invalid tool projection recipe")
        entry = {"local_id": recipe.get("local_id", "toolu_" + turn_id + "_%04d" % index),
                 "caller_name": recipe.get("caller_name", call["name"]), "content_index": ci, "part_index": pi,
                 "native_id_present": "id" in call, "codec_paths": copy.deepcopy(recipe.get("codec_paths", [])),
                 "root_wrapper": recipe.get("root_wrapper")}
        if "id" in call:
            entry["native_id"] = call["id"]
        mapping.append(entry)
    return mapping


def native_projection(contents, thought_display, tool_mapping):
    validate_display(thought_display)
    calls = collect_calls(contents)
    if not isinstance(tool_mapping, list) or len(tool_mapping) != len(calls):
        raise ReplayError("tool mapping count mismatch")
    projected_calls, local_ids = {}, set()
    for (ci, pi, call), entry in zip(calls, tool_mapping):
        expected_keys = MAPPING_KEYS | ({"native_id"} if "id" in call else set())
        if not isinstance(entry, dict) or set(entry) != expected_keys:
            raise ReplayError("invalid tool mapping fields")
        if (type(entry["content_index"]) is not int or type(entry["part_index"]) is not int
                or entry["content_index"] != ci or entry["part_index"] != pi
                or type(entry["native_id_present"]) is not bool or entry["native_id_present"] != ("id" in call)
                or ("id" in call and entry["native_id"] != call["id"])):
            raise ReplayError("tool mapping native position/ID mismatch")
        local_id, caller_name = entry["local_id"], entry["caller_name"]
        if not isinstance(local_id, str) or not re.fullmatch(r"[A-Za-z0-9_-]{1,128}", local_id) or local_id in local_ids:
            raise ReplayError("invalid/duplicate local ID in tool mapping")
        if not isinstance(caller_name, str) or not caller_name or len(caller_name.encode("utf-8")) > 1024:
            raise ReplayError("invalid caller name in tool mapping")
        local_ids.add(local_id)
        args = decode_arguments(call_arguments(call), entry["codec_paths"], entry["root_wrapper"])
        projected_calls[(ci, pi)] = {"type": "tool_use", "id": local_id, "name": caller_name, "input": args}
    thoughts, blocks = [], []
    for ci, content in enumerate(contents):
        for pi, part in enumerate(content["parts"]):
            if "text" in part:
                if part.get("thought") is True:
                    thoughts.append(part["text"])
                elif part["text"]:
                    blocks.append({"type": "text", "text": part["text"]})
            if (ci, pi) in projected_calls:
                blocks.append(projected_calls[(ci, pi)])
    text = "" if thought_display == "omitted" else "".join(thoughts)
    return [{"type": "thinking", "thinking": text}] + blocks


def project_turn(contents, binding, session_id=None, thought_display="full", tool_projections=None):
    session_id = mint_session_id() if session_id is None else validate_session_id(session_id)
    mapping = make_mapping(contents, tool_projections)
    blocks = native_projection(contents, thought_display, mapping)
    envelope = {"version": 1, **binding_fields(binding), "session_id": session_id, "thought_display": thought_display,
                "contents": copy.deepcopy(contents), "tool_mapping": mapping,
                "projection_digest": digest(blocks, binding, session_id, thought_display, mapping)}
    raw = compact(envelope).encode("utf-8")
    if len(raw) > MAX_DECODED:
        raise ReplayError("decoded size limit")
    encoded = base64.urlsafe_b64encode(raw).decode().rstrip("=")
    if len(encoded) > MAX_ENCODED:
        raise ReplayError("encoded size limit")
    blocks[0]["signature"] = PREFIX + encoded
    return blocks


def parse_bounded(raw):
    # Check nesting before json.loads can allocate a deeply nested tree.
    depth, quoted, escape = 0, False, False
    for ch in raw.decode("utf-8"):
        if quoted:
            if escape:
                escape = False
            elif ch == "\\":
                escape = True
            elif ch == '"':
                quoted = False
        elif ch == '"':
            quoted = True
        elif ch in "[{":
            depth += 1
            if depth > MAX_DEPTH:
                raise ReplayError("JSON depth limit")
        elif ch in "]}":
            depth -= 1
    def unique(pairs):
        obj = {}
        for key, value in pairs:
            if key in obj:
                raise ReplayError("duplicate JSON key")
            obj[key] = value
        return obj
    def bad_constant(value):
        raise ReplayError("nonfinite JSON number")
    return json.loads(raw, object_pairs_hook=unique, parse_constant=bad_constant)


def decode_turn(blocks, binding, max_encoded=MAX_ENCODED, max_decoded=MAX_DECODED, expected_session_id=None):
    binding_fields(binding)
    thinking = [b for b in blocks if b.get("type") == "thinking"]
    if len(thinking) != 1 or not blocks or blocks[0] is not thinking[0]:
        raise ReplayError("missing/duplicate/nonleading carrier")
    signature = thinking[0].get("signature", "")
    if not isinstance(signature, str) or not signature.startswith(PREFIX):
        raise ReplayError("missing/foreign carrier")
    encoded = signature[len(PREFIX):]
    if len(encoded) > max_encoded:
        raise ReplayError("encoded size limit")
    if not encoded or not re.fullmatch(r"[A-Za-z0-9_-]+", encoded):
        raise ReplayError("malformed carrier")
    if len(encoded) * 3 // 4 > max_decoded:
        raise ReplayError("decoded size limit")
    try:
        raw = base64.b64decode(encoded + "=" * (-len(encoded) % 4), altchars=b"-_", validate=True)
        if len(raw) > max_decoded:
            raise ReplayError("decoded size limit")
        envelope = parse_bounded(raw)
    except (ValueError, UnicodeError, RecursionError) as exc:
        raise ReplayError("malformed carrier: " + str(exc)) from exc
    if not isinstance(envelope, dict) or type(envelope.get("version")) is not int or envelope["version"] != 1 or set(envelope) != ENVELOPE_KEYS:
        raise ReplayError("invalid envelope schema/version")
    if any(envelope.get(k) != binding[k] for k in BINDING_KEYS):
        raise ReplayError("model/profile/project binding mismatch")
    session_id = validate_session_id(envelope["session_id"])
    if expected_session_id is not None and validate_session_id(expected_session_id) != session_id:
        raise ReplayError("session binding mismatch")
    display = validate_display(envelope["thought_display"])
    mapping = envelope["tool_mapping"]
    expected_blocks = native_projection(envelope["contents"], display, mapping)
    expected_digest = digest(expected_blocks, binding, session_id, display, mapping)
    if envelope["projection_digest"] != expected_digest or digest(blocks, binding, session_id, display, mapping) != expected_digest:
        raise ReplayError("projection digest mismatch")
    return envelope


def decode_history(turns, binding):
    if not isinstance(turns, list) or len(turns) > MAX_CONTENTS:
        raise ReplayError("history turn limit/type")
    envelopes, session_id, local_ids = [], None, set()
    for blocks in turns:
        envelope = decode_turn(blocks, binding)
        if session_id is not None and envelope["session_id"] != session_id:
            raise ReplayError("conflicting session IDs in history")
        session_id = envelope["session_id"]
        for entry in envelope["tool_mapping"]:
            if entry["local_id"] in local_ids:
                raise ReplayError("duplicate local ID across history turns")
            local_ids.add(entry["local_id"])
        envelopes.append(envelope)
    return {"session_id": session_id, "turns": envelopes}


def response_json(blocks, model):
    return {"id": "msg_stagea_" + uuid.uuid4().hex, "type": "message", "role": "assistant", "model": model,
            "content": copy.deepcopy(blocks),
            "stop_reason": "tool_use" if any(b["type"] == "tool_use" for b in blocks) else "end_turn",
            "stop_sequence": None, "usage": {"input_tokens": 32, "output_tokens": 24}}


def response_sse(message):
    events = []
    start = copy.deepcopy(message)
    start.update(content=[], stop_reason=None)
    start["usage"]["output_tokens"] = 0
    events.append({"type": "message_start", "message": start})
    for index, block in enumerate(message["content"]):
        kind = block["type"]
        if kind == "thinking":
            initial = {"type": kind, "thinking": "", "signature": ""}
            deltas = [{"type": "thinking_delta", "thinking": block["thinking"]},
                      {"type": "signature_delta", "signature": block["signature"]}]
        elif kind == "text":
            initial = {"type": kind, "text": ""}
            deltas = [{"type": "text_delta", "text": block["text"]}]
        else:
            initial = {"type": kind, "id": block["id"], "name": block["name"], "input": {}}
            deltas = [{"type": "input_json_delta", "partial_json": compact(block["input"])}]
        events.append({"type": "content_block_start", "index": index, "content_block": initial})
        for delta in deltas:
            events.append({"type": "content_block_delta", "index": index, "delta": delta})
        events.append({"type": "content_block_stop", "index": index})
    events.append({"type": "message_delta", "delta": {"stop_reason": message["stop_reason"], "stop_sequence": None},
                   "usage": {"output_tokens": message["usage"]["output_tokens"]}})
    events.append({"type": "message_stop"})
    return "".join("event: " + e["type"] + "\ndata: " + compact(e) + "\n\n" for e in events).encode("utf-8")


if __name__ == "__main__":
    # Fresh-decoder subprocess: only the echoed blocks and expected binding enter.
    import sys
    value = json.load(sys.stdin)
    try:
        if "turns" in value:
            result = decode_history(value["turns"], value["binding"])
            print(compact({"ok": True, "history": result}))
        else:
            result = decode_turn(value["blocks"], value["binding"], expected_session_id=value.get("expected_session_id"))
            print(compact({"ok": True, "envelope": result}))
    except ReplayError as exc:
        print(compact({"ok": False, "error": str(exc)}))
        sys.exit(2)
