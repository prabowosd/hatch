#!/usr/bin/env bash
# End to end test: load hatch.so into a real OpenLiteSpeed container and probe it.
#   tests/integration.sh [path/to/libhatch.so]
set -euo pipefail

SO="${1:-target/release/libhatch.so}"
IMAGE="${OLS_IMAGE:-docker.io/litespeedtech/openlitespeed:1.8.3-lsphp83}"
NAME="hatch-it-$$"
PORT="${PORT:-18090}"
ENGINE="$(command -v podman || command -v docker)"
B="http://127.0.0.1:$PORT"
FAILED=0

cleanup() { "$ENGINE" rm -f "$NAME" >/dev/null 2>&1 || true; }
trap cleanup EXIT

"$ENGINE" run -d --name "$NAME" -p "$PORT:8088" "$IMAGE" >/dev/null
sleep 5
"$ENGINE" cp "$SO" "$NAME:/usr/local/lsws/modules/hatch.so"
"$ENGINE" cp tests/site "$NAME:/tmp/site"
"$ENGINE" exec "$NAME" bash -c '
  set -e
  H=/usr/local/lsws/Example/html
  cp -a /tmp/site/. $H/
  chmod -R a+rX $H
  # A CyberPanel style vhost: .htaccess rewrites on, no contexts.
  cat > /usr/local/lsws/conf/vhosts/Example/vhconf.conf <<EOF
docRoot                   \$VH_ROOT/html/
index  {
  useServer               0
  indexFiles              index.html
}
rewrite  {
  enable                  1
  autoLoadHtaccess        1
}
EOF
  printf "\nmodule hatch {\n  ls_enabled 1\n}\n" >> /usr/local/lsws/conf/httpd_config.conf
  /usr/local/lsws/bin/lswsctrl restart >/dev/null 2>&1
  sleep 4
  grep -q "Module: hatch.*initialized succe" /usr/local/lsws/logs/error.log
'

check() {
  local label="$1" expected="$2" actual="$3"
  if [[ "$actual" == *"$expected"* ]]; then
    printf '  ok    %s\n' "$label"
  else
    printf '  FAIL  %s\n        expected: %s\n        actual:   %s\n' "$label" "$expected" "$actual"
    FAILED=1
  fi
}
headers() { curl -s -D - -o /dev/null "$B$1" | tr -d '\r' | tr 'A-Z' 'a-z'; }
code() { curl -s -o /dev/null -w '%{http_code}' "$B$1"; }
location() { curl -s -o /dev/null -w '%{http_code} %{redirect_url}' "$B$1"; }

echo "hatch integration ($IMAGE)"
check "Header set at the docroot"        'x-frame-options: sameorigin'      "$(headers /)"
check "Header unset"                     'x-hatch: root'                    "$(headers /)"
check "child folder adds headers"        'x-hatch-hx: yes'                  "$(headers /hx/)"
check "FilesMatch replaces Cache-Control" 'cache-control: max-age=99'       "$(headers /hx/a.css)"
check "Expires by type"                  'cache-control: max-age=86400'     "$(headers /hx/i.png)"
check "Files .env denied"                '403'                              "$(code /hx/.env)"
check "Require ip denies other clients"  '403'                              "$(code /hx/deep/)"
check "Redirect keeps the tail"          "301 $B/hx/x"                      "$(location /old/x)"
check "RedirectMatch substitutes"        "302 $B/news/2026/a"               "$(location /blog/2026/a)"
check "Redirect gone"                    '410'                              "$(code /dead)"
check "native RewriteRule still works"   'page'                             "$(curl -s "$B/rw")"
check "broken .htaccess is skipped"      '200'                              "$(code /broken/)"

"$ENGINE" exec "$NAME" bash -c 'sed -i "s/X-Hatch-Hx \"yes\"/X-Hatch-Hx \"changed\"/" /usr/local/lsws/Example/html/hx/.htaccess'
check "edits apply without a restart"    'x-hatch-hx: changed'              "$(headers /hx/)"

if "$ENGINE" exec "$NAME" bash -c 'ls /tmp/lshttpd/bak_core 2>/dev/null | grep -q .'; then
  echo "  FAIL  a core dump was written"; FAILED=1
fi
[ "$FAILED" = 0 ] && echo "all passed" || { echo "failures above"; exit 1; }
