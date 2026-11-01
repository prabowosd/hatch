# Changelog

Notable changes to hatch, newest first. Releases match the git tags; anything not yet tagged sits under Unreleased.

## [Unreleased]

## [0.1.9] - 2026-11-02

### Added
- `CacheEnable`, `CacheDisable` and `CacheMaxExpire`, emitted as `X-LiteSpeed-Cache-Control`. A backend header already set wins over hatch, and cache scope matching runs on the normalized served path so a scoped disable cannot be evaded with `//x` or `/./x` variants.

## [0.1.8] - 2026-10-31

### Added
- `DirectoryIndex`, serving the first listed file that exists (`DirectoryIndex index.php index.html`), with `disabled` to turn it off. `Options -Indexes` now counts the configured list too, so a folder with a custom index is not refused.

## [0.1.7] - 2026-10-28

### Added
- `Header edit` and `Header edit*`, substituting a pattern in a response header value the server already set. `$1`..`$9` backreferences work; a miss leaves the value untouched.
- `BrowserMatchNoCase`, matching `User-Agent` case-insensitively like `SetEnvIfNoCase`.

### Fixed
- `ErrorDocument` now serves a configured page for an error status only the backend raises (`401`, `500`, `503` and the rest), not just `403` and `404`. The check runs at `SEND_RESP_HEADER` and keeps the original status.

### Changed
- `cargo fmt --check` and `cargo clippy -- -D warnings` run in CI, with a `rustfmt.toml` matching the existing style.

### Added
- `Header edit` and `Header edit*`, substituting a pattern in a response header value the server already set. `$1`..`$9` backreferences work; a miss leaves the value untouched.
- `BrowserMatchNoCase`, matching `User-Agent` case-insensitively like `SetEnvIfNoCase`.

### Fixed
- `ErrorDocument` now serves a configured page for an error status only the backend raises (`401`, `500`, `503` and the rest), not just `403` and `404`. The check runs at `SEND_RESP_HEADER` and keeps the original status.

### Changed
- `cargo fmt --check` and `cargo clippy -- -D warnings` run in CI, with a `rustfmt.toml` matching the existing style.
