# hatch

[![build](https://github.com/prabowosd/hatch/actions/workflows/build.yml/badge.svg)](https://github.com/prabowosd/hatch/actions/workflows/build.yml)

Apache `.htaccess` compatibility for OpenLiteSpeed, as an open source module written in Rust.

OpenLiteSpeed reads `RewriteRule` from `.htaccess` on its own, but ignores everything else (`Header`, `Redirect`, `Require`, `<FilesMatch>` and friends). hatch loads into OpenLiteSpeed and applies those directives per request, the way Apache does. Edits take effect on the next request; no restart.

## Supported directives (v0.1)

| Directive | Notes |
|---|---|
| `Header set\|append\|merge\|add\|unset` | `always` and `onsuccess` accepted. `set` replaces values the server added itself. `env=` and `expr=` conditions are skipped for now. |
| `Redirect`, `RedirectPermanent`, `RedirectTemp` | Prefix match that keeps the rest of the path, like mod_alias. Status words `permanent`, `temp`, `seeother`, `gone`. |
| `RedirectMatch` | Regex with `$1`..`$9`. Targets must be a path or an `http(s)` URL. |
| `Order`, `Allow from`, `Deny from` | Apache 2.2 semantics, IPv4, IPv6, CIDR and partial addresses (`10.1`). |
| `Require all granted\|denied`, `Require ip`, `Require not ip` | Apache 2.4 semantics. |
| `<Files>`, `<Files ~>`, `<FilesMatch>` | Applied to the requested file name. |
| `<IfModule>` | Contents apply; `<IfModule !...>` fallbacks are skipped. |
| `ExpiresActive`, `ExpiresDefault`, `ExpiresByType` | `access plus N unit` and `A<seconds>` forms, `type/*` wildcards. |
| `RewriteEngine`, `RewriteRule`, `RewriteCond`, `RewriteBase` | Left to OpenLiteSpeed, untouched. |

Next (v0.2): Basic Auth (`AuthType`, `AuthUserFile`, `Require valid-user`), `ErrorDocument`, `php_value`/`php_flag`, `DirectoryIndex`, `Header ... env=`.

Unsupported directives are logged once per file change and skipped; they never break the site.

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/prabowosd/hatch/main/install.sh | sudo bash
```

The installer checks the architecture and glibc (2.34+: RHEL/Alma/Rocky 9, Ubuntu 22.04+), verifies the release checksum, backs up `httpd_config.conf` and any previous module, adds

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

On CyberPanel servers the bundled `cyberpanel_ols` module needs a paid license and stays inert without one; hatch runs next to it.

## How it works

- Hooks `URI_MAP` (redirects and access control) and `SEND_RESP_HEADER` (headers and expiry).
- For each request it walks from the docroot to the file's folder and merges every `.htaccess` on the way: headers accumulate from parent to child, the deepest access rules win, the deepest matching redirect wins.
- Parsed files are cached per worker and re-read only when their mtime or size changes. Files over 256 KB are ignored.
- Regexes use the `regex` crate, which runs in linear time, so a hostile pattern cannot stall a worker.
- Header names, values and redirect targets are validated; anything that could inject a header or a `javascript:` redirect is dropped.
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
```

## License

GPL-3.0-or-later, the same as OpenLiteSpeed. hatch is an independent project and is not affiliated with LiteSpeed Technologies or CyberPanel.
