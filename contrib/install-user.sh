#! /usr/bin/env bash
#
# Install forge-bot as a per-user systemd service. Run as the target user, not
# as root:
#
#   ./contrib/install-user.sh
#
# This keeps the bot, its config and its state inside the invoking user's home.
# The service is started and controlled with:
#
#   systemctl --user status forge-bot
#   journalctl --user -u forge-bot -f
#   systemctl --user restart forge-bot
#
# Environment:
#   BIN_SRC       Binary to install    (default ../target/release/forge-bot)
#   CONFIG_SRC    Config to install    (default ../forge-bot.toml, else the
#                                      per-user example)
#   PREFIX        Install prefix       (default $HOME/.local)
#   ENABLE_LINGER Set to 1 to run the service even while logged out
#                 (`loginctl enable-linger "$USER"`).
#
# Secrets are taken from the existing env file, or from
# contrib/forge-bot.env.example when no env file is present yet. The installer
# never writes secrets into the config or the unit.

set -euo pipefail

readonly SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly PROJECT_DIR="$(cd -- "$SCRIPT_DIR/.." && pwd)"
readonly BIN_SRC="${BIN_SRC:-$PROJECT_DIR/target/release/forge-bot}"
readonly PREFIX="${PREFIX:-$HOME/.local}"

readonly BIN_DST="$PREFIX/bin/forge-bot"
readonly CONFIG_DIR="$HOME/.config/forge-bot"
readonly CONFIG_DST="$CONFIG_DIR/forge-bot.toml"
readonly ENV_DST="$CONFIG_DIR/forge-bot.env"
readonly UNIT_DIR="$HOME/.config/systemd/user"
readonly UNIT_DST="$UNIT_DIR/forge-bot.service"

if (( EUID == 0 )); then
    echo "Do not run this installer as root: the service must run as the user." >&2
    exit 1
fi

# Prefer an existing project config; otherwise fall back to the user example.
if [[ -n "${CONFIG_SRC:-}" ]]; then
    config_src="$CONFIG_SRC"
elif [[ -f "$PROJECT_DIR/forge-bot.toml" ]]; then
    config_src="$PROJECT_DIR/forge-bot.toml"
else
    config_src="$SCRIPT_DIR/forge-bot.user.toml.example"
fi

for file in "$BIN_SRC" "$config_src" "$SCRIPT_DIR/forge-bot.user.service"; do
    if [[ ! -f "$file" ]]; then
        echo "Missing required file: $file" >&2
        exit 1
    fi
done

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

echo "Installing systemd user unit to $UNIT_DST..."
install -Dm644 "$SCRIPT_DIR/forge-bot.user.service" "$UNIT_DST"

# The unit appends stdout/stderr here. systemd opens that file before it
# creates any StateDirectory=, so the directory must exist before the first
# start or the service fails with status=209/STDOUT.
install -d -m700 "$HOME/.local/state/forge-bot"

# `systemctl --user` needs a running user manager, which needs a runtime dir.
if [[ -z "${XDG_RUNTIME_DIR:-}" && -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]]; then
    cat >&2 <<EOF

Installed, but no user session bus was found (XDG_RUNTIME_DIR is unset).
Start the service from a logged-in session with:

    systemctl --user daemon-reload
    systemctl --user enable --now forge-bot
EOF
    exit 0
fi

systemctl --user daemon-reload
systemctl --user enable --now forge-bot

if [[ "${ENABLE_LINGER:-0}" == "1" ]]; then
    echo "Enabling linger so the service survives logout..."
    loginctl enable-linger "$USER"
fi

cat <<EOF

Installed as a user service (running as $USER).

Edit secrets:  $ENV_DST
Edit config:   $CONFIG_DST
Logs:          journalctl --user -u forge-bot -f
               (also ~/.local/state/forge-bot/forge-bot.log)
Status:        systemctl --user status forge-bot
Stop:          systemctl --user stop forge-bot

Until a webhook is registered, keep \`[poller] enabled = true\` in the config.
EOF
