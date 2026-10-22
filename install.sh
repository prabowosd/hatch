#!/usr/bin/env bash
# hatch installer: Apache .htaccess compatibility module for OpenLiteSpeed.
#
#   curl -fsSL https://raw.githubusercontent.com/prabowosd/hatch/main/install.sh | sudo bash
#   sudo bash install.sh --file ./hatch.so     install a local build
#   sudo bash install.sh --version v0.1.0      pin a release
#   sudo bash install.sh --no-guard            skip the Webuzo config guard
#   sudo bash install.sh --no-watch            skip the Webuzo .htaccess watcher
#   sudo bash install.sh --status | --uninstall
#
# Every change is backed up first; if OpenLiteSpeed does not come back with the
# module loaded, the previous module and config are restored automatically.
set -euo pipefail

REPO="prabowosd/hatch"
LSWS="${LSWS_HOME:-/usr/local/lsws}"
CONF="$LSWS/conf/httpd_config.conf"
MODULE="$LSWS/modules/hatch.so"
GUARD="/usr/local/bin/hatch-guard"
GUARD_CRON="/etc/cron.d/hatch-guard"
WATCH="/usr/local/bin/hatch-reload"
WATCH_UNIT="/etc/systemd/system/hatch-reload.service"
WEBUZO_HOME="${WEBUZO_HOME:-/usr/local/webuzo}"
BACKUP_DIR="/var/lib/hatch/install-backups/$(date +%Y%m%d-%H%M%S)"
VERSION="latest"
FILE=""
ACTION="install"
NO_GUARD=0
NO_WATCH=0

say() { printf '\033[1m[hatch]\033[0m %s\n' "$*"; }
die() { printf '\033[31m[hatch] %s\033[0m\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --file) FILE="${2:?--file needs a path}"; shift 2 ;;
    --version) VERSION="${2:?--version needs a tag}"; shift 2 ;;
    --no-guard) NO_GUARD=1; shift ;;
    --no-watch) NO_WATCH=1; shift ;;
    --uninstall) ACTION="uninstall"; shift ;;
    --status) ACTION="status"; shift ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done

[ "$(id -u)" = 0 ] || die "run as root (sudo)"
[ -x "$LSWS/bin/lswsctrl" ] || die "OpenLiteSpeed not found in $LSWS (set LSWS_HOME)"
[ -f "$CONF" ] || die "missing $CONF"

has_block() { grep -qE '^module[[:space:]]+hatch[[:space:]]*\{' "$CONF"; }

is_webuzo() { [ -d "$LSWS/conf/webuzo_vh" ] || [ -f "$WEBUZO_HOME/globals.php" ]; }

install_guard() {
  # Webuzo writes httpd_config.conf from its own state, so a block added by hand
  # disappears on its next rewrite. This cron job puts the block back and restarts.
  cat > "$GUARD" <<'GUARD_SCRIPT'
#!/bin/sh
# hatch-guard: restore hatch's module block when Webuzo rewrites httpd_config.conf.
set -eu
# HATCH_GUARD_BEGIN
LSWS="${LSWS_HOME:-/usr/local/lsws}"
CONF="$LSWS/conf/httpd_config.conf"
MODULE="$LSWS/modules/hatch.so"
[ -f "$CONF" ] && [ -f "$MODULE" ] || exit 0
has_block() { grep -qE '^module[[:space:]]+hatch[[:space:]]*\{' "$CONF"; }
has_block && exit 0
if command -v flock >/dev/null 2>&1; then
  LOCK_DIR=""
  if [ -d /run ]; then LOCK_DIR=/run; elif [ -d /var/lock ]; then LOCK_DIR=/var/lock; fi
  if [ -n "$LOCK_DIR" ]; then
    exec 9>>"$LOCK_DIR/hatch-guard.lock"
    flock -n 9 || exit 0
  fi
  has_block && exit 0
fi
cp -p "$CONF" "$CONF.hatch-guard" 2>/dev/null || true
printf '\nmodule hatch {\n  ls_enabled              1\n}\n' >> "$CONF"
logger -t hatch "restored the module block in $CONF (Webuzo rewrote it); restarting OpenLiteSpeed" 2>/dev/null || true
"$LSWS/bin/lswsctrl" restart >/dev/null 2>&1 || true
# HATCH_GUARD_END
GUARD_SCRIPT
  chmod 0755 "$GUARD"
  printf '# hatch-guard: restore the module block when Webuzo rewrites httpd_config.conf\n* * * * * root %s\n' "$GUARD" > "$GUARD_CRON"
  chmod 0644 "$GUARD_CRON"
  say "Webuzo detected: hatch-guard installed, checks the config every minute"
}

remove_guard() { rm -f "$GUARD_CRON" "$GUARD"; }

install_watch() {
  # OpenLiteSpeed parses .htaccess at config load, so a rewrite edit waits for a
  # reload. This service watches the document roots and reloads within seconds.
  if ! command -v inotifywait >/dev/null 2>&1; then
    say "inotify-tools is missing; skipping the .htaccess watcher (install inotify-tools, re-run)"
    return 0
  fi
  cat > "$WATCH" <<'WATCH_SCRIPT'
#!/bin/bash
# hatch-reload: reload OpenLiteSpeed when a .htaccess file changes.
set -u
# HATCH_WATCH_BEGIN
LSWS="${LSWS_HOME:-/usr/local/lsws}"
TTL="${HATCH_RELOAD_TTL:-300}"
DEBOUNCE="${HATCH_RELOAD_DEBOUNCE:-2}"
MIN_INTERVAL="${HATCH_RELOAD_MIN_INTERVAL:-10}"
LAST=0

log() { logger -t hatch-reload -- "$*" 2>/dev/null || printf '[hatch-reload] %s\n' "$*" >&2; }

docroots() {
  grep -h '^[[:space:]]*docRoot' "$LSWS"/conf/webuzo_vh/*/vhost.conf 2>/dev/null | awk '{print $2}'
  ls -d /home/*/public_html 2>/dev/null
}

reload() {
  local wait
  wait=$(( LAST + MIN_INTERVAL - $(date +%s) ))
  [ "$wait" -gt 0 ] && sleep "$wait"
  LAST=$(date +%s)
  if "$LSWS/bin/lswsctrl" reload >/dev/null 2>&1; then
    log "reloaded OpenLiteSpeed after a .htaccess change"
  else
    log "reload failed; the panel's own cron will retry"
  fi
}

while :; do
  mapfile -t ROOTS < <(docroots | sed 's:/*$::' | sort -u | while read -r d; do [ -d "$d" ] && printf '%s\n' "$d"; done)
  if [ "${#ROOTS[@]}" -eq 0 ]; then
    log "no document roots found; retrying in 30s"
    sleep 30
    continue
  fi

  DIR=$(mktemp -d "${TMPDIR:-/tmp}/hatch-reload.XXXXXX")
  FIFO="$DIR/events"
  mkfifo "$FIFO"
  inotifywait -m -r -q -e close_write,create,moved_to,delete --format '%w%f' "${ROOTS[@]}" >"$FIFO" 2>/dev/null &
  WATCHER=$!
  exec 3<"$FIFO"

  START=$(date +%s)
  SAW=0
  while :; do
    IFS= read -r -t "$DEBOUNCE" -u 3 CHANGED
    RC=$?
    if [ "$RC" -eq 0 ]; then
      case "$CHANGED" in */.htaccess) SAW=1 ;; esac
      continue
    fi
    if [ "$RC" -le 128 ]; then
      log "inotifywait stopped; restarting"
      break
    fi
    if [ "$SAW" -eq 1 ]; then
      SAW=0
      reload
    fi
    [ $(( $(date +%s) - START )) -ge "$TTL" ] && break
  done

  exec 3<&-
  kill "$WATCHER" 2>/dev/null
  wait "$WATCHER" 2>/dev/null
  rm -rf "$DIR"
done
# HATCH_WATCH_END
WATCH_SCRIPT
  chmod 0755 "$WATCH"
  cat > "$WATCH_UNIT" <<'WATCH_UNIT_BODY'
[Unit]
Description=hatch: reload OpenLiteSpeed when a .htaccess changes
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/hatch-reload
Restart=always
RestartSec=5
Nice=10

[Install]
WantedBy=multi-user.target
WATCH_UNIT_BODY
  chmod 0644 "$WATCH_UNIT"
  systemctl daemon-reload >/dev/null 2>&1 || true
  if systemctl enable --now hatch-reload.service >/dev/null 2>&1; then
    say "Webuzo detected: hatch-reload installed, rewrite edits apply within seconds"
  else
    say "warning: hatch-reload could not be started; check systemctl status hatch-reload"
  fi
}

remove_watch() {
  systemctl disable --now hatch-reload.service >/dev/null 2>&1 || true
  rm -f "$WATCH_UNIT" "$WATCH"
  systemctl daemon-reload >/dev/null 2>&1 || true
}

loaded_since() {
  # True when the error log shows hatch initialised after the given timestamp.
  grep -a "Module: hatch" "$LSWS/logs/error.log" 2>/dev/null | awk -v t="$1" 'substr($0,1,19) >= t' | grep -q "initialized succe"
}

restart() {
  "$LSWS/bin/lswsctrl" restart >/dev/null 2>&1 || true
  for _ in 1 2 3 4 5 6 7 8 9 10; do
    sleep 1
    "$LSWS/bin/lswsctrl" status 2>&1 | grep -qi running && return 0
  done
  return 1
}

do_status() {
  if [ -f "$MODULE" ]; then say "module: $MODULE ($(sha256sum "$MODULE" | cut -c1-12))"; else say "module: not installed"; fi
  if has_block; then say "config: module hatch block present in $CONF"; else say "config: no module hatch block"; fi
  grep -a "\[hatch\] module v" "$LSWS/logs/error.log" 2>/dev/null | tail -1 | sed 's/^/[hatch] last load: /' || true
  if grep -qE '^module[[:space:]]+cyberpanel_ols' "$CONF"; then
    say "note: CyberPanel's OpenLiteSpeed core detected; hatch reads its LSIAPI layout"
  fi
  if is_webuzo; then
    if [ -x "$GUARD" ] && [ -f "$GUARD_CRON" ]; then
      say "Webuzo: detected; hatch-guard restores the block when Webuzo rewrites the config"
    else
      say "Webuzo: detected; no guard installed (run the installer to add it)"
    fi
    if [ -x "$WATCH" ] && systemctl is-active --quiet hatch-reload.service 2>/dev/null; then
      say "Webuzo: hatch-reload is running, .htaccess rewrite edits apply within seconds"
    else
      say "Webuzo: no .htaccess watcher running (run the installer to add it)"
    fi
  fi
}

do_uninstall() {
  mkdir -p "$BACKUP_DIR"
  cp -p "$CONF" "$BACKUP_DIR/httpd_config.conf"
  if has_block; then
    awk 'BEGIN{skip=0} /^module[[:space:]]+hatch[[:space:]]*\{/{skip=1} skip&&/^\}/{skip=0;next} !skip' "$BACKUP_DIR/httpd_config.conf" > "$CONF"
  fi
  [ -f "$MODULE" ] && mv "$MODULE" "$BACKUP_DIR/hatch.so"
  remove_guard
  remove_watch
  restart || die "OpenLiteSpeed did not restart; config backup in $BACKUP_DIR"
  say "uninstalled; backup in $BACKUP_DIR"
}

fetch() {
  local arch tmp url base
  case "$(uname -m)" in
    x86_64|amd64) arch="x86_64" ;;
    aarch64|arm64) arch="aarch64" ;;
    *) die "unsupported architecture $(uname -m)" ;;
  esac
  local glibc; glibc="$(ldd --version 2>/dev/null | head -1 | grep -oE '[0-9]+\.[0-9]+$' || echo 0)"
  awk -v v="$glibc" 'BEGIN{split(v,a,"."); exit !(a[1]>2 || (a[1]==2 && a[2]>=34))}' \
    || die "glibc $glibc is too old; hatch release builds need 2.34+ (RHEL/Alma/Rocky 9, Ubuntu 22.04+)"
  tmp="$(mktemp -d)"
  if [ "$VERSION" = "latest" ]; then base="https://github.com/$REPO/releases/latest/download"; else base="https://github.com/$REPO/releases/download/$VERSION"; fi
  url="$base/hatch-$arch-linux-gnu.so"
  say "downloading $url"
  curl -fsSL "$url" -o "$tmp/hatch.so" || die "download failed"
  curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" || die "checksum file missing"
  (cd "$tmp" && grep " hatch-$arch-linux-gnu.so\$" SHA256SUMS | sed "s/hatch-$arch-linux-gnu.so/hatch.so/" | sha256sum -c --quiet -) \
    || die "checksum mismatch, refusing to install"
  FILE="$tmp/hatch.so"
}

do_install() {
  [ -n "$FILE" ] || fetch
  [ -f "$FILE" ] || die "no module file at $FILE"
  head -c 4 "$FILE" | grep -q "ELF" || die "$FILE is not an ELF shared library"

  mkdir -p "$BACKUP_DIR"
  cp -p "$CONF" "$BACKUP_DIR/httpd_config.conf"
  [ -f "$MODULE" ] && cp -p "$MODULE" "$BACKUP_DIR/hatch.so"
  say "backup in $BACKUP_DIR"

  install -m 0755 "$FILE" "$MODULE"
  if ! has_block; then
    printf '\nmodule hatch {\n  ls_enabled              1\n}\n' >> "$CONF"
  fi

  local started; started="$(date '+%Y-%m-%d %H:%M:%S')"
  if restart && loaded_since "$started"; then
    local line; line="$(grep -a '\[hatch\] module v' "$LSWS/logs/error.log" | tail -1)"
    if printf '%s' "$line" | grep -q idle; then
      say "hatch cannot drive this OpenLiteSpeed build: $(printf '%s' "$line" | sed 's/.*idle: //')"
      say "rolling back; nothing is left half-applied"
    else
      say "installed: $(printf '%s' "$line" | grep -oE 'module v[0-9.]+.*')"
      say ".htaccess Header, Redirect, Require/Allow/Deny, Files and Expires directives are now applied"
      if is_webuzo; then
        if [ "$NO_GUARD" = 1 ]; then say "Webuzo detected; guard skipped (--no-guard)"; else install_guard; fi
        if [ "$NO_WATCH" = 1 ]; then say "Webuzo detected; watcher skipped (--no-watch)"; else install_watch; fi
      fi
      return 0
    fi
  else
    say "hatch did not load, rolling back"
  fi
  cp -p "$BACKUP_DIR/httpd_config.conf" "$CONF"
  if [ -f "$BACKUP_DIR/hatch.so" ]; then cp -p "$BACKUP_DIR/hatch.so" "$MODULE"; else rm -f "$MODULE"; fi
  restart || true
  grep -a -iE "hatch|module" "$LSWS/logs/error.log" | tail -5 >&2 || true
  die "rolled back; OpenLiteSpeed runs with the previous config"
}

case "$ACTION" in
  status) do_status ;;
  uninstall) do_uninstall ;;
  install) do_install ;;
esac
