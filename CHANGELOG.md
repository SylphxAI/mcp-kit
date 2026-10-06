# Changelog

## 0.7.3

- Add the `remote` feature (off by default): `remote::router` and `remote::serve` run the same `App` over Streamable HTTP (rmcp, stateless, JSON responses) as an OAuth resource server, as the MCP authorization spec asks. `Remote::new(resource_url, issuer)` sets the token audience (RFC 8707) and the issuer; `GET /.well-known/oauth-protected-resource<path>` serves the RFC 9728 metadata. Every MCP request needs a JWT access token signed by a key in the issuer's JWKS (`jwks_uri`, or discovered through RFC 8414, then OpenID Connect), with the configured `iss`, the resource in `aud`, and a live `exp`. A missing or bad token is 401 and a missing scope (`require`, or `tool_scopes` for one tool) is 403 `insufficient_scope`, each with an RFC 6750 `WWW-Authenticate` challenge naming `resource_metadata`. `App::call_as` (default: `call_result`) receives the caller as a `remote::Principal` (subject, scopes, claims). Additive; the public API of 0.7 is unchanged.

## 0.7.2

- `licence buy` accepts plain `http://` for a loopback `checkout_base` only (`127.0.0.1`, `localhost`, `[::1]`, optional numeric port), so a product's end-to-end test can run the buy flow against a local checkout server. Any other base still needs `https://`; the authority is matched exactly, so `http://localhost:80@evil.com` and `http://localhost.evil.com` stay refused. `checkout_base` is compile-time, so a user cannot turn this on. No API change.

## 0.7.1

- `release.yml`: opt-in `homebrew` and `scoop` inputs (with `homebrew-tap`, `scoop-bucket`, `description`, `homepage`, `license`) update a Homebrew formula and a Scoop manifest from the release archives after each release. Writes use a GitHub App (`TAP_APP_ID`, `TAP_APP_PRIVATE_KEY` secrets); without them the job skips. Rendering is `scripts/package-managers.mjs`. No Rust change.

## 0.7.0

- `embed::QueryModel` (feature `embed`): a query-side embedder that reads only the vocabulary and the rows of the query's own tokens with positioned reads, so a search never loads the weight table. `QueryModel::open(dir)` and `embed(text)` give the same vector as `Model::embed` under `Tokenization::Identifiers`. It moved here from lockdocs so there is one owner of the WordPiece and identifier splitting; `Model` and `QueryModel` now share one WordPiece routine. Additive; the public API of 0.6 is unchanged.

## 0.6.0

- **Breaking (licence feature):** `LicencePolicy` has a new required field `checkout_base: Option<&str>`. Add `checkout_base: None` to keep today's behaviour. The `licence` feature now also depends on `ureq`.
- `licence buy [--pack <id>] [--qty <n>] [--no-browser] [--json]`, part of `run_cli` (so every product gets `<product> licence buy`, or `pro buy` where it mounts the helper under that name). With `checkout_base: Some("https://...")` (https only) it creates a claim at the shared checkout service (`POST /api/v1/claims`), prints the browser URL and opens it unless `--no-browser`, `--json`, no display or a non-interactive run, then polls the claim honouring `Retry-After` (backoff capped at 5 s) for up to 30 minutes. On `paid` it verifies each returned token and saves the first valid one exactly as `activate` does, then prints the `status` report. On expiry or timeout it prints the recovery link `<checkout_base>/recover`. Nothing is saved before a token verifies, so Ctrl+C leaves no state. With `checkout_base: None` it prints `upgrade_url`.
- `--json` prints one JSON line when the claim exists (`event: "claim"`, `claim_id`, `browser_url`, `expires_at`) and one at the end (`event: "result"`, `status` of `paid`, `expired`, `timeout`, `invalid`, `error` or `unavailable`), so an agent can show the link to its user while the command keeps polling. Exit codes: 0 paid (or `unavailable`), 1 failed, 2 usage, 3 expired, 4 timed out. Tokens are never printed. Redirects are never followed; transport errors, 5xx and 429 are retried (honouring `Retry-After`) until the wait runs out, and any failure after the claim exists prints the recovery link (`--json`: `status: "error"` with `claim_id` and `recover`, exit 1). A warning is printed when the product's licence env var is set, since it overrides the saved file. `write_token` now writes atomically (temp file, then rename).

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
