<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/logo-wordmark-dark.svg">
    <img src="assets/logo-wordmark.svg" alt="hatch" width="420">
  </picture>
</p>

<p align="center">
  <a href="https://github.com/prabowosd/hatch/actions/workflows/build.yml"><img src="https://github.com/prabowosd/hatch/actions/workflows/build.yml/badge.svg" alt="build"></a>
  <a href="https://github.com/prabowosd/hatch/releases/latest"><img src="https://img.shields.io/github/v/release/prabowosd/hatch" alt="release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPL--3.0-blue" alt="license"></a>
</p>

> **CyberPanel:** supported from v0.1.2. CyberPanel replaces OpenLiteSpeed with its own core (2.5.x) whose `lsi_api_t` carries three extra members; hatch detects that layout at load time. v0.1.1 and older crash the worker on the first request there, so upgrade before installing on a CyberPanel server.

Apache `.htaccess` compatibility for OpenLiteSpeed, as an open source module written in Rust.

OpenLiteSpeed reads `RewriteRule` from `.htaccess` on its own, but ignores everything else (`Header`, `Redirect`, `Require`, `<FilesMatch>` and friends). hatch loads into OpenLiteSpeed and applies those directives per request, the way Apache does. Edits take effect on the next request; no restart.

## Supported directives (v0.1)

| Directive | Notes |
|---|---|
| `Header set\|append\|merge\|add\|unset` | `always` and `onsuccess` accepted. `set` replaces values the server added itself. `env=name` and `env=!name` conditions apply; `expr=` is reported and ignored. |
| `SetEnv`, `UnsetEnv` | Sets and clears request environment variables, so the backend and the rewrite engine see them. |
| `SetEnvIf`, `SetEnvIfNoCase`, `BrowserMatch` | Matches a request header, `Request_URI`, `Request_Method`, `Remote_Addr`, `Remote_Host` or `Host`. `!var` and `var=value` are accepted. |
| `php_value`, `php_admin_value`, `php_flag`, `php_admin_flag` | Needs CyberPanel's OpenLiteSpeed core, whose API carries the php config calls. On other cores the line is logged once and skipped. |
| `ErrorDocument` | A path target, for 404 and for a denied request. The page is served through an internal redirect and keeps the original status. URL and text targets need a body hatch cannot write, so they are logged and skipped. |
| `Options -Indexes` | A directory with no index file answers 403 instead of listing its contents. `+Indexes` restores the server default. Symlink and content-negotiation options (`FollowSymLinks`, `SymLinksIfOwnerMatch`, `MultiViews`) are accepted and left to the server. |
| `<Limit>`, `<LimitExcept>` | Access rules apply only to the listed HTTP methods (or to every method except them). |
| `Satisfy Any\|All` | Combines host rules (`Order`/`Allow`/`Deny`) with `Require` rules using OR (`Any`) or AND (`All`). |
| `AddDefaultCharset` | Adds a charset to the response `Content-Type` when the response has none; `Off` disables it. |
| `SSLRequireSSL` | Answers 403 when the request did not arrive over TLS. |
| `Redirect`, `RedirectPermanent`, `RedirectTemp` | Prefix match that keeps the rest of the path, like mod_alias. Status words `permanent`, `temp`, `seeother`, `gone`. |
| `RedirectMatch` | Regex with `$1`..`$9`. Targets must be a path or an `http(s)` URL. |
| `Order`, `Allow from`, `Deny from` | Apache 2.2 semantics, IPv4, IPv6, CIDR and partial addresses (`10.1`). |
| `Require all granted\|denied`, `Require ip`, `Require not ip` | Apache 2.4 semantics. |
| `<Files>`, `<Files ~>`, `<FilesMatch>` | Applied to the requested file name. |
| `<IfModule>` | Contents apply; `<IfModule !...>` fallbacks are skipped. |
| `ExpiresActive`, `ExpiresDefault`, `ExpiresByType` | `access plus N unit` and `A<seconds>` forms, `type/*` wildcards. Like mod_expires, a PHP or proxied response that sets its own `Cache-Control` or `Expires` keeps it; `Header set Cache-Control` still wins over expiry. |
| `RewriteEngine`, `RewriteRule`, `RewriteCond`, `RewriteBase` | Left to OpenLiteSpeed, untouched. |

`RequestHeader` cannot be implemented: LSIAPI can read a request header but has no call that changes one. The line is logged and skipped.

Next (v0.2): Basic Auth (`AuthType`, `AuthUserFile`, `Require valid-user`), `DirectoryIndex`.

Unsupported directives are logged once per file change and skipped; they never break the site.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/prabowosd/hatch/main/install.sh | sudo bash
```

The installer checks the architecture and glibc (2.34+: RHEL/Alma/Rocky 9 and 10, Ubuntu 22.04+), verifies the release checksum, backs up `httpd_config.conf` and any previous module, adds

```
module hatch {
  ls_enabled              1
}
```

and does a graceful restart. If OpenLiteSpeed does not come back with hatch loaded, it restores the previous module and config on its own.

```sh
sudo bash install.sh --file ./libhatch.so   # install a local build
sudo bash install.sh --version v0.1.0       # pin a release
sudo bash install.sh --status
sudo bash install.sh --uninstall
```

CyberPanel ships its own OpenLiteSpeed core. hatch recognises it by the `cyberpanel_lsi_api_abi_v1` symbol the core exports and reads the table with that layout. On a core with an unknown marker it logs one line and stays idle instead of guessing. The bundled `cyberpanel_ols` module needs a paid license and stays inert without one; hatch runs next to it.

## Docker

`install.sh` edits an OpenLiteSpeed installed on the host. For a container, mount the release `.so` and a config that enables it instead:

```yaml
services:
  openlitespeed:
    image: litespeedtech/openlitespeed:latest
    volumes:
      - ./hatch-x86_64-linux-gnu.so:/usr/local/lsws/modules/hatch.so:ro
      - ./httpd_config.conf:/usr/local/lsws/conf/httpd_config.conf:ro
```

Add this block to `httpd_config.conf`, then restart the container:

```
module hatch {
  ls_enabled 1
}
```

Use the `x86_64` or `aarch64` asset that matches the container. The image must use glibc (the official OpenLiteSpeed images do); a musl/Alpine image cannot load the module.

## How it works

- Hooks `URI_MAP` (redirects and access control) and `SEND_RESP_HEADER` (headers and expiry).
- For each request it walks from the docroot to the file's folder and merges every `.htaccess` on the way: headers accumulate from parent to child, the deepest access rules win, the deepest matching redirect wins.
- Parsed files are cached per worker and re-read only when their mtime or size changes. Files over 256 KB are ignored.
- Regexes use the `regex` crate, which runs in linear time, so a hostile pattern cannot stall a worker.
- Header names, values and redirect targets are validated; anything that could inject a header or a `javascript:` redirect is dropped.
- The server's function table is read once per worker. CyberPanel's core has a longer `lsi_api_t`, so hatch picks the layout from the core's ABI marker first and falls back to scanning the server binary.
- Every hook runs inside `catch_unwind` and fails open: a bug in hatch skips the `.htaccess` rules for that request instead of taking the server down.

## Build

The module links against OpenLiteSpeed's LSIAPI (`ols-api/`, GPLv3, from OpenLiteSpeed 1.8.3). Builds run in a container so the result matches the target glibc:

```sh
podman build -t hatch-build -f build/Containerfile build
podman run --rm -v "$PWD":/src:Z hatch-build cargo build --release
# target/release/libhatch.so
```

## Test

```sh
podman run --rm -v "$PWD":/src:Z hatch-build cargo test   # unit tests
tests/integration.sh target/release/libhatch.so           # real OpenLiteSpeed in a container
HATCH_CYBERPANEL=1 tests/integration.sh libhatch.so       # CyberPanel's OpenLiteSpeed core (x86_64)
HATCH_LOCAL=1 tests/integration.sh libhatch.so            # the OpenLiteSpeed installed on this host
```

CI runs all of them on x86_64 and aarch64: OpenLiteSpeed 1.8 (official image), 1.9 on AlmaLinux 10 (LiteSpeed RPM), and CyberPanel's core built from `build/CyberPanelfile`.

## Troubleshooting

When the module loads but a rule does not apply, create `/usr/local/lsws/conf/hatch.debug` and restart OpenLiteSpeed. hatch then logs, per request, what the server handed it: the URI, the doc root, the resolved file path and how many `.htaccess` files apply.

```sh
sudo touch /usr/local/lsws/conf/hatch.debug
sudo /usr/local/lsws/bin/lswsctrl restart
tail -f /usr/local/lsws/logs/error.log | grep '\[hatch\]'
```

The check runs once per worker thread, so the restart is required after creating or removing the file. Entries are written at WARN level to the vhost error log, so an `errorlog` block with `logLevel ERROR` will suppress them. Remove the file and restart when done, since an active trace writes several lines per request.

## License

GPL-3.0-or-later, the same as OpenLiteSpeed. hatch is an independent project and is not affiliated with LiteSpeed Technologies or CyberPanel.
