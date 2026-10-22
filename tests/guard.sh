#!/usr/bin/env bash
# Exercises hatch-guard, the script the installer leaves on Webuzo hosts.
#   tests/guard.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
FAILED=0

# The guard ships inside install.sh; lift it out so it can run against a fake LSWS.
sed -n '/^# HATCH_GUARD_BEGIN$/,/^# HATCH_GUARD_END$/p' "$ROOT/install.sh" | sed '1d;$d' > "$TMP/body"
if [ ! -s "$TMP/body" ]; then
  printf 'FAIL  no guard body found in install.sh\n'
  exit 1
fi
{ echo '#!/bin/sh'; cat "$TMP/body"; } > "$TMP/hatch-guard"
chmod 0755 "$TMP/hatch-guard"

ok() { printf '  ok    %s\n' "$1"; }
bad() { printf '  FAIL  %s\n' "$1"; FAILED=1; }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }

new_lsws() {
  rm -rf "$TMP/lsws"
  mkdir -p "$TMP/lsws/bin" "$TMP/lsws/conf" "$TMP/lsws/modules" "$TMP/lsws/logs"
  : > "$TMP/lsws/modules/hatch.so"
  : > "$TMP/lsws/logs/error.log"
  : > "$TMP/lsws/restarts"
  cat > "$TMP/lsws/bin/lswsctrl" <<'EOF'
#!/bin/sh
echo "$1" >> "$(dirname "$0")/../restarts"
EOF
  chmod 0755 "$TMP/lsws/bin/lswsctrl"
  printf 'user nobody\ngroup nobody\n' > "$TMP/lsws/conf/httpd_config.conf"
}
run_guard() { LSWS_HOME="$TMP/lsws" "$TMP/hatch-guard"; }

echo "hatch-guard"

new_lsws
run_guard
check "restores the module block" "grep -qE '^module hatch \{' '$TMP/lsws/conf/httpd_config.conf'"
check "enables the module" "grep -q 'ls_enabled' '$TMP/lsws/conf/httpd_config.conf'"
check "restarts OpenLiteSpeed" "[ \"\$(cat '$TMP/lsws/restarts')\" = restart ]"
check "keeps a copy of the rewritten config" "[ -f '$TMP/lsws/conf/httpd_config.conf.hatch-guard' ]"

cp "$TMP/lsws/conf/httpd_config.conf" "$TMP/intact"
: > "$TMP/lsws/restarts"
run_guard
check "leaves a config that already has the block alone" "cmp -s '$TMP/intact' '$TMP/lsws/conf/httpd_config.conf'"
check "does not restart without a rewrite" "[ ! -s '$TMP/lsws/restarts' ]"

new_lsws
printf '\nmodule hatch {\n  ls_enabled              1\n}\n' >> "$TMP/lsws/conf/httpd_config.conf"
run_guard
check "is a no-op when the block survived" "[ \"\$(grep -c '^module hatch' '$TMP/lsws/conf/httpd_config.conf')\" = 1 ]"

new_lsws
rm -f "$TMP/lsws/modules/hatch.so"
run_guard
check "does nothing without hatch.so" "! grep -qE '^module hatch' '$TMP/lsws/conf/httpd_config.conf'"
check "does not restart without hatch.so" "[ ! -s '$TMP/lsws/restarts' ]"

exit "$FAILED"
