#!/usr/bin/env bash
# End to end test: load hatch.so into a real OpenLiteSpeed and probe it.
#   tests/integration.sh [path/to/libhatch.so]        official OpenLiteSpeed image
#   HATCH_CYBERPANEL=1 tests/integration.sh lib.so    CyberPanel's own OpenLiteSpeed core
#   HATCH_LOCAL=1 tests/integration.sh lib.so         the OpenLiteSpeed installed on this host
set -euo pipefail

SO="${1:-target/release/libhatch.so}"
IMAGE="${OLS_IMAGE:-docker.io/litespeedtech/openlitespeed:1.8.3-lsphp83}"
CP_IMAGE="${CP_IMAGE:-hatch-cyberpanel}"
NAME="hatch-it-$$"
PORT="${PORT:-18090}"
ENGINE="$(command -v podman || command -v docker || true)"
B="http://127.0.0.1:$PORT"
FAILED=0
LABEL="$IMAGE"

if [ "${HATCH_LOCAL:-0}" = 1 ]; then
  # Same commands, run on this host instead of inside a container.
  run() { [ "$1" = exec ] && shift 2 && "$@"; }
  ENGINE=run
  B="http://127.0.0.1:8088"
  LABEL="$(head -1 /etc/os-release | cut -d= -f2)"
  cp "$SO" /usr/local/lsws/modules/hatch.so
  rm -rf /tmp/site && cp -a tests/site /tmp/site
else
  cleanup() { "$ENGINE" rm -f "$NAME" >/dev/null 2>&1 || true; }
  trap cleanup EXIT
  if [ "${HATCH_CYBERPANEL:-0}" = 1 ]; then
    # build/CyberPanelfile swaps in CyberPanel's own core; --build rebuilds it first.
    LABEL="CyberPanel OpenLiteSpeed core"
    if [ "${HATCH_BUILD_CYBERPANEL:-0}" = 1 ]; then
      "$ENGINE" build --platform linux/amd64 -t "$CP_IMAGE" -f build/CyberPanelfile build >/dev/null
    fi
    "$ENGINE" run -d --name "$NAME" -p "$PORT:8088" --platform linux/amd64 "$CP_IMAGE" >/dev/null
    sleep 3
    "$ENGINE" exec "$NAME" bash -c '/usr/local/lsws/bin/openlitespeed -v | head -1 | grep -q "LiteSpeed/2.5"'
  else
    "$ENGINE" run -d --name "$NAME" -p "$PORT:8088" "$IMAGE" >/dev/null
    sleep 5
  fi
  "$ENGINE" cp "$SO" "$NAME:/usr/local/lsws/modules/hatch.so"
  "$ENGINE" cp tests/site "$NAME:/tmp/site"
fi
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
code_post() { curl -s -o /dev/null -w '%{http_code}' -X "$2" "$B$1"; }
location() { curl -s -o /dev/null -w '%{http_code} %{redirect_url}' "$B$1"; }
charset_header() { headers "$1" | grep -i '^content-type:' | grep -qi 'charset=' && echo has-charset || echo no-charset; }

echo "hatch integration ($LABEL)"
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

# SetEnvIf reads a request header, and Header env= follows what the same request set.
check "SetEnvIf matches a request header" 'x-hatch-env: on'                 "$(curl -s -D - -o /dev/null -H 'X-Hatch-Test: yes' "$B/env/" | tr -d '\r' | tr 'A-Z' 'a-z')"
check "SetEnvIfNoCase ignores case"      'x-hatch-ua: on'                    "$(curl -s -D - -o /dev/null -H 'User-Agent: hatchprobe/1.0' "$B/env/" | tr -d '\r' | tr 'A-Z' 'a-z')"
check "negated env= applies when unset"  'x-hatch-negated: on'              "$(headers /env/)"
check "env= drops when its variable is set" '' "$(curl -s -D - -o /dev/null -H 'X-Hatch-Test: yes' "$B/env/" | tr -d '\r' | tr 'A-Z' 'a-z' | grep -c 'x-hatch-negated' | sed 's/^0$//;s/^1$/unexpected header/')"

check "Options -Indexes denies a listing" '403'                            "$(code /noindex/)"
check "ErrorDocument serves the custom page" 'custom not found'            "$(curl -s "$B/errdoc/missing")"
check "ErrorDocument keeps the 404 status" '404'                           "$(code /errdoc/missing)"
check "ErrorDocument covers a denial"      'custom forbidden'              "$(curl -s "$B/errdoc403/secret.txt")"
check "denial keeps the 403 status"        '403'                          "$(code /errdoc403/secret.txt)"

check "Header edit rewrites a value"       'x-hatch-edit: xyz123'          "$(headers /edit/)"
check "Header edit* replaces every match"  'x-hatch-edit-all: anbn'        "$(headers /edit/)"

# A stripped backslash turns ^\. into ^. and denies every file in the folder.
check "FilesMatch ^\\. keeps a normal file" '200'                          "$(code /dotguard/)"
check "FilesMatch ^\\. blocks a dotfile"   '403'                          "$(code /dotguard/.secret)"

check "Limit allows GET"                   '200'                          "$(code /limit/)"
check "Limit denies POST"                  '403'                          "$(code_post /limit/ POST)"
check "Limit denies DELETE"                '403'                          "$(code_post /limit/ DELETE)"

check "Satisfy Any allows a granted client" '200'                         "$(code /satisfy/)"
check "Satisfy All denies when the host rule denies" '403'                "$(code /satisfy-all/)"

check "AddDefaultCharset adds charset to html" 'charset=iso-8859-1'       "$(headers /charset/)"
check "AddDefaultCharset leaves images alone" 'no-charset'                "$(charset_header /charset/i.png)"

check "SSLRequireSSL denies plain HTTP"    '403'                          "$(code /sslrequire/)"

# SetEnv reaches the backend, so PHP sees the same variables rewrite rules do.
if [ "$(curl -s "$B/env/show.php")" = gold ]; then
  check "SetEnv reaches PHP"               'gold'                           "$(curl -s "$B/env/show.php")"
else
  echo "  skip  SetEnv in PHP (no PHP handler on this server)"
fi

# php_value needs CyberPanel's core, which carries the php members in its table.
if [ "${HATCH_CYBERPANEL:-0}" = 1 ]; then
  check "php_value is accepted"          'x-hatch-php: on'                 "$(headers /php/)"
  check "php_value reaches the backend"  '768M'                            "$(curl -s "$B/php/index.php")"
else
  check "php_value is reported, not fatal" 'x-hatch-php: on'               "$(headers /php/)"
  # Without the php members the .htaccess value must not reach the backend.
  check "php_value is not applied here"  ''                               "$(curl -s "$B/php/index.php" | grep -o '^768M' | sed 's/^768M$/unexpected memory_limit/')"
fi

if [ "$(curl -s "$B/hx/plain.php")" = plain ]; then
  check "PHP keeps its own Cache-Control"  'cache-control: no-store, private' "$(headers /hx/nocache.php)"
  check "PHP without one gets the expiry" 'cache-control: max-age=2592000'   "$(headers /hx/plain.php)"
else
  echo "  skip  PHP checks (no PHP handler on this server)"
fi

# ErrorDocument for a status only the backend raises needs a handler that can fail.
if [ "$(curl -s "$B/hx/plain.php")" = plain ]; then
  check "ErrorDocument serves a backend 500" 'custom 500'                    "$(curl -s "$B/errdoc500/boom.php")"
  check "ErrorDocument keeps the 500 status" '500'                          "$(code /errdoc500/boom.php)"
else
  echo "  skip  ErrorDocument 500 checks (no PHP handler on this server)"
fi

"$ENGINE" exec "$NAME" bash -c 'sed -i "s/X-Hatch-Hx \"yes\"/X-Hatch-Hx \"changed\"/" /usr/local/lsws/Example/html/hx/.htaccess'
check "edits apply without a restart"    'x-hatch-hx: changed'              "$(headers /hx/)"

# A wrong LSIAPI layout used to recurse until the worker died, so a crash here is fatal.
"$ENGINE" exec "$NAME" bash -c 'ls /tmp/lshttpd/bak_core 2>/dev/null | grep -q .' \
  && { echo "  FAIL  a core dump was written"; FAILED=1; }
"$ENGINE" exec "$NAME" bash -c 'grep -aq "signal=11" /usr/local/lsws/logs/error.log 2>/dev/null' \
  && { echo "  FAIL  a worker died with SIGSEGV"; FAILED=1; }
[ "$FAILED" = 0 ] && echo "all passed" || { echo "failures above"; exit 1; }
