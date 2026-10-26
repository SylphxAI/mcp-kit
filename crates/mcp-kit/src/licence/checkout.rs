//! The subset of Keel's docs.toml checkout configuration needed by a CLI.

use std::collections::HashSet;

/// A named price/seat-pack option from a storefront's `money.offers.*.prices`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoneyPriceOption {
    pub id: String,
    pub key: String,
    pub label: String,
    pub quantity: u32,
}

impl MoneyPriceOption {
    /// Same selector as Keel: explicit `id`, otherwise `key:quantity`.
    pub fn selector(&self) -> String {
        if self.id.trim().is_empty() {
            format!("{}:{}", self.key, self.quantity)
        } else {
            self.id.trim().to_string()
        }
    }
}

/// Public Money checkout settings, separate from the offline licence policy.
/// Read these from the product's existing storefront rather than duplicating them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoneyCheckout {
    pub api_origin: String,
    pub parent: String,
    pub publishable_key: String,
    pub product: String,
    pub checkout_enabled: bool,
    pub prices: Vec<MoneyPriceOption>,
    pub consent_version: String,
    pub consent_text: String,
    pub success_url: String,
    pub cancel_url: String,
    pub recovery_url: String,
}

impl MoneyCheckout {
    /// Read `[money]` and `[money.offers.<offer>]` from an embedded Keel docs.toml.
    /// Unrelated docs and presentation fields are intentionally ignored.
    pub fn from_docs_toml(source: &str, offer: &str) -> Result<Self, String> {
        let doc = source
            .parse::<toml_edit::DocumentMut>()
            .map_err(|_| "invalid storefront docs.toml".to_string())?;
        let money = doc.get("money").ok_or("docs.toml needs [money]")?;
        let item = money
            .get("offers")
            .and_then(|v| v.get(offer))
            .ok_or("unknown Money checkout offer")?;
        let text = |value: &toml_edit::Item, key: &str, default: &str| -> Result<String, String> {
            match value.get(key) {
                None => Ok(default.to_string()),
                Some(v) => v
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| format!("{key} must be a string")),
            }
        };
        let checkout_enabled = match item.get("checkout_enabled") {
            None => true,
            Some(v) => v.as_bool().ok_or("checkout_enabled must be a boolean")?,
        };
        let mut prices = Vec::new();
        if let Some(raw) = item.get("prices") {
            for price in raw
                .as_array_of_tables()
                .ok_or("prices must be an array of tables")?
            {
                let field = |key: &str| -> Result<String, String> {
                    match price.get(key) {
                        None => Ok(String::new()),
                        Some(v) => v
                            .as_str()
                            .map(str::to_string)
                            .ok_or_else(|| format!("price {key} must be a string")),
                    }
                };
                let quantity = match price.get("quantity") {
                    None => 1,
                    Some(value) => value
                        .as_integer()
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or("price quantity must be a positive integer")?,
                };
                prices.push(MoneyPriceOption {
                    id: field("id")?,
                    key: field("key")?,
                    label: field("label")?,
                    quantity,
                });
            }
        }
        let config = Self {
            api_origin: text(money, "api_origin", "https://api.sylphx.com")?,
            parent: text(money, "parent", "")?,
            publishable_key: text(money, "publishable_key", "")?,
            product: text(item, "product", "")?,
            checkout_enabled,
            prices,
            consent_version: text(item, "consent_version", "")?,
            consent_text: text(item, "consent_text", "")?,
            success_url: text(item, "success_url", "")?,
            cancel_url: text(item, "cancel_url", "")?,
            recovery_url: text(item, "recovery_url", "")?,
        };
        config.validate()?;
        Ok(config)
    }

    /// Fail closed before collecting personal data or contacting Money.
    pub fn validate(&self) -> Result<(), String> {
        let key = &self.publishable_key;
        if !key
            .strip_prefix("sylphx_pk_")
            .or_else(|| key.strip_prefix("pk_"))
            .is_some_and(|s| {
                !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            })
        {
            return Err("Money requires a publishable key; secret keys are refused".into());
        }
        let api = safe_url(&self.api_origin, true)
            .ok_or("api_origin must be HTTPS (or loopback HTTP)")?;
        if api.path() != "/" || api.query().is_some() || api.fragment().is_some() {
            return Err("api_origin must be an origin without a path, query or fragment".into());
        }
        let parts: Vec<_> = self.parent.split('/').collect();
        let segment = |s: &&str| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        };
        if parts.len() != 6
            || parts[0] != "orgs"
            || parts[2] != "projects"
            || parts[4] != "envs"
            || ![parts[1], parts[3], parts[5]].iter().all(segment)
        {
            return Err("parent must match orgs/{org}/projects/{project}/envs/{env}".into());
        }
        if self.product.trim().is_empty() {
            return Err("Money product is required".into());
        }
        if safe_url(&self.recovery_url, false).is_none() {
            return Err("recovery_url must be an HTTPS URL".into());
        }
        if !self.checkout_enabled {
            return Ok(());
        }
        if self.consent_version.trim().is_empty()
            || self.consent_version.len() > 64
            || self.consent_text.trim().is_empty()
            || self.consent_text.len() > 2000
        {
            return Err(
                "consent_version (1..64 bytes) and exact consent_text (1..2000 bytes) are required"
                    .into(),
            );
        }
        if safe_url(&self.success_url, false).is_none()
            || !self.success_url.contains("{CHECKOUT_SESSION_ID}")
            || safe_url(&self.cancel_url, false).is_none()
        {
            return Err(
                "HTTPS success_url (with {CHECKOUT_SESSION_ID}) and cancel_url are required".into(),
            );
        }
        let mut selectors = HashSet::new();
        if self.prices.is_empty()
            || self.prices.iter().any(|p| {
                p.key.trim().is_empty()
                    || p.label.trim().is_empty()
                    || !(1..=10_000).contains(&p.quantity)
                    || !selectors.insert(p.selector())
            })
        {
            return Err(
                "checkout needs distinct price options with a key, label and quantity 1..10000"
                    .into(),
            );
        }
        Ok(())
    }

    pub(super) fn selection(
        &self,
        pack: Option<&str>,
        qty: Option<u32>,
    ) -> Result<(&str, u32), String> {
        let option = match pack {
            None => self.prices.first(),
            Some(id) => {
                // Explicit selectors win over the unambiguous-key convenience alias.
                if let Some(option) = self.prices.iter().find(|p| p.selector() == id) {
                    return self.selection_quantity(option, qty);
                }
                let mut matches = self.prices.iter().filter(|p| p.key == id);
                let first = matches.next();
                if matches.next().is_some() {
                    return Err(
                        "ambiguous --pack; use its distinct option id or key:quantity".into(),
                    );
                }
                first
            }
        }
        .ok_or("unknown --pack (choose an option from the storefront)")?;
        self.selection_quantity(option, qty)
    }

    fn selection_quantity<'a>(
        &self,
        option: &'a MoneyPriceOption,
        qty: Option<u32>,
    ) -> Result<(&'a str, u32), String> {
        // Preserve the established CLI contract: --qty is a pack count, so the
        // Money line quantity is pack seats × pack count (5 seats × 2 = 10).
        let quantity = option
            .quantity
            .checked_mul(qty.unwrap_or(1))
            .filter(|quantity| (1..=10_000).contains(quantity))
            .ok_or("--pack and --qty must multiply to between 1 and 10000 seats")?;
        Ok((&option.key, quantity))
    }
}

/// Reject credentials, parser-normalized whitespace/backslashes, and non-web schemes.
/// HTTP is permitted only for a literal loopback API origin, never payment links.
pub(super) fn safe_url(raw: &str, loopback: bool) -> Option<url::Url> {
    if raw
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '\\' | '"' | '<' | '>'))
    {
        return None;
    }
    let authority = raw.split_once("://")?.1.split(['/', '?', '#']).next()?;
    if authority.contains('@') {
        return None;
    }
    let parsed = url::Url::parse(raw).ok()?;
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.host_str().is_none() {
        return None;
    }
    match parsed.scheme() {
        "https" if raw.starts_with("https://") => Some(parsed),
        "http"
            if loopback
                && raw.starts_with("http://")
                && ["localhost", "127.0.0.1", "[::1]"].iter().any(|host| {
                    authority.strip_prefix(host).is_some_and(|port| {
                        port.is_empty()
                            || port.strip_prefix(':').is_some_and(|p| {
                                !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())
                            })
                    })
                }) =>
        {
            Some(parsed)
        }
        _ => None,
    }
}
