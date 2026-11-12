"""Optional, synthetic Stage A tests; not a native provider implementation."""
import copy
import json
import math
import unittest

from codec import (PREFIX, ReplayError, canonical, decode_turn, decode_history, digest, project_turn,
                   response_json, response_sse, validate_session_id, mint_session_id)

BINDING = {"profile_id": "synthetic-profile", "project_id": "synthetic-project",
           "native_model": "gemini-4-argon-low"}


def native_fixture():
    return [
        {"role": "model", "parts": [
            {"thought": True, "text": "synthetic thought α", "thoughtSignature": "fake-thought-1"},
            {"text": "visible 雪"},
            {"thought": True, "text": " / synthetic thought β"},
            {"functionCall": {"name": "Read", "args": {"file_path": "/fixture/read.txt", "offset": 1.0}},
             "thoughtSignature": "fake-call-signed"},
            {"functionCall": {"name": "Read", "id": "native-second", "args": {"file_path": "/fixture/read.txt", "limit": 1}}},
        ], "unknown": {"preserve": [True, None, "é"]}},
        {"role": "model", "parts": [{"thoughtSignature": "fake-late-signature"}, {"text": ""}]},
    ]


class CodecTests(unittest.TestCase):
    def test_single_leading_thought_exact_native_and_mapping(self):
        native = native_fixture()
        blocks = project_turn(native, BINDING)
        self.assertEqual(blocks[0]["thinking"], "synthetic thought α / synthetic thought β")
        self.assertEqual(sum(b["type"] == "thinking" for b in blocks), 1)
        self.assertTrue(blocks[0]["signature"].startswith(PREFIX))
        decoded = decode_turn(blocks, BINDING)
        self.assertEqual(json.dumps(decoded["contents"]), json.dumps(native))
        self.assertFalse(decoded["tool_mapping"][0]["native_id_present"])
        self.assertNotIn("native_id", decoded["tool_mapping"][0])
        self.assertEqual(decoded["tool_mapping"][1]["native_id"], "native-second")
        self.assertNotIn("id", decoded["contents"][0]["parts"][3]["functionCall"])

    def test_empty_carrier_never_fabricates_thought(self):
        for parts in ([{"functionCall": {"name": "Read", "args": {"file_path": "/fixture/read.txt"}}, "thoughtSignature": "fake"}],
                      [{"text": "signed ordinary text", "thoughtSignature": "fake"}],
                      [{"thoughtSignature": "fake-signature-only"}]):
            with self.subTest(parts=parts):
                native = [{"role": "model", "parts": parts}]
                blocks = project_turn(native, BINDING)
                self.assertEqual(blocks[0]["thinking"], "")
                self.assertEqual(decode_turn(blocks, BINDING)["contents"], native)

    def test_binary64_number_key_order_unicode_normalization(self):
        self.assertEqual(canonical({"雪": [1, -0.0, 1e-3], "a": "é"}),
                         canonical({"a": "é", "雪": [1.0, 0, 0.001]}))
        self.assertNotEqual(canonical("é"), canonical("é"))
        self.assertNotEqual(canonical([1, 2]), canonical([2, 1]))
        self.assertNotEqual(canonical(True), canonical(1))
        for value in (math.nan, math.inf, -math.inf, 2**53, -(2**53), float(2**53)):
            with self.subTest(value=value), self.assertRaises(ReplayError):
                canonical(value)

    def test_digest_excludes_only_block_signatures_not_tool_argument_keys(self):
        blocks = project_turn(native_fixture(), BINDING)
        metadata = decode_turn(blocks, BINDING)
        bound_digest = lambda value: digest(value, BINDING, metadata["session_id"], metadata["thought_display"], metadata["tool_mapping"])
        altered = copy.deepcopy(blocks)
        altered[0]["signature"] = "irrelevant-to-digest"
        self.assertEqual(bound_digest(blocks), bound_digest(altered))
        tool = next(b for b in altered if b["type"] == "tool_use")
        tool["input"]["signature"] = "must-be-hashed"
        self.assertNotEqual(bound_digest(blocks), bound_digest(altered))
        altered[0]["thinking"] += "changed"
        self.assertNotEqual(bound_digest(blocks), bound_digest(altered))

    def test_adjacent_text_merging_on_both_sides_only(self):
        native = [{"role": "model", "parts": [{"text": "a"}, {"text": "b"}]}]
        blocks = project_turn(native, BINDING)
        merged = [blocks[0], {"type": "text", "text": "ab"}]
        self.assertEqual(decode_turn(merged, BINDING)["contents"], native)

    def test_negative_controls(self):
        blocks = project_turn(native_fixture(), BINDING)
        variants = []
        variants.append(blocks[1:])
        variants.append([blocks[0], blocks[0]] + blocks[1:])
        changed = copy.deepcopy(blocks)
        next(b for b in changed if b["type"] == "tool_use")["input"]["offset"] = 2
        variants.append(changed)
        changed = copy.deepcopy(blocks)
        changed[2], changed[3] = changed[3], changed[2]
        variants.append(changed)
        changed = copy.deepcopy(blocks)
        changed[0]["signature"] = PREFIX + "!not-base64!"
        variants.append(changed)
        for value in variants:
            with self.subTest(value=value), self.assertRaises(ReplayError):
                decode_turn(value, BINDING)
        for key in BINDING:
            with self.subTest(key=key), self.assertRaisesRegex(ReplayError, "binding"):
                decode_turn(blocks, dict(BINDING, **{key: "foreign"}))

    def test_low_to_high_never_relaxes_native_model_binding(self):
        blocks = project_turn(native_fixture(), BINDING)
        with self.assertRaisesRegex(ReplayError, "binding"):
            decode_turn(blocks, dict(BINDING, native_model="gemini-4-argon-high"))

    def test_unsafe_tool_numbers_fail_before_emission(self):
        native = native_fixture()
        native[0]["parts"][3]["functionCall"]["args"]["offset"] = 2**53
        with self.assertRaises(ReplayError):
            project_turn(native, BINDING)

    def test_native_id_reuse_never_reuses_local_ids(self):
        native = native_fixture()
        native[0]["parts"][3]["functionCall"]["id"] = "native-second"
        turns = [project_turn(native, BINDING, session_id="11") for _ in range(2)]
        ids = [b["id"] for blocks in turns for b in blocks if b["type"] == "tool_use"]
        self.assertEqual(len(set(ids)), 4)
        self.assertEqual(decode_history(turns, BINDING)["session_id"], "11")

    def test_ambiguous_parallel_idless_calls_rejected(self):
        native = native_fixture()
        del native[0]["parts"][4]["functionCall"]["id"]
        with self.assertRaisesRegex(ReplayError, "ambiguous"):
            project_turn(native, BINDING)

    def test_bounds_and_malformed_json(self):
        blocks = project_turn(native_fixture(), BINDING)
        with self.assertRaisesRegex(ReplayError, "encoded"):
            decode_turn(blocks, BINDING, max_encoded=20)
        with self.assertRaisesRegex(ReplayError, "decoded"):
            decode_turn(blocks, BINDING, max_decoded=20)
        with self.assertRaisesRegex(ReplayError, "depth"):
            canonical([[[[0]]]], max_depth=2)

    def test_unknown_field_on_merged_text_is_not_silently_dropped(self):
        with self.assertRaisesRegex(ReplayError, "unknown"):
            digest([{"type": "text", "text": "a"}, {"type": "text", "text": "b", "unknown": 1}], BINDING, "1", "full", [])

    def test_huge_integer_is_rejected_without_float_overflow(self):
        with self.assertRaises(ReplayError):
            canonical(2**4096)

    def test_exact_size_boundaries_and_tampered_mapping(self):
        import base64
        blocks = project_turn(native_fixture(), BINDING)
        encoded = blocks[0]["signature"][len(PREFIX):]
        raw = base64.urlsafe_b64decode(encoded + "=" * (-len(encoded) % 4))
        decode_turn(blocks, BINDING, max_encoded=len(encoded), max_decoded=len(raw))
        with self.assertRaisesRegex(ReplayError, "encoded"):
            decode_turn(blocks, BINDING, max_encoded=len(encoded) - 1)
        with self.assertRaisesRegex(ReplayError, "decoded"):
            decode_turn(blocks, BINDING, max_decoded=len(raw) - 1)
        envelope = json.loads(raw)
        envelope["tool_mapping"][0]["part_index"] += 1
        blocks[0]["signature"] = PREFIX + base64.urlsafe_b64encode(json.dumps(envelope).encode()).decode().rstrip("=")
        with self.assertRaisesRegex(ReplayError, "mapping"):
            decode_turn(blocks, BINDING)

    def test_separate_responses_have_distinct_message_ids(self):
        first = response_json([{"type": "text", "text": "a"}], "synthetic")
        second = response_json([{"type": "text", "text": "b"}], "synthetic")
        self.assertNotEqual(first["id"], second["id"])

    def test_json_sse_semantic_equivalence_fixture_only(self):
        blocks = project_turn(native_fixture(), BINDING)
        message = response_json(blocks, "synthetic-selector")
        events = [json.loads(frame.split("data: ", 1)[1]) for frame in response_sse(message).decode().split("\n\n") if frame]
        rebuilt = []
        for event in events:
            if event["type"] == "content_block_start":
                rebuilt.append(copy.deepcopy(event["content_block"]))
            elif event["type"] == "content_block_delta":
                block, delta = rebuilt[event["index"]], event["delta"]
                for kind in ("text", "thinking", "signature"):
                    if kind in delta:
                        block[kind] = block.get(kind, "") + delta[kind]
                if "partial_json" in delta:
                    block["input"] = json.loads(delta["partial_json"])
        self.assertEqual(rebuilt, message["content"])
        self.assertEqual(events[-1]["type"], "message_stop")
        self.assertEqual(events[-2]["delta"]["stop_reason"], "tool_use")


class FinalV1Tests(unittest.TestCase):
    def test_completed_parameterless_call_preserves_absent_args(self):
        native = [{"role": "model", "parts": [{"functionCall": {"name": "empty"}, "thoughtSignature": "synthetic-signature"}]}]
        recipe = [{"caller_name": "empty", "local_id": "toolu_parameterless_golden", "codec_paths": [], "root_wrapper": None}]
        blocks = project_turn(native, BINDING, session_id="19", tool_projections=recipe)
        self.assertEqual(blocks[1]["input"], {})
        restored = decode_turn(blocks, BINDING)
        self.assertEqual(restored["contents"], native)
        self.assertNotIn("args", restored["contents"][0]["parts"][0]["functionCall"])
        for value in (None, [], "", 1):
            changed = copy.deepcopy(native)
            changed[0]["parts"][0]["functionCall"]["args"] = value
            with self.subTest(args=value), self.assertRaises(ReplayError):
                project_turn(changed, BINDING, session_id="19")
        for key in ("partialArgs", "willContinue"):
            changed = copy.deepcopy(native)
            changed[0]["parts"][0]["functionCall"][key] = True
            with self.subTest(key=key), self.assertRaises(ReplayError):
                project_turn(changed, BINDING, session_id="19")
        # Optional synthetic-vector export from this unit test, never a CLI run.
        import os
        if os.environ.get("ANTIGRAVITY_UNIT_GOLDEN_STDOUT") == "1":
            from codec import compact, digest_input
            canonical_input = digest_input(blocks, BINDING, "19", "full", restored["tool_mapping"])
            print(compact({"name": "parameterless-omitted-args", "binding": BINDING,
                  "native_contents": native, "session_id": "19", "thought_display": "full",
                  "tool_projections": recipe, "projected_blocks": blocks, "envelope": restored,
                  "canonical_digest_input_utf8": canonical_input.decode("utf-8"),
                  "canonical_digest_input_hex": canonical_input.hex(), "sha256": restored["projection_digest"]}))

    @staticmethod
    def wrapped_fixture(root=False):
        args = {"json": json.dumps({"file_path": "/fixture/read.txt", "offset": 1.0})} if root else {
            "file_path": json.dumps("/fixture/read.txt"), "offset": 1.0}
        native = [{"role": "model", "parts": [
            {"thought": True, "text": "Genuine synthetic native thought 雪", "thoughtSignature": "raw-thought"},
            {"functionCall": {"name": "native_read_alias", "id": "reused-native-id", "args": args}, "thoughtSignature": "raw-call"}]}]
        recipe = [{"caller_name": "Read", "codec_paths": [] if root else ["/file_path"],
                   "root_wrapper": "json" if root else None}]
        return native, recipe

    @staticmethod
    def replace_envelope(blocks, mutate):
        import base64
        copied = copy.deepcopy(blocks)
        encoded = copied[0]["signature"][len(PREFIX):]
        envelope = json.loads(base64.urlsafe_b64decode(encoded + "=" * (-len(encoded) % 4)))
        mutate(envelope)
        copied[0]["signature"] = PREFIX + base64.urlsafe_b64encode(json.dumps(envelope).encode()).decode().rstrip("=")
        return copied

    def test_session_id_canonical_nonzero_i64(self):
        for value in ("1", "-1", "9223372036854775807", "-9223372036854775808"):
            self.assertEqual(validate_session_id(value), value)
        for value in ("0", "-0", "01", "-01", "+1", " 1", "1 ", "1.0", "1e0", "9223372036854775808", "-9223372036854775809", 1, None):
            with self.subTest(value=value), self.assertRaises(ReplayError):
                validate_session_id(value)
        minted = [mint_session_id() for _ in range(20)]
        self.assertEqual(len(set(minted)), 20)
        for value in minted:
            validate_session_id(value)

    def test_omitted_display_keeps_genuine_native_thought_and_codec_raw_strings(self):
        native, recipe = self.wrapped_fixture()
        original = json.dumps(native)
        blocks = project_turn(native, BINDING, session_id="-37", thought_display="omitted", tool_projections=recipe)
        self.assertEqual(blocks[0]["thinking"], "")
        self.assertEqual(blocks[1]["name"], "Read")
        self.assertEqual(blocks[1]["input"], {"file_path": "/fixture/read.txt", "offset": 1.0})
        result = decode_turn(blocks, BINDING, expected_session_id="-37")
        self.assertEqual(result["thought_display"], "omitted")
        self.assertEqual(result["session_id"], "-37")
        self.assertEqual(result["tool_mapping"][0]["caller_name"], "Read")
        self.assertEqual(result["tool_mapping"][0]["codec_paths"], ["/file_path"])
        self.assertIsNone(result["tool_mapping"][0]["root_wrapper"])
        self.assertEqual(json.dumps(result["contents"]), original)
        self.assertEqual(json.dumps(native), original)

    def test_root_json_codec_and_full_display(self):
        native, recipe = self.wrapped_fixture(root=True)
        blocks = project_turn(native, BINDING, session_id="37", thought_display="full", tool_projections=recipe)
        self.assertEqual(blocks[0]["thinking"], native[0]["parts"][0]["text"])
        self.assertEqual(blocks[1]["input"], {"file_path": "/fixture/read.txt", "offset": 1.0})
        result = decode_turn(blocks, BINDING)
        self.assertEqual(result["contents"], native)
        self.assertEqual(result["tool_mapping"][0]["root_wrapper"], "json")

    def test_metadata_session_display_mapping_bound_into_digest(self):
        native, recipe = self.wrapped_fixture()
        blocks = project_turn(native, BINDING, session_id="7", thought_display="omitted", tool_projections=recipe)
        mutations = [lambda e: e.update(session_id="8"), lambda e: e.update(thought_display="full"),
                     lambda e: e["tool_mapping"][0].update(caller_name="Other"),
                     lambda e: e["tool_mapping"][0].update(local_id="toolu_changed"),
                     lambda e: e["tool_mapping"][0].update(codec_paths=[])]
        for mutation in mutations:
            with self.subTest(mutation=mutation), self.assertRaises(ReplayError):
                decode_turn(self.replace_envelope(blocks, mutation), BINDING)
        for key, value in (("name", "Other"), ("id", "toolu_changed"), ("input", {"file_path": "/fixture/changed.txt", "offset": 1})):
            altered = copy.deepcopy(blocks)
            altered[1][key] = value
            with self.subTest(key=key), self.assertRaisesRegex(ReplayError, "digest"):
                decode_turn(altered, BINDING)

    def test_conflicting_history_sessions_and_local_ids_fail(self):
        first = project_turn(native_fixture(), BINDING, session_id="17")
        second = project_turn(native_fixture(), BINDING, session_id="18")
        with self.assertRaisesRegex(ReplayError, "session"):
            decode_history([first, second], BINDING)
        with self.assertRaisesRegex(ReplayError, "session"):
            decode_turn(first, BINDING, expected_session_id="18")
        with self.assertRaisesRegex(ReplayError, "local ID"):
            decode_history([first, first], BINDING)
        self.assertEqual(decode_history([first, project_turn(native_fixture(), BINDING, session_id="17")], BINDING)["session_id"], "17")

    def test_malformed_leaf_json_fails_before_projection(self):
        for raw in ("bad json", "NaN", "1e309", "{\"x\":1,\"x\":2}", "9007199254740992", 123):
            native, recipe = self.wrapped_fixture()
            native[0]["parts"][1]["functionCall"]["args"]["file_path"] = raw
            with self.subTest(raw=raw), self.assertRaises(ReplayError):
                project_turn(native, BINDING, session_id="1", tool_projections=recipe)

    def test_root_wrapper_requires_exact_key_and_decoded_object(self):
        bad_args = [{}, {"json": "{}", "extra": 1}, {"json": "[]"}, {"json": "null"}, {"json": "bad"}, {"json": 1}]
        for args in bad_args:
            native, recipe = self.wrapped_fixture(root=True)
            native[0]["parts"][1]["functionCall"]["args"] = args
            with self.subTest(args=args), self.assertRaises(ReplayError):
                project_turn(native, BINDING, session_id="1", tool_projections=recipe)
        native, recipe = self.wrapped_fixture(root=True)
        recipe[0]["codec_paths"] = ["/json"]
        with self.assertRaises(ReplayError):
            project_turn(native, BINDING, session_id="1", tool_projections=recipe)

    def test_codec_paths_are_concrete_nonoverlapping_bounded_argument_pointers(self):
        args = {"file_path": '"/fixture/read.txt"', "payload": {"x": "1"}, "array": ["1"], "a/b": {"~key": ['{\"x\":1,\"s\":\"雪\"}']}}
        bad_paths = [["/missing"], ["/file_path", "/file_path"], ["/payload", "/payload/x"],
                     ["/array/9"], ["/array/-"], ["/array/01"], ["/a~2b"], [""], ["file_path"],
                     ["/functionCall/args"], ["/x" + str(i) for i in range(65)]]
        for paths in bad_paths:
            native, recipe = self.wrapped_fixture()
            native[0]["parts"][1]["functionCall"]["args"] = copy.deepcopy(args)
            recipe[0]["codec_paths"] = paths
            with self.subTest(paths=paths), self.assertRaises(ReplayError):
                project_turn(native, BINDING, session_id="1", tool_projections=recipe)
        native, recipe = self.wrapped_fixture()
        native[0]["parts"][1]["functionCall"]["args"] = copy.deepcopy(args)
        recipe[0]["codec_paths"] = ["/a~1b/~0key/0"]
        blocks = project_turn(native, BINDING, session_id="1", tool_projections=recipe)
        self.assertEqual(blocks[1]["input"]["a/b"]["~key"][0], {"x": 1, "s": "雪"})
        self.assertEqual(decode_turn(blocks, BINDING)["contents"], native)

    def test_invalid_display_and_mapping_recipes_are_rejected(self):
        native, recipe = self.wrapped_fixture()
        for display in ("hidden", "", None):
            with self.subTest(display=display), self.assertRaises(ReplayError):
                project_turn(native, BINDING, session_id="1", thought_display=display, tool_projections=recipe)
        for changed in ([], [{"caller_name": "", "codec_paths": [], "root_wrapper": None}],
                        [{"caller_name": "Read", "codec_paths": [], "root_wrapper": None, "unknown": 1}]):
            with self.subTest(recipe=changed), self.assertRaises(ReplayError):
                project_turn(native, BINDING, session_id="1", tool_projections=changed)


if __name__ == "__main__":
    unittest.main()
