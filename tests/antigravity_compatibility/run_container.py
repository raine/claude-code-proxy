"""Optional host-side Docker launcher; never executes Claude on the host.

Requires explicit --image, --claude-binary and a new absolute --out directory.
Only nonroot Linux callers, trusted local rootful Docker and single native ELF
installations are supported. No image pull/build, binary download or auth import.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import time
from urllib.parse import urlsplit
import uuid

from harness import case_specs, safe_identity

SOURCE = Path(__file__).resolve().parent
TOTAL_SECONDS = 600
CLEANUP_SECONDS = 30
LABEL = "antigravity.compatibility.run"
IMAGE_ID = re.compile(r"sha256:[a-f0-9]{64}")
CONTAINER_ID = re.compile(r"[a-f0-9]{64}")


def safe_path(value):
    text = str(value)
    if any(c in ',"' or ord(c) < 32 or ord(c) == 127 for c in text):
        raise ValueError("Paths cannot contain CSV delimiters, quotes or ASCII controls")
    return Path(text)


def valid_image(value):
    if IMAGE_ID.fullmatch(value):
        return True
    # Intentionally conservative Docker reference subset: explicit tag or sha256.
    component = r"[a-z0-9]+(?:(?:[._]|__|-+)[a-z0-9]+)*"
    registry = r"(?:[a-z0-9]+(?:[.-][a-z0-9]+)*(?::[0-9]+)?/)?"
    repository = r"(?:" + component + r"/)*" + component
    suffix = r"(?::[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}|@sha256:[a-f0-9]{64})"
    return re.fullmatch(registry + repository + suffix, value) is not None


def binary_identity(path):
    before = path.lstat()
    if not stat.S_ISREG(before.st_mode) or not os.access(path, os.R_OK | os.X_OK):
        raise ValueError("CLI must be one readable/executable regular native ELF file")
    digest = hashlib.sha256()
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as stream:
        opened = os.fstat(stream.fileno())
        if (opened.st_dev, opened.st_ino) != (before.st_dev, before.st_ino):
            raise ValueError("CLI changed while opening")
        header = stream.read(64)
        if (len(header) != 64 or header[:7] != b"\x7fELF\x02\x01\x01"
                or header[7] not in (0, 3) or int.from_bytes(header[16:18], "little") not in (2, 3)):
            raise ValueError("CLI must be a Linux 64-bit little-endian native ELF, not a wrapper")
        architecture = {62: "amd64", 183: "arm64"}.get(int.from_bytes(header[18:20], "little"))
        if architecture is None:
            raise ValueError("Unsupported ELF architecture")
        digest.update(header)
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
        after = os.fstat(stream.fileno())
    fields = ("st_dev", "st_ino", "st_size", "st_mtime_ns", "st_ctime_ns", "st_mode")
    if any(getattr(before, key) != getattr(after, key) for key in fields):
        raise ValueError("CLI changed while hashing")
    return dict(zip(fields, (getattr(after, key) for key in fields)),
                sha256=digest.hexdigest(), architecture=architecture)


def validate(args):
    if sys.platform != "linux" or not safe_identity():
        raise ValueError("Requires Linux and positive equal real/effective UID and GID")
    if not valid_image(args.image):
        raise ValueError("Image must be an explicitly tagged/digest-qualified name or full sha256 ID")
    binary = safe_path(args.claude_binary)
    if not binary.is_absolute():
        raise ValueError("CLI path must be absolute")
    binary = safe_path(binary.resolve(strict=True))
    identity = binary_identity(binary)
    source = safe_path(SOURCE)
    source = safe_path(source.resolve(strict=True))
    if not source.is_dir():
        raise ValueError("Fixture source must be a directory")
    out = safe_path(args.out)
    if (not out.is_absolute() or str(out) != args.out or os.path.normpath(args.out) != args.out
            or not out.name.startswith("antigravity-compatibility-")
            or out.name == "antigravity-compatibility-"):
        raise ValueError("Output must be a normalized absolute new antigravity-compatibility-* directory")
    if os.path.lexists(out):
        raise ValueError("Output already exists (including symlink)")
    if out.parent.resolve(strict=True) != out.parent or not stat.S_ISDIR(out.parent.lstat().st_mode):
        raise ValueError("Output parent must be an existing real directory without symlink ancestors")
    for mounted in (source, binary):
        if out == mounted or out in mounted.parents or mounted in out.parents:
            raise ValueError("Output overlaps mounted fixture inputs or CLI")
    if (len(set(args.cases)) != len(args.cases)
            or ("catalog-default" in args.cases and args.cases != ["catalog-default"])):
        raise ValueError("Cases must be unique; catalog-default runs separately")
    parent = out.parent.stat()
    return binary, source, out, identity, (parent.st_dev, parent.st_ino)


class Deadline:
    """One budget, including preflight and cleanup; never reset after polling."""
    def __init__(self):
        self.end = time.monotonic() + TOTAL_SECONDS

    def remaining(self, cleanup=False, cap=None):
        value = self.end - time.monotonic() - (0 if cleanup else CLEANUP_SECONDS)
        if value <= 0:
            raise subprocess.TimeoutExpired("launcher total deadline", TOTAL_SECONDS)
        return min(value, cap) if cap is not None else value


class Docker:
    def __init__(self, deadline):
        self.deadline = deadline
        keys = ("PATH", "DOCKER_CONFIG", "DOCKER_HOST", "DOCKER_CONTEXT",
                "DOCKER_TLS_VERIFY", "DOCKER_CERT_PATH", "DOCKER_API_VERSION")
        self.env = {key: os.environ[key] for key in keys if key in os.environ}
        self.env.setdefault("PATH", "/usr/bin:/bin")
        # Preserve the normal config location without forwarding HOME or auth env.
        if "DOCKER_CONFIG" not in self.env:
            self.env["DOCKER_CONFIG"] = str(Path.home() / ".docker")
        self.prefix = ["docker"]

    def call(self, args, cleanup=False):
        return subprocess.run(self.prefix + args, env=self.env, stdin=subprocess.DEVNULL,
                              capture_output=True, text=True,
                              timeout=self.deadline.remaining(cleanup=cleanup, cap=10))

    def text(self, args):
        result = self.call(args)
        if result.returncode:
            raise ValueError("Docker read-only preflight failed: " + " ".join(args[:2]))
        return result.stdout

    def preflight(self, image, architecture):
        # DOCKER_CONTEXT wins over DOCKER_HOST; otherwise a host selects default
        # directly. With neither, context show reads Docker's saved currentContext.
        context = self.env.get("DOCKER_CONTEXT")
        endpoint = None if context else self.env.get("DOCKER_HOST")
        if not endpoint:
            context = context or self.text(["context", "show"]).strip()
            if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", context):
                raise ValueError("Unsupported Docker context name")
            rows = json.loads(self.text(["context", "inspect", context]))
            if not isinstance(rows, list) or len(rows) != 1 or rows[0].get("Name") != context:
                raise ValueError("Ambiguous Docker context")
            endpoint = rows[0]["Endpoints"]["docker"]["Host"]
        parsed = urlsplit(endpoint)
        if (parsed.scheme != "unix" or parsed.netloc or parsed.query or parsed.fragment
                or not parsed.path.startswith("/") or parsed.path.startswith("//")
                or any(ord(c) < 32 or ord(c) == 127 for c in endpoint)):
            raise ValueError("Only a trusted local unix Docker endpoint is supported")
        # Pin the selected endpoint for ALL later operations. No context mutation.
        self.prefix = ["docker", "--host", endpoint]
        self.env.pop("DOCKER_CONTEXT", None)
        self.env.pop("DOCKER_HOST", None)
        server = json.loads(self.text(["version", "--format", "{{json .}}"]))["Server"]
        info = json.loads(self.text(["info", "--format", "{{json .}}"])).copy()
        security = info.get("SecurityOptions")
        if (server.get("Os") != "linux" or info.get("OSType") != "linux"
                or not server.get("Platform", {}).get("Name", "").startswith("Docker Engine")
                or not any(c.get("Name") == "Engine" for c in server.get("Components", []))
                or not isinstance(security, list) or not all(isinstance(s, str) for s in security)
                or any("rootless" in s.lower() or "userns" in s.lower() for s in security)):
            raise ValueError("Requires known rootful Docker/Linux engine without user namespace remapping")
        rows = json.loads(self.text(["image", "inspect", image]))
        if not isinstance(rows, list) or len(rows) != 1:
            raise ValueError("Image inspection must resolve exactly one local image")
        row = rows[0]
        image_id = row.get("Id", "")
        if (not IMAGE_ID.fullmatch(image_id) or row.get("Os") != "linux"
                or row.get("Architecture") != architecture):
            raise ValueError("Image must be immutable Linux and match the ELF architecture")
        config = row["Config"]
        if not isinstance(config, dict) or config.get("Volumes") not in (None, {}):
            raise ValueError("Image-declared VOLUMEs are unsupported")
        inherited = config.get("Env") or []
        if not isinstance(inherited, list):
            raise ValueError("Invalid image environment")
        keys = set()
        for item in inherited:
            if not isinstance(item, str) or "=" not in item:
                raise ValueError("Invalid image environment entry")
            key = item.split("=", 1)[0]
            if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", key) or key in keys:
                raise ValueError("Invalid or duplicate image environment key")
            keys.add(key)
        # Never persist image credential VALUES, inherited argv, or engine config.
        return {"image_id": image_id, "architecture": architecture, "env_keys": sorted(keys),
                "endpoint": endpoint, "context": context, "engine": server["Platform"]["Name"],
                "security_options": security, "os": "linux", "volumes": False}


class Output:
    """Pinned private directory FDs; control/ is never visible to the container."""
    def __init__(self, path, uid, gid, create=True, parent_identity=None):
        self.path, self.uid, self.gid = path, uid, gid
        self.fds, self.identities = {}, {}
        try:
            if create:
                if path.parent.resolve(strict=True) != path.parent:
                    raise ValueError("Output parent was replaced by a symlink")
                parent_fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
                try:
                    parent = os.fstat(parent_fd)
                    if parent_identity is not None and (parent.st_dev, parent.st_ino) != parent_identity:
                        raise ValueError("Output parent changed after validation")
                    os.mkdir(path.name, mode=0o700, dir_fd=parent_fd)
                finally:
                    os.close(parent_fd)
            self._pin("root", path)
            for name in ("control", "work"):
                if create:
                    os.mkdir(name, mode=0o700, dir_fd=self.fds["root"])
                self._pin(name, path / name)
        except BaseException:
            self.close()
            raise

    def _pin(self, name, path):
        before = path.lstat()
        fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        self.fds[name] = fd
        after = os.fstat(fd)
        identity = (after.st_dev, after.st_ino)
        if ((before.st_dev, before.st_ino) != identity or after.st_uid != self.uid
                or after.st_gid != self.gid or stat.S_IMODE(after.st_mode) != 0o700):
            raise ValueError("Output directory identity/ownership/private mode check failed")
        self.identities[name] = identity

    def verify(self):
        if self.path.resolve(strict=True) != self.path:
            raise ValueError("Output path was replaced with a symlink")
        for name, identity in self.identities.items():
            path = self.path if name == "root" else self.path / name
            value = path.lstat()
            if (not stat.S_ISDIR(value.st_mode) or (value.st_dev, value.st_ino) != identity
                    or value.st_uid != self.uid or value.st_gid != self.gid
                    or stat.S_IMODE(value.st_mode) != 0o700):
                raise ValueError("Output directory changed")

    def open(self, name):
        if Path(name).name != name:
            raise ValueError("Evidence name must be one path component")
        fd = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                     0o600, dir_fd=self.fds["control"])
        return os.fdopen(fd, "w")

    def dump(self, name, value):
        with self.open(name) as stream:
            json.dump(value, stream, indent=2)
            stream.write("\n")

    def read_cid(self):
        # Docker creates an empty cidfile BEFORE daemon creation. Poll metadata,
        # not contents; an existing complete cid is opened/read only once.
        value = os.stat("container.cid", dir_fd=self.fds["control"], follow_symlinks=False)
        if not stat.S_ISREG(value.st_mode) or value.st_uid != self.uid or value.st_nlink != 1:
            raise ValueError("Invalid invocation cidfile")
        if value.st_size == 0:
            return None
        fd = os.open("container.cid", os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK,
                     dir_fd=self.fds["control"])
        with os.fdopen(fd, "r", encoding="ascii") as stream:
            value = os.fstat(stream.fileno())
            if (not stat.S_ISREG(value.st_mode) or value.st_uid != self.uid
                    or value.st_nlink != 1 or value.st_size not in (64, 65)):
                raise ValueError("Invalid invocation cidfile")
            cid = stream.read(66).rstrip("\n")
        if not CONTAINER_ID.fullmatch(cid):
            raise ValueError("Invalid invocation container ID")
        return cid

    def close(self):
        for fd in self.fds.values():
            os.close(fd)
        self.fds.clear()

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()


def docker_command(out, name, cases, *, image, binary, uid, gid, env_keys=(), source=SOURCE, docker=("docker",)):
    command = list(docker) + ["run", "--rm", "--init", "--pull", "never", "--name", name,
            "--label", LABEL + "=" + name, "--cidfile", str(out / "control" / "container.cid"),
            "--network", "none", "--user", "%s:%s" % (uid, gid), "--cap-drop", "ALL",
            "--security-opt", "no-new-privileges", "--read-only", "--ipc", "private",
            "--pids-limit", "512", "--memory", "8g", "--cpus", "12", "--no-healthcheck",
            "--tmpfs", "/tmp:rw,nosuid,nodev,mode=1777",
            "--tmpfs", "/fixture-home:rw,nosuid,nodev,uid=%s,gid=%s,mode=0700" % (uid, gid),
            "--mount", "type=bind,source=%s,target=/fixture,readonly" % source,
            "--mount", "type=bind,source=%s,target=/out" % (out / "work"),
            "--mount", "type=bind,source=%s,target=/opt/fixture-cli,readonly" % binary,
            "--workdir", "/fixture", "--entrypoint", "/usr/bin/env"]
    # Docker client config may inject proxy credentials even when host env is
    # clean. Explicit blanks override both those defaults and image ENV values.
    proxy_keys = {key for upper in ("HTTP_PROXY", "HTTPS_PROXY", "FTP_PROXY", "ALL_PROXY", "NO_PROXY")
                  for key in (upper, upper.lower())}
    for key in sorted(set(env_keys) | proxy_keys):
        command += ["--env", key + "="]
    return command + [image, "-i", "PATH=/usr/bin:/bin", "PYTHONDONTWRITEBYTECODE=1",
                      "LANG=C.UTF-8", "HOME=/fixture-home", "python3", "-B", "/fixture/harness.py"] + cases


def absent(result, cid):
    return result.returncode != 0 and result.stderr.strip() in (
        "Error: No such container: " + cid, "Error response from daemon: No such container: " + cid)


def confirm(docker, cid, name, image, cleanup=False):
    result = docker.call(["container", "inspect", cid], cleanup=cleanup)
    if absent(result, cid):
        return None
    if result.returncode:
        raise ValueError("Container ownership inspection failed")
    rows = json.loads(result.stdout)
    if not isinstance(rows, list) or len(rows) != 1:
        raise ValueError("Ambiguous container ownership")
    row = rows[0]
    if (row.get("Id") != cid or row.get("Image") != image or row.get("Name") != "/" + name
            or row.get("Config", {}).get("Labels", {}).get(LABEL) != name):
        raise ValueError("Container ID/name/label/image ownership mismatch")
    return {"id": cid, "name": name, "image": image, "label": {LABEL: name}}


def failure(error):
    if isinstance(error, KeyboardInterrupt):
        return 130, "Interrupted"
    if isinstance(error, subprocess.TimeoutExpired):
        return 124, "Timeout: total launcher deadline or bounded Docker operation"
    return 1, str(error)


def execute(args, validated, deadline):
    binary, source, out, identity, parent_identity = validated
    uid, gid = os.getuid(), os.getgid()
    docker = Docker(deadline)
    preflight = docker.preflight(args.image, identity["architecture"])
    name = "antigravity-compat-" + uuid.uuid4().hex
    with Output(out, uid, gid, parent_identity=parent_identity) as output:
        output.dump("preflight.json", preflight)
        output.dump("binary.json", dict(path=str(binary), **identity))
        command = docker_command(out, name, args.cases, image=preflight["image_id"], binary=binary,
                                 uid=uid, gid=gid, env_keys=preflight["env_keys"], source=source, docker=docker.prefix)
        output.dump("docker-command.json", command)
        code, error, cleanup_errors = 1, None, []
        process, cid, owned, gone = None, None, False, False
        launch_attempted = False
        try:
            with output.open("container.log") as log:
                output.verify()
                if binary_identity(binary) != identity:
                    raise ValueError("CLI changed after validation")
                deadline.remaining()
                launch_attempted = True
                process = subprocess.Popen(command, env=docker.env, stdin=subprocess.DEVNULL,
                                           stdout=log, stderr=subprocess.STDOUT)
                while cid is None:
                    deadline.remaining()
                    try:
                        cid = output.read_cid()
                    except FileNotFoundError:
                        pass
                    if cid is None:
                        if process.poll() is not None:
                            raise ValueError("Docker exited without a complete invocation cidfile")
                        time.sleep(min(0.1, deadline.remaining()))
                inspection = confirm(docker, cid, name, preflight["image_id"])
                gone = inspection is None
                owned = not gone
                output.dump("docker-inspect.json", inspection or {"id": cid, "absent": True})
                code = process.wait(timeout=deadline.remaining())
                if code:
                    error = "Container exited with status %s" % code
        except (OSError, ValueError, KeyError, TypeError, subprocess.TimeoutExpired, KeyboardInterrupt) as exc:
            code, error = failure(exc)
        finally:
            # Never recover an unverified name/label through a broad query. If an
            # interrupt preceded cid confirmation, leave an explicit orphan warning.
            if launch_attempted and not owned and not gone:
                error = (error or "Unconfirmed container") + "; possible orphan: " + name + " (" + LABEL + "=" + name + ")"
                code = code or 1
            if owned:
                for args_cleanup in (["stop", "--time", "5", cid], ["rm", "--force", cid]):
                    try:
                        result = docker.call(args_cleanup, cleanup=True)
                        if absent(result, cid):
                            break
                        if result.returncode:
                            cleanup_errors.append("Owned container " + args_cleanup[0] + " failed")
                    except (OSError, ValueError, subprocess.TimeoutExpired, KeyboardInterrupt) as exc:
                        cleanup_errors.append(failure(exc)[1])
            if process is not None and process.poll() is None:
                try:
                    process.terminate()
                    try:
                        process.wait(timeout=deadline.remaining(cleanup=True, cap=2))
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=deadline.remaining(cleanup=True, cap=2))
                except (OSError, subprocess.TimeoutExpired, KeyboardInterrupt) as exc:
                    cleanup_errors.append("Docker client wait: " + failure(exc)[1])
        code = code or (1 if cleanup_errors else 0)
        result = {"exit": code, "error": error, "cleanup_errors": cleanup_errors,
                  "artifacts": str(out), "name": name, "container_id": cid, "owned": owned}
        output.dump("result.json", result)
    print(json.dumps(result))
    return code


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--image", required=True)
    parser.add_argument("--claude-binary", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("cases", nargs="+", choices=list(case_specs()) + ["matrix", "catalog-default"])
    args = parser.parse_args(argv)
    try:
        validated = validate(args)
    except (OSError, ValueError, RuntimeError) as exc:
        parser.error(str(exc))
    deadline = Deadline()
    try:
        return execute(args, validated, deadline)
    except (OSError, ValueError, KeyError, TypeError, subprocess.TimeoutExpired, KeyboardInterrupt) as exc:
        code, error = failure(exc)
        print(json.dumps({"exit": code, "error": error}), file=sys.stderr)
        return code


if __name__ == "__main__":
    sys.exit(main())
