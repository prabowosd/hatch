#!/usr/bin/env bash
# hatch installer: Apache .htaccess compatibility module for OpenLiteSpeed.
#
#   curl -fsSL https://raw.githubusercontent.com/prabowosd/hatch/main/install.sh | sudo bash
#   sudo bash install.sh --file ./hatch.so     install a local build
#   sudo bash install.sh --version v0.1.0      pin a release
#   sudo bash install.sh --status | --uninstall
#
# Every change is backed up first; if OpenLiteSpeed does not come back with the
# module loaded, the previous module and config are restored automatically.
set -euo pipefail

REPO="prabowosd/hatch"
LSWS="${LSWS_HOME:-/usr/local/lsws}"
CONF="$LSWS/conf/httpd_config.conf"
MODULE="$LSWS/modules/hatch.so"
BACKUP_DIR="/var/lib/hatch/install-backups/$(date +%Y%m%d-%H%M%S)"
VERSION="latest"
FILE=""
ACTION="install"

say() { printf '\033[1m[hatch]\033[0m %s\n' "$*"; }
die() { printf '\033[31m[hatch] %s\033[0m\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
  case "$1" in
    --file) FILE="${2:?--file needs a path}"; shift 2 ;;
    --version) VERSION="${2:?--version needs a tag}"; shift 2 ;;
    --uninstall) ACTION="uninstall"; shift ;;
    --status) ACTION="status"; shift ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *) die "unknown option: $1" ;;
  esac
done

[ "$(id -u)" = 0 ] || die "run as root (sudo)"
[ -x "$LSWS/bin/lswsctrl" ] || die "OpenLiteSpeed not found in $LSWS (set LSWS_HOME)"
[ -f "$CONF" ] || die "missing $CONF"

has_block() { grep -qE '^module[[:space:]]+hatch[[:space:]]*\{' "$CONF"; }

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
    say "note: cyberpanel_ols is also configured; it stays inert without its paid license"
  fi
}

do_uninstall() {
  mkdir -p "$BACKUP_DIR"
  cp -p "$CONF" "$BACKUP_DIR/httpd_config.conf"
  if has_block; then
    awk 'BEGIN{skip=0} /^module[[:space:]]+hatch[[:space:]]*\{/{skip=1} skip&&/^\}/{skip=0;next} !skip' "$BACKUP_DIR/httpd_config.conf" > "$CONF"
  fi
  [ -f "$MODULE" ] && mv "$MODULE" "$BACKUP_DIR/hatch.so"
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
    say "installed: $(grep -a '\[hatch\] module v' "$LSWS/logs/error.log" | tail -1 | grep -oE 'module v[0-9.]+')"
    say ".htaccess Header, Redirect, Require/Allow/Deny, Files and Expires directives are now applied"
    return 0
  fi

  say "hatch did not load, rolling back"
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
