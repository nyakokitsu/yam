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
INIT_SCRIPT=/etc/init.d/S99yam

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

choose_device() {
  devices=$(adb devices </dev/null | awk 'NR > 1 && $2 == "device" {print $1}' | while IFS= read -r serial; do
    arch=$(adb -s "$serial" shell uname -m </dev/null 2>/dev/null | tr -d '\r') || continue
    if [ "$arch" = aarch64 ] || [ "$arch" = arm64 ]; then
      model=$(adb -s "$serial" shell getprop ro.product.model </dev/null 2>/dev/null | tr -d '\r')
      printf '%s\t%s\n' "$serial" "${model:-Yandex Station}"
    fi
  done
  )
  [ -n "$devices" ] || return 1
  count=$(printf '%s\n' "$devices" | wc -l | tr -d ' ')
  if [ "$count" -eq 1 ]; then
    printf '%s\n' "$devices" | awk -F '\t' '{print $1}'
    return 0
  fi
  echo 'Select Yandex Station:' >&2
  printf '%s\n' "$devices" | awk -F '\t' '{printf "  %d) %s (%s)\n", NR, $2, $1}' >&2
  printf 'Number: ' >&2
  IFS= read -r selection </dev/tty
  case "$selection" in
    ''|*[!0-9]*) echo 'Invalid selection.' >&2; return 1 ;;
  esac
  selected=$(printf '%s\n' "$devices" | sed -n "${selection}p")
  [ -n "$selected" ] || { echo 'Invalid selection.' >&2; return 1; }
  printf '%s\n' "$selected" | awk -F '\t' '{print $1}'
}

DEVICE=$(choose_device || true)
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
  adb -s "$DEVICE" shell "set -eu
    mount -o remount,rw /
    trap 'mount -o remount,ro /' EXIT
    rm -f $INIT_SCRIPT
    if [ -f $DEVICE_PREFIX/yamd.pid ] && kill -0 \$(cat $DEVICE_PREFIX/yamd.pid) 2>/dev/null; then kill \$(cat $DEVICE_PREFIX/yamd.pid); fi
    rm -rf $DEVICE_PREFIX" </dev/null
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
adb -s "$DEVICE" shell "set -eu
  mount -o remount,rw /
  trap 'mount -o remount,ro /' EXIT
  cat >$INIT_SCRIPT <<'EOF'
#!/bin/sh

case \"\${1:-}\" in
  start)
    prefix=/data/quazar/yam
    pidfile=\"\$prefix/yamd.pid\"
    [ -x \"\$prefix/bin/yamd\" ] || exit 0
    mkdir -p \"\$prefix/data\"
    if [ -f \"\$pidfile\" ] && kill -0 \"\$(cat \"\$pidfile\")\" 2>/dev/null; then
      exit 0
    fi
    rm -f \"\$pidfile\"
    start-stop-daemon --start --background --make-pidfile --pidfile \"\$pidfile\" --startas \"\$prefix/bin/yamd\" -- --data-dir \"\$prefix/data\"
    ;;
  stop)
    prefix=/data/quazar/yam
    [ -f \"\$prefix/yamd.pid\" ] && kill \"\$(cat \"\$prefix/yamd.pid\")\" 2>/dev/null || true
    rm -f \"\$prefix/yamd.pid\"
    ;;
esac
EOF
  chmod 0755 $INIT_SCRIPT" </dev/null

echo "YAM $VERSION CLI installed in $BIN_DIR/yam"
echo "yamd deployed to $DEVICE ($DEVICE_PREFIX). Open http://$STATION_IP:8099"
echo "yamd is configured to start automatically after the speaker reboots."
