# Installs tui-do-core on this machine as a background service, from a
# GitHub release. Run over ssh by a home core (see src/install/mod.rs):
#
#   ssh <host> bash -s -- <version> <repo> <port> < remote.sh
#
# Prints one JSON object per line: {"step": "..."} while working, then
# {"address": ..., "token": ..., ...} or {"error": ...}. Messages never
# contain double quotes.
set -eu

version="$1"
repo="$2"
port="$3"

say() { printf '{"step":"%s"}\n' "$1"; }
fail() { printf '{"error":"%s"}\n' "$1"; exit 1; }

say "Checking this machine"
os=$(uname -s)
arch=$(uname -m)
case "$os-$arch" in
  Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-gnu ;;
  Darwin-arm64) target=aarch64-apple-darwin ;;
  *) fail "tui-do-core has no build for $os $arch" ;;
esac
command -v curl >/dev/null 2>&1 || fail "curl isn't installed"
command -v tar >/dev/null 2>&1 || fail "tar isn't installed"

tailscale_ip=""
for cli in tailscale /usr/local/bin/tailscale /opt/homebrew/bin/tailscale /Applications/Tailscale.app/Contents/MacOS/Tailscale; do
  if ip=$("$cli" ip -4 2>/dev/null | head -n 1) && [ -n "$ip" ]; then
    tailscale_ip="$ip"
    break
  fi
done
[ -n "$tailscale_ip" ] || fail "Tailscale isn't running on this machine"

say "Downloading tui-do-core $version for $target"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
base="https://github.com/$repo/releases/download/v$version"
archive="tui-do-core-$target.tar.gz"
curl -fsSL "$base/$archive" -o "$tmp/$archive" || fail "couldn't download $archive from $repo v$version"
curl -fsSL "$base/$archive.sha256" -o "$tmp/$archive.sha256" || fail "couldn't download the checksum"
expected=$(cut -d ' ' -f 1 "$tmp/$archive.sha256")
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$tmp/$archive" | cut -d ' ' -f 1)
else
  actual=$(shasum -a 256 "$tmp/$archive" | cut -d ' ' -f 1)
fi
[ "$expected" = "$actual" ] || fail "the download didn't match its checksum"

say "Installing to ~/.tui-do"
tar -xzf "$tmp/$archive" -C "$tmp"
mkdir -p "$HOME/.tui-do/bin" "$HOME/.tui-do/core"
chmod 700 "$HOME/.tui-do/core"
# Every version runs from this one path, which macOS keeps privacy grants by
install -m 755 "$tmp/tui-do-core-$target/tui-do-core" "$HOME/.tui-do/bin/tui-do-core.new"
mv -f "$HOME/.tui-do/bin/tui-do-core.new" "$HOME/.tui-do/bin/tui-do-core"

say "Starting the service"
shell="${SHELL:-/bin/sh}"
linger="n/a"
if [ "$os" = "Linux" ]; then
  command -v systemctl >/dev/null 2>&1 || fail "systemd isn't available"
  unit_dir="$HOME/.config/systemd/user"
  mkdir -p "$unit_dir"
  # Through a login shell, so sessions get the user's PATH. KillMode=process
  # leaves the holders running sessions alone when the service stops.
  cat > "$unit_dir/tui-do-core.service" <<EOF
[Unit]
Description=TUI-DO core
After=network-online.target

[Service]
KillMode=process
ExecStart=$shell -lc 'exec "\$HOME/.tui-do/bin/tui-do-core"'
Restart=always
RestartSec=2

[Install]
WantedBy=default.target
EOF
  systemctl --user daemon-reload || fail "systemctl --user isn't usable"
  systemctl --user enable tui-do-core >/dev/null 2>&1 || true
  systemctl --user restart tui-do-core || fail "the service didn't start"
  linger=$(loginctl show-user "$(id -un)" -p Linger --value 2>/dev/null || echo unknown)
else
  label=dev.tui-do.core
  plist="$HOME/Library/LaunchAgents/$label.plist"
  mkdir -p "$HOME/Library/LaunchAgents"
  cat > "$plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key>
  <array><string>$shell</string><string>-l</string><string>-c</string><string>exec "\$0"</string><string>$HOME/.tui-do/bin/tui-do-core</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>$HOME/.tui-do/core/core.log</string>
  <key>StandardOutPath</key><string>$HOME/.tui-do/core/core.log</string>
</dict>
</plist>
EOF
  domain="gui/$(id -u)"
  if launchctl print "$domain/$label" >/dev/null 2>&1; then
    launchctl bootout "$domain/$label" || true
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      launchctl print "$domain/$label" >/dev/null 2>&1 || break
      sleep 0.5
    done
  fi
  launchctl bootstrap "$domain" "$plist" || fail "launchd didn't start the service"
fi

say "Waiting for the core"
for _ in $(seq 1 40); do
  curl -fsS "http://127.0.0.1:$port/v0/health" >/dev/null 2>&1 && break
  sleep 0.5
done
curl -fsS "http://127.0.0.1:$port/v0/health" >/dev/null 2>&1 || fail "the core didn't come up"
token=$(cat "$HOME/.tui-do/core/token")

say "Turning on remote access"
curl -fsS -X PATCH -H "Authorization: Bearer $token" -H "Content-Type: application/json" \
  -d '{"remoteAccess":true}' "http://127.0.0.1:$port/v0/device" >/dev/null || fail "couldn't turn on remote access"

printf '{"address":"http://%s:%s","token":"%s","target":"%s","linger":"%s"}\n' \
  "$tailscale_ip" "$port" "$token" "$target" "$linger"
