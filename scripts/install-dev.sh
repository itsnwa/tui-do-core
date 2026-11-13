#!/bin/sh
# Build the core in release mode, install it to ~/.tui-do and (re)start it as
# a launchd agent, so it runs in the background and starts at login.
#
# Running sessions keep going: the new core picks them up from their holders.
#
# Uninstall (sessions outlive the agent, so end them too):
#   launchctl bootout "gui/$(id -u)/dev.tui-do.core"
#   pkill -f 'tui-do-core hold'
#   rm -r ~/Library/LaunchAgents/dev.tui-do.core.plist ~/.tui-do/bin
set -eu

cd "$(dirname "$0")/.."
cargo build --release

# Sign like the releases, so macOS keeps its privacy grants (Local Network,
# Files & Folders) across builds instead of asking again for each one
if security find-identity -v -p codesigning | grep -q "Developer ID Application"; then
  codesign --force --timestamp=none --identifier dev.tui-do.core \
    --sign "Developer ID Application" target/release/tui-do-core
else
  echo "No Developer ID Application identity; macOS will ask for permissions again" >&2
fi

bin_dir="$HOME/.tui-do/bin"
core_dir="$HOME/.tui-do/core"
mkdir -p "$bin_dir" "$core_dir"
# Renamed into place, so holders running the last build are left alone. Every
# build runs from this one path, which macOS keeps privacy grants by.
install -m 755 target/release/tui-do-core "$bin_dir/tui-do-core.new"
mv -f "$bin_dir/tui-do-core.new" "$bin_dir/tui-do-core"

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
