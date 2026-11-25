use sqlx::{Executor, Postgres, query};

/// Bind tenant context for the duration of the current transaction.
/// Callers MUST pass a transaction executor (`&mut *tx`), never a bare pool.
pub async fn set_org_context<'a, E>(executor: E, org_id: &str) -> Result<(), sqlx::Error>
where
    E: Executor<'a, Database = Postgres>,
{
    let trimmed = org_id.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("system") {
        return Err(sqlx::Error::Configuration(
            "tenant_id must be non-empty and must not be 'system'".into(),
        ));
    }

    // Transaction-scoped setting (`true`) ensures tenant context automatically clears
    // on COMMIT or ROLLBACK across connection checkouts.
    query("SELECT set_config('app.current_tenant', $1, true);")
        .bind(trimmed)
        .execute(executor)
        .await?;
    Ok(())
}

pub fn get_default_tenant() -> String {
    let configured = ::server_config::get().bootstrap_org_id.trim();
    if configured.is_empty() || configured.eq_ignore_ascii_case("system") {
        "bootstrap".to_string()
    } else {
        configured.to_string()
    }
}

pub fn signed_tenant_id(claims: &crate::Claims) -> Option<String> {
    claims
        .organization_id
        .as_deref()
        .map(str::trim)
        .filter(|tenant_id| !tenant_id.is_empty() && !tenant_id.eq_ignore_ascii_case("system"))
        .map(str::to_string)
}

#[derive(serde::Deserialize)]
pub struct UiTenantQuery {
    pub tenant_id: Option<String>,
    pub tenant: Option<String>,
    pub mobile_optimized: Option<bool>,
    pub fields: Option<String>,
    pub bypass_cache: Option<bool>,
}

pub fn ui_tenant_id(query: &UiTenantQuery) -> String {
    query
        .tenant_id
        .as_deref()
        .or(query.tenant.as_deref())
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::signed_tenant_id;

    #[test]
    fn signed_tenant_id_rejects_empty_and_system_claims() {
        let claims = |organization_id: Option<&str>| crate::Claims {
            sub: "user-1".to_string(),
            exp: 0,
            iat: 0,
            organization_id: organization_id.map(str::to_string),
            username: String::new(),
            email: String::new(),
            roles: vec![],
            session_id: None,
            jti: String::new(),
        };

        assert_eq!(
            signed_tenant_id(&claims(Some("tenant-a"))).as_deref(),
            Some("tenant-a")
        );
        assert_eq!(signed_tenant_id(&claims(Some(" system "))), None);
        assert_eq!(signed_tenant_id(&claims(Some(" "))), None);
        assert_eq!(signed_tenant_id(&claims(None)), None);
    }
}
