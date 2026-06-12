#!/usr/bin/env bash
# Provision a Mac as the Gitea Actions runner for the Apple client CI
# (.gitea/workflows/apple.yml). Idempotent — safe to re-run. Run ON THE MAC, or from a
# dev box:
#
#   ssh <mac> GITEA_RUNNER_TOKEN=<registration token> bash -s < scripts/ci/setup-macos-runner.sh
#
# Installs: rustup (+ both darwin targets for the universal xcframework), Node.js (the
# runner executes JS actions like actions/checkout via `node` from PATH — host mode does
# not auto-provision it), the act_runner binary (host mode — jobs run directly on macOS,
# no containers), and a LaunchAgent that keeps the runner daemon alive. Registration only
# happens once (.runner file); the token is NOT persisted by this script.
#
# Env knobs: GITEA_INSTANCE (default https://git.unom.io), GITEA_RUNNER_TOKEN (required
# for first-time registration only), RUNNER_NAME (default: LocalHostName), RUNNER_LABELS
# (default macos-arm64:host — matches apple.yml's runs-on), ACT_RUNNER_VERSION,
# NODE_VERSION.
#
# NOT installed here: Xcode. swift build/test work with Command Line Tools, but
# scripts/build-xcframework.sh needs xcodebuild (-create-xcframework) from a full Xcode.
set -euo pipefail

INSTANCE="${GITEA_INSTANCE:-https://git.unom.io}"
VERSION="${ACT_RUNNER_VERSION:-1.0.8}"
RUNNER_NAME="${RUNNER_NAME:-$(scutil --get LocalHostName)}"
LABELS="${RUNNER_LABELS:-macos-arm64:host}"
RUNNER_HOME="$HOME/ci/act-runner"
BIN_DIR="$HOME/.local/bin"
PLIST="$HOME/Library/LaunchAgents/io.gitea.act_runner.plist"

# --- Rust toolchain (the xcframework is built from the Rust core) -----------------------
if [ ! -x "$HOME/.cargo/bin/rustup" ]; then
    echo "==> installing rustup"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --no-modify-path --profile minimal
fi
"$HOME/.cargo/bin/rustup" target add aarch64-apple-darwin x86_64-apple-darwin

# --- Node.js (actions runtime; sudo-free tarball install) --------------------------------
NODE_VERSION="${NODE_VERSION:-22.22.3}"
mkdir -p "$BIN_DIR"
if ! "$BIN_DIR/node" --version 2>/dev/null | grep -q "^v${NODE_VERSION}$"; then
    echo "==> installing node v$NODE_VERSION"
    NODE_DIR="$HOME/.local/node-v$NODE_VERSION"
    mkdir -p "$NODE_DIR"
    curl -fL "https://nodejs.org/dist/v${NODE_VERSION}/node-v${NODE_VERSION}-darwin-arm64.tar.gz" \
        | tar -xz --strip-components=1 -C "$NODE_DIR"
    ln -sf "$NODE_DIR/bin/node" "$BIN_DIR/node"
fi
"$BIN_DIR/node" --version

# --- act_runner binary -------------------------------------------------------------------
# Renamed upstream to "gitea-runner" as of 1.0 (dl.gitea.com/act_runner/ stops at 0.6.x);
# we keep the local name act_runner — the CLI surface is unchanged.
mkdir -p "$BIN_DIR" "$RUNNER_HOME"
if ! "$BIN_DIR/act_runner" --version 2>/dev/null | grep -q "$VERSION"; then
    echo "==> installing act_runner (gitea-runner) $VERSION"
    curl -fL "https://dl.gitea.com/gitea-runner/${VERSION}/gitea-runner-${VERSION}-darwin-arm64" \
        -o "$BIN_DIR/act_runner.tmp"
    chmod +x "$BIN_DIR/act_runner.tmp"
    mv "$BIN_DIR/act_runner.tmp" "$BIN_DIR/act_runner"
fi
"$BIN_DIR/act_runner" --version

# --- config + one-time registration ------------------------------------------------------
cd "$RUNNER_HOME"
[ -f config.yaml ] || "$BIN_DIR/act_runner" generate-config > config.yaml
if [ ! -f .runner ]; then
    if [ -z "${GITEA_RUNNER_TOKEN:-}" ]; then
        echo "ERROR: not registered yet — re-run with GITEA_RUNNER_TOKEN=<token>" >&2
        echo "       (org unom -> Settings -> Actions -> Runners -> Create new runner)" >&2
        exit 1
    fi
    "$BIN_DIR/act_runner" register --no-interactive \
        --instance "$INSTANCE" \
        --token "$GITEA_RUNNER_TOKEN" \
        --name "$RUNNER_NAME" \
        --labels "$LABELS"
fi

# --- LaunchAgent: keep the daemon alive across crashes and (GUI) logins ------------------
# PATH must carry the CLT tools, cargo and act_runner itself; jobs inherit it.
# If the system developer dir is CLT-only but a full Xcode is installed, hand jobs a
# DEVELOPER_DIR override — the per-process equivalent of `xcode-select -s`, no sudo needed.
DEVELOPER_DIR_XML=""
if ! /usr/bin/xcodebuild -version >/dev/null 2>&1; then
    for app in /Applications/Xcode.app /Applications/Xcode*.app; do
        if DEVELOPER_DIR="$app/Contents/Developer" /usr/bin/xcodebuild -version >/dev/null 2>&1; then
            DEVELOPER_DIR_XML="<key>DEVELOPER_DIR</key><string>$app/Contents/Developer</string>"
            echo "==> using full Xcode at $app via DEVELOPER_DIR"
            break
        fi
    done
fi

mkdir -p "$(dirname "$PLIST")"
cat > "$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>io.gitea.act_runner</string>
    <key>ProgramArguments</key>
    <array>
        <string>$BIN_DIR/act_runner</string>
        <string>daemon</string>
        <string>--config</string>
        <string>$RUNNER_HOME/config.yaml</string>
    </array>
    <key>WorkingDirectory</key><string>$RUNNER_HOME</string>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>$HOME/.cargo/bin:$BIN_DIR:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>
        <key>HOME</key><string>$HOME</string>
        $DEVELOPER_DIR_XML
    </dict>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><true/>
    <key>StandardOutPath</key><string>$RUNNER_HOME/runner.log</string>
    <key>StandardErrorPath</key><string>$RUNNER_HOME/runner.log</string>
</dict>
</plist>
EOF

UID_NUM="$(id -u)"
launchctl bootout "gui/$UID_NUM/io.gitea.act_runner" 2>/dev/null || true
if launchctl bootstrap "gui/$UID_NUM" "$PLIST" 2>/dev/null; then
    echo "==> runner LaunchAgent bootstrapped (gui/$UID_NUM)"
else
    # No GUI session (pure-SSH box, nobody logged in): land it in the user domain for now.
    # For boot persistence without a GUI login, either enable auto-login for this user or
    # promote the plist to a root-owned LaunchDaemon in /Library/LaunchDaemons (sudo).
    launchctl bootout "user/$UID_NUM/io.gitea.act_runner" 2>/dev/null || true
    launchctl bootstrap "user/$UID_NUM" "$PLIST"
    echo "==> runner LaunchAgent bootstrapped (user/$UID_NUM — no GUI session)"
    echo "    NOTE: won't auto-start after reboot until auto-login is enabled or the"
    echo "    plist is promoted to a LaunchDaemon."
fi

sleep 2
tail -5 "$RUNNER_HOME/runner.log" 2>/dev/null || true

if ! /usr/bin/xcodebuild -version >/dev/null 2>&1 && [ -z "$DEVELOPER_DIR_XML" ]; then
    echo "WARNING: xcodebuild not usable (Command Line Tools only, no full Xcode found) —"
    echo "         apple.yml's xcframework step needs a full Xcode in /Applications, with"
    echo "         its license accepted once: sudo xcodebuild -license accept"
fi
echo "OK: runner '$RUNNER_NAME' labels=$LABELS instance=$INSTANCE"
