#!/usr/bin/env bash
# Exercises hatch-reload, the .htaccess watcher the installer leaves on Webuzo hosts.
#   tests/watch.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

if ! command -v inotifywait >/dev/null 2>&1; then
  echo "hatch-reload: skipped (inotifywait is not installed)"
  exit 0
fi

TMP="$(mktemp -d)"
PID=""
cleanup() {
  if [ -n "$PID" ]; then kill -- -"$PID" 2>/dev/null || true; fi
  rm -rf "$TMP"
}
trap cleanup EXIT

sed -n '/^# HATCH_WATCH_BEGIN$/,/^# HATCH_WATCH_END$/p' "$ROOT/install.sh" | sed '1d;$d' > "$TMP/body"
if [ ! -s "$TMP/body" ]; then
  printf 'FAIL  no watcher body found in install.sh\n'
  exit 1
fi
{ echo '#!/bin/bash'; cat "$TMP/body"; } > "$TMP/hatch-reload"
chmod 0755 "$TMP/hatch-reload"

mkdir -p "$TMP/lsws/bin" "$TMP/lsws/conf/webuzo_vh/site.test" "$TMP/site"
cat > "$TMP/lsws/bin/lswsctrl" <<'EOF'
#!/bin/sh
echo "$1" >> "$(dirname "$0")/../reloads"
EOF
chmod 0755 "$TMP/lsws/bin/lswsctrl"
printf 'docRoot                 %s\n' "$TMP/site" > "$TMP/lsws/conf/webuzo_vh/site.test/vhost.conf"
: > "$TMP/lsws/reloads"

LSWS_HOME="$TMP/lsws" HATCH_RELOAD_DEBOUNCE=1 HATCH_RELOAD_MIN_INTERVAL=0 HATCH_RELOAD_TTL=60 \
  setsid "$TMP/hatch-reload" >/dev/null 2>&1 &
PID=$!
sleep 2

FAILED=0
count() { wc -l < "$TMP/lsws/reloads" | tr -d ' '; }
ok() { printf '  ok    %s\n' "$1"; }
bad() { printf '  FAIL  %s\n' "$1"; FAILED=1; }
expect() {
  local i
  for i in $(seq 1 60); do
    [ "$(count)" -ge "$2" ] && break
    sleep 0.2
  done
  if [ "$(count)" -ge "$2" ]; then ok "$1"; else bad "$1 (got $(count), want $2)"; fi
}

echo "hatch-reload"

printf 'RewriteEngine On\n' > "$TMP/site/.htaccess"
expect "a new .htaccess reloads OpenLiteSpeed" 1

printf 'plain\n' > "$TMP/site/index.html"
sleep 3
if [ "$(count)" -eq 1 ]; then ok "a normal file write does not reload"; else bad "a normal file write does not reload"; fi

printf 'RewriteEngine On\nRewriteRule ^a$ /b [R=302,L]\n' >> "$TMP/site/.htaccess"
expect "editing .htaccess reloads again" 2

exit "$FAILED"
