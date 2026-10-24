"""Safety/observation tests written before the executable harness."""
import unittest
from pathlib import Path

from harness import (cli_command, clean_env, first_assistant_turn, assistant_turns, wire_fields,
                     reserialize_row, numeric_json, fixture, stream_cli_command,
                     expected_tool_error_retained, schema_inventory, compaction_status)
from codec import canonical
from run_container import docker_command


class HarnessTests(unittest.TestCase):
    def test_assistant_normalization_preserves_original_system_boundaries(self):
        from codec import project_turn, decode_history, ReplayError
        from test_codec import BINDING
        native = [{"role": "model", "parts": [{"text": "answer", "thoughtSignature": "synthetic"}]}]
        blocks = project_turn(native, BINDING, session_id="19")
        messages = [{"role": "assistant", "content": [blocks[0]]},
                    {"role": "assistant", "content": "answer"}]
        self.assertEqual(decode_history(assistant_turns(messages), BINDING)["turns"][0]["contents"], native)
        self.assertEqual(assistant_turns([{"role": "assistant", "content": []}] + messages), assistant_turns(messages))
        messages.insert(1, {"role": "system", "content": "real boundary"})
        self.assertEqual(len(assistant_turns(messages)), 2)
        with self.assertRaises(ReplayError):
            decode_history(assistant_turns(messages), BINDING)

    def test_distinct_adjacent_complete_turns_keep_their_carriers(self):
        from codec import project_turn, decode_history
        from test_codec import BINDING
        native = [{"role": "model", "parts": [{"text": "answer"}]}]
        blocks = project_turn(native, BINDING, session_id="19")
        turns = assistant_turns([{"role": "assistant", "content": blocks},
                                 {"role": "assistant", "content": blocks},
                                 {"role": "user", "content": "<system-reminder>ordinary user text</system-reminder>"}])
        self.assertEqual(len(turns), 2)
        self.assertEqual(len(decode_history(turns, BINDING)["turns"]), 2)

    def test_assistant_fragment_block_bound_is_cumulative(self):
        from codec import ReplayError
        with self.assertRaises(ReplayError):
            assistant_turns([{"role": "assistant", "content": [{"type": "text", "text": "x"}] * 4097},
                             {"role": "assistant", "content": "one more"}])

    def test_turn_extraction_never_repairs_missing_carrier(self):
        messages = [{"role": "user", "content": "fixture"},
                    {"role": "assistant", "content": [{"type": "text", "text": "one"}]},
                    {"role": "assistant", "content": [{"type": "tool_use", "id": "local"}]},
                    {"role": "user", "content": [{"type": "tool_result"}]},
                    {"role": "assistant", "content": [{"type": "thinking", "signature": "later"}]}]
        blocks = first_assistant_turn(messages)
        self.assertEqual([b["type"] for b in blocks], ["text", "tool_use"])

    def test_wire_observations_do_not_invent_omitted_fields(self):
        fields = wire_fields({"model": "antigravity/gemini-4-argon-low[1m]", "stream": True,
                              "output_config": {"effort": "high"}})
        self.assertEqual(fields["model"]["value"], "antigravity/gemini-4-argon-low[1m]")
        self.assertEqual(fields["output_config.effort"]["value"], "high")
        self.assertEqual(fields["thinking"], {"present": False})
        self.assertEqual(fields["tool_choice"], {"present": False})
        self.assertEqual(fields["top_level_keys"], ["model", "output_config", "stream"])
        self.assertEqual(fields["output_config.format"], {"present": False})
        extra = wire_fields({"synthetic_unknown_semantic": True, "output_config": {"format": {"type": "json_schema"}}})
        self.assertIn("synthetic_unknown_semantic", extra["top_level_keys"])
        self.assertEqual(extra["output_config.format"], {"present": True, "value": {"type": "json_schema"}})

    def test_no_inherited_environment_and_no_permission_bypass(self):
        env = clean_env(Path("/out/case"), 12345)
        self.assertEqual(env["ANTHROPIC_API_KEY"], "stage-a-fake-key-not-a-credential")
        self.assertEqual(env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:12345")
        self.assertNotIn("ANTHROPIC_AUTH_TOKEN", env)
        self.assertNotIn("HTTP_PROXY", env)
        cmd = cli_command("antigravity/gemini-4-argon-low", {}, "synthetic", session_id="test-id")
        self.assertIn("--bare", cmd)
        self.assertEqual(cmd[cmd.index("--tools") + 1], "Read")
        self.assertIn("Read(//fixture/read.txt)", cmd)
        self.assertNotIn("--dangerously-skip-permissions", cmd)
        self.assertNotIn("bypassPermissions", cmd)

    def test_stream_input_keeps_isolation_and_uses_no_positional_prompt(self):
        cmd = stream_cli_command("synthetic", {}, "synthetic-session")
        self.assertEqual(cmd[cmd.index("--input-format") + 1], "stream-json")
        self.assertEqual(cmd[cmd.index("--output-format") + 1], "stream-json")
        self.assertNotIn("--", cmd)
        self.assertIn("--bare", cmd)
        self.assertIn("Read(//fixture/read.txt)", cmd)

    def test_synthetic_reserialization_changes_only_tool_input_representation(self):
        row = {"uuid": "synthetic", "message": {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "", "signature": "unchanged"},
            {"type": "tool_use", "id": "local", "name": "Read", "input": {"z": [1, {"雪": -0.0, "a": True}], "a": 2}}
        ]}}
        changed = reserialize_row(row)
        self.assertEqual(canonical(changed), canonical(row))
        self.assertEqual(changed["message"]["content"][0], row["message"]["content"][0])
        self.assertEqual(list(changed["message"]["content"][1]["input"]), ["a", "z"])
        self.assertIsInstance(changed["message"]["content"][1]["input"]["a"], float)
        self.assertIsInstance(row["message"]["content"][1]["input"]["a"], int)

    def test_exponent_wire_lexemes_are_numerically_equivalent(self):
        import json
        value = {"z": [1.0, 0.0, 0.001, {"雪": "é"}], "a": True}
        wire = numeric_json(value)
        self.assertIn("e", wire)
        self.assertEqual(canonical(json.loads(wire)), canonical(value))

    def test_valid_numeric_read_uses_only_schema_parameters(self):
        native = fixture("numeric_valid")
        args = next(p["functionCall"]["args"] for c in native for p in c["parts"] if "functionCall" in p)
        self.assertEqual(set(args), {"file_path", "offset", "limit"})

    @unittest.skipUnless(Path("/opt/fixture-cli").is_file(), "HTTP fixture tests run only inside offline container")
    def test_local_http_nonstream_and_sse_fixture_formats(self):
        import http.client
        import json
        import tempfile
        import threading
        from harness import FixtureServer
        from codec import project_turn
        from test_codec import BINDING, native_fixture
        with tempfile.TemporaryDirectory() as tmp:
            expected = project_turn(native_fixture(), BINDING)
            for stream in (False, True):
                server = FixtureServer(Path(tmp), expected)
                thread = threading.Thread(target=server.serve_forever, daemon=True)
                thread.start()
                try:
                    connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=5)
                    connection.request("POST", "/v1/messages", json.dumps({"model": "synthetic", "stream": stream}), {"Content-Type": "application/json"})
                    response = connection.getresponse()
                    raw = response.read()
                    self.assertEqual(response.status, 200)
                    if stream:
                        self.assertIn(b"event: message_stop", raw)
                        self.assertEqual(response.getheader("Content-Type"), "text/event-stream")
                    else:
                        self.assertEqual(json.loads(raw)["content"], expected)
                        self.assertEqual(response.getheader("Content-Type"), "application/json")
                    connection.close()
                finally:
                    server.shutdown()
                    server.server_close()
                    thread.join(timeout=5)

    def test_expected_tool_error_requires_error_and_exact_replay(self):
        import copy
        good = [{"decoded": True, "native_exact": True, "tool_results": [
            {"id": "toolu_stagea_0000", "is_error": True, "fixture_read": False}]}]
        self.assertTrue(expected_tool_error_retained(good))
        self.assertFalse(expected_tool_error_retained([]))
        for mutation in ("decoded", "native_exact", "is_error", "missing_result"):
            bad = copy.deepcopy(good)
            if mutation in ("decoded", "native_exact"):
                bad[0][mutation] = False
            elif mutation == "is_error":
                bad[0]["tool_results"][0]["is_error"] = False
            else:
                bad[0]["tool_results"] = []
            with self.subTest(mutation=mutation):
                self.assertFalse(expected_tool_error_retained(bad))

    def test_nonbare_command_requires_session_hook_disable(self):
        import json
        cmd = cli_command("synthetic", {"disableAllHooks": True}, "/compact", bare=False)
        self.assertNotIn("--bare", cmd)
        self.assertNotIn("--disable-slash-commands", cmd)
        self.assertTrue(json.loads(cmd[cmd.index("--settings") + 1])["disableAllHooks"])
        self.assertIn("--strict-mcp-config", cmd)
        self.assertIn("Read(//fixture/read.txt)", cmd)
        self.assertEqual(cmd[cmd.index("--setting-sources") + 1], "")
        with self.assertRaises(ValueError):
            cli_command("synthetic", {}, "/compact", bare=False)

    @unittest.skipUnless(Path("/opt/fixture-cli").is_file(), "HTTP fixture tests run only inside offline container")
    def test_http_replay_rejection_precedes_generation_callback(self):
        import http.client
        import json
        import tempfile
        import threading
        from harness import FixtureServer, BINDING, LOW
        from codec import project_turn, decode_turn
        from test_codec import native_fixture
        with tempfile.TemporaryDirectory() as tmp:
            blocks = project_turn(native_fixture(), BINDING)
            wrong_model = project_turn(native_fixture(), dict(BINDING, native_model="gemini-4-argon-high"))
            server = FixtureServer(Path(tmp), blocks, enforce_replay=True, signed_completions=True)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            def post(content):
                connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=5)
                messages = [{"role": "user", "content": "synthetic"}]
                if content is not None:
                    messages += [{"role": "assistant", "content": content}, {"role": "user", "content": "continue"}]
                connection.request("POST", "/v1/messages", json.dumps({"model": LOW, "stream": False, "messages": messages}), {"Content-Type": "application/json"})
                response = connection.getresponse()
                status, body = response.status, json.loads(response.read())
                connection.close()
                return status, body
            try:
                self.assertEqual(post(None)[0], 200)
                self.assertEqual(len(server.native_generation_events), 1)
                for invalid in (blocks[1:], wrong_model):
                    status, body = post(invalid)
                    self.assertEqual(status, 400)
                    self.assertEqual(body["error"]["type"], "invalid_request_error")
                    self.assertIn("replay rejected", body["error"]["message"])
                    self.assertEqual(len(server.native_generation_events), 1)
                status, completion = post(blocks)
                self.assertEqual(status, 200)
                restored = decode_turn(completion["content"], BINDING)
                self.assertEqual(restored["contents"][0]["parts"][0]["text"], "SYNTHETIC_STAGE_A_DONE")
                self.assertIn("thoughtSignature", restored["contents"][0]["parts"][0])
                self.assertEqual(completion["content"][0]["thinking"], "")
                self.assertEqual(len(server.native_generation_events), 2)
            finally:
                server.shutdown()
                server.server_close()
                thread.join(timeout=5)

    def test_compaction_case_forwards_signed_completion_flag(self):
        import tempfile
        from unittest.mock import patch
        from harness import run_case, case_specs
        with tempfile.TemporaryDirectory() as tmp:
            with patch("harness.FixtureServer", side_effect=RuntimeError("stop before launching")) as server:
                with self.assertRaisesRegex(RuntimeError, "stop before launching"):
                    run_case("compact-nonbare", case_specs()["compact-nonbare"], Path(tmp))
                self.assertIs(server.call_args.kwargs.get("signed_completions"), True)

    def test_compaction_oracle_requires_real_boundary_and_signed_history(self):
        lost = [{"history_present": True, "decoded": False, "carrier_count": 0}]
        self.assertEqual(compaction_status(0, lost, True), "UNVERIFIED")
        self.assertEqual(compaction_status(1, lost, False), "UNVERIFIED")
        self.assertEqual(compaction_status(1, lost, True), "UNSUPPORTED")
        retained = [{"history_present": True, "decoded": True, "matches_signed_completion": True}]
        self.assertEqual(compaction_status(1, retained, True), "PASS")
        retained[0]["matches_signed_completion"] = False
        self.assertEqual(compaction_status(1, retained, True), "UNVERIFIED")

    def test_catalog_default_advertisement_does_not_allow_extra_execution(self):
        cmd = cli_command("synthetic", {"disableAllHooks": True}, "catalog only", bare=False, tools="default")
        self.assertEqual(cmd[cmd.index("--tools") + 1], "default")
        self.assertEqual(cmd[cmd.index("--allowedTools") + 1], "Read(//fixture/read.txt)")
        self.assertEqual(cmd[cmd.index("--permission-mode") + 1], "dontAsk")
        self.assertEqual(cmd[cmd.index("--permission-prompts") + 1], "none")

    def test_schema_inventory_distinguishes_keywords_from_property_names(self):
        tools = [{"name": "Synthetic", "description": "local", "input_schema": {
            "type": "object", "properties": {"not_a_keyword": {"anyOf": [{"type": "string"}, {"type": "null"}]},
                                              "type": {"type": "array", "items": {"$ref": "#/$defs/item"}}},
            "$defs": {"item": {"type": "integer", "minimum": 0}},
            "additionalProperties": False, "required": ["not_a_keyword"],
            "default": {"not_a_schema_keyword": 1}}}]
        inventory = schema_inventory(tools)
        self.assertEqual(inventory["tool_names"], ["Synthetic"])
        self.assertEqual(inventory["tool_type_fields"], [{"name": "Synthetic", "present": False}])
        self.assertNotIn("not_a_keyword", inventory["schema_keywords"])
        self.assertNotIn("not_a_schema_keyword", inventory["schema_keywords"])
        self.assertEqual(inventory["schema_keywords"]["type"], 5)
        self.assertEqual(inventory["schema_types"], ["array", "integer", "null", "object", "string"])
        self.assertIn("$ref", inventory["schema_keywords"])

    @unittest.skipUnless(Path("/opt/fixture-cli").is_file(), "HTTP fixture tests run only inside offline container")
    def test_http_catalog_cannot_emit_fixture_tool_calls(self):
        import http.client
        import json
        import tempfile
        import threading
        from harness import FixtureServer, BINDING, LOW
        from codec import project_turn
        from test_codec import native_fixture
        with tempfile.TemporaryDirectory() as tmp:
            server = FixtureServer(Path(tmp), project_turn(native_fixture(), BINDING), catalog_only=True)
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                connection = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=5)
                connection.request("POST", "/v1/messages", json.dumps({"model": LOW, "stream": False}), {"Content-Type": "application/json"})
                response = connection.getresponse()
                body = json.loads(response.read())
                self.assertEqual(response.status, 200)
                self.assertEqual(body["stop_reason"], "end_turn")
                self.assertTrue(all(b["type"] == "text" for b in body["content"]))
                connection.close()
            finally:
                server.shutdown()
                server.server_close()
                thread.join(timeout=5)

    @unittest.skipUnless(Path("/opt/fixture-cli").is_file(), "HTTP fixture tests run only inside offline container")
    def test_final_v1_server_uses_wire_display_and_validated_history_session(self):
        import http.client
        import json
        import tempfile
        import threading
        from harness import FixtureServer, BINDING, LOW
        from codec import project_turn, decode_turn
        from test_codec import FinalV1Tests
        native, recipe = FinalV1Tests.wrapped_fixture()
        with tempfile.TemporaryDirectory() as tmp:
            server = FixtureServer(Path(tmp), [], enforce_replay=True, signed_completions=True,
                                   initial_native=native, tool_projections=recipe, native_session_id="123")
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            def post(messages):
                c = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=5)
                c.request("POST", "/v1/messages", json.dumps({"model": LOW, "stream": False,
                          "thinking": {"type": "adaptive", "display": "omitted"}, "messages": messages}), {"Content-Type": "application/json"})
                response = c.getresponse()
                result = response.status, json.loads(response.read())
                c.close()
                return result
            try:
                messages = [{"role": "user", "content": "synthetic"}]
                status, first = post(messages)
                self.assertEqual(status, 200)
                self.assertEqual(first["content"][0]["thinking"], "")
                self.assertEqual(first["content"][1]["name"], "Read")
                self.assertEqual(first["content"][1]["input"]["file_path"], "/fixture/read.txt")
                restored = decode_turn(first["content"], BINDING)
                self.assertEqual(restored["session_id"], "123")
                self.assertEqual(restored["contents"], native)
                server.native_session_id = "999"  # Must NOT override the validated history session.
                messages += [{"role": "assistant", "content": first["content"]}, {"role": "user", "content": "continue"}]
                status, second = post(messages)
                self.assertEqual(status, 200)
                self.assertEqual(decode_turn(second["content"], BINDING)["session_id"], "123")
                conflicting = project_turn(native, BINDING, session_id="124", tool_projections=recipe)
                messages += [{"role": "assistant", "content": conflicting}, {"role": "user", "content": "continue"}]
                status, error = post(messages)
                self.assertEqual(status, 400)
                self.assertIn("session", error["error"]["message"])
                self.assertEqual(len(server.native_generation_events), 2)
            finally:
                server.shutdown()
                server.server_close()
                thread.join(timeout=5)

    def test_container_boundaries(self):
        command = docker_command(Path("/out/new"), "owned-stage-a", ["minimal"],
                                 image="sha256:" + "a" * 64, binary=Path("/synthetic/native"),
                                 uid=2345, gid=3456)
        self.assertEqual(command[command.index("--network") + 1], "none")
        self.assertEqual(command[command.index("--user") + 1], "2345:3456")
        self.assertIn("--read-only", command)
        self.assertNotIn("--privileged", command)
        self.assertNotIn("--pid", command)  # Docker default is a private PID namespace.
        self.assertNotIn("--publish", command)
        mounts = [command[i + 1] for i, v in enumerate(command) if v == "--mount"]
        self.assertEqual(len(mounts), 3)
        self.assertTrue(any("target=/opt/fixture-cli,readonly" in m for m in mounts))
        self.assertTrue(any("target=/fixture,readonly" in m for m in mounts))
        self.assertTrue(any("target=/out" in m for m in mounts))
        self.assertFalse(any("docker.sock" in m for m in mounts))


if __name__ == "__main__":
    unittest.main()
