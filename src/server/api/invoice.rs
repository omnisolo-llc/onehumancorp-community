use std::sync::Arc;

use ::server_common::Claims;
use ::server_omnisolo::invoice::invoice_service_server::InvoiceService;
use ::server_omnisolo::invoice::*;
use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post, put},
};
use serde::Deserialize;
use tonic::{Request, Response, Status};

use crate::hub::Hub;
use crate::rpc_error::RpcError;

pub struct InvoiceServiceImpl {
    pub hub: Arc<Hub>,
}

// PostgreSQL stores timestamps; protobuf exposes Unix seconds. A decoding error
// must not silently turn a real due date into 1970 or manufacture a current date.
fn invoice_timestamp(row: &sqlx::postgres::PgRow, column: &str) -> Result<i64, RpcError> {
    use sqlx::Row;
    row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>(column)
        .map(|value| value.map_or(0, |date| date.timestamp()))
        .map_err(|_| {
            RpcError::internal("Invoice timestamp is incompatible with the runtime schema")
        })
}

fn sqlite_invoice_timestamp(row: &sqlx::sqlite::SqliteRow, column: &str) -> Result<i64, RpcError> {
    use sqlx::Row;
    if let Ok(ts) = row.try_get::<Option<i64>, _>(column) {
        return Ok(ts.unwrap_or(0));
    }
    if let Ok(dt) = row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>(column) {
        return Ok(dt.map_or(0, |d| d.timestamp()));
    }
    if let Ok(raw) = row.try_get::<Option<String>, _>(column) {
        let Some(text) = raw else {
            return Ok(0);
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(0);
        }
        if let Ok(num) = trimmed.parse::<i64>() {
            return Ok(num);
        }
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
            return Ok(dt.timestamp());
        }
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S") {
            return Ok(naive.and_utc().timestamp());
        }
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S%.f") {
            return Ok(naive.and_utc().timestamp());
        }
    }
    Err(RpcError::internal(
        "SQLite invoice timestamp is incompatible with the runtime schema",
    ))
}

fn decode_sqlite_invoice_row(
    row: &sqlx::sqlite::SqliteRow,
    line_items: Vec<InvoiceLineItem>,
) -> Result<Invoice, RpcError> {
    use sqlx::Row;
    let total_amount: f64 = row.try_get("total_amount").unwrap_or_default();
    let total_amount_cents: i32 = row
        .try_get::<Option<i32>, _>("total_amount_cents")
        .ok()
        .flatten()
        .unwrap_or_else(|| (total_amount * 100.0).round() as i32);
    let currency: String = row
        .try_get::<Option<String>, _>("currency")
        .ok()
        .flatten()
        .unwrap_or_else(|| "USD".to_string());
    Ok(Invoice {
        id: row.try_get("id").unwrap_or_default(),
        client_id: row.try_get("client_id").unwrap_or_default(),
        client_name: row.try_get("client_name").unwrap_or_default(),
        status: row.try_get("status").unwrap_or_default(),
        due_date: sqlite_invoice_timestamp(row, "due_date")?,
        base_currency: row
            .try_get::<Option<String>, _>("base_currency")
            .ok()
            .flatten()
            .unwrap_or_else(|| currency.clone()),
        transaction_currency: row
            .try_get::<Option<String>, _>("transaction_currency")
            .ok()
            .flatten()
            .unwrap_or_else(|| currency.clone()),
        currency,
        exchange_rate: row
            .try_get::<Option<f64>, _>("exchange_rate")
            .ok()
            .flatten()
            .unwrap_or(1.0),
        total_amount,
        total_amount_cents,
        payment_status: row
            .try_get::<Option<String>, _>("payment_status")
            .ok()
            .flatten()
            .unwrap_or_else(|| "unpaid".to_string()),
        view_count: row
            .try_get::<Option<i32>, _>("view_count")
            .ok()
            .flatten()
            .unwrap_or(0),
        amount_paid_cents: row
            .try_get::<Option<i32>, _>("amount_paid_cents")
            .ok()
            .flatten()
            .unwrap_or(0),
        stripe_invoice_id: row
            .try_get::<Option<String>, _>("stripe_invoice_id")
            .ok()
            .flatten()
            .unwrap_or_default(),
        stripe_payment_link: row
            .try_get::<Option<String>, _>("stripe_payment_link")
            .ok()
            .flatten()
            .unwrap_or_default(),
        line_items,
        created_at: sqlite_invoice_timestamp(row, "created_at")?,
        updated_at: sqlite_invoice_timestamp(row, "updated_at")?,
    })
}

fn invoice_tenant(claims: &Claims) -> Result<String, StatusCode> {
    claims
        .organization_id
        .as_deref()
        .map(str::trim)
        .filter(|tenant| !tenant.is_empty())
        .map(str::to_owned)
        .ok_or(StatusCode::UNAUTHORIZED)
}

fn require_invoice_owner(claims: &Claims) -> Result<(), StatusCode> {
    if claims
        .roles
        .iter()
        .any(|role| role.eq_ignore_ascii_case("owner") || role.eq_ignore_ascii_case("admin"))
    {
        Ok(())
    } else {
        Err(StatusCode::FORBIDDEN)
    }
}

fn invoice_http_error(error: Status) -> StatusCode {
    match error.code() {
        tonic::Code::InvalidArgument => StatusCode::UNPROCESSABLE_ENTITY,
        tonic::Code::PermissionDenied => StatusCode::FORBIDDEN,
        tonic::Code::Unauthenticated => StatusCode::UNAUTHORIZED,
        tonic::Code::NotFound => StatusCode::NOT_FOUND,
        tonic::Code::FailedPrecondition => StatusCode::CONFLICT,
        tonic::Code::Unimplemented => StatusCode::NOT_IMPLEMENTED,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    }
}

fn validate_manual_invoice_status(status: &str) -> Result<(), RpcError> {
    // This endpoint manages unpaid local drafts, never payment or delivery.
    if matches!(status, "draft" | "void") {
        Ok(())
    } else {
        Err(RpcError::failed_precondition(
            "Payment and delivery status require verified evidence",
        ))
    }
}

fn validate_invoice_items(items: &[InvoiceLineItem]) -> Result<i32, RpcError> {
    if items.is_empty() || items.len() > 100 {
        return Err(RpcError::invalid_argument(
            "One to 100 invoice line items are required",
        ));
    }
    let mut total = 0_i64;
    for item in items {
        if item.description.trim().is_empty()
            || item.description.len() > 4000
            || item.quantity <= 0
            || !item.unit_price.is_finite()
            || item.unit_price < 0.0
            || item.unit_price > 999_999.99
            || !item.amount.is_finite()
            || item.amount < 0.0
        {
            return Err(RpcError::invalid_argument("Invalid invoice line item"));
        }
        let unit = (item.unit_price * 100.0).round();
        if (item.unit_price * 100.0 - unit).abs() > 0.00001 {
            return Err(RpcError::invalid_argument(
                "Unit price must use currency minor units",
            ));
        }
        let amount = (unit as i64)
            .checked_mul(i64::from(item.quantity))
            .ok_or_else(|| RpcError::invalid_argument("Invoice amount overflow"))?;
        if (item.amount * 100.0 - amount as f64).abs() > 0.00001 {
            return Err(RpcError::invalid_argument(
                "Line amount must equal quantity times unit price",
            ));
        }
        total = total
            .checked_add(amount)
            .filter(|value| *value <= 99_999_999)
            .ok_or_else(|| RpcError::invalid_argument("Invoice total exceeds supported amount"))?;
    }
    i32::try_from(total).map_err(|_| RpcError::invalid_argument("Invoice total overflow"))
}

#[tonic::async_trait]
impl InvoiceService for InvoiceServiceImpl {
    async fn create_invoice(
        &self,
        request: Request<CreateInvoiceRequest>,
    ) -> Result<Response<Invoice>, Status> {
        let req = request.into_inner();
        if req.tenant_id.trim().is_empty()
            || req.client_id.trim().is_empty()
            || req.client_id.len() > 255
            || req.client_name.trim().is_empty()
            || req.client_name.len() > 500
            || req.due_date <= 0
        {
            return Err(Status::invalid_argument(
                "Valid tenant, client and due date are required",
            ));
        }
        if !matches!(
            req.currency.as_str(),
            "USD" | "EUR" | "GBP" | "CAD" | "AUD" | "NZD" | "CHF" | "SGD" | "HKD"
        ) || (!req.base_currency.is_empty() && req.base_currency != req.currency)
            || (!req.transaction_currency.is_empty() && req.transaction_currency != req.currency)
            || !req.exchange_rate.is_finite()
            || !(req.exchange_rate == 0.0 || req.exchange_rate == 1.0)
        {
            return Err(Status::invalid_argument(
                "Use a supported two-decimal currency without unverified conversion",
            ));
        }
        let total_cents = validate_invoice_items(&req.line_items)?;
        let due_date = chrono::DateTime::from_timestamp(req.due_date, 0)
            .ok_or_else(|| Status::invalid_argument("Invoice due date is out of range"))?;

        let invoice_id = uuid::Uuid::new_v4().to_string();
        let total_amount = f64::from(total_cents) / 100.0;
        let status = "draft".to_string();

        // This operation creates a local draft, not a provider checkout session.
        // Empty means payment has not been configured; never invent a payable URL.
        let stripe_payment_link = String::new();

        let base_currency = if req.base_currency.is_empty() {
            "USD".to_string()
        } else {
            req.base_currency.clone()
        };
        let transaction_currency = if req.transaction_currency.is_empty() {
            req.currency.clone()
        } else {
            req.transaction_currency.clone()
        };
        let exchange_rate = if req.exchange_rate == 0.0 {
            1.0
        } else {
            req.exchange_rate
        };

        let db = resolve_hub_db(&self.hub);
        let mut saved_items = Vec::new();
        match &db.store {
            crate::db::DbStore::Sqlite(sqlite_pool) => {
                let mut tx = sqlite_pool
                    .begin_with("BEGIN IMMEDIATE")
                    .await
                    .map_err(|e| Status::internal(e.to_string()))?;

                sqlx::query(
                    "INSERT INTO invoices \
                     (id, tenant_id, client_id, client_name, status, due_date, currency, base_currency, transaction_currency, exchange_rate, total_amount, stripe_payment_link, total_amount_cents, amount_paid_cents, payment_status, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, 'draft', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
                )
                .bind(&invoice_id)
                .bind(&req.tenant_id)
                .bind(&req.client_id)
                .bind(&req.client_name)
                .bind(&status)
                .bind(req.due_date)
                .bind(&req.currency)
                .bind(&base_currency)
                .bind(&transaction_currency)
                .bind(exchange_rate)
                .bind(total_amount)
                .bind(&stripe_payment_link)
                .bind(total_cents)
                .execute(&mut *tx)
                .await
                .map_err(|e| Status::internal(e.to_string()))?;

                for item in req.line_items {
                    let item_id = uuid::Uuid::new_v4().to_string();
                    sqlx::query(
                        "INSERT INTO invoice_line_items \
                         (id, tenant_id, invoice_id, description, quantity, unit_price, amount) \
                         VALUES (?, ?, ?, ?, ?, ?, ?)",
                    )
                    .bind(&item_id)
                    .bind(&req.tenant_id)
                    .bind(&invoice_id)
                    .bind(&item.description)
                    .bind(item.quantity)
                    .bind(item.unit_price)
                    .bind(item.amount)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| Status::internal(e.to_string()))?;

                    saved_items.push(InvoiceLineItem {
                        id: item_id,
                        invoice_id: invoice_id.clone(),
                        description: item.description,
                        quantity: item.quantity,
                        unit_price: item.unit_price,
                        amount: item.amount,
                    });
                }

                tx.commit()
                    .await
                    .map_err(|e| Status::internal(e.to_string()))?;
            }
            crate::db::DbStore::Postgres => {
                let pool = &self.hub.pool;
                let mut tx = pool
                    .begin()
                    .await
                    .map_err(|e| Status::internal(e.to_string()))?;

                // Set tenant context for RLS
                ::server_common::auth_utils::set_org_context(&mut *tx, &req.tenant_id)
                    .await
                    .map_err(|e| Status::internal(e.to_string()))?;

                sqlx::query(
                    "INSERT INTO invoices (id, tenant_id, client_id, client_name, status, due_date, currency, base_currency, transaction_currency, exchange_rate, total_amount, stripe_payment_link, total_amount_cents, amount_paid_cents, payment_status)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, 0, 'draft')",
                )
                .bind(&invoice_id)
                .bind(&req.tenant_id)
                .bind(&req.client_id)
                .bind(&req.client_name)
                .bind(&status)
                .bind(due_date)
                .bind(&req.currency)
                .bind(&base_currency)
                .bind(&transaction_currency)
                .bind(exchange_rate)
                .bind(total_amount)
                .bind(&stripe_payment_link)
                .bind(total_cents)
                .execute(&mut *tx)
                .await
                .map_err(|e| Status::internal(e.to_string()))?;

                for item in req.line_items {
                    let item_id = uuid::Uuid::new_v4().to_string();
                    sqlx::query(
                        "INSERT INTO invoice_line_items (id, tenant_id, invoice_id, description, quantity, unit_price, amount)
                         VALUES ($1, $2, $3, $4, $5, $6, $7)",
                    )
                    .bind(&item_id)
                    .bind(&req.tenant_id)
                    .bind(&invoice_id)
                    .bind(&item.description)
                    .bind(item.quantity)
                    .bind(item.unit_price)
                    .bind(item.amount)
                    .execute(&mut *tx)
                    .await
                    .map_err(|e| Status::internal(e.to_string()))?;

                    saved_items.push(InvoiceLineItem {
                        id: item_id,
                        invoice_id: invoice_id.clone(),
                        description: item.description,
                        quantity: item.quantity,
                        unit_price: item.unit_price,
                        amount: item.amount,
                    });
                }

                tx.commit()
                    .await
                    .map_err(|e| Status::internal(e.to_string()))?;
            }
        }

        Ok(Response::new(Invoice {
            id: invoice_id,
            client_id: req.client_id,
            client_name: req.client_name,
            status,
            due_date: req.due_date,
            currency: req.currency,
            base_currency,
            transaction_currency,
            exchange_rate,
            total_amount,
            total_amount_cents: total_cents,
            payment_status: "draft".to_string(),
            view_count: 0,
            amount_paid_cents: 0,
            stripe_invoice_id: "".to_string(),
            stripe_payment_link,
            line_items: saved_items,
            created_at: chrono::Utc::now().timestamp(),
            updated_at: chrono::Utc::now().timestamp(),
        }))
    }

    async fn get_invoice(
        &self,
        request: Request<GetInvoiceRequest>,
    ) -> Result<Response<Invoice>, Status> {
        let req = request.into_inner();
        let db = resolve_hub_db(&self.hub);
        if let crate::db::DbStore::Sqlite(sqlite_pool) = &db.store {
            use sqlx::Row;
            let row = sqlx::query("SELECT * FROM invoices WHERE id = ? AND tenant_id = ?")
                .bind(&req.invoice_id)
                .bind(&req.tenant_id)
                .fetch_optional(sqlite_pool)
                .await
                .map_err(|e| Status::internal(e.to_string()))?
                .ok_or_else(|| Status::not_found("Invoice not found"))?;
            let item_rows = sqlx::query(
                "SELECT id, invoice_id, description, quantity, unit_price, amount \
                 FROM invoice_line_items WHERE invoice_id = ? AND tenant_id = ?",
            )
            .bind(&req.invoice_id)
            .bind(&req.tenant_id)
            .fetch_all(sqlite_pool)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
            let line_items = item_rows
                .into_iter()
                .map(|item_row| InvoiceLineItem {
                    id: item_row.try_get("id").unwrap_or_default(),
                    invoice_id: item_row.try_get("invoice_id").unwrap_or_default(),
                    description: item_row.try_get("description").unwrap_or_default(),
                    quantity: item_row.try_get("quantity").unwrap_or_default(),
                    unit_price: item_row.try_get("unit_price").unwrap_or_default(),
                    amount: item_row.try_get("amount").unwrap_or_default(),
                })
                .collect();
            let invoice = decode_sqlite_invoice_row(&row, line_items)?;
            return Ok(Response::new(invoice));
        }

        let pool = &self.hub.pool;
        let mut tx = pool
            .begin()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        // Set tenant context for RLS
        ::server_common::auth_utils::set_org_context(&mut *tx, &req.tenant_id)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        use sqlx::Row;

        let rows = sqlx::query(
            "SELECT i.*,
                    li.id as li_id,
                    li.description as li_description,
                    li.quantity as li_quantity,
                    li.unit_price as li_unit_price,
                    li.amount as li_amount
             FROM invoices i
             LEFT JOIN invoice_line_items li ON i.id = li.invoice_id AND i.tenant_id = li.tenant_id
             WHERE i.id = $1 AND i.tenant_id = $2",
        )
        .bind(&req.invoice_id)
        .bind(&req.tenant_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| Status::internal(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        if rows.is_empty() {
            return Err(Status::not_found("Invoice not found"));
        }

        let first_row_id: String = rows[0].try_get("id").unwrap_or_default();
        let first_row_client_id: String = rows[0].try_get("client_id").unwrap_or_default();
        let first_row_client_name: String = rows[0].try_get("client_name").unwrap_or_default();
        let first_row_status: String = rows[0].try_get("status").unwrap_or_default();
        let first_row_due_date = invoice_timestamp(&rows[0], "due_date")?;
        let created_at = invoice_timestamp(&rows[0], "created_at")?;
        let updated_at = invoice_timestamp(&rows[0], "updated_at")?;
        let first_row_currency: String = rows[0].try_get("currency").unwrap_or_default();
        let first_row_base_currency: String = rows[0].try_get("base_currency").unwrap_or_default();
        let first_row_transaction_currency: String =
            rows[0].try_get("transaction_currency").unwrap_or_default();
        let first_row_exchange_rate: f64 = rows[0].try_get("exchange_rate").unwrap_or_default();
        let first_row_total_amount: f64 = rows[0].try_get("total_amount").unwrap_or_default();
        let first_row_total_amount_cents: i32 =
            rows[0].try_get("total_amount_cents").unwrap_or_default();
        let first_row_payment_status: String =
            rows[0].try_get("payment_status").unwrap_or_default();
        let first_row_view_count: i32 = rows[0].try_get("view_count").unwrap_or_default();
        let first_row_amount_paid_cents: i32 =
            rows[0].try_get("amount_paid_cents").unwrap_or_default();
        let first_row_stripe_invoice_id: String =
            rows[0].try_get("stripe_invoice_id").unwrap_or_default();
        let first_row_stripe_payment_link: String =
            rows[0].try_get("stripe_payment_link").unwrap_or_default();

        let mut line_items = Vec::new();
        for row in rows {
            if row.try_get::<String, _>("li_id").is_ok() {
                line_items.push(InvoiceLineItem {
                    id: row.try_get("li_id").unwrap_or_default(),
                    invoice_id: req.invoice_id.clone(),
                    description: row.try_get("li_description").unwrap_or_default(),
                    quantity: row.try_get("li_quantity").unwrap_or_default(),
                    unit_price: row.try_get("li_unit_price").unwrap_or_default(),
                    amount: row.try_get("li_amount").unwrap_or_default(),
                });
            }
        }

        let invoice = Invoice {
            id: first_row_id,
            client_id: first_row_client_id,
            client_name: first_row_client_name,
            status: first_row_status,
            due_date: first_row_due_date,
            currency: first_row_currency,
            base_currency: first_row_base_currency,
            transaction_currency: first_row_transaction_currency,
            exchange_rate: first_row_exchange_rate,
            total_amount: first_row_total_amount,
            total_amount_cents: first_row_total_amount_cents,
            payment_status: first_row_payment_status,
            view_count: first_row_view_count,
            amount_paid_cents: first_row_amount_paid_cents,
            stripe_invoice_id: first_row_stripe_invoice_id,
            stripe_payment_link: first_row_stripe_payment_link,
            line_items,
            created_at,
            updated_at,
        };

        Ok(Response::new(invoice))
    }

    async fn list_invoices(
        &self,
        request: Request<ListInvoicesRequest>,
    ) -> Result<Response<ListInvoicesResponse>, Status> {
        let req = request.into_inner();
        let db = resolve_hub_db(&self.hub);
        if let crate::db::DbStore::Sqlite(sqlite_pool) = &db.store {
            let rows =
                sqlx::query("SELECT * FROM invoices WHERE tenant_id = ? ORDER BY created_at DESC")
                    .bind(&req.tenant_id)
                    .fetch_all(sqlite_pool)
                    .await
                    .map_err(|e| Status::internal(e.to_string()))?;
            let mut invoices = Vec::with_capacity(rows.len());
            for row in &rows {
                invoices.push(decode_sqlite_invoice_row(row, vec![])?);
            }
            return Ok(Response::new(ListInvoicesResponse { invoices }));
        }

        let pool = &self.hub.pool;
        let mut tx = pool
            .begin()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        // Set tenant context for RLS
        ::server_common::auth_utils::set_org_context(&mut *tx, &req.tenant_id)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        use sqlx::Row;

        let rows =
            sqlx::query("SELECT * FROM invoices WHERE tenant_id = $1 ORDER BY created_at DESC")
                .bind(&req.tenant_id)
                .fetch_all(&mut *tx)
                .await
                .map_err(|e| Status::internal(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let mut invoices = Vec::new();
        for row in rows {
            invoices.push(Invoice {
                id: row.try_get("id").unwrap_or_default(),
                client_id: row.try_get("client_id").unwrap_or_default(),
                client_name: row.try_get("client_name").unwrap_or_default(),
                status: row.try_get("status").unwrap_or_default(),
                due_date: invoice_timestamp(&row, "due_date")?,
                currency: row.try_get("currency").unwrap_or_default(),
                base_currency: row.try_get("base_currency").unwrap_or_default(),
                transaction_currency: row.try_get("transaction_currency").unwrap_or_default(),
                exchange_rate: row.try_get("exchange_rate").unwrap_or_default(),
                total_amount: row.try_get("total_amount").unwrap_or_default(),
                total_amount_cents: row.try_get("total_amount_cents").unwrap_or_default(),
                payment_status: row.try_get("payment_status").unwrap_or_default(),
                view_count: row.try_get("view_count").unwrap_or_default(),
                amount_paid_cents: row.try_get("amount_paid_cents").unwrap_or_default(),
                stripe_invoice_id: row.try_get("stripe_invoice_id").unwrap_or_default(),
                stripe_payment_link: row.try_get("stripe_payment_link").unwrap_or_default(),
                line_items: vec![],
                created_at: invoice_timestamp(&row, "created_at")?,
                updated_at: invoice_timestamp(&row, "updated_at")?,
            });
        }

        Ok(Response::new(ListInvoicesResponse { invoices }))
    }

    async fn update_invoice_status(
        &self,
        request: Request<UpdateInvoiceStatusRequest>,
    ) -> Result<Response<Invoice>, Status> {
        let req = request.into_inner();
        validate_manual_invoice_status(&req.status)?;

        let db = resolve_hub_db(&self.hub);
        if let crate::db::DbStore::Sqlite(sqlite_pool) = &db.store {
            use sqlx::Row;
            let mut tx = sqlite_pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(|e| Status::internal(e.to_string()))?;
            let changed = sqlx::query(
                "UPDATE invoices SET status = ?, updated_at = CURRENT_TIMESTAMP \
                 WHERE id = ? AND tenant_id = ? AND status = 'draft' AND COALESCE(amount_paid_cents, 0) = 0",
            )
            .bind(&req.status)
            .bind(&req.invoice_id)
            .bind(&req.tenant_id)
            .execute(&mut *tx)
            .await
            .map_err(|_| Status::unavailable("Invoice update unavailable"))?
            .rows_affected();
            if changed != 1 {
                return Err(Status::failed_precondition(
                    "Only an existing unpaid draft can be changed",
                ));
            }
            let row = sqlx::query("SELECT * FROM invoices WHERE id = ? AND tenant_id = ?")
                .bind(&req.invoice_id)
                .bind(&req.tenant_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| Status::internal(e.to_string()))?;
            let items_rows = sqlx::query(
                "SELECT * FROM invoice_line_items WHERE invoice_id = ? AND tenant_id = ?",
            )
            .bind(&req.invoice_id)
            .bind(&req.tenant_id)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
            tx.commit()
                .await
                .map_err(|e| Status::internal(e.to_string()))?;
            let line_items = items_rows
                .into_iter()
                .map(|item_row| InvoiceLineItem {
                    id: item_row.try_get("id").unwrap_or_default(),
                    invoice_id: item_row.try_get("invoice_id").unwrap_or_default(),
                    description: item_row.try_get("description").unwrap_or_default(),
                    quantity: item_row.try_get("quantity").unwrap_or_default(),
                    unit_price: item_row.try_get("unit_price").unwrap_or_default(),
                    amount: item_row.try_get("amount").unwrap_or_default(),
                })
                .collect();
            let invoice = decode_sqlite_invoice_row(&row, line_items)?;
            return Ok(Response::new(invoice));
        }

        let pool = &self.hub.pool;
        let mut tx = pool
            .begin()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        // Set tenant context for RLS
        ::server_common::auth_utils::set_org_context(&mut *tx, &req.tenant_id)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let changed = sqlx::query(
            "UPDATE invoices SET status = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2 AND tenant_id = $3 AND status = 'draft' AND COALESCE(amount_paid_cents, 0) = 0",
        )
        .bind(&req.status).bind(&req.invoice_id).bind(&req.tenant_id)
        .execute(&mut *tx).await.map_err(|_| Status::unavailable("Invoice update unavailable"))?
        .rows_affected();
        if changed != 1 {
            return Err(Status::failed_precondition(
                "Only an existing unpaid draft can be changed",
            ));
        }

        use sqlx::Row;

        let row = sqlx::query("SELECT * FROM invoices WHERE id = $1 AND tenant_id = $2")
            .bind(&req.invoice_id)
            .bind(&req.tenant_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let items_rows = sqlx::query(
            "SELECT * FROM invoice_line_items WHERE invoice_id = $1 AND tenant_id = $2",
        )
        .bind(&req.invoice_id)
        .bind(&req.tenant_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| Status::internal(e.to_string()))?;

        tx.commit()
            .await
            .map_err(|e| Status::internal(e.to_string()))?;

        let mut line_items = Vec::new();
        for item_row in items_rows {
            line_items.push(InvoiceLineItem {
                id: item_row.try_get("id").unwrap_or_default(),
                invoice_id: item_row.try_get("invoice_id").unwrap_or_default(),
                description: item_row.try_get("description").unwrap_or_default(),
                quantity: item_row.try_get("quantity").unwrap_or_default(),
                unit_price: item_row.try_get("unit_price").unwrap_or_default(),
                amount: item_row.try_get("amount").unwrap_or_default(),
            });
        }

        let invoice = Invoice {
            id: row.try_get("id").unwrap_or_default(),
            client_id: row.try_get("client_id").unwrap_or_default(),
            client_name: row.try_get("client_name").unwrap_or_default(),
            status: row.try_get("status").unwrap_or_default(),
            due_date: invoice_timestamp(&row, "due_date")?,
            currency: row.try_get("currency").unwrap_or_default(),
            base_currency: row.try_get("base_currency").unwrap_or_default(),
            transaction_currency: row.try_get("transaction_currency").unwrap_or_default(),
            exchange_rate: row.try_get("exchange_rate").unwrap_or_default(),
            total_amount: row.try_get("total_amount").unwrap_or_default(),
            total_amount_cents: row.try_get("total_amount_cents").unwrap_or_default(),
            payment_status: row.try_get("payment_status").unwrap_or_default(),
            view_count: row.try_get("view_count").unwrap_or_default(),
            amount_paid_cents: row.try_get("amount_paid_cents").unwrap_or_default(),
            stripe_invoice_id: row.try_get("stripe_invoice_id").unwrap_or_default(),
            stripe_payment_link: row.try_get("stripe_payment_link").unwrap_or_default(),
            line_items,
            created_at: invoice_timestamp(&row, "created_at")?,
            updated_at: invoice_timestamp(&row, "updated_at")?,
        };

        Ok(Response::new(invoice))
    }

    async fn draft_invoice_from_context(
        &self,
        _request: Request<DraftInvoiceFromContextRequest>,
    ) -> Result<Response<DraftInvoiceFromContextResponse>, Status> {
        // Free-form context cannot authorize fabricated hours, rates or an FX
        // conversion. The supported creation path requires explicit line items.
        Err(Status::failed_precondition(
            "Provide owner-approved line items through invoice creation; no invoice was generated from unverified context",
        ))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateInvoiceHttp {
    pub client_id: String,
    pub client_name: String,
    pub due_date: i64,
    pub currency: String,
    pub line_items: Vec<CreateInvoiceLineHttp>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateInvoiceLineHttp {
    pub description: String,
    pub quantity: i32,
    pub unit_price: f64,
}

fn http_invoice_items(
    items: Vec<CreateInvoiceLineHttp>,
) -> Result<Vec<InvoiceLineItem>, StatusCode> {
    let mapped: Vec<_> = items
        .into_iter()
        .map(|item| InvoiceLineItem {
            id: String::new(),
            invoice_id: String::new(),
            description: item.description,
            quantity: item.quantity,
            unit_price: item.unit_price,
            amount: item.unit_price * f64::from(item.quantity),
        })
        .collect();
    validate_invoice_items(&mapped).map_err(|error| invoice_http_error(error.into()))?;
    Ok(mapped)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateInvoiceStatusHttp {
    pub status: String,
}

#[derive(Deserialize)]
pub struct GenerateInvoiceHttp {
    pub job_id: String,
    pub customer_id: String,
}

async fn generate_invoice_handler(
    State(_hub): State<Arc<Hub>>,
    Extension(claims): Extension<Claims>,
    Json(payload): Json<GenerateInvoiceHttp>,
) -> Result<impl IntoResponse, StatusCode> {
    invoice_tenant(&claims)?;
    require_invoice_owner(&claims)?;
    if payload.job_id.trim().is_empty()
        || payload.customer_id.trim().is_empty()
        || payload.job_id.len() > 255
        || payload.customer_id.len() > 255
    {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    // Keep an offline job pending until actual authorized billable work exists.
    // Acknowledging it as invoiced would lose the client's unsynchronized work.
    Ok((
        StatusCode::NOT_IMPLEMENTED,
        Json(serde_json::json!({
            "success": false, "code": "approved_line_items_required",
            "message": "Create an invoice with approved line items; no invoice was generated for this job."
        })),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SendInvoiceHttp {
    #[serde(default)]
    pub recipient_email: Option<String>,
    #[serde(default)]
    pub from_email: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

fn resolve_hub_db(hub: &Hub) -> crate::db::DB {
    match hub.usage_ledger() {
        Some(::server_harness::middleware::usage_ledger::UsageLedger::Sqlite(sqlite)) => {
            crate::db::DB {
                pool: hub.pool.clone(),
                store: crate::db::DbStore::Sqlite(sqlite),
            }
        }
        _ => crate::db::db_for_pg_pool(&hub.pool),
    }
}

fn invoice_checkout_operation_id(
    tenant_id: &str,
    invoice_id: &str,
    amount_due_cents: i64,
) -> String {
    use sha2::{Digest, Sha256};
    format!(
        "invoice:{:x}",
        Sha256::digest(format!("{tenant_id}:{invoice_id}:{amount_due_cents}"))
    )
}

fn invoice_checkout_metadata(
    tenant_id: &str,
    invoice_id: &str,
) -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::from([
        ("checkout_kind".to_string(), "invoice_payment".to_string()),
        ("invoice_id".to_string(), invoice_id.to_string()),
        ("tenant_id".to_string(), tenant_id.to_string()),
    ])
}

struct PayableInvoice {
    id: String,
    client_id: String,
    client_name: String,
    status: String,
    currency: String,
    total_amount_cents: i64,
    amount_paid_cents: i64,
    stripe_payment_link: Option<String>,
    stripe_checkout_session_id: Option<String>,
}

impl PayableInvoice {
    fn amount_due_cents(&self) -> Result<i64, StatusCode> {
        let status = self.status.to_ascii_lowercase();
        if matches!(
            status.as_str(),
            "void" | "paid" | "refunded" | "canceled" | "cancelled"
        ) {
            return Err(StatusCode::CONFLICT);
        }
        let due = self.total_amount_cents - self.amount_paid_cents;
        if due <= 0 || due > 99_999_999 {
            return Err(StatusCode::CONFLICT);
        }
        Ok(due)
    }
}

async fn load_payable_invoice(
    db: &crate::db::DB,
    tenant_id: &str,
    invoice_id: &str,
) -> Result<PayableInvoice, StatusCode> {
    use sqlx::Row;
    match &db.store {
        crate::db::DbStore::Sqlite(sqlite_pool) => {
            let row = sqlx::query(
                "SELECT id, COALESCE(client_id, '') AS client_id, \
                 COALESCE(client_name, 'Client') AS client_name, status, \
                 COALESCE(currency, 'USD') AS currency, total_amount, total_amount_cents, \
                 COALESCE(amount_paid_cents, 0) AS amount_paid_cents, \
                 stripe_payment_link, stripe_checkout_session_id \
                 FROM invoices WHERE id = ? AND tenant_id = ?",
            )
            .bind(invoice_id)
            .bind(tenant_id)
            .fetch_optional(sqlite_pool)
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
            .ok_or(StatusCode::NOT_FOUND)?;

            let total_f64: f64 = row.try_get("total_amount").unwrap_or(0.0);
            let total_cents = row
                .try_get::<Option<i32>, _>("total_amount_cents")
                .ok()
                .flatten()
                .map(i64::from)
                .unwrap_or_else(|| (total_f64 * 100.0).round() as i64);
            let paid_cents = i64::from(row.try_get::<i32, _>("amount_paid_cents").unwrap_or(0));
            Ok(PayableInvoice {
                id: row.try_get("id").unwrap_or_default(),
                client_id: row.try_get("client_id").unwrap_or_default(),
                client_name: row.try_get("client_name").unwrap_or_default(),
                status: row.try_get("status").unwrap_or_default(),
                currency: row
                    .try_get("currency")
                    .unwrap_or_else(|_| "USD".to_string()),
                total_amount_cents: total_cents,
                amount_paid_cents: paid_cents,
                stripe_payment_link: row.try_get("stripe_payment_link").ok().flatten(),
                stripe_checkout_session_id: row
                    .try_get("stripe_checkout_session_id")
                    .ok()
                    .flatten(),
            })
        }
        crate::db::DbStore::Postgres => {
            let mut tx = db
                .pool
                .begin()
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            ::server_common::auth_utils::set_org_context(&mut *tx, tenant_id)
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            let row = sqlx::query(
                "SELECT id, COALESCE(client_id, customer_id, '') AS client_id, \
                 COALESCE(client_name, 'Client') AS client_name, status, \
                 COALESCE(currency, 'USD') AS currency, total_amount, total_amount_cents, \
                 COALESCE(amount_paid_cents, 0) AS amount_paid_cents, \
                 stripe_payment_link, stripe_checkout_session_id \
                 FROM invoices WHERE id = $1 AND tenant_id = $2",
            )
            .bind(invoice_id)
            .bind(tenant_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
            .ok_or(StatusCode::NOT_FOUND)?;
            tx.commit()
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;

            let total_f64: f64 = row.try_get("total_amount").unwrap_or(0.0);
            let total_cents = row
                .try_get::<Option<i32>, _>("total_amount_cents")
                .ok()
                .flatten()
                .map(i64::from)
                .unwrap_or_else(|| (total_f64 * 100.0).round() as i64);
            let paid_cents = i64::from(row.try_get::<i32, _>("amount_paid_cents").unwrap_or(0));
            Ok(PayableInvoice {
                id: row.try_get("id").unwrap_or_default(),
                client_id: row.try_get("client_id").unwrap_or_default(),
                client_name: row.try_get("client_name").unwrap_or_default(),
                status: row.try_get("status").unwrap_or_default(),
                currency: row
                    .try_get("currency")
                    .unwrap_or_else(|_| "USD".to_string()),
                total_amount_cents: total_cents,
                amount_paid_cents: paid_cents,
                stripe_payment_link: row.try_get("stripe_payment_link").ok().flatten(),
                stripe_checkout_session_id: row
                    .try_get("stripe_checkout_session_id")
                    .ok()
                    .flatten(),
            })
        }
    }
}

async fn ensure_invoice_checkout_session(
    hub: &Hub,
    tenant_id: &str,
    invoice: &PayableInvoice,
) -> Result<(String, String), StatusCode> {
    if let (Some(session_id), Some(url)) = (
        invoice
            .stripe_checkout_session_id
            .as_deref()
            .filter(|s| crate::integrations::stripe::safe_checkout::valid_provider_id(s, "cs_")),
        invoice
            .stripe_payment_link
            .as_deref()
            .filter(|u| u.starts_with("https://checkout.stripe.com/")),
    ) {
        return Ok((session_id.to_string(), url.to_string()));
    }

    let due_cents = invoice.amount_due_cents()?;
    let db_view = resolve_hub_db(hub);
    let stripe_key = crate::api::tool_integrations::stripe_key_for_tenant(&db_view, tenant_id)
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let stripe_client = crate::integrations::stripe::client::StripeClient::new(stripe_key);
    stripe_client
        .require_api_key()
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;

    let operation_id = invoice_checkout_operation_id(tenant_id, &invoice.id, due_cents);
    let metadata = invoice_checkout_metadata(tenant_id, &invoice.id);
    let currency = invoice.currency.to_ascii_lowercase();
    let receipt = stripe_client
        .create_checkout_session_with_metadata(
            crate::integrations::stripe::safe_checkout::CheckoutRequest {
                name: &format!("Invoice #{}", invoice.id),
                reference: &invoice.id,
                amount_cents: due_cents,
                interval: None,
                product: None,
                currency: &currency,
                operation_id: &operation_id,
            },
            &metadata,
            false,
        )
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;

    Ok((receipt.id, receipt.url))
}

async fn create_invoice_checkout_session_handler(
    State(hub): State<Arc<Hub>>,
    Extension(claims): Extension<Claims>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    let tenant_id = invoice_tenant(&claims)?;
    require_invoice_owner(&claims)?;

    let db = resolve_hub_db(&hub);
    let invoice = load_payable_invoice(&db, &tenant_id, &id).await?;
    let _ = invoice.amount_due_cents()?;
    let (session_id, checkout_url) =
        ensure_invoice_checkout_session(&hub, &tenant_id, &invoice).await?;

    let next_status = if invoice.status.eq_ignore_ascii_case("draft") {
        "open"
    } else {
        invoice.status.as_str()
    };
    let next_payment_status = if invoice.amount_paid_cents > 0 {
        "partially_paid"
    } else {
        "unpaid"
    };
    match &db.store {
        crate::db::DbStore::Sqlite(sqlite_pool) => {
            sqlx::query(
                "UPDATE invoices SET stripe_payment_link = ?, stripe_checkout_session_id = ?, \
                 status = ?, payment_status = ?, updated_at = CURRENT_TIMESTAMP \
                 WHERE id = ? AND tenant_id = ?",
            )
            .bind(&checkout_url)
            .bind(&session_id)
            .bind(next_status)
            .bind(next_payment_status)
            .bind(&invoice.id)
            .bind(&tenant_id)
            .execute(sqlite_pool)
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        }
        crate::db::DbStore::Postgres => {
            let mut tx = hub
                .pool
                .begin()
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            ::server_common::auth_utils::set_org_context(&mut *tx, &tenant_id)
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            sqlx::query(
                "UPDATE invoices SET stripe_payment_link = $1, stripe_checkout_session_id = $2, \
                 status = $3, payment_status = $4, updated_at = CURRENT_TIMESTAMP \
                 WHERE id = $5 AND tenant_id = $6",
            )
            .bind(&checkout_url)
            .bind(&session_id)
            .bind(next_status)
            .bind(next_payment_status)
            .bind(&invoice.id)
            .bind(&tenant_id)
            .execute(&mut *tx)
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            tx.commit()
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        }
    }

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "success": true,
            "invoice_id": invoice.id,
            "checkout_session_id": session_id,
            "checkout_url": checkout_url,
            "status": next_status,
            "payment_status": next_payment_status,
        })),
    ))
}

async fn send_invoice_handler(
    State(hub): State<Arc<Hub>>,
    Extension(claims): Extension<Claims>,
    Path(id): Path<String>,
    payload: Option<Json<SendInvoiceHttp>>,
) -> Result<impl IntoResponse, StatusCode> {
    let tenant_id = invoice_tenant(&claims)?;
    require_invoice_owner(&claims)?;
    let body = payload.map(|Json(v)| v).unwrap_or(SendInvoiceHttp {
        recipient_email: None,
        from_email: None,
        subject: None,
        message: None,
    });

    let db = resolve_hub_db(&hub);
    let invoice = load_payable_invoice(&db, &tenant_id, &id).await?;
    let due_cents = invoice.amount_due_cents()?;

    let recipient_email = if let Some(email) = body
        .recipient_email
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        email.to_string()
    } else {
        let email: Option<String> = match &db.store {
            crate::db::DbStore::Sqlite(sqlite_pool) => {
                sqlx::query_scalar("SELECT email FROM customers WHERE id = ? AND tenant_id = ?")
                    .bind(&invoice.client_id)
                    .bind(&tenant_id)
                    .fetch_optional(sqlite_pool)
                    .await
                    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
            }
            crate::db::DbStore::Postgres => {
                let mut tx = hub
                    .pool
                    .begin()
                    .await
                    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
                ::server_common::auth_utils::set_org_context(&mut *tx, &tenant_id)
                    .await
                    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
                let email = sqlx::query_scalar(
                    "SELECT email FROM customers WHERE id = $1 AND tenant_id = $2",
                )
                .bind(&invoice.client_id)
                .bind(&tenant_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
                let _ = tx.commit().await;
                email
            }
        };
        email
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty())
            .ok_or(StatusCode::UNPROCESSABLE_ENTITY)?
    };
    crate::domain::invoice::validate_email_address(&recipient_email, "recipient")
        .map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;

    // 1. Create or reuse a real tenant Stripe checkout session (fails closed if unconfigured).
    let (session_id, checkout_url) =
        ensure_invoice_checkout_session(&hub, &tenant_id, &invoice).await?;

    // Persist the verified checkout session immediately without claiming 'sent'.
    match &db.store {
        crate::db::DbStore::Sqlite(sqlite_pool) => {
            sqlx::query(
                "UPDATE invoices SET stripe_payment_link = ?, stripe_checkout_session_id = ?, \
                 updated_at = CURRENT_TIMESTAMP WHERE id = ? AND tenant_id = ?",
            )
            .bind(&checkout_url)
            .bind(&session_id)
            .bind(&invoice.id)
            .bind(&tenant_id)
            .execute(sqlite_pool)
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        }
        crate::db::DbStore::Postgres => {
            let mut tx = hub
                .pool
                .begin()
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            ::server_common::auth_utils::set_org_context(&mut *tx, &tenant_id)
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            sqlx::query(
                "UPDATE invoices SET stripe_payment_link = $1, stripe_checkout_session_id = $2, \
                 updated_at = CURRENT_TIMESTAMP WHERE id = $3 AND tenant_id = $4",
            )
            .bind(&checkout_url)
            .bind(&session_id)
            .bind(&invoice.id)
            .bind(&tenant_id)
            .execute(&mut *tx)
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            tx.commit()
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        }
    }

    // 2. Send real email via tenant ConnectionVault Resend key.
    let comm_id = uuid::Uuid::new_v4().to_string();
    let subject = body
        .subject
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("Invoice #{} from OmniSolo", invoice.id));
    let message_text = body
        .message
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            format!(
                "Hello {}, your invoice #{} for {} {}.{:02} is ready for payment.",
                invoice.client_name,
                invoice.id,
                invoice.currency,
                due_cents / 100,
                due_cents % 100
            )
        });
    let html_body = format!(
        "<p>{}</p><p><a href=\"{}\">Pay Invoice Online</a></p>",
        crate::domain::invoice::escape_html_text(&message_text),
        crate::domain::invoice::escape_html_text(&checkout_url)
    );

    let ledger = match &db.store {
        crate::db::DbStore::Sqlite(sqlite_pool) => {
            ::server_harness::middleware::usage_ledger::UsageLedger::Sqlite(sqlite_pool.clone())
        }
        crate::db::DbStore::Postgres => {
            ::server_harness::middleware::usage_ledger::UsageLedger::Postgres(hub.pool.clone())
        }
    };
    let receipt = crate::domain::invoice::send_invoice_email_via_vault(
        ledger,
        &tenant_id,
        &recipient_email,
        body.from_email.as_deref(),
        &subject,
        &html_body,
        &comm_id,
        None,
    )
    .await
    .map_err(|err| {
        tracing::warn!(invoice_id = %invoice.id, reason = %err, "Invoice email delivery failed; invoice stays unsent");
        if err.contains("not configured") || err.contains("Resend connection") {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::BAD_GATEWAY
        }
    })?;

    // 3. Only transition draft -> sent AFTER both Stripe and Resend succeeded.
    let next_payment_status = if invoice.amount_paid_cents > 0 {
        "partially_paid"
    } else {
        "unpaid"
    };
    match &db.store {
        crate::db::DbStore::Sqlite(sqlite_pool) => {
            let mut tx = sqlite_pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            sqlx::query(
                "INSERT INTO invoice_communication_events \
                 (id, tenant_id, invoice_id, status, channel, drafted_content, provider_message_id, recipient_email) \
                 VALUES (?, ?, ?, 'sent', 'email', ?, ?, ?)",
            )
            .bind(&comm_id)
            .bind(&tenant_id)
            .bind(&invoice.id)
            .bind(&message_text)
            .bind(&receipt.provider_message_id)
            .bind(&recipient_email)
            .execute(&mut *tx)
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            sqlx::query(
                "UPDATE invoices SET status = 'sent', payment_status = ?, \
                 stripe_payment_link = ?, stripe_checkout_session_id = ?, updated_at = CURRENT_TIMESTAMP \
                 WHERE id = ? AND tenant_id = ?",
            )
            .bind(next_payment_status)
            .bind(&checkout_url)
            .bind(&session_id)
            .bind(&invoice.id)
            .bind(&tenant_id)
            .execute(&mut *tx)
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            tx.commit()
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        }
        crate::db::DbStore::Postgres => {
            let mut tx = hub
                .pool
                .begin()
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            ::server_common::auth_utils::set_org_context(&mut *tx, &tenant_id)
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            sqlx::query(
                "INSERT INTO invoice_communication_events \
                 (id, tenant_id, invoice_id, status, channel, drafted_content, provider_message_id, recipient_email) \
                 VALUES ($1, $2, $3, 'sent', 'email', $4, $5, $6)",
            )
            .bind(&comm_id)
            .bind(&tenant_id)
            .bind(&invoice.id)
            .bind(&message_text)
            .bind(&receipt.provider_message_id)
            .bind(&recipient_email)
            .execute(&mut *tx)
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            sqlx::query(
                "UPDATE invoices SET status = 'sent', payment_status = $1, \
                 stripe_payment_link = $2, stripe_checkout_session_id = $3, updated_at = CURRENT_TIMESTAMP \
                 WHERE id = $4 AND tenant_id = $5",
            )
            .bind(next_payment_status)
            .bind(&checkout_url)
            .bind(&session_id)
            .bind(&invoice.id)
            .bind(&tenant_id)
            .execute(&mut *tx)
            .await
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            tx.commit()
                .await
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        }
    }

    Ok((
        StatusCode::OK,
        Json(serde_json::json!({
            "success": true,
            "invoice_id": invoice.id,
            "status": "sent",
            "payment_status": next_payment_status,
            "checkout_session_id": session_id,
            "checkout_url": checkout_url,
            "provider_message_id": receipt.provider_message_id,
        })),
    ))
}

pub fn router<S: Clone + Send + Sync + 'static>(hub: Arc<Hub>) -> axum::Router<S> {
    Router::new()
        .route("/", get(list_invoices_handler).post(create_invoice_handler))
        .route("/generate", post(generate_invoice_handler))
        .route("/{id}/status", put(update_invoice_status_handler))
        .route(
            "/{id}/checkout-session",
            post(create_invoice_checkout_session_handler),
        )
        .route("/{id}/send", post(send_invoice_handler))
        .with_state(hub)
}

use serde::Serialize;

#[derive(Serialize)]
pub struct InvoiceMobileView {
    pub id: String,
    pub client_name: String,
    pub status: String,
    pub due_date: i64,
    pub currency: String,
    pub total_amount: f64,
    pub total_amount_cents: i32,
    pub payment_status: String,
    pub amount_paid_cents: i32,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Serialize)]
pub struct InvoiceLineItemView {
    pub id: String,
    pub invoice_id: String,
    pub description: String,
    pub quantity: i32,
    pub unit_price: f64,
    pub amount: f64,
}

#[derive(Serialize)]
pub struct InvoiceStandardView {
    pub id: String,
    pub client_id: String,
    pub client_name: String,
    pub status: String,
    pub due_date: i64,
    pub currency: String,
    pub total_amount: f64,
    pub total_amount_cents: i32,
    pub payment_status: String,
    pub view_count: i32,
    pub amount_paid_cents: i32,
    pub stripe_invoice_id: String,
    pub stripe_payment_link: String,
    pub line_items: Vec<InvoiceLineItemView>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub fn map_invoices_for_mobile(
    invoices: Vec<::server_omnisolo::invoice::Invoice>,
) -> Vec<InvoiceMobileView> {
    invoices
        .into_iter()
        .map(|inv| InvoiceMobileView {
            id: inv.id,
            client_name: inv.client_name,
            status: inv.status,
            due_date: inv.due_date,
            currency: inv.currency,
            total_amount: inv.total_amount,
            total_amount_cents: inv.total_amount_cents,
            payment_status: inv.payment_status,
            amount_paid_cents: inv.amount_paid_cents,
            created_at: inv.created_at,
            updated_at: inv.updated_at,
        })
        .collect()
}

pub fn map_invoices_standard(
    invoices: Vec<::server_omnisolo::invoice::Invoice>,
) -> Vec<InvoiceStandardView> {
    invoices
        .into_iter()
        .map(|inv| InvoiceStandardView {
            id: inv.id,
            client_id: inv.client_id,
            client_name: inv.client_name,
            status: inv.status,
            due_date: inv.due_date,
            currency: inv.currency,
            total_amount: inv.total_amount,
            total_amount_cents: inv.total_amount_cents,
            payment_status: inv.payment_status,
            view_count: inv.view_count,
            amount_paid_cents: inv.amount_paid_cents,
            stripe_invoice_id: inv.stripe_invoice_id,
            stripe_payment_link: inv.stripe_payment_link,
            line_items: inv
                .line_items
                .into_iter()
                .map(|li| InvoiceLineItemView {
                    id: li.id,
                    invoice_id: li.invoice_id,
                    description: li.description,
                    quantity: li.quantity,
                    unit_price: li.unit_price,
                    amount: li.amount,
                })
                .collect(),
            created_at: inv.created_at,
            updated_at: inv.updated_at,
        })
        .collect()
}

pub async fn list_invoices_handler(
    State(_hub): State<Arc<Hub>>,
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
    Extension(claims): Extension<Claims>,
) -> Result<impl IntoResponse, StatusCode> {
    let tenant_id = invoice_tenant(&claims)?;

    let service = InvoiceServiceImpl { hub: _hub };
    let req = Request::new(ListInvoicesRequest { tenant_id });

    let mobile_optimized = query
        .get("mobile_optimized")
        .map(|s| s == "true")
        .unwrap_or(false);
    match service.list_invoices(req).await {
        Ok(resp) => {
            let inner = resp.into_inner();
            if mobile_optimized {
                let mapped = map_invoices_for_mobile(inner.invoices);
                Ok(Json(serde_json::json!({ "invoices": mapped })))
            } else {
                let mapped = map_invoices_standard(inner.invoices);
                Ok(Json(serde_json::json!({ "invoices": mapped })))
            }
        }
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn create_invoice_handler(
    State(_hub): State<Arc<Hub>>,
    Extension(claims): Extension<Claims>,
    Json(payload): Json<CreateInvoiceHttp>,
) -> Result<impl IntoResponse, StatusCode> {
    let tenant_id = invoice_tenant(&claims)?;
    require_invoice_owner(&claims)?;
    let service = InvoiceServiceImpl { hub: _hub };

    let mapped_line_items = http_invoice_items(payload.line_items)?;

    let req = Request::new(CreateInvoiceRequest {
        tenant_id,
        client_id: payload.client_id,
        client_name: payload.client_name,
        due_date: payload.due_date,
        currency: payload.currency.clone(),
        base_currency: payload.currency.clone(),
        transaction_currency: payload.currency,
        exchange_rate: 1.0,
        line_items: mapped_line_items,
    });

    service
        .create_invoice(req)
        .await
        .map(|resp| Json(resp.into_inner()))
        .map_err(invoice_http_error)
}

async fn update_invoice_status_handler(
    State(_hub): State<Arc<Hub>>,
    Extension(claims): Extension<Claims>,
    Path(id): Path<String>,
    Json(payload): Json<UpdateInvoiceStatusHttp>,
) -> Result<impl IntoResponse, StatusCode> {
    let tenant_id = invoice_tenant(&claims)?;
    require_invoice_owner(&claims)?;
    let service = InvoiceServiceImpl { hub: _hub };

    let req = Request::new(UpdateInvoiceStatusRequest {
        tenant_id,
        invoice_id: id,
        status: payload.status,
    });

    service
        .update_invoice_status(req)
        .await
        .map(|resp| Json(resp.into_inner()))
        .map_err(invoice_http_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invoice_rejects_invalid_and_inconsistent_amounts() {
        let mut item = InvoiceLineItem {
            id: String::new(),
            invoice_id: String::new(),
            description: "Service".into(),
            quantity: 3,
            unit_price: 10.25,
            amount: 30.75,
        };
        assert_eq!(validate_invoice_items(&[item.clone()]).unwrap(), 3075);
        item.amount = 10.25;
        assert!(validate_invoice_items(&[item.clone()]).is_err());
        item.amount = f64::NAN;
        assert!(validate_invoice_items(&[item.clone()]).is_err());
        item.unit_price = -1.0;
        assert!(validate_invoice_items(&[item]).is_err());
        assert!(validate_invoice_items(&[]).is_err());
    }
    use crate::hub::Hub;
    use ::server_omnisolo::invoice::{InvoiceLineItem, UpdateInvoiceStatusRequest};

    #[tokio::test]
    async fn test_invoice_logic_rejects_fabricated_payment_before_database_access() {
        // No hidden early-success returns on missing databases. Persistence is
        // exercised separately by native_business_regression on the real stack.
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel(100);
        let service = InvoiceServiceImpl {
            hub: Arc::new(Hub::new(tx, pool)),
        };
        for status in ["paid", "sent", "refunded", "partially_paid", "unknown"] {
            let error = service
                .update_invoice_status(Request::new(UpdateInvoiceStatusRequest {
                    tenant_id: "tenant-a".into(),
                    invoice_id: "invoice-a".into(),
                    status: status.into(),
                }))
                .await
                .unwrap_err();
            assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        }
        let error = service
            .draft_invoice_from_context(Request::new(DraftInvoiceFromContextRequest::default()))
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    }

    #[test]
    fn invoice_http_payload_rejects_wrapped_quantity_and_malformed_items() {
        for json in [
            r#"{"description":"x","quantity":4294967297,"unit_price":1}"#,
            r#"{"description":"x","quantity":1,"unit_price":1,"tenant_id":"other"}"#,
            r#"{"description":"x","quantity":1}"#,
        ] {
            assert!(serde_json::from_str::<CreateInvoiceLineHttp>(json).is_err());
        }
        assert!(
            http_invoice_items(vec![CreateInvoiceLineHttp {
                description: "Item".into(),
                quantity: -1,
                unit_price: 10.0,
            }])
            .is_err()
        );
        let items = http_invoice_items(vec![CreateInvoiceLineHttp {
            description: "Item".into(),
            quantity: 2,
            unit_price: 12.34,
        }])
        .unwrap();
        assert_eq!(validate_invoice_items(&items).unwrap(), 2468);
        assert!(validate_manual_invoice_status("draft").is_ok());
        assert!(validate_manual_invoice_status("void").is_ok());
    }

    #[tokio::test]
    async fn invoice_send_and_checkout_session_require_owner_authority() {
        use axum::body::Body;
        use tower::ServiceExt;

        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .unwrap();
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let hub = Arc::new(Hub::new(tx, pool));
        let viewer_claims = Claims {
            sub: "viewer-1".to_string(),
            exp: i64::MAX,
            iat: 0,
            organization_id: Some("tenant-a".to_string()),
            username: "viewer".to_string(),
            email: "viewer@example.com".to_string(),
            roles: vec!["viewer".to_string()],
            session_id: None,
            jti: "jti-viewer".to_string(),
        };

        for uri in ["/inv-1/checkout-session", "/inv-1/send"] {
            let app = router(hub.clone()).layer(Extension(viewer_claims.clone()));
            let resp = app
                .oneshot(
                    axum::http::Request::builder()
                        .method("POST")
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "uri: {uri}");
        }

        let op1 = invoice_checkout_operation_id("tenant-a", "inv-1", 5000);
        let op2 = invoice_checkout_operation_id("tenant-a", "inv-1", 5000);
        let op3 = invoice_checkout_operation_id("tenant-a", "inv-1", 7500);
        assert_eq!(op1, op2);
        assert_ne!(op1, op3);
        let meta = invoice_checkout_metadata("tenant-a", "inv-1");
        assert_eq!(meta["checkout_kind"], "invoice_payment");
        assert_eq!(meta["invoice_id"], "inv-1");
        assert_eq!(meta["tenant_id"], "tenant-a");
    }

    #[tokio::test]
    async fn sqlite_invoice_crud_and_real_loopback_send_flow_fail_closed_when_unconfigured() {
        use aes_gcm::{
            Aes256Gcm,
            aead::{Aead, AeadCore, KeyInit, OsRng, Payload},
        };
        use axum::body::{Body, to_bytes};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tower::ServiceExt;

        let _vault_guard = crate::domain::invoice::VAULT_ENV_LOCK.lock().await;
        let prev_keys = std::env::var_os("OMNISOLO_CONNECTION_KEYS");
        let prev_active = std::env::var_os("OMNISOLO_CONNECTION_ACTIVE_KEY");
        let prev_stripe_base = std::env::var_os("STRIPE_API_BASE");
        let prev_resend_base = std::env::var_os("RESEND_API_BASE");
        let prev_from = std::env::var_os("OMNISOLO_RESEND_FROM_EMAIL");
        let prev_public = std::env::var_os("PUBLIC_APP_URL");

        let sqlite = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(2)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let db = Arc::new(crate::db::DB::from_sqlite_pool(sqlite.clone()).unwrap());
        db.run_migrations().await.unwrap();
        let ledger =
            ::server_harness::middleware::usage_ledger::UsageLedger::Sqlite(sqlite.clone());
        ::server_harness::middleware::connection_vault::ConnectionVault::initialize_schema(&ledger)
            .await
            .unwrap();

        sqlx::query(
            "INSERT INTO tenants (id, name) VALUES ('tenant-inv', 'Invoice Tenant') ON CONFLICT (id) DO NOTHING",
        )
        .execute(&sqlite)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO customers (id, tenant_id, name, email) VALUES ('cust-inv-1', 'tenant-inv', 'Alice Buyer', 'alice@example.com')",
        )
        .execute(&sqlite)
        .await
        .unwrap();

        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let hub = Arc::new(Hub::new(tx, db.pool.clone()));
        hub.set_db(db.clone());
        let service = InvoiceServiceImpl { hub: hub.clone() };

        // 1. Create, get, list, and void on SQLite without any Postgres instance.
        let due_ts = (chrono::Utc::now() + chrono::Duration::days(14)).timestamp();
        let created_void = service
            .create_invoice(Request::new(CreateInvoiceRequest {
                tenant_id: "tenant-inv".into(),
                client_id: "cust-inv-1".into(),
                client_name: "Alice Buyer".into(),
                due_date: due_ts,
                currency: "USD".into(),
                base_currency: "USD".into(),
                transaction_currency: "USD".into(),
                exchange_rate: 1.0,
                line_items: vec![InvoiceLineItem {
                    id: String::new(),
                    invoice_id: String::new(),
                    description: "Draft to void".into(),
                    quantity: 1,
                    unit_price: 50.0,
                    amount: 50.0,
                }],
            }))
            .await
            .expect("SQLite create_invoice")
            .into_inner();
        assert_eq!(created_void.total_amount_cents, 5000);
        assert_eq!(created_void.status, "draft");

        let fetched = service
            .get_invoice(Request::new(GetInvoiceRequest {
                tenant_id: "tenant-inv".into(),
                invoice_id: created_void.id.clone(),
            }))
            .await
            .expect("SQLite get_invoice")
            .into_inner();
        assert_eq!(fetched.line_items.len(), 1);
        assert_eq!(fetched.due_date, due_ts);

        let voided = service
            .update_invoice_status(Request::new(UpdateInvoiceStatusRequest {
                tenant_id: "tenant-inv".into(),
                invoice_id: created_void.id.clone(),
                status: "void".into(),
            }))
            .await
            .expect("SQLite update_invoice_status")
            .into_inner();
        assert_eq!(voided.status, "void");

        let created = service
            .create_invoice(Request::new(CreateInvoiceRequest {
                tenant_id: "tenant-inv".into(),
                client_id: "cust-inv-1".into(),
                client_name: "Alice Buyer".into(),
                due_date: due_ts,
                currency: "USD".into(),
                base_currency: "USD".into(),
                transaction_currency: "USD".into(),
                exchange_rate: 1.0,
                line_items: vec![InvoiceLineItem {
                    id: String::new(),
                    invoice_id: String::new(),
                    description: "Website retainer".into(),
                    quantity: 2,
                    unit_price: 125.0,
                    amount: 250.0,
                }],
            }))
            .await
            .expect("SQLite create sendable invoice")
            .into_inner();
        assert_eq!(created.total_amount_cents, 25000);

        let listed = service
            .list_invoices(Request::new(ListInvoicesRequest {
                tenant_id: "tenant-inv".into(),
            }))
            .await
            .expect("SQLite list_invoices")
            .into_inner();
        assert_eq!(listed.invoices.len(), 2);

        let owner_claims = Claims {
            sub: "owner-inv".to_string(),
            exp: i64::MAX,
            iat: 0,
            organization_id: Some("tenant-inv".to_string()),
            username: "owner".to_string(),
            email: "owner@example.com".to_string(),
            roles: vec!["owner".to_string()],
            session_id: None,
            jti: "jti-owner-inv".to_string(),
        };
        let app = router(hub.clone()).layer(Extension(owner_claims));

        // 2. Fail closed when Stripe + Resend credentials are absent from ConnectionVault.
        unsafe {
            std::env::set_var(
                "OMNISOLO_CONNECTION_KEYS",
                format!(r#"{{"v1":"{}"}}"#, "66".repeat(32)),
            );
            std::env::set_var("OMNISOLO_CONNECTION_ACTIVE_KEY", "v1");
            std::env::set_var("PUBLIC_APP_URL", "https://owner.example");
            std::env::set_var("OMNISOLO_RESEND_FROM_EMAIL", "billing@owner.example");
        }
        let unconfigured_resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/{}/send", created.id))
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unconfigured_resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let status_after_fail: String =
            sqlx::query_scalar("SELECT status FROM invoices WHERE id = ?")
                .bind(&created.id)
                .fetch_one(&sqlite)
                .await
                .unwrap();
        assert_eq!(status_after_fail, "draft");

        // Helper to seed an encrypted ConnectionVault key on SQLite.
        let seed_vault_key = |pool: sqlx::SqlitePool,
                              provider: &'static str,
                              secret: &'static str| async move {
            let revision = uuid::Uuid::new_v4().to_string();
            let aad = serde_json::to_vec(&("tenant-inv", provider, &revision, "v1")).unwrap();
            let cipher = Aes256Gcm::new_from_slice(&[0x66_u8; 32]).unwrap();
            let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
            let ct = cipher
                .encrypt(
                    &nonce,
                    Payload {
                        msg: secret.as_bytes(),
                        aad: &aad,
                    },
                )
                .unwrap();
            let hex = |b: &[u8]| -> String { b.iter().map(|x| format!("{x:02x}")).collect() };
            sqlx::query(
                "INSERT INTO ohc_provider_connections \
                 (tenant_id, provider, revision, state, key_id, nonce_hex, ciphertext_hex, verified_at) \
                 VALUES ('tenant-inv', ?, ?, 'verified', 'v1', ?, ?, ?)",
            )
            .bind(provider)
            .bind(&revision)
            .bind(hex(&nonce))
            .bind(hex(&ct))
            .bind(chrono::Utc::now().timestamp())
            .execute(&pool)
            .await
            .unwrap();
        };

        // 3. Seed Stripe + Resend into ConnectionVault and run real local loopback servers.
        seed_vault_key(sqlite.clone(), "stripe", "sk_live_invoice_loopback_key_1").await;
        seed_vault_key(sqlite.clone(), "resend", "re_live_invoice_loopback_key_1").await;

        let stripe_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stripe_addr = stripe_listener.local_addr().unwrap();
        let stripe_srv = tokio::spawn(async move {
            let (mut stream, _) = stripe_listener.accept().await.unwrap();
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
                    let len = headers
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if req.len() >= end + 4 + len {
                        break;
                    }
                }
            }
            let body = serde_json::json!({
                "id": "cs_test_inv_sqlite_1",
                "url": "https://checkout.stripe.com/c/pay/cs_test_inv_sqlite_1",
                "payment_status": "unpaid",
                "amount_total": 25000,
                "currency": "usd"
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
            String::from_utf8(req).unwrap()
        });

        let resend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let resend_addr = resend_listener.local_addr().unwrap();
        let resend_srv = tokio::spawn(async move {
            let (mut stream, _) = resend_listener.accept().await.unwrap();
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
                    let len = headers
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if req.len() >= end + 4 + len {
                        break;
                    }
                }
            }
            let body = r#"{"id":"re_msg_sqlite_send_1"}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
            String::from_utf8(req).unwrap()
        });

        unsafe {
            std::env::set_var("STRIPE_API_BASE", format!("http://{stripe_addr}"));
            std::env::set_var("RESEND_API_BASE", format!("http://{resend_addr}"));
        }

        let send_resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/{}/send", created.id))
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        unsafe {
            match prev_keys {
                Some(v) => std::env::set_var("OMNISOLO_CONNECTION_KEYS", v),
                None => std::env::remove_var("OMNISOLO_CONNECTION_KEYS"),
            }
            match prev_active {
                Some(v) => std::env::set_var("OMNISOLO_CONNECTION_ACTIVE_KEY", v),
                None => std::env::remove_var("OMNISOLO_CONNECTION_ACTIVE_KEY"),
            }
            match prev_stripe_base {
                Some(v) => std::env::set_var("STRIPE_API_BASE", v),
                None => std::env::remove_var("STRIPE_API_BASE"),
            }
            match prev_resend_base {
                Some(v) => std::env::set_var("RESEND_API_BASE", v),
                None => std::env::remove_var("RESEND_API_BASE"),
            }
            match prev_from {
                Some(v) => std::env::set_var("OMNISOLO_RESEND_FROM_EMAIL", v),
                None => std::env::remove_var("OMNISOLO_RESEND_FROM_EMAIL"),
            }
            match prev_public {
                Some(v) => std::env::set_var("PUBLIC_APP_URL", v),
                None => std::env::remove_var("PUBLIC_APP_URL"),
            }
        }

        assert_eq!(send_resp.status(), StatusCode::OK);
        let send_bytes = to_bytes(send_resp.into_body(), 64 * 1024).await.unwrap();
        let send_json: serde_json::Value = serde_json::from_slice(&send_bytes).unwrap();
        assert_eq!(send_json["status"], "sent");
        assert_eq!(send_json["checkout_session_id"], "cs_test_inv_sqlite_1");
        assert_eq!(send_json["provider_message_id"], "re_msg_sqlite_send_1");

        let stripe_req = stripe_srv.await.unwrap();
        assert!(stripe_req.contains("POST /v1/checkout/sessions"));
        let resend_req = resend_srv.await.unwrap();
        assert!(resend_req.contains("POST /emails"));
        assert!(resend_req.contains("alice@example.com"));
        assert!(resend_req.contains("cs_test_inv_sqlite_1"));

        let (inv_status, inv_cs): (String, Option<String>) =
            sqlx::query_as("SELECT status, stripe_checkout_session_id FROM invoices WHERE id = ?")
                .bind(&created.id)
                .fetch_one(&sqlite)
                .await
                .unwrap();
        assert_eq!(inv_status, "sent");
        assert_ne!(inv_status, "delivered");
        assert_eq!(inv_cs.as_deref(), Some("cs_test_inv_sqlite_1"));

        let (comm_status, comm_msg_id): (String, Option<String>) = sqlx::query_as(
            "SELECT status, provider_message_id FROM invoice_communication_events WHERE invoice_id = ?",
        )
        .bind(&created.id)
        .fetch_one(&sqlite)
        .await
        .unwrap();
        assert_eq!(comm_status, "sent");
        assert_eq!(comm_msg_id.as_deref(), Some("re_msg_sqlite_send_1"));
    }
}

#[cfg(test)]
mod payload_tests {
    use super::*;

    #[test]
    fn test_invoice_mobile_payload_optimization() {
        let inv = ::server_omnisolo::invoice::Invoice {
            id: "inv-1".to_string(),
            client_id: "client-1".to_string(),
            client_name: "John Doe".to_string(),
            status: "DRAFT".to_string(),
            due_date: 1234567890,
            currency: "USD".to_string(),
            base_currency: "USD".to_string(),
            transaction_currency: "USD".to_string(),
            exchange_rate: 1.0,
            total_amount: 100.0,
            total_amount_cents: 10000,
            payment_status: "UNPAID".to_string(),
            view_count: 5,
            amount_paid_cents: 0,
            stripe_invoice_id: "in_123".to_string(),
            stripe_payment_link: "https://stripe.com/pay/123".to_string(),
            line_items: vec![::server_omnisolo::invoice::InvoiceLineItem {
                id: "li-1".to_string(),
                invoice_id: "inv-1".to_string(),
                description: "Test".to_string(),
                quantity: 1,
                unit_price: 100.0,
                amount: 100.0,
            }],
            created_at: 1234567800,
            updated_at: 1234567800,
        };

        // Test mobile mapping
        let mobile_mapped = map_invoices_for_mobile(vec![inv.clone()]);
        assert_eq!(mobile_mapped.len(), 1);
        let m_inv = &mobile_mapped[0];

        // Assert fields are present
        assert_eq!(m_inv.id, "inv-1");
        assert_eq!(m_inv.client_name, "John Doe");
        assert_eq!(m_inv.currency, "USD");
        assert_eq!(m_inv.total_amount_cents, 10000);

        // Serialize and verify omitted fields
        let json_val = serde_json::to_value(m_inv).unwrap();
        assert!(json_val.get("stripe_invoice_id").is_none());
        assert!(json_val.get("stripe_payment_link").is_none());
        assert!(json_val.get("client_id").is_none());
        assert!(json_val.get("view_count").is_none());
        assert!(json_val.get("line_items").is_none());

        // Test standard mapping
        let standard_mapped = map_invoices_standard(vec![inv]);
        assert_eq!(standard_mapped.len(), 1);
        let s_inv = &standard_mapped[0];
        assert_eq!(s_inv.client_id, "client-1");
        assert_eq!(s_inv.stripe_invoice_id, "in_123");
        assert_eq!(s_inv.line_items.len(), 1);
    }
}
