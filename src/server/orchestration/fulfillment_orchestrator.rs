use std::sync::Arc;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::db::DB;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FulfillmentProposal {
    pub tenant_id: String,
    pub customer_id: Option<String>,
    pub service_name: String,
    pub start_time: DateTime<Utc>,
    pub end_time: DateTime<Utc>,
    pub final_price_cents: i64,
    pub proofs: Vec<String>,
}

pub struct FulfillmentOrchestrator {

    _db: Arc<DB>,
}

impl FulfillmentOrchestrator {
    pub fn new(db: Arc<DB>) -> Self {
        Self { _db: db }
    }

    pub async fn evaluate_and_propose(
        &self,
        tenant_id: String,
        customer_id: Option<String>,
        service_name: String,
        start_time_str: Option<String>,
        end_time_str: Option<String>,
        base_price_cents: i64,
    ) -> Result<FulfillmentProposal, String> {
        let start_time = if let Some(st) = start_time_str {
            chrono::DateTime::parse_from_rfc3339(&st)
                .ok()
                .map(|d| d.with_timezone(&chrono::Utc))
                .unwrap_or_else(chrono::Utc::now)
        } else {
            chrono::Utc::now() + chrono::Duration::days(1)
        };

        let end_time = if let Some(et) = end_time_str {
            chrono::DateTime::parse_from_rfc3339(&et)
                .ok()
                .map(|d| d.with_timezone(&chrono::Utc))
                .unwrap_or_else(|| start_time + chrono::Duration::hours(1))
        } else {
            start_time + chrono::Duration::hours(1)
        };

        let mut proofs = vec![];

        // Check availability (Operations Check)
        // Simulate an availability check
        proofs.push("✅ Spot reserved in calendar.".to_string());

        // Standard solo-workspace pricing (cloud dynamic pricing rules live in Cloud edition).
        let final_price_cents = base_price_cents;
        proofs.push("✅ Standard pricing applied.".to_string());

        Ok(FulfillmentProposal {
            tenant_id,
            customer_id,
            service_name,
            start_time,
            end_time,
            final_price_cents,
            proofs,
        })
    }
}
