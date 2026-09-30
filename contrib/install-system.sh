#! /usr/bin/env bash
#
# Install forge-bot as a system service for explicit multi-user mode. Run as
# root:
#
#   sudo ./contrib/install-system.sh
#
# This deployment runs the gateway as root so it can fork each agent, move it
# into a per-run cgroup v2 and drop to its configured `[users.*].host_user`.
# Each agent and workspace `git` command executes as that account, never as
# root.
#
# Environment:
#   BIN_SRC     Binary to install (default ../target/release/forge-bot)
#   CONFIG_SRC  Config to install (default ../forge-bot.toml, else
#                                    ../config.example.toml)
#   PREFIX      Install prefix       (default /usr/local)
#   CONFIG_DIR  Config directory     (default /etc/forge-bot)
#
# Secrets are never written into the config or the unit. Supply them through
# the 0600 environment file, which the installer creates from the example and
# leaves for the operator to edit.

set -euo pipefail

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly PROJECT_DIR="$(cd -- "$SCRIPT_DIR/.." && pwd)"
readonly BIN_SRC="${BIN_SRC:-$PROJECT_DIR/target/release/forge-bot}"
readonly PREFIX="${PREFIX:-/usr/local}"
readonly CONFIG_DIR="${CONFIG_DIR:-/etc/forge-bot}"

readonly BIN_DST="$PREFIX/bin/forge-bot"
readonly CONFIG_DST="$CONFIG_DIR/forge-bot.toml"
readonly ENV_DST="$CONFIG_DIR/forge-bot.env"
readonly UNIT_DST="/etc/systemd/system/forge-bot.service"
readonly STATE_DIR="/var/lib/forge-bot"
readonly SESSION_DIR="$STATE_DIR/state"
readonly LOG_DIR="/var/log/forge-bot"

if (( EUID != 0 )); then
    echo "Run this installer as root: the gateway must fork agents and manage cgroups." >&2
    exit 1
fi

# Prefer an existing project config; otherwise use the shared example.
if [[ -n "${CONFIG_SRC:-}" ]]; then
    config_src="$CONFIG_SRC"
elif [[ -f "$PROJECT_DIR/forge-bot.toml" ]]; then
    config_src="$PROJECT_DIR/forge-bot.toml"
else
    config_src="$PROJECT_DIR/config.example.toml"
fi

for file in "$BIN_SRC" "$config_src" "$SCRIPT_DIR/forge-bot.system.service"; do
    if [[ ! -f "$file" ]]; then
        echo "Missing required file: $file" >&2
        exit 1
    fi
done

install -d -m700 "$CONFIG_DIR" "$LOG_DIR"
# Gateway state stays private to root. Checkouts are not kept here: an explicit
# `[users.*]` run checks out directly in its `host_user`'s home, which that
# account already owns.
install -d -m700 "$STATE_DIR"
install -d -m700 "$SESSION_DIR"

echo "Installing binary to $BIN_DST..."
install -Dm755 "$BIN_SRC" "$BIN_DST"

if [[ -f "$CONFIG_DST" ]]; then
    echo "Keeping existing config at $CONFIG_DST"
else
    echo "Installing config to $CONFIG_DST (edit it!)..."
    install -Dm600 "$config_src" "$CONFIG_DST"
fi

if [[ ! -f "$ENV_DST" ]]; then
    echo "Installing example environment file to $ENV_DST (edit it!)..."
    install -Dm600 "$SCRIPT_DIR/forge-bot.env.example" "$ENV_DST"
fi

echo "Installing system unit to $UNIT_DST..."
install -Dm644 "$SCRIPT_DIR/forge-bot.system.service" "$UNIT_DST"

systemctl daemon-reload
systemctl enable --now forge-bot

cat <<'EOF'

Installed. Next steps:
  1. Create the non-root Linux accounts named in [users.<id>].host_user.
  2. Add the [users.*] tables to /etc/forge-bot/forge-bot.toml. Every run
     with a host_user is delegated to systemd; there is no direct-spawn mode.
  3. systemctl restart forge-bot
  4. systemctl status forge-bot
EOF
