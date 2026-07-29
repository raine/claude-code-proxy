---
name: ccproxy-chatgpt-review
description: Create a sanitized, tracked-files-only gitingest bundle of the ccproxy Rust repository for upload to ChatGPT Pro. Use when asked to let ChatGPT study ccproxy, prepare a full-repository digest, perform architecture, correctness, security, or reliability review, or produce prioritized optimization suggestions from the current working tree.
---

# ccproxy ChatGPT Review

Export the current ccproxy working tree into numbered, upload-ready files. Keep
the operation local: build the bundle and report its paths, but never upload it
or send repository content to a remote service.

## Workflow

1. Resolve the repository root with `git rev-parse --show-toplevel`.
2. Confirm `Cargo.toml` declares the `claude-code-proxy` package. Stop if this is
   a different repository.
3. Invoke `scripts/build_review_bundle.py` by its absolute path, resolved from
   this skill directory:

   ```bash
   python3 /absolute/path/to/this-skill/scripts/build_review_bundle.py \
     --repo "$(git rev-parse --show-toplevel)" \
     --output-root "$(git rev-parse --show-toplevel)/dist/chatgpt-review"
   ```

4. Treat a secret-scan failure as a hard stop. Report only detector names and
   affected paths; never print or copy the matched value.
5. Read `00-MANIFEST.md` and verify that the bundle contains the expected
   tracked source, tests, fixtures, Cargo metadata, documentation, scripts,
   hooks, and CI files. Keep `.agents`, generated outputs, credentials, logs,
   databases, and traffic captures excluded.
6. If relevant untracked source exists, list it first. Use
   `--include-untracked` only after the user confirms those paths are safe and
   should leave the machine.
7. Return clickable absolute paths for `00-MANIFEST.md`,
   `01-REVIEW-PROMPT.md`, and every numbered digest part. Tell the user to
   upload all numbered files to the same ChatGPT Pro conversation and paste the
   contents of `01-REVIEW-PROMPT.md`.

## Scope and safety

- Snapshot tracked files from the current working tree, not only `HEAD`; this
  preserves tracked uncommitted edits.
- Exclude untracked and ignored files by default. Do not use gitingest directly
  on the live repository because it can ingest untracked files.
- Preserve `Cargo.lock`, `flake.lock`, `.gitignore`, and other review-relevant
  metadata even though gitingest normally omits some lockfiles.
- Use the bundled ccproxy prompt, which asks ChatGPT to verify streaming,
  retry, reducer terminal events, session state, OAuth storage, redaction,
  resource limits, and the loopback trust boundary with file-level evidence.
- The script uses isolated `uvx` execution of gitingest 0.3.1 when available.
  First use can download the package from PyPI; repository contents remain
  local until the user uploads the resulting files.
- A large digest is split only at gitingest file boundaries. Upload every part;
  no part alone represents the whole repository.

Do not present the generated digest as a completed review. It is an input bundle
for ChatGPT; any findings still require evidence and, where applicable, local
reproduction and tests.
