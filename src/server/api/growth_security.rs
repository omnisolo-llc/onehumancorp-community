//! Security helpers and tenant-scoped query primitives for `api::growth`.

use super::{OnboardingMetric, TeamInvitesResponse};
use crate::services::growth::invites::TeamInvite;
use axum::{
    Extension,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use sqlx::{PgPool, Row};

pub(crate) const EMBED_HTML_CSP: &str = "default-src 'none'; script-src 'none'; style-src 'unsafe-inline'; img-src https: data:; font-src 'self'; base-uri 'none'; form-action 'none'";
pub(crate) const EMBED_SVG_CSP: &str =
    "default-src 'none'; script-src 'none'; style-src 'unsafe-inline'; base-uri 'none'";

pub(crate) fn escape_html(input: &str, max_len: usize) -> String {
    let mut out = String::with_capacity(input.len().min(max_len));
    for ch in input.chars().filter(|c| !c.is_control()).take(max_len) {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(ch),
        }
    }
    out
}

pub(crate) fn encode_query_component(input: &str, max_len: usize) -> String {
    let cleaned: String = input
        .chars()
        .filter(|c| !c.is_control())
        .take(max_len)
        .collect();
    urlencoding::encode(cleaned.trim()).into_owned()
}

pub(crate) async fn tenants_has_plan_tier_column(pool: &PgPool) -> bool {
    sqlx::query_scalar(
        "SELECT EXISTS (
            SELECT 1 FROM pg_attribute
            WHERE attrelid = to_regclass('tenants')
              AND attname = 'plan_tier'
              AND NOT attisdropped
        )",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(false)
}

pub(crate) async fn tenant_has_pro_plan(pool: &PgPool, tenant_id: &str) -> bool {
    if !tenants_has_plan_tier_column(pool).await {
        return false;
    }
    let row: Option<Option<String>> =
        sqlx::query_scalar("SELECT plan_tier FROM tenants WHERE CAST(id AS TEXT) = $1")
            .bind(tenant_id)
            .fetch_optional(pool)
            .await
            .unwrap_or_default();
    row.flatten()
        .is_some_and(|plan| plan.eq_ignore_ascii_case("pro"))
}

pub(crate) async fn tenant_business_name_and_pro(
    pool: &PgPool,
    tenant_id: &str,
) -> Option<(String, bool)> {
    if tenants_has_plan_tier_column(pool).await {
        let row: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT name as business_name, plan_tier FROM tenants WHERE CAST(id AS TEXT) = $1",
        )
        .bind(tenant_id)
        .fetch_optional(pool)
        .await
        .unwrap_or_default();
        return row.map(|(name, plan)| {
            let has_pro = plan.is_some_and(|p| p.eq_ignore_ascii_case("pro"));
            (name, has_pro)
        });
    }
    let name: Option<String> =
        sqlx::query_scalar("SELECT name FROM tenants WHERE CAST(id AS TEXT) = $1")
            .bind(tenant_id)
            .fetch_optional(pool)
            .await
            .unwrap_or_default();
    name.map(|n| (n, false))
}

pub(crate) fn hardened_html_response(html: String) -> Response {
    let mut resp = (StatusCode::OK, html).into_response();
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(EMBED_HTML_CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    resp
}

pub(crate) fn hardened_svg_response(svg: String) -> Response {
    let mut resp = (StatusCode::OK, svg).into_response();
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("image/svg+xml; charset=utf-8"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(EMBED_SVG_CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    resp
}

pub(crate) fn require_signed_tenant(
    claims: Option<Extension<::server_common::Claims>>,
) -> Result<String, StatusCode> {
    let claims = claims.ok_or(StatusCode::UNAUTHORIZED)?.0;
    ::server_common::auth_utils::signed_tenant_id(&claims).ok_or(StatusCode::FORBIDDEN)
}

/// Derives `AuthInfo` for growth handlers. When verified JWT `Claims` are present,
/// untrusted `x-tenant-id` / `x-agent-id` headers are NEVER consulted
/// and invalid/missing `organization_id` (including `"system"`) fails closed (`None`).
pub(crate) fn derive_growth_auth_info(
    claims: Option<&::server_common::Claims>,
    headers: &axum::http::HeaderMap,
) -> Option<::server_auth::orchestration::AuthInfo> {
    if let Some(claims) = claims {
        let tenant_id = ::server_common::auth_utils::signed_tenant_id(claims)?;
        let agent_id = if claims.sub.trim().is_empty() {
            "growth-user".to_string()
        } else {
            claims.sub.clone()
        };
        let spiffe_id = format!("peer://ohc.app/{tenant_id}/{agent_id}");
        return Some(::server_auth::orchestration::AuthInfo {
            org_id: tenant_id,
            agent_id,
            spiffe_id,
        });
    }

    let tenant_id = headers
        .get("x-tenant-id")
        .and_then(|h| h.to_str().ok())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "default-team".to_string());
    let agent_id = headers
        .get("x-agent-id")
        .and_then(|h| h.to_str().ok())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("growth-agent")
        .to_string();

    Some(::server_auth::orchestration::AuthInfo {
        org_id: tenant_id.clone(),
        agent_id: agent_id.clone(),
        spiffe_id: format!("peer://ohc.app/{tenant_id}/{agent_id}"),
    })
}

pub(crate) async fn fetch_tenant_team_invites(
    pool: &PgPool,
    tenant_id: &str,
    team_id: &str,
    cursor: Option<String>,
    limit: usize,
) -> Result<TeamInvitesResponse, StatusCode> {
    let bounded_limit = limit.clamp(1, 100);
    let mut tx = pool
        .begin()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    ::server_common::auth_utils::set_org_context(&mut *tx, tenant_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let rows = if let Some(c) = cursor {
        sqlx::query(
            "SELECT id, tenant_id, team_id, inviter_id, invitee_id, status, created_at, updated_at
             FROM team_invites
             WHERE tenant_id = $1 AND team_id = $2
               AND created_at <= (SELECT created_at FROM team_invites WHERE id = $3 AND tenant_id = $1)
               AND id < $3
             ORDER BY created_at DESC, id DESC
             LIMIT $4",
        )
        .bind(tenant_id)
        .bind(team_id)
        .bind(c)
        .bind(bounded_limit as i64)
        .fetch_all(&mut *tx)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    } else {
        sqlx::query(
            "SELECT id, tenant_id, team_id, inviter_id, invitee_id, status, created_at, updated_at
             FROM team_invites
             WHERE tenant_id = $1 AND team_id = $2
             ORDER BY created_at DESC, id DESC
             LIMIT $3",
        )
        .bind(tenant_id)
        .bind(team_id)
        .bind(bounded_limit as i64)
        .fetch_all(&mut *tx)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    };
    tx.commit()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let invites: Vec<TeamInvite> = rows
        .into_iter()
        .map(|row| TeamInvite {
            id: row.get("id"),
            tenant_id: row.get("tenant_id"),
            team_id: row.get("team_id"),
            inviter_id: row.get("inviter_id"),
            invitee_id: row.get("invitee_id"),
            status: row.get("status"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        })
        .collect();
    let next_cursor = if invites.len() == bounded_limit {
        invites.last().map(|i| i.id.clone())
    } else {
        None
    };
    Ok(TeamInvitesResponse {
        invites,
        next_cursor,
    })
}

pub(crate) async fn fetch_tenant_team_invite_metrics(
    pool: &PgPool,
    tenant_id: &str,
    team_id: &str,
) -> Result<(i64, i64), StatusCode> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    ::server_common::auth_utils::set_org_context(&mut *tx, tenant_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let total_invites: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM team_invites WHERE tenant_id = $1 AND team_id = $2",
    )
    .bind(tenant_id)
    .bind(team_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let active_referrals: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(conversions), 0) FROM referrals WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_one(&mut *tx)
    .await
    .unwrap_or(0);
    tx.commit()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok((total_invites, active_referrals))
}

pub(crate) async fn fetch_tenant_onboarding_metrics(
    pool: &PgPool,
    tenant_id: &str,
) -> Result<Vec<OnboardingMetric>, StatusCode> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    ::server_common::auth_utils::set_org_context(&mut *tx, tenant_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let rows = sqlx::query(
        "SELECT f.step, COUNT(*) as count
         FROM onboarding_funnels f
         INNER JOIN users u ON u.id::text = f.user_id::text
         WHERE u.tenant_id::text = $1
         GROUP BY f.step",
    )
    .bind(tenant_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| {
        ::server_telemetry::record_error_signal("[bug] Failed to fetch onboarding metrics");
        tracing::error!("Failed to fetch onboarding metrics: {:?}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    tx.commit()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(rows
        .into_iter()
        .map(|r| OnboardingMetric {
            step: r.get("step"),
            count: r.get::<i64, _>("count") as i32,
        })
        .collect())
}

pub(crate) async fn increment_tenant_referral_counter(
    pool: &PgPool,
    tenant_id: &str,
    referral_id: &str,
    conversions: bool,
) -> Result<(), StatusCode> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    ::server_common::auth_utils::set_org_context(&mut *tx, tenant_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let sql = if conversions {
        "UPDATE referrals SET conversions = conversions + 1 WHERE id = $1 AND tenant_id = $2"
    } else {
        "UPDATE referrals SET clicks = clicks + 1 WHERE id = $1 AND tenant_id = $2"
    };
    let result = sqlx::query(sql)
        .bind(referral_id)
        .bind(tenant_id)
        .execute(&mut *tx)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if result.rows_affected() == 0 {
        return Err(StatusCode::NOT_FOUND);
    }
    tx.commit()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(())
}

pub(crate) async fn accept_tenant_team_invite(
    pool: &PgPool,
    tenant_id: &str,
    invite_id: &str,
) -> Result<(String, String), StatusCode> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    ::server_common::auth_utils::set_org_context(&mut *tx, tenant_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let row = sqlx::query(
        "UPDATE team_invites
         SET status = 'ACCEPTED', updated_at = CURRENT_TIMESTAMP
         WHERE id = $1 AND tenant_id = $2
         RETURNING team_id, invitee_id",
    )
    .bind(invite_id)
    .bind(tenant_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let Some(row) = row else {
        return Err(StatusCode::NOT_FOUND);
    };
    tx.commit()
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok((row.get("team_id"), row.get("invitee_id")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::growth::{
        GetTeamInvitesQuery, GrowthState, InviteIdRequest, ReferralIdRequest, SecretMenuParams,
        handle_discount_code_embed, handle_get_team_invites, handle_onboarding_metrics,
        handle_referral_click_post, handle_referral_convert, handle_secret_menu_embed,
        handle_team_invite_accept, handle_team_invites_metrics,
    };
    use axum::Json;
    use axum::extract::Query;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn test_claims(org: Option<&str>) -> ::server_common::Claims {
        ::server_common::Claims {
            sub: "user-1".to_string(),
            exp: 9_999_999_999,
            iat: 1,
            organization_id: org.map(str::to_string),
            username: "owner".to_string(),
            email: "owner@example.com".to_string(),
            roles: vec!["ADMIN".to_string()],
            session_id: None,
            jti: "jti-1".to_string(),
        }
    }

    fn lazy_growth_state() -> GrowthState {
        let pool = crate::db::secure_pg_pool_options()
            .acquire_timeout(std::time::Duration::from_millis(50))
            .max_connections(1)
            .connect_lazy("postgres://postgres:postgres@127.0.0.1:1/ohc")
            .unwrap();
        let (event_tx, _) = tokio::sync::mpsc::channel(8);
        GrowthState {
            hub: Arc::new(crate::hub::Hub::new(event_tx, pool.clone())),
            pool,
            viral_loop_tracker: Arc::new(
                crate::services::growth::viral_loop::ViralLoopTracker::new(),
            ),
        }
    }

    #[test]
    fn require_signed_tenant_rejects_missing_and_invalid_claims() {
        assert_eq!(require_signed_tenant(None), Err(StatusCode::UNAUTHORIZED));
        assert_eq!(
            require_signed_tenant(Some(Extension(test_claims(None)))),
            Err(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            require_signed_tenant(Some(Extension(test_claims(Some("system"))))),
            Err(StatusCode::FORBIDDEN)
        );
        assert_eq!(
            require_signed_tenant(Some(Extension(test_claims(Some("tenant-a"))))),
            Ok("tenant-a".to_string())
        );
    }

    #[tokio::test]
    async fn growth_auth_fallback_never_falls_back_to_x_tenant_id_when_claims_present() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::get;
        use tower::ServiceExt;

        let mut spoofed_headers = axum::http::HeaderMap::new();
        spoofed_headers.insert("x-tenant-id", HeaderValue::from_static("victim"));
        spoofed_headers.insert("x-agent-id", HeaderValue::from_static("spoofed-agent"));

        // Claims with system or None org MUST fail closed (None), never honoring x-tenant-id.
        assert!(
            derive_growth_auth_info(Some(&test_claims(Some("system"))), &spoofed_headers).is_none()
        );
        assert!(derive_growth_auth_info(Some(&test_claims(None)), &spoofed_headers).is_none());
        assert!(
            derive_growth_auth_info(Some(&test_claims(Some("   "))), &spoofed_headers).is_none()
        );

        // Valid Claims ignore spoofed x-tenant-id and bind org_id strictly to signed Claims.
        let derived =
            derive_growth_auth_info(Some(&test_claims(Some("tenant-a"))), &spoofed_headers)
                .expect("valid claims should yield AuthInfo");
        assert_eq!(derived.org_id, "tenant-a");
        assert_eq!(derived.agent_id, "user-1");

        // End-to-end middleware check: Claims { org: "system" } + x-tenant-id: victim must NOT
        // inject AuthInfo into downstream handlers.
        let app = Router::new()
            .route(
                "/check",
                get(
                    |auth: Option<Extension<::server_auth::orchestration::AuthInfo>>| async move {
                        match auth {
                            Some(Extension(info)) => (StatusCode::OK, info.org_id),
                            None => (StatusCode::FORBIDDEN, "no-auth-info".to_string()),
                        }
                    },
                ),
            )
            .layer(axum::middleware::from_fn(
                crate::api::growth::growth_auth_fallback_middleware,
            ));

        let mut req = Request::builder()
            .uri("/check")
            .header("x-tenant-id", "victim")
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(test_claims(Some("system")));
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn discount_code_embed_escapes_xss_and_emits_strict_csp() {
        let mut params = HashMap::new();
        params.insert("tenant".to_string(), "t\" onclick=\"alert(1)".to_string());
        params.insert(
            "discount".to_string(),
            "<script>alert(1)</script>".to_string(),
        );
        params.insert(
            "code".to_string(),
            "<img src=x onerror=alert(2)>".to_string(),
        );

        let resp = handle_discount_code_embed(Query(params))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
            "nosniff"
        );
        let csp = resp
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(csp.contains("default-src 'none'"));
        assert!(csp.contains("script-src 'none'"));

        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(!html.contains("<img src=x onerror=alert(2)>"));
        assert!(!html.contains("onclick=\"alert(1)"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(html.contains("&lt;img src=x onerror=alert(2)&gt;"));
    }

    #[tokio::test]
    async fn secret_menu_embed_escapes_xss_and_emits_strict_csp() {
        let state = lazy_growth_state();
        let params = SecretMenuParams {
            tenant: "acme\" onmouseover=\"alert(1)".to_string(),
            item_name: Some("<script>alert(1)</script>".to_string()),
            item_desc: Some("<svg onload=alert(2)>".to_string()),
            access_code: Some("<b>CODE</b>".to_string()),
            shares_req: Some("3<script>alert(3)</script>".to_string()),
        };

        let resp = handle_secret_menu_embed(Extension(state), Query(params))
            .await
            .into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
            "nosniff"
        );
        let csp = resp
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(csp.contains("script-src 'none'"));

        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let html = String::from_utf8(body.to_vec()).unwrap();
        assert!(!html.contains("<script>"));
        assert!(!html.contains("<svg onload="));
        assert!(!html.contains("onmouseover=\"alert(1)"));
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(html.contains("&lt;svg onload=alert(2)&gt;"));
    }

    #[tokio::test]
    async fn growth_tenant_handlers_fail_closed_without_verified_claims() {
        let state = lazy_growth_state();
        let sys_claims = Some(Extension(test_claims(Some("system"))));

        assert_eq!(
            handle_get_team_invites(
                Extension(state.clone()),
                None,
                Query(GetTeamInvitesQuery {
                    team_id: "victim".to_string(),
                    cursor: None,
                    limit: Some(10),
                }),
            )
            .await
            .unwrap_err(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            handle_team_invites_metrics(
                Extension(state.clone()),
                sys_claims.clone(),
                Query(GetTeamInvitesQuery {
                    team_id: "victim".to_string(),
                    cursor: None,
                    limit: None,
                }),
            )
            .await
            .unwrap_err(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            handle_onboarding_metrics(Extension(state.clone()), None)
                .await
                .unwrap_err(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            handle_referral_click_post(
                Extension(state.clone()),
                None,
                Json(ReferralIdRequest {
                    id: "ref-1".to_string(),
                }),
            )
            .await
            .unwrap_err(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            handle_referral_convert(
                Extension(state.clone()),
                sys_claims.clone(),
                Json(ReferralIdRequest {
                    id: "ref-1".to_string(),
                }),
            )
            .await
            .unwrap_err(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            handle_team_invite_accept(
                Extension(state),
                None,
                Json(InviteIdRequest {
                    id: "inv-1".to_string(),
                }),
            )
            .await
            .unwrap_err(),
            StatusCode::UNAUTHORIZED
        );
    }
}
