use std::time::Duration;

use ::server_harness::middleware::{connection_vault::ConnectionVault, usage_ledger::UsageLedger};
use ::server_integrations_resend::client::ResendClientWrapper;
use serde_json::Value;
use sqlx::PgPool;

const DEFAULT_RESEND_API_BASE: &str = "https://api.resend.com";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResendEmailReceipt {
    pub provider_message_id: String,
}

pub struct VaultResendDeliveryClient {
    api_key: String,
    api_base: reqwest::Url,
    http_client: reqwest::Client,
}

impl VaultResendDeliveryClient {
    pub fn new(api_key: String, api_base: &str) -> Result<Self, String> {
        let key = api_key.trim();
        if !key.starts_with("re_")
            || key.len() < 12
            || key.len() > 4096
            || key.chars().any(char::is_control)
        {
            return Err("A valid tenant Resend API key is required".into());
        }
        let parsed = validated_resend_base_url(api_base)?;
        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "Unable to initialize outbound email HTTP client".to_string())?;
        Ok(Self {
            api_key: key.to_owned(),
            api_base: parsed,
            http_client,
        })
    }

    pub async fn send_email_with_receipt(
        &self,
        to: &str,
        from: &str,
        subject: &str,
        html_body: &str,
        idempotency_key: &str,
    ) -> Result<ResendEmailReceipt, String> {
        validate_email_address(to, "recipient")?;
        validate_email_address(from, "sender")?;
        if subject.trim().is_empty()
            || subject.len() > 500
            || subject.chars().any(char::is_control)
            || html_body.trim().is_empty()
            || html_body.len() > 100_000
        {
            return Err("Invalid outbound email subject or body".into());
        }
        if idempotency_key.trim().is_empty()
            || idempotency_key.len() > 255
            || idempotency_key.chars().any(char::is_control)
        {
            return Err("A bounded email idempotency key is required".into());
        }

        let endpoint = self
            .api_base
            .join("/emails")
            .map_err(|_| "Invalid Resend endpoint".to_string())?;
        let payload = serde_json::json!({
            "from": from,
            "to": [to],
            "subject": subject,
            "html": html_body,
        });

        let response = self
            .http_client
            .post(endpoint)
            .bearer_auth(&self.api_key)
            .header("Idempotency-Key", idempotency_key)
            .json(&payload)
            .send()
            .await
            .map_err(|_| "Outbound email delivery failed at network boundary".to_string())?;

        if !response.status().is_success() {
            return Err(format!(
                "Resend API rejected outbound email with HTTP {}",
                response.status().as_u16()
            ));
        }

        let body: Value = response
            .json()
            .await
            .map_err(|_| "Resend API response was not valid JSON".to_string())?;
        let provider_message_id = body
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty() && id.len() <= 255 && !id.chars().any(char::is_control))
            .ok_or_else(|| "Resend API response omitted a valid provider message ID".to_string())?
            .to_owned();

        Ok(ResendEmailReceipt {
            provider_message_id,
        })
    }
}

#[async_trait::async_trait]
impl ResendClientWrapper for VaultResendDeliveryClient {
    async fn send_email(
        &self,
        to: &str,
        from: &str,
        subject: &str,
        html_body: &str,
    ) -> Result<(), String> {
        let key = format!("resend:{}", uuid::Uuid::new_v4());
        self.send_email_with_receipt(to, from, subject, html_body, &key)
            .await
            .map(|_| ())
    }
}

fn validated_resend_base_url(raw: &str) -> Result<reqwest::Url, String> {
    let url = reqwest::Url::parse(raw.trim()).map_err(|_| "Invalid Resend API base URL")?;
    let is_loopback = matches!(
        url.host_str(),
        Some("127.0.0.1" | "localhost" | "::1" | "[::1]")
    );
    let allowed = (url.scheme() == "https" && url.host_str() == Some("api.resend.com"))
        || (is_loopback && matches!(url.scheme(), "http" | "https"));
    if !allowed
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Resend API base URL must be https://api.resend.com or loopback".into());
    }
    Ok(url)
}

pub fn validate_email_address(value: &str, label: &str) -> Result<(), String> {
    let trimmed = value.trim();
    let Some((local, domain)) = trimmed.split_once('@') else {
        return Err(format!("Valid {label} email address is required"));
    };
    if trimmed.len() > 320
        || local.is_empty()
        || domain.is_empty()
        || !domain.contains('.')
        || trimmed.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(format!("Valid {label} email address is required"));
    }
    Ok(())
}

pub fn configured_resend_from_email(override_from: Option<&str>) -> Result<String, String> {
    let candidate = override_from
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            std::env::var("OMNISOLO_RESEND_FROM_EMAIL")
                .ok()
                .or_else(|| std::env::var("RESEND_FROM_EMAIL").ok())
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        })
        .ok_or_else(|| {
            "Sender email is not configured (set OMNISOLO_RESEND_FROM_EMAIL)".to_string()
        })?;
    validate_email_address(&candidate, "sender")?;
    Ok(candidate)
}

pub fn resend_api_base_from_env() -> String {
    std::env::var("RESEND_API_BASE")
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_RESEND_API_BASE.to_owned())
}

pub fn escape_html_text(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

type InvoiceSendabilityRow = (
    String,
    Option<String>,
    f64,
    Option<i32>,
    Option<String>,
    Option<String>,
);

#[allow(clippy::too_many_arguments)]
pub async fn send_invoice_email_via_vault(
    ledger: UsageLedger,
    tenant_id: &str,
    recipient_email: &str,
    from_email: Option<&str>,
    subject: &str,
    html_body: &str,
    idempotency_key: &str,
    api_base_override: Option<&str>,
) -> Result<ResendEmailReceipt, String> {
    let vault = ConnectionVault::from_environment(ledger)
        .map_err(|_| "Tenant email connection vault is not configured".to_string())?;
    let resend_key = vault.read_key(tenant_id, "resend").await.map_err(|_| {
        "A verified tenant Resend connection is required to send emails".to_string()
    })?;
    let from = configured_resend_from_email(from_email)?;
    let api_base = api_base_override
        .map(ToOwned::to_owned)
        .unwrap_or_else(resend_api_base_from_env);
    let client = VaultResendDeliveryClient::new(resend_key.to_string(), &api_base)?;
    client
        .send_email_with_receipt(recipient_email, &from, subject, html_body, idempotency_key)
        .await
}

pub async fn handle_invoice_action(
    tenant_id: &str,
    payload: &Value,
    pool: &PgPool,
) -> Result<(), sqlx::Error> {
    let ledger = match crate::db::sqlite_pool_for_pg(pool).or_else(|| {
        (!pool.is_closed())
            .then(crate::db::get_sqlite_pool_if_exists)
            .flatten()
    }) {
        Some(sqlite) => UsageLedger::Sqlite(sqlite),
        None => UsageLedger::Postgres(pool.clone()),
    };
    execute_invoice_reminder_delivery(tenant_id, payload, &ledger, None, None)
        .await
        .map(|_| ())
        .map_err(|err| sqlx::Error::Io(std::io::Error::other(err)))
}

/// Real, fail-closed execution for approved invoice reminder followups (F09).
/// Persists a `drafted` communication event before network I/O, sends via the
/// tenant's verified `ConnectionVault` `"resend"` key, stores the real provider
/// message ID with `status = 'sent'`, and marks `'failed'` + returns `Err` when
/// Resend is unconfigured or rejects delivery. Never fabricates `'delivered'`.
pub async fn execute_invoice_reminder_delivery(
    tenant_id: &str,
    payload: &Value,
    ledger: &UsageLedger,
    from_email_override: Option<&str>,
    api_base_override: Option<&str>,
) -> Result<Option<ResendEmailReceipt>, String> {
    let Some(invoice_id) = payload
        .get("invoice_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return Ok(None);
    };

    let draft_content = payload
        .get("generated_response")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| "Invoice reminder message content is required".to_string())?;

    let payload_recipient = payload
        .get("recipient_email")
        .or_else(|| payload.get("customer_email"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);

    let (should_send, client_id, payment_link) = match ledger {
        UsageLedger::Postgres(pool) => {
            let mut tx = pool
                .begin()
                .await
                .map_err(|e| format!("Failed to begin invoice reminder lookup: {e}"))?;
            ::server_common::auth_utils::set_org_context(&mut *tx, tenant_id)
                .await
                .map_err(|e| format!("Failed to scope tenant context: {e}"))?;
            let invoice_res: Option<InvoiceSendabilityRow> = sqlx::query_as(
                "SELECT status, payment_status, total_amount, amount_paid_cents, \
                     COALESCE(client_id, customer_id), stripe_payment_link \
                     FROM invoices WHERE id = $1 AND tenant_id = $2",
            )
            .bind(invoice_id)
            .bind(tenant_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| format!("Failed to load invoice for reminder: {e}"))?;

            let recent_reminder: Option<(String,)> = sqlx::query_as(
                "SELECT id FROM invoice_communication_events \
                 WHERE invoice_id = $1 AND tenant_id = $2 \
                   AND status IN ('sent', 'delivered') \
                   AND created_at > CURRENT_TIMESTAMP - INTERVAL '1 day'",
            )
            .bind(invoice_id)
            .bind(tenant_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| format!("Failed to check recent invoice reminders: {e}"))?;
            tx.commit()
                .await
                .map_err(|e| format!("Failed to finish invoice reminder lookup: {e}"))?;

            if recent_reminder.is_some() {
                return Ok(None);
            }
            evaluate_invoice_sendability(invoice_res)
        }
        UsageLedger::Sqlite(pool) => {
            let invoice_res: Option<InvoiceSendabilityRow> = sqlx::query_as(
                "SELECT status, payment_status, total_amount, amount_paid_cents, \
                     client_id, stripe_payment_link \
                     FROM invoices WHERE id = ? AND tenant_id = ?",
            )
            .bind(invoice_id)
            .bind(tenant_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("Failed to load SQLite invoice for reminder: {e}"))?;

            let recent_reminder: Option<(String,)> = sqlx::query_as(
                "SELECT id FROM invoice_communication_events \
                 WHERE invoice_id = ? AND tenant_id = ? \
                   AND status IN ('sent', 'delivered') \
                   AND created_at > datetime('now', '-1 day')",
            )
            .bind(invoice_id)
            .bind(tenant_id)
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("Failed to check recent SQLite invoice reminders: {e}"))?;

            if recent_reminder.is_some() {
                return Ok(None);
            }
            evaluate_invoice_sendability(invoice_res)
        }
    };

    if !should_send {
        tracing::info!(
            "Skipping reminder for invoice {}: already paid, cancelled, or not found",
            invoice_id
        );
        return Ok(None);
    }

    let recipient_email = match payload_recipient {
        Some(email) => email,
        None => lookup_customer_email(ledger, tenant_id, client_id.as_deref()).await?,
    };
    validate_email_address(&recipient_email, "recipient")?;

    let comm_id = uuid::Uuid::new_v4().to_string();
    insert_draft_communication_event(
        ledger,
        &comm_id,
        tenant_id,
        invoice_id,
        draft_content,
        &recipient_email,
    )
    .await?;

    let subject = payload
        .get("subject")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("Payment reminder for Invoice #{invoice_id}"));

    let mut html_body = format!("<p>{}</p>", escape_html_text(draft_content));
    if let Some(link) = payment_link
        .as_deref()
        .map(str::trim)
        .filter(|s| s.starts_with("https://"))
    {
        let safe_link = escape_html_text(link);
        html_body.push_str(&format!(
            "<p><a href=\"{safe_link}\">Pay Invoice Online</a></p>"
        ));
    }

    let delivery_res = send_invoice_email_via_vault(
        ledger.clone(),
        tenant_id,
        &recipient_email,
        from_email_override,
        &subject,
        &html_body,
        &comm_id,
        api_base_override,
    )
    .await;

    match delivery_res {
        Ok(receipt) => {
            finalize_communication_event(
                ledger,
                &comm_id,
                tenant_id,
                invoice_id,
                "sent",
                Some(&receipt.provider_message_id),
                None,
            )
            .await?;
            Ok(Some(receipt))
        }
        Err(err) => {
            let _ = finalize_communication_event(
                ledger,
                &comm_id,
                tenant_id,
                invoice_id,
                "failed",
                None,
                Some(&err),
            )
            .await;
            Err(err)
        }
    }
}

fn evaluate_invoice_sendability(
    invoice_res: Option<InvoiceSendabilityRow>,
) -> (bool, Option<String>, Option<String>) {
    if let Some((
        status,
        payment_status_opt,
        total_amount,
        amount_paid_cents_opt,
        client_id,
        link,
    )) = invoice_res
    {
        let status = status.to_lowercase();
        let payment_status = payment_status_opt.unwrap_or_default().to_lowercase();
        let total_cents = (total_amount * 100.0).round() as i64;
        let amount_paid_cents = amount_paid_cents_opt.unwrap_or(0) as i64;

        let ok = !["paid", "refunded", "void", "cancelled", "canceled"]
            .contains(&payment_status.as_str())
            && ["open", "sent", "pending", "overdue", "draft"].contains(&status.as_str())
            && total_cents > amount_paid_cents;
        (ok, client_id, link)
    } else {
        (false, None, None)
    }
}

async fn lookup_customer_email(
    ledger: &UsageLedger,
    tenant_id: &str,
    client_id: Option<&str>,
) -> Result<String, String> {
    let Some(client_id) = client_id.map(str::trim).filter(|s| !s.is_empty()) else {
        return Err("Recipient email is required; invoice has no linked customer".into());
    };
    let email: Option<String> = match ledger {
        UsageLedger::Postgres(pool) => {
            let mut tx = pool
                .begin()
                .await
                .map_err(|_| "Unable to query customer email".to_string())?;
            ::server_common::auth_utils::set_org_context(&mut *tx, tenant_id)
                .await
                .map_err(|_| "Unable to scope customer query".to_string())?;
            let found =
                sqlx::query_scalar("SELECT email FROM customers WHERE id = $1 AND tenant_id = $2")
                    .bind(client_id)
                    .bind(tenant_id)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|_| "Unable to query customer email".to_string())?;
            let _ = tx.commit().await;
            found
        }
        UsageLedger::Sqlite(pool) => {
            sqlx::query_scalar("SELECT email FROM customers WHERE id = ? AND tenant_id = ?")
                .bind(client_id)
                .bind(tenant_id)
                .fetch_optional(pool)
                .await
                .map_err(|_| "Unable to query SQLite customer email".to_string())?
        }
    };
    email
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| "Customer record has no email address for invoice delivery".to_string())
}

async fn insert_draft_communication_event(
    ledger: &UsageLedger,
    id: &str,
    tenant_id: &str,
    invoice_id: &str,
    draft_content: &str,
    recipient_email: &str,
) -> Result<(), String> {
    match ledger {
        UsageLedger::Postgres(pool) => {
            let mut tx = pool
                .begin()
                .await
                .map_err(|e| format!("Failed to begin communication draft write: {e}"))?;
            ::server_common::auth_utils::set_org_context(&mut *tx, tenant_id)
                .await
                .map_err(|e| format!("Failed to scope tenant context: {e}"))?;
            sqlx::query(
                "INSERT INTO invoice_communication_events \
                 (id, tenant_id, invoice_id, status, channel, drafted_content, recipient_email) \
                 VALUES ($1, $2, $3, 'drafted', 'email', $4, $5)",
            )
            .bind(id)
            .bind(tenant_id)
            .bind(invoice_id)
            .bind(draft_content)
            .bind(recipient_email)
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("Failed to persist communication draft: {e}"))?;
            tx.commit()
                .await
                .map_err(|e| format!("Failed to commit communication draft: {e}"))?;
        }
        UsageLedger::Sqlite(pool) => {
            sqlx::query(
                "INSERT INTO invoice_communication_events \
                 (id, tenant_id, invoice_id, status, channel, drafted_content, recipient_email) \
                 VALUES (?, ?, ?, 'drafted', 'email', ?, ?)",
            )
            .bind(id)
            .bind(tenant_id)
            .bind(invoice_id)
            .bind(draft_content)
            .bind(recipient_email)
            .execute(pool)
            .await
            .map_err(|e| format!("Failed to persist SQLite communication draft: {e}"))?;
        }
    }
    Ok(())
}

async fn finalize_communication_event(
    ledger: &UsageLedger,
    id: &str,
    tenant_id: &str,
    invoice_id: &str,
    status: &str,
    provider_message_id: Option<&str>,
    error_detail: Option<&str>,
) -> Result<(), String> {
    match ledger {
        UsageLedger::Postgres(pool) => {
            let mut tx = pool
                .begin()
                .await
                .map_err(|e| format!("Failed to begin communication receipt write: {e}"))?;
            ::server_common::auth_utils::set_org_context(&mut *tx, tenant_id)
                .await
                .map_err(|e| format!("Failed to scope tenant context: {e}"))?;
            sqlx::query(
                "UPDATE invoice_communication_events \
                 SET status = $1, provider_message_id = $2, error_detail = $3, \
                     updated_at = CURRENT_TIMESTAMP \
                 WHERE id = $4 AND tenant_id = $5",
            )
            .bind(status)
            .bind(provider_message_id)
            .bind(error_detail)
            .bind(id)
            .bind(tenant_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("Failed to record communication outcome: {e}"))?;
            if status == "sent" {
                sqlx::query(
                    "UPDATE invoices SET updated_at = CURRENT_TIMESTAMP \
                     WHERE id = $1 AND tenant_id = $2",
                )
                .bind(invoice_id)
                .bind(tenant_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| format!("Failed to update invoice timestamp: {e}"))?;
            }
            tx.commit()
                .await
                .map_err(|e| format!("Failed to commit communication receipt: {e}"))?;
        }
        UsageLedger::Sqlite(pool) => {
            sqlx::query(
                "UPDATE invoice_communication_events \
                 SET status = ?, provider_message_id = ?, error_detail = ?, \
                     updated_at = CURRENT_TIMESTAMP \
                 WHERE id = ? AND tenant_id = ?",
            )
            .bind(status)
            .bind(provider_message_id)
            .bind(error_detail)
            .bind(id)
            .bind(tenant_id)
            .execute(pool)
            .await
            .map_err(|e| format!("Failed to record SQLite communication outcome: {e}"))?;
            if status == "sent" {
                sqlx::query(
                    "UPDATE invoices SET updated_at = CURRENT_TIMESTAMP \
                     WHERE id = ? AND tenant_id = ?",
                )
                .bind(invoice_id)
                .bind(tenant_id)
                .execute(pool)
                .await
                .map_err(|e| format!("Failed to update SQLite invoice timestamp: {e}"))?;
            }
        }
    }
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct InvoiceDraft {
    pub id: String,
    pub tenant_id: String,
    pub customer_id: String,
    pub project_name: String,
    pub milestone_name: String,
    pub total_amount_cents: i64,
    pub line_items: Vec<InvoiceDraftLineItem>,
    pub message_context: InvoiceMessageContext,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct InvoiceDraftLineItem {
    pub description: String,
    pub amount_cents: i64,
}

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct InvoiceMessageContext {
    pub generated_message: String,
    pub suggested_channel: String,
}

#[cfg(test)]
pub(crate) static VAULT_ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    struct VaultEnvGuard {
        _lock: tokio::sync::MutexGuard<'static, ()>,
        keys: Option<std::ffi::OsString>,
        active: Option<std::ffi::OsString>,
    }

    impl Drop for VaultEnvGuard {
        fn drop(&mut self) {
            unsafe {
                match &self.keys {
                    Some(v) => std::env::set_var("OMNISOLO_CONNECTION_KEYS", v),
                    None => std::env::remove_var("OMNISOLO_CONNECTION_KEYS"),
                }
                match &self.active {
                    Some(v) => std::env::set_var("OMNISOLO_CONNECTION_ACTIVE_KEY", v),
                    None => std::env::remove_var("OMNISOLO_CONNECTION_ACTIVE_KEY"),
                }
            }
        }
    }

    async fn with_vault_env() -> VaultEnvGuard {
        let guard = VaultEnvGuard {
            _lock: VAULT_ENV_LOCK.lock().await,
            keys: std::env::var_os("OMNISOLO_CONNECTION_KEYS"),
            active: std::env::var_os("OMNISOLO_CONNECTION_ACTIVE_KEY"),
        };
        unsafe {
            std::env::set_var(
                "OMNISOLO_CONNECTION_KEYS",
                format!(r#"{{"v1":"{}"}}"#, "33".repeat(32)),
            );
            std::env::set_var("OMNISOLO_CONNECTION_ACTIVE_KEY", "v1");
        }
        guard
    }

    async fn setup_sqlite_fixture() -> (sqlx::SqlitePool, UsageLedger) {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::raw_sql(
            "CREATE TABLE invoices (
                id TEXT PRIMARY KEY,
                tenant_id TEXT NOT NULL,
                client_id TEXT NOT NULL,
                status TEXT NOT NULL,
                payment_status TEXT,
                total_amount REAL NOT NULL,
                amount_paid_cents INTEGER DEFAULT 0,
                stripe_payment_link TEXT,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE customers (
                id TEXT PRIMARY KEY,
                tenant_id TEXT NOT NULL,
                email TEXT
            );
            CREATE TABLE invoice_communication_events (
                id TEXT PRIMARY KEY,
                tenant_id TEXT NOT NULL,
                invoice_id TEXT NOT NULL,
                status TEXT NOT NULL,
                channel TEXT NOT NULL,
                drafted_content TEXT NOT NULL,
                provider_message_id TEXT,
                recipient_email TEXT,
                error_detail TEXT,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            );",
        )
        .execute(&pool)
        .await
        .unwrap();
        let ledger = UsageLedger::Sqlite(pool.clone());
        ConnectionVault::initialize_schema(&ledger).await.unwrap();
        (pool, ledger)
    }

    async fn seed_resend_key(pool: &sqlx::SqlitePool, tenant_id: &str, api_key: &str) {
        use aes_gcm::{
            Aes256Gcm,
            aead::{Aead, AeadCore, KeyInit, OsRng, Payload},
        };
        let key_bytes = vec![0x33_u8; 32];
        let revision = uuid::Uuid::new_v4().to_string();
        let aad = serde_json::to_vec(&(tenant_id, "resend", &revision, "v1")).unwrap();
        let cipher = Aes256Gcm::new_from_slice(&key_bytes).unwrap();
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: api_key.as_bytes(),
                    aad: &aad,
                },
            )
            .unwrap();
        let encode =
            |bytes: &[u8]| -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() };
        sqlx::query(
            "INSERT INTO ohc_provider_connections \
             (tenant_id, provider, revision, state, key_id, nonce_hex, ciphertext_hex, verified_at) \
             VALUES (?, 'resend', ?, 'verified', 'v1', ?, ?, ?)",
        )
        .bind(tenant_id)
        .bind(&revision)
        .bind(encode(&nonce))
        .bind(encode(&ciphertext))
        .bind(chrono::Utc::now().timestamp())
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn reminder_fails_closed_when_resend_is_unconfigured_and_never_marks_delivered() {
        let _env = with_vault_env().await;
        let (pool, ledger) = setup_sqlite_fixture().await;
        sqlx::query("INSERT INTO customers (id, tenant_id, email) VALUES ('cust-1', 'tenant-a', 'client@example.com')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO invoices (id, tenant_id, client_id, status, payment_status, total_amount, amount_paid_cents) VALUES ('inv-1', 'tenant-a', 'cust-1', 'sent', 'unpaid', 250.0, 0)")
            .execute(&pool)
            .await
            .unwrap();

        let payload = serde_json::json!({
            "invoice_id": "inv-1",
            "generated_response": "Friendly reminder that invoice inv-1 is overdue."
        });
        let err = execute_invoice_reminder_delivery(
            "tenant-a",
            &payload,
            &ledger,
            Some("billing@owner.example"),
            Some("http://127.0.0.1:1"),
        )
        .await
        .unwrap_err();
        assert!(err.contains("Resend connection"));

        let (status, provider_id): (String, Option<String>) = sqlx::query_as(
            "SELECT status, provider_message_id FROM invoice_communication_events WHERE invoice_id = 'inv-1'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "failed");
        assert!(provider_id.is_none());
    }

    #[tokio::test]
    async fn reminder_sends_real_resend_request_and_stores_provider_message_id_without_fake_delivered()
     {
        let _env = with_vault_env().await;
        let (pool, ledger) = setup_sqlite_fixture().await;
        seed_resend_key(&pool, "tenant-a", "re_live_loopback_key_123456").await;
        sqlx::query("INSERT INTO customers (id, tenant_id, email) VALUES ('cust-2', 'tenant-a', 'buyer@example.com')")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO invoices (id, tenant_id, client_id, status, payment_status, total_amount, amount_paid_cents, stripe_payment_link) VALUES ('inv-2', 'tenant-a', 'cust-2', 'sent', 'unpaid', 500.0, 0, 'https://checkout.stripe.com/c/pay/cs_test_inv2')")
            .execute(&pool)
            .await
            .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut req = Vec::new();
            let mut buf = [0_u8; 4096];
            loop {
                let n = stream.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                req.extend_from_slice(&buf[..n]);
                if let Some(end) = req.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&req[..end]);
                    let content_len = headers
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if req.len() >= end + 4 + content_len {
                        break;
                    }
                }
            }
            let body = r#"{"id":"re_msg_verified_987"}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
            String::from_utf8(req).unwrap()
        });

        let payload = serde_json::json!({
            "invoice_id": "inv-2",
            "generated_response": "Please settle invoice inv-2 at your convenience."
        });
        let receipt = execute_invoice_reminder_delivery(
            "tenant-a",
            &payload,
            &ledger,
            Some("billing@owner.example"),
            Some(&format!("http://{addr}")),
        )
        .await
        .expect("real loopback Resend delivery should succeed")
        .expect("receipt should be returned");
        assert_eq!(receipt.provider_message_id, "re_msg_verified_987");

        let raw_req = server.await.unwrap();
        assert!(raw_req.starts_with("POST /emails "));
        assert!(
            raw_req
                .to_ascii_lowercase()
                .contains("authorization: bearer re_live_loopback_key_123456")
        );
        let (_, body_str) = raw_req.split_once("\r\n\r\n").unwrap();
        let sent_json: Value = serde_json::from_str(body_str).unwrap();
        assert_eq!(sent_json["to"][0], "buyer@example.com");
        assert_eq!(sent_json["from"], "billing@owner.example");
        assert!(sent_json["html"].as_str().unwrap().contains("cs_test_inv2"));

        let (status, provider_id): (String, Option<String>) = sqlx::query_as(
            "SELECT status, provider_message_id FROM invoice_communication_events WHERE invoice_id = 'inv-2'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(status, "sent");
        assert_ne!(status, "delivered");
        assert_eq!(provider_id.as_deref(), Some("re_msg_verified_987"));
    }
}
