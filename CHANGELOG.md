# Changelog

## 0.4.0

- Add the `licence` feature (off by default): `LicencePolicy`, `verify`, token discovery (env var, then a file in the config dir), `require`, the non-error `ProRequired` tool result with `pro_required` structured content, and a `licence status | activate <token>` CLI helper. Tokens are the Ed25519 format anymd and GPDT already verify. Additive; the public API of 0.3 is unchanged.
