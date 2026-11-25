//! Real checkout creation with a caller-owned operation identity. Creating a
//! session is never reported as payment, and missing configuration fails closed.
use super::client::StripeClient;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, time::Duration};

/// A checkout's commercial terms and stable operation identity travel together.
/// Amounts use minor units; this is not proof that the customer paid.
pub struct CheckoutRequest<'a> {
    pub name: &'a str,
    pub reference: &'a str,
    pub amount_cents: i64,
    pub interval: Option<&'a str>,
    pub product: Option<&'a str>,
    pub currency: &'a str,
    pub operation_id: &'a str,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckoutReceipt {
    pub id: String,
    pub url: String,
    pub payment_status: String,
    pub amount_total: i64,
    pub currency: String,
}

pub fn valid_provider_id(value: &str, prefix: &str) -> bool {
    value.starts_with(prefix)
        && value.len() > prefix.len()
        && value.len() <= 255
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}
fn secure_origin(value: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(value).map_err(|_| "Invalid payment origin")?;
    let local = matches!(
        url.host_str(),
        Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
    );
    if !(url.scheme() == "https" || (local && url.scheme() == "http"))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Payment origin must be HTTPS (HTTP is restricted to loopback)".into());
    }
    Ok(url)
}
pub fn money_minor_units(value: f64) -> Result<i64, String> {
    if !value.is_finite() || value <= 0.0 || value > 999_999.99 {
        return Err("Invalid checkout amount".into());
    }
    let cents = (value * 100.0).round();
    if (value * 100.0 - cents).abs() > 0.00001 {
        return Err("Checkout amount must use currency minor units".into());
    }
    Ok(cents as i64)
}
fn validate_metadata_entry(key: &str, value: &str) -> Result<(), String> {
    if key.is_empty()
        || key.len() > 40
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        || value.len() > 500
        || value.chars().any(char::is_control)
    {
        return Err("Invalid checkout metadata entry".into());
    }
    Ok(())
}

struct CheckoutFormParams<'a> {
    name: &'a str,
    reference: &'a str,
    amount: i64,
    interval: Option<&'a str>,
    product: Option<&'a str>,
    currency: &'a str,
    return_base: &'a str,
    metadata: &'a BTreeMap<String, String>,
    copy_metadata_to_subscription: bool,
}

fn checkout_form(params: CheckoutFormParams<'_>) -> Result<BTreeMap<String, String>, String> {
    let CheckoutFormParams {
        name,
        reference,
        amount,
        interval,
        product,
        currency,
        return_base,
        metadata,
        copy_metadata_to_subscription,
    } = params;
    if name.trim().is_empty()
        || name.len() > 500
        || reference.trim().is_empty()
        || reference.len() > 200
        || amount <= 0
        || amount > 99_999_999
    {
        return Err("Invalid checkout description, reference or amount".into());
    }
    if metadata.len() > 20 {
        return Err("Too many checkout metadata entries".into());
    }
    // The legacy public amount argument is a major-unit decimal. Zero-/three-
    // decimal currencies require explicit currency-aware amounts, not guessing.
    let currency = currency.to_ascii_lowercase();
    if !matches!(
        currency.as_str(),
        "usd" | "eur" | "gbp" | "cad" | "aud" | "nzd" | "sgd" | "hkd" | "inr" | "brl" | "mxn"
    ) {
        return Err("Currency requires an explicit minor-unit implementation".into());
    }
    let base = secure_origin(return_base)?;
    let mut form = BTreeMap::from([
        (
            "success_url".into(),
            base.join("payments/return?session_id={CHECKOUT_SESSION_ID}")
                .map_err(|_| "Invalid return URL")?
                .to_string(),
        ),
        (
            "cancel_url".into(),
            base.join("payments/cancel")
                .map_err(|_| "Invalid cancel URL")?
                .to_string(),
        ),
        ("client_reference_id".into(), reference.into()),
        (
            "mode".into(),
            if interval.is_some() {
                "subscription"
            } else {
                "payment"
            }
            .into(),
        ),
        ("line_items[0][price_data][currency]".into(), currency),
        (
            "line_items[0][price_data][product_data][name]".into(),
            name.into(),
        ),
        (
            "line_items[0][price_data][unit_amount]".into(),
            amount.to_string(),
        ),
        ("line_items[0][quantity]".into(), "1".into()),
        ("payment_method_types[0]".into(), "card".into()),
    ]);
    if let Some(interval) = interval {
        if !matches!(interval, "day" | "week" | "month" | "year") {
            return Err("Invalid subscription interval".into());
        }
        form.insert(
            "line_items[0][price_data][recurring][interval]".into(),
            interval.into(),
        );
    }
    if let Some(product) = product {
        if product.len() > 255 || product.chars().any(char::is_control) {
            return Err("Invalid product reference".into());
        }
        form.insert("metadata[product_id]".into(), product.into());
        if interval.is_some() && copy_metadata_to_subscription {
            form.insert(
                "subscription_data[metadata][product_id]".into(),
                product.into(),
            );
        }
    }
    for (key, value) in metadata {
        validate_metadata_entry(key, value)?;
        form.insert(format!("metadata[{key}]"), value.clone());
        if interval.is_some() && copy_metadata_to_subscription {
            form.insert(format!("subscription_data[metadata][{key}]"), value.clone());
        }
    }
    Ok(form)
}
fn parse_receipt(
    value: serde_json::Value,
    amount: i64,
    currency: &str,
) -> Result<CheckoutReceipt, String> {
    let receipt: CheckoutReceipt = serde_json::from_value(value)
        .map_err(|_| "Checkout response lacks required provider evidence")?;
    // Stripe-hosted Checkout URLs legitimately contain query/fragment state.
    // Validate the destination separately from an operator-configured origin.
    let url = reqwest::Url::parse(&receipt.url).map_err(|_| "Invalid checkout URL")?;
    if receipt.url.len() > 16_384
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
        || !url.path().starts_with("/c/pay/")
        || !valid_provider_id(&receipt.id, "cs_")
        || url.scheme() != "https"
        || url.host_str() != Some("checkout.stripe.com")
        || receipt.amount_total != amount
        || !receipt.currency.eq_ignore_ascii_case(currency)
        || !matches!(
            receipt.payment_status.as_str(),
            "paid" | "unpaid" | "no_payment_required"
        )
    {
        return Err("Checkout response did not match the requested payment".into());
    }
    Ok(receipt)
}
impl StripeClient {
    pub async fn create_checkout_session_idempotent(
        &self,
        request: CheckoutRequest<'_>,
    ) -> Result<CheckoutReceipt, String> {
        self.create_checkout_session_with_metadata(request, &BTreeMap::new(), false)
            .await
    }

    pub async fn create_checkout_session_with_metadata(
        &self,
        request: CheckoutRequest<'_>,
        metadata: &BTreeMap<String, String>,
        copy_metadata_to_subscription: bool,
    ) -> Result<CheckoutReceipt, String> {
        let CheckoutRequest {
            name,
            reference,
            amount_cents,
            interval,
            product,
            currency,
            operation_id,
        } = request;
        let key = self.require_api_key()?;
        if operation_id.trim().is_empty()
            || operation_id.len() > 255
            || operation_id.chars().any(char::is_control)
        {
            return Err("Stable checkout operation identity is required".into());
        }
        let base = secure_origin(&Self::api_base())?;
        let local = matches!(
            base.host_str(),
            Some("127.0.0.1" | "localhost" | "::1" | "[::1]")
        );
        if base.host_str() != Some("api.stripe.com") && !local {
            return Err("Untrusted Stripe API origin".into());
        }
        let return_base = std::env::var("PUBLIC_APP_URL")
            .or_else(|_| std::env::var("BASE_URL"))
            .map_err(|_| "PUBLIC_APP_URL is required for customer checkout")?;
        let form = checkout_form(CheckoutFormParams {
            name,
            reference,
            amount: amount_cents,
            interval,
            product,
            currency,
            return_base: &return_base,
            metadata,
            copy_metadata_to_subscription,
        })?;
        let response = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "Unable to initialize payment client")?
            .post(
                base.join("/v1/checkout/sessions")
                    .map_err(|_| "Invalid payment endpoint")?,
            )
            .basic_auth(key, Some(""))
            .header("Idempotency-Key", operation_id)
            .form(&form)
            .send()
            .await
            .map_err(|_| "Checkout outcome is unknown; reconcile the operation before retrying")?;
        if !response.status().is_success() {
            return Err(format!(
                "Stripe checkout returned HTTP {}; no success was recorded",
                response.status().as_u16()
            ));
        }
        parse_receipt(
            response
                .json()
                .await
                .map_err(|_| "Invalid checkout response")?,
            amount_cents,
            currency,
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn amount_is_checked_not_truncated() {
        assert_eq!(money_minor_units(10.25).unwrap(), 1025);
        for value in [f64::NAN, f64::INFINITY, -1.0, 0.0, 1.001, f64::MAX] {
            assert!(money_minor_units(value).is_err());
        }
    }
    #[test]
    fn checkout_binds_amount_and_safe_return_origin() {
        let empty = BTreeMap::new();
        let form = checkout_form(CheckoutFormParams {
            name: "Gate repair",
            reference: "proposal-1",
            amount: 15000,
            interval: None,
            product: None,
            currency: "usd",
            return_base: "https://owner.example",
            metadata: &empty,
            copy_metadata_to_subscription: false,
        })
        .unwrap();
        assert_eq!(form["line_items[0][price_data][unit_amount]"], "15000");
        assert_eq!(form["client_reference_id"], "proposal-1");
        assert!(
            checkout_form(CheckoutFormParams {
                name: "Gate",
                reference: "id",
                amount: 100,
                interval: None,
                product: None,
                currency: "jpy",
                return_base: "https://owner.example",
                metadata: &empty,
                copy_metadata_to_subscription: false,
            })
            .is_err()
        );
        assert!(
            checkout_form(CheckoutFormParams {
                name: "Gate",
                reference: "id",
                amount: 100,
                interval: None,
                product: None,
                currency: "usd",
                return_base: "http://attacker.example",
                metadata: &empty,
                copy_metadata_to_subscription: false,
            })
            .is_err()
        );
    }
    #[test]
    fn platform_subscription_checkout_propagates_metadata_to_subscription_data() {
        let metadata = BTreeMap::from([
            ("checkout_kind".into(), "platform_plan".into()),
            ("tier".into(), "Pro".into()),
            ("tenant_id".into(), "tenant-42".into()),
        ]);
        let form = checkout_form(CheckoutFormParams {
            name: "Pro",
            reference: "tenant-42",
            amount: 7900,
            interval: Some("month"),
            product: None,
            currency: "usd",
            return_base: "https://owner.example",
            metadata: &metadata,
            copy_metadata_to_subscription: true,
        })
        .unwrap();
        assert_eq!(form["mode"], "subscription");
        assert_eq!(form["metadata[checkout_kind]"], "platform_plan");
        assert_eq!(form["metadata[tier]"], "Pro");
        assert_eq!(form["metadata[tenant_id]"], "tenant-42");
        assert_eq!(
            form["subscription_data[metadata][checkout_kind]"],
            "platform_plan"
        );
        assert_eq!(form["subscription_data[metadata][tier]"], "Pro");
        assert_eq!(form["subscription_data[metadata][tenant_id]"], "tenant-42");

        let bad_metadata = BTreeMap::from([("bad key!".into(), "val".into())]);
        assert!(
            checkout_form(CheckoutFormParams {
                name: "Pro",
                reference: "tenant-42",
                amount: 7900,
                interval: Some("month"),
                product: None,
                currency: "usd",
                return_base: "https://owner.example",
                metadata: &bad_metadata,
                copy_metadata_to_subscription: true,
            })
            .is_err()
        );
    }
    #[test]
    fn success_needs_real_id_url_amount_and_currency() {
        let value = serde_json::json!({"id":"cs_test_fixture","url":"https://checkout.stripe.com/c/pay/cs_test_fixture","payment_status":"unpaid","amount_total":2500,"currency":"usd"});
        assert!(parse_receipt(value.clone(), 2500, "usd").is_ok());
        let mut fragment = value.clone();
        fragment["url"] = serde_json::json!(
            "https://checkout.stripe.com/c/pay/cs_test_fixture?locale=en#provider-state"
        );
        assert!(parse_receipt(fragment, 2500, "usd").is_ok());
        let mut credentials = value.clone();
        credentials["url"] =
            serde_json::json!("https://user@checkout.stripe.com/c/pay/cs_test_fixture");
        assert!(parse_receipt(credentials, 2500, "usd").is_err());
        assert!(parse_receipt(value.clone(), 2501, "usd").is_err());
        let mut invalid = value;
        invalid["url"] = serde_json::json!("https://attacker.example");
        assert!(parse_receipt(invalid, 2500, "usd").is_err());
    }
}
