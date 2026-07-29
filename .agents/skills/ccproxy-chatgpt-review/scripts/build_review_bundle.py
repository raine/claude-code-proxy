#!/usr/bin/env python3
"""Build a sanitized gitingest bundle for a ChatGPT repository review."""

from __future__ import annotations

import argparse
import hashlib
import os
import re
import shutil
import subprocess
import sys
import tempfile
from datetime import datetime
from pathlib import Path

GITINGEST_VERSION = "0.3.1"
DEFAULT_MAX_FILE_SIZE = 10_485_760
DEFAULT_SPLIT_BYTES = 12_000_000
SEPARATOR = "=" * 48

TOOL_OR_GENERATED_DIRS = {
    ".agents",
    ".codex",
    ".git",
    ".venv",
    "__pycache__",
    "dist",
    "node_modules",
    "target",
    "venv",
}

SENSITIVE_EXACT_NAMES = {
    ".env",
    ".envrc",
    ".netrc",
    ".npmrc",
    ".pypirc",
    "auth.json",
    "cookies.json",
    "cookies.txt",
    "credentials.json",
    "secrets.json",
    "secrets.toml",
    "secrets.yaml",
    "secrets.yml",
    "service-account.json",
}

SENSITIVE_SUFFIXES = {
    ".db",
    ".har",
    ".jks",
    ".jsonl",
    ".key",
    ".keystore",
    ".log",
    ".mobileprovision",
    ".p12",
    ".pcap",
    ".pcapng",
    ".pem",
    ".pfx",
    ".sqlite",
    ".sqlite3",
}

SECRET_DETECTORS = (
    ("private-key", re.compile(rb"-----BEGIN (?:[A-Z0-9]+ )?PRIVATE KEY-----")),
    ("aws-access-key", re.compile(rb"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b")),
    ("github-token", re.compile(rb"\bgh[pousr]_[A-Za-z0-9_]{30,}\b")),
    (
        "openai-or-anthropic-key",
        re.compile(rb"\bsk-(?:ant-|proj-)?[A-Za-z0-9_-]{24,}\b"),
    ),
    ("slack-token", re.compile(rb"\bxox[baprs]-[A-Za-z0-9-]{20,}\b")),
    ("google-api-key", re.compile(rb"\bAIza[0-9A-Za-z_-]{35}\b")),
)

# Gitingest 0.3.1 ignores these useful text files by default. Passing the exact
# patterns removes them from its default ignore set. The staging tree itself
# contains only files selected by this script.
REVIEW_RELEVANT_INCLUDE_OVERRIDES = (
    "**",
    ".classpath",
    ".gitattributes",
    ".gitignore",
    ".gitmodules",
    ".project",
    "*.gradle",
    "Cargo.lock",
    "Gemfile.lock",
    "Pipfile.lock",
    "bun.lock",
    "bun.lockb",
    "package-lock.json",
    "poetry.lock",
    "yarn.lock",
)


class BundleError(RuntimeError):
    """An expected, user-facing bundle error."""


def positive_int(raw: str) -> int:
    value = int(raw)
    if value <= 0:
        raise argparse.ArgumentTypeError("value must be greater than zero")
    return value


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Create a local, sanitized gitingest bundle for ChatGPT Pro."
    )
    parser.add_argument(
        "--repo",
        default=".",
        help="Path inside the Git repository to export (default: current directory).",
    )
    parser.add_argument(
        "--output-root",
        help="Parent directory for the timestamped bundle.",
    )
    parser.add_argument(
        "--prompt-template",
        help="Review prompt template; defaults to this skill's bundled asset.",
    )
    parser.add_argument(
        "--include-untracked",
        action="store_true",
        help="Include non-ignored untracked files after the caller reviews their paths.",
    )
    parser.add_argument(
        "--max-file-size",
        type=positive_int,
        default=DEFAULT_MAX_FILE_SIZE,
        help=f"Maximum bytes per source file (default: {DEFAULT_MAX_FILE_SIZE}).",
    )
    parser.add_argument(
        "--split-bytes",
        type=positive_int,
        default=DEFAULT_SPLIT_BYTES,
        help=f"Split digest near this size at file boundaries (default: {DEFAULT_SPLIT_BYTES}).",
    )
    return parser.parse_args()


def run_git(
    repo: Path, *args: str, check: bool = True
) -> subprocess.CompletedProcess[bytes]:
    result = subprocess.run(
        ["git", "-C", str(repo), *args],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    if check and result.returncode != 0:
        detail = result.stderr.decode("utf-8", errors="replace").strip()
        raise BundleError(f"git {' '.join(args)} failed: {detail}")
    return result


def resolve_repo(raw_path: str) -> Path:
    candidate = Path(raw_path).expanduser().resolve()
    if not candidate.exists():
        raise BundleError(f"repository path does not exist: {candidate}")
    result = run_git(candidate, "rev-parse", "--show-toplevel")
    return Path(os.fsdecode(result.stdout).strip()).resolve()


def git_paths(repo: Path, *args: str) -> list[Path]:
    result = run_git(repo, "ls-files", "-z", *args)
    paths = []
    for raw in result.stdout.split(b"\0"):
        if not raw:
            continue
        rel = Path(os.fsdecode(raw))
        if rel.is_absolute() or ".." in rel.parts:
            raise BundleError(f"unsafe path reported by git: {rel!s}")
        paths.append(rel)
    return sorted(set(paths), key=lambda item: item.as_posix())


def exclusion_reason(rel: Path, *, tracked: bool) -> str | None:
    lower_parts = tuple(part.lower() for part in rel.parts)
    if any(part in TOOL_OR_GENERATED_DIRS for part in lower_parts):
        return "tool-or-generated-directory"

    name = rel.name.lower()
    if name.startswith(".env"):
        return "environment-file"
    if name in SENSITIVE_EXACT_NAMES:
        return "sensitive-filename"
    if rel.suffix.lower() in SENSITIVE_SUFFIXES:
        return "sensitive-or-runtime-file"
    if not tracked and name == "config.json":
        return "untracked-runtime-config"
    return None


def scan_secret_markers(path: Path) -> list[str]:
    try:
        data = path.read_bytes()
    except OSError as exc:
        raise BundleError(f"cannot read {path}: {exc}") from exc
    if b"\0" in data[:8192]:
        return []
    return [name for name, detector in SECRET_DETECTORS if detector.search(data)]


def collect_snapshot(
    repo: Path,
    stage_root: Path,
    *,
    include_untracked: bool,
    max_file_size: int,
) -> dict[str, object]:
    tracked_paths = git_paths(repo, "--cached")
    untracked_paths = git_paths(repo, "--others", "--exclude-standard")
    tracked_set = set(tracked_paths)
    selected = list(tracked_paths)
    if include_untracked:
        selected.extend(path for path in untracked_paths if path not in tracked_set)

    skipped: list[tuple[Path, str]] = []
    missing: list[Path] = []
    secret_hits: list[tuple[Path, list[str]]] = []
    included: list[Path] = []
    included_bytes = 0

    for rel in sorted(set(selected), key=lambda item: item.as_posix()):
        tracked = rel in tracked_set
        reason = exclusion_reason(rel, tracked=tracked)
        if reason:
            skipped.append((rel, reason))
            continue

        source = repo / rel
        if not source.exists() and not source.is_symlink():
            missing.append(rel)
            continue
        if source.is_dir():
            skipped.append((rel, "submodule-or-directory-entry"))
            continue

        destination = stage_root / rel
        destination.parent.mkdir(parents=True, exist_ok=True)

        if source.is_symlink():
            destination.write_text(
                f"SYMLINK -> {os.readlink(source)}\n",
                encoding="utf-8",
            )
            included.append(rel)
            included_bytes += destination.stat().st_size
            continue

        try:
            source_size = source.stat().st_size
        except OSError as exc:
            skipped.append((rel, f"stat-error:{exc.__class__.__name__}"))
            continue

        if source_size > max_file_size:
            skipped.append((rel, f"larger-than-{max_file_size}-bytes"))
            continue

        hits = scan_secret_markers(source)
        if hits:
            secret_hits.append((rel, hits))
            continue

        shutil.copy2(source, destination)
        included.append(rel)
        included_bytes += source_size

    if secret_hits:
        lines = [
            "high-confidence secret markers were detected; no bundle was created:",
        ]
        for rel, detectors in secret_hits:
            lines.append(f"  - {rel.as_posix()}: {', '.join(detectors)}")
        lines.append(
            "Inspect or remove those values before exporting. Matched values were not printed."
        )
        raise BundleError("\n".join(lines))

    excluded_untracked = [
        rel
        for rel in untracked_paths
        if not include_untracked or exclusion_reason(rel, tracked=False)
    ]

    return {
        "included": included,
        "included_bytes": included_bytes,
        "tracked_count": len(tracked_paths),
        "untracked": untracked_paths,
        "excluded_untracked": excluded_untracked,
        "skipped": skipped,
        "missing": missing,
    }


def choose_gitingest_command() -> tuple[list[str], str]:
    uvx = shutil.which("uvx")
    if uvx:
        package = f"gitingest=={GITINGEST_VERSION}"
        return [uvx, "--from", package, "gitingest"], f"{package} via uvx"

    installed = shutil.which("gitingest")
    if installed:
        return [installed], "gitingest from PATH"

    raise BundleError(
        "neither uvx nor gitingest is available; on macOS install uv with `brew install uv`"
    )


def run_gitingest(
    stage_root: Path, raw_output: Path, max_file_size: int
) -> tuple[str, str]:
    command, identity = choose_gitingest_command()
    command.extend(
        [
            str(stage_root),
            "--output",
            str(raw_output),
            "--max-size",
            str(max_file_size),
            "--include-gitignored",
        ]
    )
    for pattern in REVIEW_RELEVANT_INCLUDE_OVERRIDES:
        command.extend(["--include-pattern", pattern])

    result = subprocess.run(
        command,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).strip()
        raise BundleError(
            f"gitingest failed with exit code {result.returncode}: {detail}"
        )
    if not raw_output.is_file() or raw_output.stat().st_size == 0:
        raise BundleError("gitingest completed without creating a non-empty digest")
    return identity, result.stdout


def safe_slug(name: str) -> str:
    slug = re.sub(r"[^A-Za-z0-9._-]+", "-", name).strip("-._").lower()
    return slug or "repository"


def choose_output_root(repo: Path, requested: str | None) -> Path:
    if requested:
        return Path(requested).expanduser().resolve()

    dist_probe = repo / "dist" / "chatgpt-review" / ".probe"
    ignored = run_git(
        repo,
        "check-ignore",
        "--no-index",
        "-q",
        "--",
        str(dist_probe),
        check=False,
    )
    if ignored.returncode == 0:
        return (repo / "dist" / "chatgpt-review").resolve()
    return (Path(tempfile.gettempdir()) / "chatgpt-review").resolve()


def make_bundle_dir(output_root: Path, repo_name: str, timestamp: str) -> Path:
    output_root.mkdir(parents=True, exist_ok=True)
    base = output_root / f"{safe_slug(repo_name)}-{timestamp}"
    candidate = base
    counter = 2
    while candidate.exists():
        candidate = output_root / f"{base.name}-{counter}"
        counter += 1
    candidate.mkdir()
    return candidate


def split_digest(
    raw_path: Path, bundle_dir: Path, slug: str, split_bytes: int
) -> list[Path]:
    if raw_path.stat().st_size <= split_bytes:
        destination = bundle_dir / f"02-{slug}-gitingest.txt"
        shutil.copy2(raw_path, destination)
        return [destination]

    text = raw_path.read_text(encoding="utf-8")
    marker = re.compile(rf"(?m)^(?={re.escape(SEPARATOR)}\n(?:FILE|SYMLINK): )")
    starts = [match.start() for match in marker.finditer(text)]
    if not starts:
        raise BundleError(
            "digest exceeds split size but no gitingest file boundaries were found"
        )

    preamble = text[: starts[0]]
    sections = [
        text[start : starts[index + 1] if index + 1 < len(starts) else len(text)]
        for index, start in enumerate(starts)
    ]
    preamble_size = len(preamble.encode("utf-8"))
    groups: list[list[str]] = []
    current: list[str] = []
    current_size = preamble_size

    for section in sections:
        section_size = len(section.encode("utf-8"))
        if current and current_size + section_size > split_bytes:
            groups.append(current)
            current = []
            current_size = preamble_size
        current.append(section)
        current_size += section_size
    if current:
        groups.append(current)

    total = len(groups)
    outputs = []
    for index, group in enumerate(groups, start=1):
        destination = bundle_dir / (
            f"02-{slug}-gitingest-part-{index:03d}-of-{total:03d}.txt"
        )
        header = (
            f"CHATGPT REVIEW DIGEST PART {index}/{total}\n"
            "Upload and read every numbered part before drawing conclusions.\n\n"
        )
        destination.write_text(
            header + preamble + "".join(group),
            encoding="utf-8",
        )
        outputs.append(destination)
    return outputs


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def git_metadata(repo: Path) -> tuple[str, str, str]:
    branch_result = run_git(
        repo, "symbolic-ref", "--quiet", "--short", "HEAD", check=False
    )
    branch = os.fsdecode(branch_result.stdout).strip() or "detached-HEAD"
    commit = os.fsdecode(
        run_git(repo, "rev-parse", "--short=12", "HEAD").stdout
    ).strip()
    status = os.fsdecode(
        run_git(repo, "status", "--short", "--untracked-files=all").stdout
    ).rstrip()
    return branch, commit, status


def render_path_list(paths: list[Path], *, limit: int = 200) -> str:
    if not paths:
        return "- None"
    lines = [f"- `{path.as_posix()}`" for path in paths[:limit]]
    if len(paths) > limit:
        lines.append(f"- ... and {len(paths) - limit} more")
    return "\n".join(lines)


def write_review_prompt(
    template_path: Path,
    destination: Path,
    *,
    repo_name: str,
    branch: str,
    commit: str,
    generated_at: str,
    digest_paths: list[Path],
) -> None:
    if not template_path.is_file():
        raise BundleError(f"review prompt template not found: {template_path}")
    content = template_path.read_text(encoding="utf-8")
    replacements = {
        "{{REPOSITORY_NAME}}": repo_name,
        "{{BRANCH}}": branch,
        "{{COMMIT}}": commit,
        "{{GENERATED_AT}}": generated_at,
        "{{DIGEST_FILES}}": "\n".join(f"  - `{path.name}`" for path in digest_paths),
    }
    for marker, value in replacements.items():
        content = content.replace(marker, value)
    destination.write_text(content, encoding="utf-8")


def write_manifest(
    destination: Path,
    *,
    repo: Path,
    branch: str,
    commit: str,
    status: str,
    generated_at: str,
    snapshot: dict[str, object],
    gitingest_identity: str,
    digest_paths: list[Path],
    estimated_tokens: str,
) -> None:
    skipped = snapshot["skipped"]
    skipped_lines = [f"- `{path.as_posix()}` — {reason}" for path, reason in skipped]
    digest_lines = [
        f"- `{path.name}` — {path.stat().st_size:,} bytes — SHA-256 `{sha256(path)}`"
        for path in digest_paths
    ]
    status_block = status if status else "(clean)"
    content = f"""# ChatGPT review bundle manifest

- Source: `{repo}`
- Branch: `{branch}`
- Commit: `{commit}`
- Generated: `{generated_at}`
- Snapshot policy: tracked files from the current working tree
- Tracked paths reported by Git: {snapshot["tracked_count"]}
- Files included in the sanitized staging tree: {len(snapshot["included"])}
- Included source bytes: {snapshot["included_bytes"]:,}
- Gitingest runtime: `{gitingest_identity}`
- Gitingest estimated tokens: {estimated_tokens}

## Upload together

Upload `01-REVIEW-PROMPT.md` and every digest file below to one ChatGPT Pro
conversation. Then paste the text of `01-REVIEW-PROMPT.md` as your message.

{os.linesep.join(digest_lines)}

## Working tree status at export

```text
{status_block}
```

## Excluded untracked paths

These were not sent to gitingest. Review them before using
`--include-untracked`.

{render_path_list(snapshot["excluded_untracked"])}

## Other skipped paths

{os.linesep.join(skipped_lines) if skipped_lines else "- None"}

## Deleted or missing tracked paths

{render_path_list(snapshot["missing"])}

## Security boundary

The script excluded high-risk filenames and failed closed on high-confidence
secret markers. This is a local preparation step, not a guarantee that the
repository contains no confidential business logic or unusual credential
format. No repository content was uploaded by the script.
"""
    destination.write_text(content, encoding="utf-8")


def extract_estimated_tokens(gitingest_stdout: str, raw_digest: Path) -> str:
    with raw_digest.open(encoding="utf-8") as handle:
        digest_prefix = handle.read(256_000)
    match = re.search(
        r"(?m)^Estimated tokens:\s*(.+)$",
        gitingest_stdout + "\n" + digest_prefix,
    )
    return match.group(1).strip() if match else "not reported"


def main() -> int:
    args = parse_args()
    try:
        repo = resolve_repo(args.repo)
        branch, commit, status = git_metadata(repo)
        generated = datetime.now().astimezone()
        timestamp = generated.strftime("%Y%m%d-%H%M%S")
        generated_at = generated.isoformat(timespec="seconds")
        repo_name = repo.name
        slug = safe_slug(repo_name)

        script_dir = Path(__file__).resolve().parent
        default_template = script_dir.parent / "assets" / "review-prompt.md"
        template = (
            Path(args.prompt_template).expanduser().resolve()
            if args.prompt_template
            else default_template
        )

        with tempfile.TemporaryDirectory(prefix="chatgpt-review-stage-") as temp:
            temp_root = Path(temp)
            stage_root = temp_root / repo_name
            stage_root.mkdir()
            snapshot = collect_snapshot(
                repo,
                stage_root,
                include_untracked=args.include_untracked,
                max_file_size=args.max_file_size,
            )
            if not snapshot["included"]:
                raise BundleError("no safe files remained after snapshot filtering")

            raw_digest = temp_root / "gitingest-raw.txt"
            gitingest_identity, gitingest_stdout = run_gitingest(
                stage_root,
                raw_digest,
                args.max_file_size,
            )
            estimated_tokens = extract_estimated_tokens(
                gitingest_stdout,
                raw_digest,
            )

            output_root = choose_output_root(repo, args.output_root)
            bundle_dir = make_bundle_dir(output_root, repo_name, timestamp)
            digest_paths = split_digest(
                raw_digest,
                bundle_dir,
                slug,
                args.split_bytes,
            )
            prompt_path = bundle_dir / "01-REVIEW-PROMPT.md"
            manifest_path = bundle_dir / "00-MANIFEST.md"
            write_review_prompt(
                template,
                prompt_path,
                repo_name=repo_name,
                branch=branch,
                commit=commit,
                generated_at=generated_at,
                digest_paths=digest_paths,
            )
            write_manifest(
                manifest_path,
                repo=repo,
                branch=branch,
                commit=commit,
                status=status,
                generated_at=generated_at,
                snapshot=snapshot,
                gitingest_identity=gitingest_identity,
                digest_paths=digest_paths,
                estimated_tokens=estimated_tokens,
            )

        print(f"Bundle created: {bundle_dir}")
        print(f"Manifest: {manifest_path}")
        print(f"Review prompt: {prompt_path}")
        for digest_path in digest_paths:
            print(f"Digest: {digest_path}")
        print(
            f"Included {len(snapshot['included'])} files; "
            f"excluded {len(snapshot['excluded_untracked'])} untracked paths."
        )
        print("No repository content was uploaded.")
        return 0
    except BundleError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
