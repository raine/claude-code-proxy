# SIWC test fixtures

This RSA private key and its public JWKS were generated solely for local mock
OAuth tests. They are public test fixtures, never production credentials.

`claude-code-2.1.310.json` preserves the request structure emitted by the
installed native Claude Code 2.1.310 in an isolated, empty test project. System
and message text and tool descriptions are replaced with synthetic text, the
model is `gpt-test`, and metadata uses a fixture identity. It contains the
observed 12 built-in tool schemas, cache-control blocks, system-role message,
thinking/context-management/output-config fields, and streaming flag. It
contains no OAuth credentials, user project content, or original instructions.
