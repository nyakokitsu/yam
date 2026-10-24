#!/bin/sh
set -eu

ACTION=install
case "${1:-}" in
  --uninstall|uninstall) ACTION=uninstall; shift ;;
esac
VERSION=${1:-${YAM_VERSION:-latest}}
BIN_DIR=${2:-${YAM_BIN_DIR:-$HOME/.local/bin}}
REPOSITORY=https://github.com/nyakokitsu/yam
DEVICE_PREFIX=/data/quazar/yam

if [ "$ACTION" = install ]; then
  case "$(uname -s):$(uname -m)" in
    Linux:x86_64|Linux:amd64) CLI_ASSET=yam-x86_64-unknown-linux-musl ;;
    Darwin:arm64|Darwin:aarch64) CLI_ASSET=yam-aarch64-apple-darwin ;;
    *) echo "Unsupported host platform $(uname -s) $(uname -m)." >&2; exit 3 ;;
  esac
fi

case "$VERSION" in
  latest) RELEASE_URL="$REPOSITORY/releases/latest/download" ;;
  v*) RELEASE_URL="$REPOSITORY/releases/download/$VERSION" ;;
  *) RELEASE_URL="$REPOSITORY/releases/download/v$VERSION" ;;
esac

tmpdir=$(mktemp -d "${TMPDIR:-/tmp}/yam.XXXXXX")
trap 'rm -rf "$tmpdir"' EXIT HUP INT TERM
download() {
  url=$1 output=$2
  if command -v curl >/dev/null 2>&1; then
    curl -fL --proto '=https,http' --tlsv1.2 "$url" -o "$output"
  elif command -v wget >/dev/null 2>&1; then
    wget -O "$output" "$url"
  else
    echo 'curl or wget is required.' >&2; exit 4
  fi
}
verify() {
  file=$1
  expected=$(awk -v name="$(basename "$file")" '$2 == name {print $1}' "$tmpdir/SHA256SUMS")
  [ -n "$expected" ] || { echo "No checksum for $(basename "$file")" >&2; exit 5; }
  if command -v sha256sum >/dev/null 2>&1; then
    actual=$(sha256sum "$file" | awk '{print $1}')
  elif command -v openssl >/dev/null 2>&1; then
    actual=$(openssl dgst -sha256 "$file" | awk '{print $NF}')
  else
    echo 'sha256sum or openssl is required.' >&2; exit 5
  fi
  [ "$actual" = "$expected" ] || { echo "Checksum mismatch for $(basename "$file")" >&2; exit 5; }
}

command -v adb >/dev/null 2>&1 || {
  echo 'adb is required to deploy yamd. Install Android Platform Tools first.' >&2; exit 4;
}

find_device() {
  adb devices </dev/null | awk 'NR > 1 && $2 == "device" {print $1}' | while IFS= read -r serial; do
    arch=$(adb -s "$serial" shell uname -m </dev/null 2>/dev/null | tr -d '\r') || continue
    case "$arch" in aarch64|arm64) printf '%s\n' "$serial"; return 0;; esac
  done
  return 1
}

DEVICE=$(find_device || true)
if [ -z "$DEVICE" ]; then
  printf 'Yandex Station IP address: ' >&2
  IFS= read -r ip </dev/tty
  [ -n "$ip" ] || { echo 'IP address is required.' >&2; exit 6; }
  DEVICE="$ip:5555"
  adb connect "$DEVICE" </dev/null >&2
  sleep 1
  adb -s "$DEVICE" get-state </dev/null >/dev/null
fi

case "$DEVICE" in
  *:*) STATION_IP=${DEVICE%:*} ;;
  *)
    STATION_IP=$(adb -s "$DEVICE" shell 'ip route get 1.1.1.1 2>/dev/null' </dev/null 2>/dev/null | awk '{for (i = 1; i <= NF; i++) if ($i == "src") print $(i + 1)}' | tr -d '\r' | head -n 1)
    if [ -z "$STATION_IP" ]; then
      printf 'Yandex Station IP address: ' >&2
      IFS= read -r STATION_IP </dev/tty
    fi
    ;;
esac
[ -n "$STATION_IP" ] || { echo 'IP address is required.' >&2; exit 6; }

if [ "$ACTION" = uninstall ]; then
  rm -f "$BIN_DIR/yam" "$BIN_DIR/yam-bin"
  adb -s "$DEVICE" shell "if [ -f $DEVICE_PREFIX/yamd.pid ] && kill -0 \$(cat $DEVICE_PREFIX/yamd.pid) 2>/dev/null; then kill \$(cat $DEVICE_PREFIX/yamd.pid); fi; rm -rf $DEVICE_PREFIX" </dev/null
  echo "YAM removed from $BIN_DIR and $DEVICE ($DEVICE_PREFIX)."
  exit 0
fi

download "$RELEASE_URL/SHA256SUMS" "$tmpdir/SHA256SUMS"
download "$RELEASE_URL/$CLI_ASSET" "$tmpdir/$CLI_ASSET"
download "$RELEASE_URL/yamd-aarch64-linux-musl" "$tmpdir/yamd-aarch64-linux-musl"
verify "$tmpdir/$CLI_ASSET"
verify "$tmpdir/yamd-aarch64-linux-musl"

mkdir -p "$BIN_DIR"
install -m 0755 "$tmpdir/$CLI_ASSET" "$BIN_DIR/yam-bin"
cat >"$BIN_DIR/yam" <<EOF
#!/bin/sh
exec env YAM_URL="\${YAM_URL:-http://$STATION_IP:8099}" "$BIN_DIR/yam-bin" "\$@"
EOF
chmod 0755 "$BIN_DIR/yam"
adb -s "$DEVICE" shell "mkdir -p $DEVICE_PREFIX/bin $DEVICE_PREFIX/data" </dev/null
adb -s "$DEVICE" push "$tmpdir/yamd-aarch64-linux-musl" "$DEVICE_PREFIX/bin/yamd.new" </dev/null >/dev/null
adb -s "$DEVICE" shell "chmod 0755 $DEVICE_PREFIX/bin/yamd.new; if [ -f $DEVICE_PREFIX/yamd.pid ] && kill -0 \$(cat $DEVICE_PREFIX/yamd.pid) 2>/dev/null; then kill \$(cat $DEVICE_PREFIX/yamd.pid); fi; mv $DEVICE_PREFIX/bin/yamd.new $DEVICE_PREFIX/bin/yamd; start-stop-daemon --start --background --make-pidfile --pidfile $DEVICE_PREFIX/yamd.pid --startas $DEVICE_PREFIX/bin/yamd -- --data-dir $DEVICE_PREFIX/data" </dev/null

echo "YAM $VERSION CLI installed in $BIN_DIR/yam"
echo "yamd deployed to $DEVICE ($DEVICE_PREFIX). Open http://$STATION_IP:8099"
