---
title: Account profiles
description: Save multiple Codex OAuth accounts and Anthropic API keys, and switch a running proxy from the monitor.
---

Codex and Anthropic support named account profiles. Each proxy process uses **one account per provider at a time**. The monitor can switch that account without restarting. There is no round-robin, quota-based switching, or automatic account failover.

## Codex accounts

Authenticate each account separately through its own browser flow:

```sh
claude-code-proxy codex auth login --profile work
claude-code-proxy codex auth login --profile personal
claude-code-proxy codex profile list
claude-code-proxy codex profile use work
```

You can also use `codex auth device --profile NAME`. `codex profile add NAME` is a convenience for browser login and refuses to overwrite an existing profile. Use `auth login --profile NAME` to deliberately reauthenticate it. Make sure the browser signs in to the intended account for each login; two profile names alone do not create two accounts.

```sh
claude-code-proxy codex auth status --profile work
claude-code-proxy serve --codex-profile personal
```

OAuth refresh updates only the selected profile's file. Named profiles do not fall back to the default account, legacy auth file, or macOS keychain. Default-profile keychain behavior is unchanged. Do not copy the same refresh token into multiple profiles or concurrently use one profile in several proxy processes: refresh-token rotation is not coordinated across processes.

## Anthropic API-key profiles

Anthropic profiles store API keys, not Claude Pro/Max browser logins. API usage is billed separately.

Provide a key through `CCP_ANTHROPIC_API_KEY`, then `ANTHROPIC_API_KEY`, or pipe it from a secret manager on standard input. Keys are not accepted as command-line arguments or printed in status/list output.

```sh
# Configure the key in this shell or secret manager first.
claude-code-proxy anthropic auth login --profile work
# Change the key in your shell/secret manager before adding another account.
claude-code-proxy anthropic profile add personal
claude-code-proxy anthropic profile list
claude-code-proxy anthropic profile use work
claude-code-proxy anthropic auth status --profile personal
```

A terminal login with no key environment variable reports instructions instead of echoing a secret prompt. A piped key is read up to 16 KiB. Login saves the key locally but does not validate it or make a paid API request.

When a named profile is selected, **only its saved key is used**, even if a different default API key is present in the environment or `config.json`. A missing/invalid named profile fails authentication rather than silently switching accounts. The upstream base URL remains the shared Anthropic provider setting.

## Selection and process lifetime

Selection precedence:

1. `serve --codex-profile NAME` / `--anthropic-profile NAME`, or `auth ... --profile NAME`.
2. `CCP_CODEX_PROFILE` / `CCP_ANTHROPIC_PROFILE`.
3. Saved default selected by `<provider> profile use NAME`.
4. The implicit `default` profile, retaining existing credentials/configuration.

```sh
claude-code-proxy serve --codex-profile work --anthropic-profile personal
```

These settings choose the startup account. `profile use` affects **future processes only**. To change the running proxy, open Accounts (`a`), select a profile, press `Enter`, and confirm with `y`. The switch applies to **all sessions** using that provider and does not change the saved default. It is rejected while inference requests or streams are active; wait for them to finish and retry. Missing or locally invalid credentials leave the current account unchanged. No live authentication or paid validation request is made.

Switching rebuilds the provider clients and resets Codex WebSocket, continuation, and compaction state so the new account cannot reuse the old account's connection or response IDs. To use different accounts concurrently, run separate proxies on separate ports and select a different profile for each; do not share a rotating Codex OAuth profile across those processes.

To restore existing behavior:

```sh
claude-code-proxy codex profile use default
claude-code-proxy anthropic profile use default
```

Unset any profile environment override or pass `--profile default` / the corresponding serve flag to override it explicitly.

## Monitor UI

Press `a` in the proxy monitor (built-in or separately attached) to open Accounts. It lists every profile with local credential status, Codex expiry, `[selected]` for the running process, and `[saved-default]` for future processes. Scroll with arrows/`j`/`k`, page keys, or `Home`/`End`; `Enter` requests a switch, `y` confirms, and `n`/`Esc` cancels. Outside confirmation, `a`/`Esc` closes Accounts. Metadata refreshes about every five seconds without making provider requests or displaying secrets. These are local statuses, not quota or live API-key validation.

Attached monitors use the same loopback-only control endpoint as the integrated monitor. Older services report that runtime switching is unsupported. Demo mode cannot switch real accounts. A disconnected monitor cannot submit a switch. If a control request times out, check `[selected]` after reconnecting before retrying; closing the dashboard does not undo a submitted switch.

## Storage and removal

Files live under the proxy configuration directory (`CCP_CONFIG_DIR` when set):

- `accounts/codex/NAME.json`: OAuth access/refresh credentials.
- `accounts/anthropic/NAME.json`: API key.
- `account-profiles.json`: default profile names only, no secrets.

Named credentials are local files, not encrypted by the proxy. On Unix the existing auth writer restricts credential files to mode `0600` and their immediate directory to `0700`. On Windows protect the configuration directory with appropriate account permissions. Do not commit these files or include them in shared backups/logs.

Names use 1-64 lowercase ASCII letters, digits, `-`, or `_`. Paths, dotted names, uppercase names, and Windows device names are rejected. `default` is reserved for existing provider configuration.

```sh
claude-code-proxy codex profile use default
claude-code-proxy codex profile remove work --yes
claude-code-proxy anthropic profile remove personal --yes
```

Removal requires `--yes` and refuses to delete the saved default profile until another default is selected. The implicit `default` profile cannot be removed with this command. Codex `auth logout --profile NAME` retains its existing logout semantics and clears only that selected account's local credentials; it may leave a saved default pointing to an unauthenticated profile. Anthropic `auth logout --profile NAME` prints the explicit removal command.

Removing local credentials does not revoke provider access or erase credentials already cached by a running proxy. Stop/restart affected processes before removal; a running Codex refresh may recreate its credential file. Revoke the key/token upstream when revocation is intended.
