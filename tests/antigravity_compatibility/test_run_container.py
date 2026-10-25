"""Launcher contract tests: every process is a fail-closed stub, never Docker/Claude."""
import contextlib
import io
import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import harness
import run_container as launcher

IMAGE = "sha256:" + "a" * 64
CID = "b" * 64
FOREIGN = "c" * 64


class DockerStub:
    def __init__(self, test):
        self.test = test
        self.calls = []
        self.envs = []
        self.command = None
        self.code = 0
        self.wait_error = None
        self.inspect_error = None
        self.stop_error = False
        self.mismatch = None
        self.cid = CID
        self.absent = False
        self.before_run = None
        self.context_host = "unix:///var/run/docker.sock"
        self.image = {"Id": IMAGE, "Os": "linux", "Architecture": "amd64", "Config": {
            "Env": ["PATH=/malicious", "ANTHROPIC_API_KEY=image-secret", "LD_PRELOAD=/evil.so"],
            "Volumes": None, "Entrypoint": ["/evil"], "WorkingDir": "/evil",
            "Healthcheck": {"Test": ["CMD", "/evil"]}}}
        self.info = {"OSType": "linux", "SecurityOptions": ["name=seccomp,profile=builtin"]}
        self.version = {"Server": {"Os": "linux", "Platform": {"Name": "Docker Engine - Community"},
                                   "Components": [{"Name": "Engine"}]}}
        self.failure = None
        self.terminated = False

    def run(self, argv, **kwargs):
        self.test.assertIsInstance(argv, list)
        self.test.assertGreater(kwargs["timeout"], 0)
        self.test.assertLessEqual(kwargs["timeout"], launcher.TOTAL_SECONDS)
        self.calls.append(argv)
        self.envs.append(kwargs["env"])
        args = argv[1:]
        if args[:1] == ["--host"]:
            args = args[2:]
        if self.failure == args[:2]:
            return subprocess.CompletedProcess(argv, 1, "", "synthetic failure")
        if args == ["context", "show"]:
            value = "selected\n"
        elif args[:2] == ["context", "inspect"]:
            value = json.dumps([{"Name": args[2], "Endpoints": {"docker": {"Host": self.context_host}}}])
        elif args == ["version", "--format", "{{json .}}"]:
            value = json.dumps(self.version)
        elif args == ["info", "--format", "{{json .}}"]:
            value = json.dumps(self.info)
        elif args[:2] == ["image", "inspect"]:
            self.test.assertFalse(self.test.out.exists())
            value = json.dumps([self.image])
            if self.before_run:
                self.before_run()
        elif args[:2] == ["container", "inspect"]:
            self.test.assertEqual(args[2], CID)
            if self.inspect_error:
                raise self.inspect_error
            if self.absent:
                return subprocess.CompletedProcess(argv, 1, "", "Error: No such container: " + CID)
            name = self.command[self.command.index("--name") + 1]
            label = self.command[self.command.index("--label") + 1].split("=", 1)
            row = {"Id": CID, "Name": "/" + name, "Image": IMAGE,
                   "Config": {"Labels": {label[0]: label[1]}}, "State": {"Running": False}}
            if self.mismatch == "id":
                row["Id"] = FOREIGN
            if self.mismatch == "image":
                row["Image"] = "sha256:" + FOREIGN
            if self.mismatch == "label":
                row["Config"]["Labels"][label[0]] = "foreign"
            if self.mismatch == "name":
                row["Name"] = "/foreign"
            value = json.dumps([row])
        elif args[:1] in (["stop"], ["rm"]):
            self.test.assertEqual(args[-1], CID)
            if self.stop_error:
                return subprocess.CompletedProcess(argv, 1, "", "synthetic cleanup failure")
            value = CID
        else:
            raise AssertionError("Unexpected subprocess: " + repr(argv))
        return subprocess.CompletedProcess(argv, 0, value, "")

    def popen(self, argv, **kwargs):
        self.test.assertIsNone(self.command, "Only one run may be launched")
        self.command = argv
        self.envs.append(kwargs["env"])
        self.test.assertIn("run", argv)
        cidfile = Path(argv[argv.index("--cidfile") + 1])
        self.test.assertEqual(cidfile.parent, self.test.out / "control")
        if self.cid == "symlink":
            cidfile.symlink_to(self.test.binary)
        elif self.cid is not None:
            cidfile.write_text(self.cid + "\n")
        return self

    def poll(self):
        return None if self.wait_error and not self.terminated else self.code

    def wait(self, timeout):
        self.test.assertGreater(timeout, 0)
        if self.wait_error and not self.terminated:
            error, self.wait_error = self.wait_error, None
            raise error
        return self.code

    def terminate(self):
        self.terminated = True

    def kill(self):
        self.terminated = True


class LauncherTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.out = self.root / "antigravity-compatibility-unit"
        self.binary = self.root / "synthetic-native"
        # ELF-shaped but deliberately incomplete: never a runnable program.
        self.binary.write_bytes(b"\x7fELF\x02\x01\x01\0" + b"\0" * 8 + b"\x02\0\x3e\0" + b"\0" * 44)
        self.binary.chmod(0o700)
        self.stub = DockerStub(self)
        self.stack = contextlib.ExitStack()
        self.addCleanup(self.stack.close)
        self.stack.enter_context(patch.object(launcher.subprocess, "run", side_effect=self.stub.run))
        self.stack.enter_context(patch.object(launcher.subprocess, "Popen", side_effect=self.stub.popen))
        self.stack.enter_context(patch("os.system", side_effect=AssertionError("No shell")))
        # The test container's /tmp is noexec. Model access from permission bits;
        # keep both the mount and deliberately incomplete ELF non-runnable.
        self.stack.enter_context(patch.object(launcher.os, "access", side_effect=lambda path, mode:
                                             stat.S_IMODE(Path(path).stat().st_mode) & 0o500 == 0o500))
        self.stack.enter_context(patch.dict(os.environ, {
            "PATH": "/usr/bin:/bin", "HOME": str(self.root),
            "ANTHROPIC_API_KEY": "host-secret", "HTTP_PROXY": "host-proxy"}, clear=True))
        self.stdout = self.stack.enter_context(contextlib.redirect_stdout(io.StringIO()))
        self.stack.enter_context(contextlib.redirect_stderr(io.StringIO()))

    def args(self, **changes):
        values = {"image": "example/fixture:local", "claude-binary": str(self.binary), "out": str(self.out)}
        values.update(changes)
        result = []
        for key, value in values.items():
            if value is not None:
                result += ["--" + key, value]
        return result + ["minimal"]

    def invoke(self, args=None):
        return launcher.main(self.args() if args is None else args)

    def invalid(self, args=None):
        with patch.object(launcher.os, "mkdir", side_effect=AssertionError("No mkdir for invalid input")):
            with self.assertRaises((SystemExit, ValueError)):
                self.invoke(args)
        self.assertFalse(self.out.exists())
        self.assertEqual(self.stub.calls, [])
        self.assertIsNone(self.stub.command)

    def test_explicit_configuration_required(self):
        for key in ("image", "claude-binary"):
            with self.subTest(key=key):
                self.invalid(self.args(**{key: None}))

    def test_invalid_image_references_have_no_side_effects(self):
        for image in ("--privileged", "latest", "repo", "repo:", "repo:tag extra", "repo@sha256:abc", "repo:tag\n"):
            with self.subTest(image=image):
                self.invalid(self.args(image=image))

    def test_invalid_cases_have_no_side_effects(self):
        for cases in (["../outside"], ["minimal", "minimal"], ["catalog-default", "minimal"], ["--privileged"]):
            with self.subTest(cases=cases):
                self.invalid(self.args()[:-1] + cases)

    def test_identity_rejects_root_privilege_mismatch_and_nonlinux(self):
        for names in ((0, 0, 1, 1), (1, 2, 3, 3), (1, 1, 0, 0), (1, 1, 3, 4)):
            with self.subTest(identity=names), contextlib.ExitStack() as stack:
                for fn, value in zip(("getuid", "geteuid", "getgid", "getegid"), names):
                    stack.enter_context(patch.object(os, fn, return_value=value))
                self.invalid()
        with patch.object(launcher.sys, "platform", "darwin"):
            self.invalid()

    def test_binary_must_be_absolute_readable_executable_regular_native_elf(self):
        self.invalid(self.args(**{"claude-binary": "relative"}))
        self.invalid(self.args(**{"claude-binary": str(self.root)}))
        self.invalid(self.args(**{"claude-binary": str(self.root / "missing")}))
        self.binary.chmod(0o600)
        self.invalid()
        self.binary.chmod(0o700)
        self.binary.write_text("#!/usr/bin/env node\n")
        self.invalid()
        self.binary.unlink()
        os.mkfifo(self.binary)
        self.invalid()

    def test_output_must_be_new_normalized_absolute_with_real_existing_parent(self):
        for out in ("antigravity-compatibility-unit", str(self.root / "wrong"),
                    str(self.root / "missing" / self.out.name), str(self.root) + "/../" + self.root.name + "/" + self.out.name):
            with self.subTest(out=out):
                self.invalid(self.args(out=out))
        alias = self.root / "alias"
        alias.symlink_to(self.root, target_is_directory=True)
        self.invalid(self.args(out=str(alias / self.out.name)))
        self.out.symlink_to(self.root / "missing")
        with self.assertRaises((SystemExit, ValueError)):
            self.invoke()
        self.assertTrue(self.out.is_symlink())
        self.assertEqual(self.stub.calls, [])

    def test_existing_output_is_not_modified(self):
        self.out.mkdir()
        marker = self.out / "marker"
        marker.write_text("preserve")
        with self.assertRaises((SystemExit, ValueError)):
            self.invoke()
        self.assertEqual(marker.read_text(), "preserve")
        self.assertEqual(self.stub.calls, [])

    def test_mount_delimiters_rejected_but_colon_and_binary_symlink_supported(self):
        for char in (",", '"', "\n", "\x7f"):
            self.invalid(self.args(out=str(self.root / (self.out.name + char))))
            bad = self.root / ("native" + char)
            bad.symlink_to(self.binary)
            self.invalid(self.args(**{"claude-binary": str(bad)}))
        alias = self.root / "native:alias"
        alias.symlink_to(self.binary)
        self.assertEqual(self.invoke(self.args(**{"claude-binary": str(alias)})), 0)
        mounts = self.mounts()
        self.assertIn("source=" + str(self.binary) + ",target=/opt/fixture-cli,readonly", " ".join(mounts))

    def test_overlap_and_unsafe_fixture_path_fail_before_docker(self):
        with patch.object(launcher, "SOURCE", self.root):
            self.invalid()
        with patch.object(launcher, "SOURCE", self.root / 'bad,"'):
            self.invalid()

    def mounts(self):
        cmd = self.stub.command
        return [cmd[i + 1] for i, arg in enumerate(cmd) if arg == "--mount"]

    def result(self):
        return json.loads((self.out / "control" / "result.json").read_text())

    def test_success_uses_pinned_image_clean_env_and_host_only_control(self):
        self.assertEqual(self.invoke(), 0)
        cmd = self.stub.command
        self.assertIn(IMAGE, cmd)
        self.assertNotIn("example/fixture:local", cmd)
        for option, value in (("--pull", "never"), ("--user", "%s:%s" % (os.getuid(), os.getgid())),
                              ("--network", "none"), ("--ipc", "private"), ("--workdir", "/fixture"),
                              ("--entrypoint", "/usr/bin/env"), ("--cap-drop", "ALL")):
            self.assertEqual(cmd[cmd.index(option) + 1], value)
        for option in ("--read-only", "--init", "--no-healthcheck"):
            self.assertIn(option, cmd)
        for option in ("-v", "--privileged", "--pid", "--env-file", "--publish"):
            self.assertNotIn(option, cmd)
        self.assertIn("-i", cmd)
        self.assertEqual(cmd[-4:], ["python3", "-B", "/fixture/harness.py", "minimal"])
        self.assertIn("HOME=/fixture-home", cmd)
        self.assertIn("/fixture-home:rw,nosuid,nodev,uid=%s,gid=%s,mode=0700" % (os.getuid(), os.getgid()), cmd)
        self.assertEqual(len(self.mounts()), 3)
        self.assertIn("type=bind,source=" + str(self.out / "work") + ",target=/out", self.mounts())
        self.assertFalse(any("/control" in mount or "docker.sock" in mount for mount in self.mounts()))
        for directory in (self.out, self.out / "control", self.out / "work"):
            self.assertEqual(stat.S_IMODE(directory.stat().st_mode), 0o700)
        self.assertIn("ANTHROPIC_API_KEY=", cmd)
        self.assertIn("LD_PRELOAD=", cmd)
        for env in self.stub.envs:
            self.assertNotIn("ANTHROPIC_API_KEY", env)
            self.assertNotIn("HTTP_PROXY", env)
            self.assertLessEqual(set(env), {"PATH", "DOCKER_CONFIG", "DOCKER_HOST", "DOCKER_CONTEXT",
                                            "DOCKER_TLS_VERIFY", "DOCKER_CERT_PATH", "DOCKER_API_VERSION"})
        for path in (self.out / "control").iterdir():
            if path.is_file():
                self.assertNotIn("image-secret", path.read_text())
                self.assertNotIn("host-secret", path.read_text())
        self.assertEqual(self.result()["exit"], 0)

    def test_context_overrides_host_and_endpoint_is_pinned(self):
        with patch.dict(os.environ, {"DOCKER_CONTEXT": "chosen", "DOCKER_HOST": "tcp://remote:2375"}):
            self.assertEqual(self.invoke(), 0)
        self.assertIn(["docker", "context", "inspect", "chosen"], self.stub.calls)
        self.assertEqual(self.stub.command[:3], ["docker", "--host", "unix:///var/run/docker.sock"])
        self.assertNotIn("DOCKER_CONTEXT", self.stub.envs[-1])

    def test_remote_effective_host_rejected_before_daemon_and_output(self):
        with patch.dict(os.environ, {"DOCKER_HOST": "tcp://remote:2375"}):
            self.assertNotEqual(self.invoke(), 0)
        self.assertFalse(self.out.exists())
        self.assertEqual(self.stub.calls, [])

    def test_remote_context_missing_image_and_unknown_engine_fail_before_output(self):
        for change in ("remote", "image", "rootless", "podman", "volumes", "bad-env", "architecture"):
            with self.subTest(change=change):
                old = self.stub
                self.stub.context_host = "ssh://remote" if change == "remote" else "unix:///var/run/docker.sock"
                if change == "image":
                    self.stub.failure = ["image", "inspect"]
                if change == "rootless":
                    self.stub.info["SecurityOptions"] = ["name=rootless"]
                if change == "podman":
                    self.stub.version["Server"]["Platform"]["Name"] = "Podman"
                if change == "volumes":
                    self.stub.image["Config"]["Volumes"] = {"/leak": {}}
                if change == "bad-env":
                    self.stub.image["Config"]["Env"] = ["invalid-key=x"]
                if change == "architecture":
                    self.stub.image["Architecture"] = "arm64"
                self.assertNotEqual(self.invoke(), 0)
                self.assertFalse(self.out.exists())
                self.assertIsNone(self.stub.command)
                replacement = DockerStub(self)
                old.__dict__.update(replacement.__dict__)

    def test_binary_change_between_preflight_and_run_is_rejected(self):
        self.stub.before_run = lambda: self.binary.write_bytes(b"changed")
        self.assertNotEqual(self.invoke(), 0)
        self.assertIsNone(self.stub.command)

    def test_success_nonzero_timeout_interrupt_and_cleanup_failure(self):
        self.stub.code = 17
        self.assertEqual(self.invoke(), 17)
        self.assertEqual(self.result()["exit"], 17)

    def test_timeout_is_bounded_and_only_owned_id_is_cleaned(self):
        self.stub.wait_error = subprocess.TimeoutExpired("synthetic", 1)
        self.assertEqual(self.invoke(), 124)
        self.assertTrue(any("stop" in argv and argv[-1] == CID for argv in self.stub.calls))
        self.assertIn("timeout", self.result()["error"].lower())

    def test_interrupt_preserves_primary_and_cleanup_errors(self):
        self.stub.wait_error = KeyboardInterrupt()
        self.stub.stop_error = True
        self.assertEqual(self.invoke(), 130)
        result = self.result()
        self.assertIn("interrupt", result["error"].lower())
        self.assertTrue(result["cleanup_errors"])

    def test_already_removed_container_is_not_cleanup_failure(self):
        self.stub.absent = True
        self.assertEqual(self.invoke(), 0)
        self.assertEqual(self.result()["cleanup_errors"], [])
        self.assertFalse(any("stop" in argv or "rm" in argv for argv in self.stub.calls))

    def test_foreign_id_label_image_or_name_never_authorizes_cleanup(self):
        for mismatch in ("id", "image", "label", "name"):
            with self.subTest(mismatch=mismatch):
                self.out = self.root / ("antigravity-compatibility-" + mismatch)
                self.stub.command = None
                self.stub.calls.clear()
                self.stub.mismatch = mismatch
                self.assertNotEqual(self.invoke(), 0)
                self.assertFalse(any("stop" in argv or "rm" in argv for argv in self.stub.calls))
                self.assertIn("possible orphan", self.result()["error"])

    def test_missing_malformed_and_symlink_cid_never_authorizes_cleanup(self):
        for cid in (None, "foreign-name", "symlink"):
            with self.subTest(cid=cid):
                self.out = self.root / ("antigravity-compatibility-" + str(cid))
                self.stub.command = None
                self.stub.calls.clear()
                self.stub.cid = cid
                self.assertNotEqual(self.invoke(), 0)
                self.assertFalse(any("stop" in argv or "rm" in argv for argv in self.stub.calls))
                self.assertIn("possible orphan", self.result()["error"])

    def test_preflight_timeout_has_no_output(self):
        with patch.object(launcher.subprocess, "run", side_effect=subprocess.TimeoutExpired("docker", 1)):
            self.assertEqual(self.invoke(), 124)
        self.assertFalse(self.out.exists())

    def test_deadline_reserves_cleanup_without_resetting_total_budget(self):
        with patch.object(launcher.time, "monotonic", return_value=10) as clock:
            deadline = launcher.Deadline()
            clock.return_value = 10 + launcher.TOTAL_SECONDS - launcher.CLEANUP_SECONDS
            with self.assertRaises(subprocess.TimeoutExpired):
                deadline.remaining()
            self.assertLessEqual(deadline.remaining(cleanup=True), launcher.CLEANUP_SECONDS)
            clock.return_value = 10 + launcher.TOTAL_SECONDS
            with self.assertRaises(subprocess.TimeoutExpired):
                deadline.remaining(cleanup=True)

    def test_arm64_elf_requires_matching_image(self):
        header = bytearray(self.binary.read_bytes())
        header[18:20] = (183).to_bytes(2, "little")
        self.binary.write_bytes(header)
        self.stub.image["Architecture"] = "arm64"
        self.assertEqual(self.invoke(), 0)

    def test_resolved_binary_mount_delimiters_are_rejected(self):
        unsafe = self.root / 'native,unsafe'
        self.binary.rename(unsafe)
        self.binary.symlink_to(unsafe)
        self.invalid()

    def test_no_output_creation_on_any_readonly_preflight_failure(self):
        for operation in (["context", "show"], ["version", "--format"],
                          ["info", "--format"], ["image", "inspect"]):
            with self.subTest(operation=operation):
                self.stub.failure = operation
                with patch.object(launcher.os, "mkdir", side_effect=AssertionError("No preflight mkdir")):
                    self.assertNotEqual(self.invoke(), 0)
                self.assertIsNone(self.stub.command)

    def test_client_config_proxy_defaults_are_always_blanked(self):
        self.stub.image["Config"]["Env"] = []
        self.assertEqual(self.invoke(), 0)
        for key in ("HTTP_PROXY", "HTTPS_PROXY", "FTP_PROXY", "ALL_PROXY", "NO_PROXY"):
            self.assertIn(key + "=", self.stub.command)
            self.assertIn(key.lower() + "=", self.stub.command)

    def test_explicit_docker_config_does_not_need_home_or_passwd_entry(self):
        with patch.dict(os.environ, {"DOCKER_CONFIG": str(self.root / "docker-config")}):
            with patch.object(launcher.Path, "home", side_effect=AssertionError("Do not resolve unused HOME")):
                self.assertEqual(self.invoke(), 0)

    def test_empty_cid_is_pending_and_read_only_after_docker_finishes_writing(self):
        self.stub.cid = ""
        self.stub.poll = lambda: None if not (self.out / "control" / "container.cid").stat().st_size > 1 else 0
        def complete_cid(seconds):
            (self.out / "control" / "container.cid").write_text(CID + "\n")
        # Docker creates a genuinely empty file before asking the daemon to create.
        original_popen = self.stub.popen
        def popen(argv, **kwargs):
            result = original_popen(argv, **kwargs)
            (self.out / "control" / "container.cid").write_text("")
            return result
        with patch.object(launcher.subprocess, "Popen", side_effect=popen), patch.object(launcher.time, "sleep", side_effect=complete_cid):
            self.assertEqual(self.invoke(), 0)

    def test_interrupt_during_launch_reports_unique_possible_orphan(self):
        with patch.object(launcher.subprocess, "Popen", side_effect=KeyboardInterrupt()):
            self.assertEqual(self.invoke(), 130)
        result = self.result()
        self.assertIn("possible orphan", result["error"])
        self.assertIn(result["name"], result["error"])
        self.assertFalse(any("stop" in argv or "rm" in argv for argv in self.stub.calls))

    def test_parent_symlink_replacement_during_preflight_creates_no_output(self):
        parent = self.root / "parent"
        parent.mkdir()
        self.out = parent / self.out.name
        destination = self.root / "destination"
        destination.mkdir()
        def replace_parent():
            parent.rmdir()
            parent.symlink_to(destination, target_is_directory=True)
        self.stub.before_run = replace_parent
        self.assertNotEqual(self.invoke(), 0)
        self.assertFalse((destination / self.out.name).exists())
        self.assertIsNone(self.stub.command)

    def test_observed_output_directory_replacement_is_rejected(self):
        self.assertEqual(self.invoke(), 0)
        with launcher.Output(self.out, os.getuid(), os.getgid(), create=False) as output:
            work = self.out / "work"
            work.rename(self.out / "old-work")
            work.mkdir(mode=0o700)
            with self.assertRaises(ValueError):
                output.verify()

    def test_cleanup_error_turns_success_into_failure(self):
        self.stub.stop_error = True
        self.assertNotEqual(self.invoke(), 0)
        self.assertTrue(self.result()["cleanup_errors"])

    def test_inspection_timeout_never_authorizes_cleanup(self):
        self.stub.inspect_error = subprocess.TimeoutExpired("inspect", 1)
        self.assertEqual(self.invoke(), 124)
        self.assertIn("possible orphan", self.result()["error"])
        self.assertFalse(any("stop" in argv or "rm" in argv for argv in self.stub.calls))

    def test_evidence_writer_never_follows_or_overwrites_existing_leaf(self):
        self.assertEqual(self.invoke(), 0)
        with launcher.Output(self.out, os.getuid(), os.getgid(), create=False) as output:
            victim = self.root / "victim"
            victim.write_text("unchanged")
            (self.out / "control" / "attack").symlink_to(victim)
            for name in ("attack", "docker-command.json"):
                with self.assertRaises(OSError):
                    with output.open(name):
                        self.fail("existing leaf opened")
            self.assertEqual(victim.read_text(), "unchanged")


class HarnessAdmissionTests(unittest.TestCase):
    def test_other_positive_equal_nonroot_identity_is_accepted(self):
        with contextlib.ExitStack() as stack:
            for name, value in (("getuid", 2345), ("geteuid", 2345), ("getgid", 3456), ("getegid", 3456)):
                stack.enter_context(patch.object(os, name, return_value=value))
            self.assertTrue(harness.safe_identity())

    def test_root_and_real_effective_mismatches_are_rejected(self):
        for values in ((0, 0, 4, 4), (4, 4, 0, 0), (4, 5, 6, 6), (4, 4, 6, 7), (-1, -1, 2, 2)):
            with self.subTest(identity=values), contextlib.ExitStack() as stack:
                for name, value in zip(("getuid", "geteuid", "getgid", "getegid"), values):
                    stack.enter_context(patch.object(os, name, return_value=value))
                self.assertFalse(harness.safe_identity())


if __name__ == "__main__":
    unittest.main()
