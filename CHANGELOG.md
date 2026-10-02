# Changelog

## 0.5.0

- **Breaking (licence feature):** `LicencePolicy` has a new required field `tier: &str` ("Pro", "Team"). Add `tier: "Pro"` to your policy literal to keep today's wording; a product that sells a team plan sets `tier: "Team"`. `ProRequired` also gains a public `tier: String`; set it if you construct one yourself.
- The tier now appears in the notice ("<feature> is part of <product> <tier>"), in `licence status` ("<product> <tier>: active") and `licence activate`, and inside the structured content: `pro_required` keeps its name and gains `"tier"`.
- `Licence::expires_soon(within: Duration) -> bool`: true when the licence expires within the window (inclusive) or already has; false when it has no `expiresAt`.
- `licence status` warns "expires in N days" with `policy.upgrade_url` as the renewal link during the last 30 days.

## 0.4.0

- Add the `licence` feature (off by default): `LicencePolicy`, `verify`, token discovery (env var, then a file in the config dir), `require`, the non-error `ProRequired` tool result with `pro_required` structured content, and a `licence status | activate <token>` CLI helper. Tokens are the Ed25519 format anymd and GPDT already verify. Additive; the public API of 0.3 is unchanged.
- `LicencePolicy.require_product`: refuse tokens that name no product (true for the Money issuer, false for anymd back-compat).
- `App::call_result` (default wraps `call`) lets a gated tool return `licence::required_result`.
- `licence status` prints the token source and the verify error.
