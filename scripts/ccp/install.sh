#!/usr/bin/env bash
# Install the `ccp` helper: a systemd user service for claude-code-proxy plus a wrapper script.
# Linux/WSL only (needs systemd --user). Idempotent — safe to re-run; overwrites the unit and wrapper with these versions.
# See README.md (in this dir) for details
set -euo pipefail

bin="$HOME/.local/bin/claude-code-proxy"
wrapper="$HOME/.local/bin/ccp"
unit="$HOME/.config/systemd/user/claude-code-proxy.service"

# --- sanity: systemd --user must be available (Linux/WSL only; not macOS) ---
if ! systemctl --user show-environment >/dev/null 2>&1; then
  echo "error: 'systemctl --user' isn't available. This installer is Linux/WSL only — it uses systemd." >&2
  echo "       macOS has no systemd (it would need a launchd agent instead); this script doesn't set that up." >&2
  echo "       WSL: set systemd=true in /etc/wsl.conf, then 'wsl --shutdown' and reopen." >&2
  exit 1
fi

# --- sanity: the proxy binary the service will run ---
if [[ ! -x "$bin" ]]; then
  echo "warning: $bin not found or not executable." >&2
  echo "         Install it first (eg 'just install' in the repo, or copy a build there); the service ExecStart points at it." >&2
fi

mkdir -p "$(dirname "$unit")" "$(dirname "$wrapper")"

# --- systemd user unit ---
cat > "$unit" <<'EOF'
[Unit]
Description=Claude Code Proxy for Codex

[Service]
Type=simple
ExecStart=%h/.local/bin/claude-code-proxy serve
Restart=on-failure
RestartSec=2
Environment=CCP_LOG_STDERR=1

[Install]
WantedBy=default.target
EOF
echo "wrote $unit"

# --- ccp wrapper ---
cat > "$wrapper" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

svc="claude-code-proxy.service"

case "${1:-status}" in
  start)
    systemctl --user start "$svc"
    ;;
  stop)
    systemctl --user stop "$svc"
    ;;
  restart)
    systemctl --user restart "$svc"
    ;;
  status)
    systemctl --user status "$svc" --no-pager
    ;;
  logs)
    if [[ "${2:-}" == "-f" || "${2:-}" == "--follow" ]]; then
      journalctl --user -u "$svc" -f
    else
      journalctl --user -u "$svc" --no-pager
    fi
    ;;
  on)
    systemctl --user enable --now "$svc"
    ;;
  off)
    systemctl --user disable --now "$svc"
    ;;
  health)
    curl -fsS http://127.0.0.1:18765/healthz && echo
    ;;
  *)
    echo "Usage: ccp {start|stop|restart|status|logs [-f]|on|off|health}"
    exit 2
    ;;
esac
EOF
chmod +x "$wrapper"
echo "wrote $wrapper (executable)"

# --- reload so systemd sees the (re)written unit ---
systemctl --user daemon-reload
echo "reloaded systemd user units"

echo
echo "Done. Next steps:"
echo "  - ensure ~/.local/bin is on your PATH"
echo "  - 'ccp on'      enable + start (also starts on login)"
echo "  - 'ccp status'  check it's active"
echo "  - 'ccp health'  confirm it's serving on 127.0.0.1:18765"