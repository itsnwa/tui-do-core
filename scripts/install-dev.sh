#!/bin/sh
# Build the core in release mode, install it to ~/.tui-do/bin and (re)start it
# as a launchd agent, so it runs in the background and starts at login.
#
# Restarting the agent ends any running sessions.
#
# Uninstall:
#   launchctl bootout "gui/$(id -u)/dev.tui-do.core"
#   rm ~/Library/LaunchAgents/dev.tui-do.core.plist ~/.tui-do/bin/tui-do-core
set -eu

cd "$(dirname "$0")/.."
cargo build --release

bin_dir="$HOME/.tui-do/bin"
core_dir="$HOME/.tui-do/core"
mkdir -p "$bin_dir" "$core_dir"
install -m 755 target/release/tui-do-core "$bin_dir/tui-do-core"

label="dev.tui-do.core"
plist="$HOME/Library/LaunchAgents/$label.plist"
user_shell="${SHELL:-/bin/zsh}"

# Run through a login shell so the core (and sessions started without an
# explicit env) get the user's PATH instead of launchd's minimal one.
cat > "$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key>
  <array>
    <string>$user_shell</string>
    <string>-l</string>
    <string>-c</string>
    <string>exec "\$0"</string>
    <string>$bin_dir/tui-do-core</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardErrorPath</key><string>$core_dir/core.log</string>
  <key>StandardOutPath</key><string>$core_dir/core.log</string>
</dict>
</plist>
EOF

domain="gui/$(id -u)"
if launchctl print "$domain/$label" >/dev/null 2>&1; then
  launchctl bootout "$domain/$label" || true
  # bootstrap fails while the old instance is still shutting down
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    launchctl print "$domain/$label" >/dev/null 2>&1 || break
    sleep 0.5
  done
fi
launchctl bootstrap "$domain" "$plist"
echo "tui-do-core installed and running ($label). Log: $core_dir/core.log"
