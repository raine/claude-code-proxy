---
title: Compatibility and limitations
description: Canonical security, account, protocol, model, context, multimodal, reasoning, tool, session, rate-limit, and deployment boundaries.
---

claude-code-proxy targets Claude Code's practical Anthropic API usage rather than complete protocol equivalence.

## Accounts and provider policy

- Provider subscriptions, allowed models, regions, quotas, and enforcement remain under each provider's control.
- OpenAI has publicly welcomed using Codex through other coding harnesses, but public statements do not guarantee future policy or account treatment.
- Kimi, Grok, and Cursor use unofficial client integrations. Review the terms and account risk for your use.
- Upstream rate limits are shared with other clients on the same account.

## Listener security

- Incoming clients are not authenticated.
- The default bind address is `127.0.0.1`.
- A non-loopback listener requires an external firewall or authenticating reverse proxy.
- The proxy stores subscription credentials and can consume account quota, so an exposed listener is equivalent to exposing that capability.

## Anthropic API scope

- Messages supports streaming and non-streaming responses for the fields exercised by Claude Code.
- `?beta=true` does not select a separate implementation.
- Token counts are local estimates, not exact upstream tokenizer or billing counts.
- Claude Code title generation and other structured background requests are forwarded and consume provider tokens.
- Anthropic-specific fields without a provider mapping can be dropped.
- Native OpenAI Responses passthrough is opt-in and limited to registered Codex models and response creation.
- Codex Images passthrough is separately opt-in, restricted to `gpt-image-2`, and supports generation plus JSON or multipart edits. Variations, masks, remote image URLs, and URL-formatted outputs are unsupported.

## Models and context

- Local registration does not guarantee account access to a model.
- Unknown model IDs have no implicit provider fallback.
- Anthropic-style aliases route only to the configured Codex or Kimi alias provider.
- `[1m]` is a Claude Code client hint and does not change upstream context.
- Provider context limits can be lower than Claude Code's local threshold.
- Switching provider or model can clear provider-specific continuation assumptions while Claude Code retains portable history.

## Codex

- Base64 user images and supported base64 tool-result images map to Responses images. Remote URLs and malformed or unsupported nested images remain text placeholders.
- Reasoning summaries can appear as thinking blocks. Codex decides whether a summary is emitted.
- Encrypted reasoning and compaction items remain provider continuation data and are not exposed as raw chain of thought.
- Hosted web search supports mapped domain filters, but the Anthropic `max_uses` value is not enforced because Codex exposes no equivalent limit.
- Strict JSON schema output is translated. Other Anthropic-only output settings can be omitted.
- `previous_response_id` and server compaction state are in memory and require stable session routing.
- Automatic transport fallback occurs only before an upstream request is sent, which avoids replaying possible side effects.

## Kimi

- The proxy exposes one Kimi Code wire model plus local aliases.
- Reasoning effort supports Kimi's low, medium, and high levels.
- Images in tool results use Kimi tool-message image parts.
- The persistent device ID is part of the login identity and must remain stable.

## Grok

- Model availability varies by account and region.
- Hosted general web search and X search are translated with citations and usage.
- The implemented multimodal path does not claim general image or video compatibility.

## Cursor Agent

- An installed Cursor Agent JavaScript bundle supplies protobuf classes.
- The dynamic catalog reflects the installed Cursor Agent and can differ across machines.
- The native tool bridge covers recognized `Read`, `Write`, and `Bash` calls when matching tools and a session ID are present.
- Cursor workspace callbacks and arbitrary native tool forms do not have a general Claude tool bridge.
- Conversation and pending tool state are in memory. Restarts clear them.
- Cursor count-tokens uses a rough rendered-prompt estimate.

## Diagnostics and privacy

- Structured logs redact known credential keys, but user-provided strings can still contain secrets.
- Error captures contain complete redacted failed responses.
- Traffic captures intentionally preserve prompts and tool content for message/Responses diagnostics.
- Image generation and edit routes never create traffic captures or persist prompts, uploads, data URLs, generated base64, or upstream error bodies.
- Verbose logging and traffic capture should be scoped to a focused local investigation.

For provider-specific behavior, use the [provider pages](/providers/choosing-a-provider/). For released changes, see the [Changelog](/reference/changelog/).
