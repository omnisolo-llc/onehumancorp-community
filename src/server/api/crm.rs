use crate::domain::repository::models::Opportunity;
use crate::utils::cache::HybridCache;
use ::server_common::Claims;
use axum::{
    Extension, Json,
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::OnceLock;

pub static CRM_OPPORTUNITIES_CACHE: OnceLock<HybridCache<CachedOpportunities>> = OnceLock::new();

#[derive(Serialize, Deserialize, Clone)]
pub struct CachedOpportunities {
    pub rows: Vec<Opportunity>,
    pub total_count: i64,
}

#[derive(Deserialize)]
pub struct OpportunitiesQuery {
    pub tenant_id: Option<String>,
    pub mobile_optimized: Option<bool>,
}

#[derive(Deserialize)]
pub struct UpdateOpportunityStageRequest {
    pub opportunity_id: String,
    pub stage: String,
}

fn resolve_authenticated_tenant(
    claims: &Claims,
    requested_tenant_id: Option<&str>,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    let raw_org = match claims.organization_id.as_deref().map(str::trim) {
        Some(org) if !org.is_empty() => org,
        _ => {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"error": "authenticated tenant context required"})),
            ));
        }
    };

    if raw_org.eq_ignore_ascii_case("system")
        || raw_org.len() > 255
        || raw_org.chars().any(char::is_control)
    {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "invalid tenant context"})),
        ));
    }

    if let Some(requested) = requested_tenant_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        && requested != raw_org
    {
        return Err((
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({"error": "tenant mismatch"})),
        ));
    }

    Ok(raw_org.to_string())
}

async fn fetch_opportunities_and_count(
    db: &Arc<crate::db::DB>,
    tenant_id: &str,
    mobile_optimized: bool,
) -> Result<(Vec<Opportunity>, i64), sqlx::Error> {
    match &db.store {
        crate::db::DbStore::Postgres => {
            let db_clone1 = db.clone();
            let db_clone2 = db.clone();
            let t_id1 = tenant_id.to_string();
            let t_id2 = tenant_id.to_string();

            let (list_res, count_res) = tokio::join!(
                tokio::spawn(async move {
                    let mut tx = db_clone1.pool.begin().await?;
                    ::server_common::auth_utils::set_org_context(&mut *tx, &t_id1).await?;
                    let rows = sqlx::query_as::<_, Opportunity>(if mobile_optimized {
                        "SELECT id, '' as tenant_id, lead_id, title, stage, estimated_value, priority, created_at, updated_at FROM opportunities WHERE tenant_id = $1 ORDER BY created_at DESC"
                    } else {
                        "SELECT id, tenant_id, lead_id, title, stage, estimated_value, priority, created_at, updated_at FROM opportunities WHERE tenant_id = $1 ORDER BY created_at DESC"
                    })
                    .bind(&t_id1)
                    .fetch_all(&mut *tx)
                    .await?;
                    tx.commit().await?;
                    Ok::<_, sqlx::Error>(rows)
                }),
                tokio::spawn(async move {
                    let mut tx = db_clone2.pool.begin().await?;
                    ::server_common::auth_utils::set_org_context(&mut *tx, &t_id2).await?;
                    let count: (i64,) =
                        sqlx::query_as("SELECT count(*) FROM opportunities WHERE tenant_id = $1")
                            .bind(&t_id2)
                            .fetch_one(&mut *tx)
                            .await?;
                    tx.commit().await?;
                    Ok::<_, sqlx::Error>(count.0)
                })
            );

            let rows = list_res.map_err(|err| sqlx::Error::Protocol(err.to_string()))??;
            let total_count = count_res.map_err(|err| sqlx::Error::Protocol(err.to_string()))??;
            Ok((rows, total_count))
        }
        crate::db::DbStore::Sqlite(sqlite_pool) => {
            let rows = sqlx::query_as::<_, Opportunity>(if mobile_optimized {
                "SELECT id, '' as tenant_id, lead_id, title, stage, estimated_value, priority, created_at, updated_at FROM opportunities WHERE tenant_id = ?1 ORDER BY created_at DESC"
            } else {
                "SELECT id, tenant_id, lead_id, title, stage, estimated_value, priority, created_at, updated_at FROM opportunities WHERE tenant_id = ?1 ORDER BY created_at DESC"
            })
            .bind(tenant_id)
            .fetch_all(sqlite_pool)
            .await?;

            let count: (i64,) =
                sqlx::query_as("SELECT count(*) FROM opportunities WHERE tenant_id = ?1")
                    .bind(tenant_id)
                    .fetch_one(sqlite_pool)
                    .await?;
            Ok((rows, count.0))
        }
    }
}

pub async fn list_opportunities_handler(
    State(db): State<Arc<crate::db::DB>>,
    Query(query): Query<OpportunitiesQuery>,
    Extension(claims): Extension<Claims>,
) -> impl IntoResponse {
    let tenant_id = match resolve_authenticated_tenant(&claims, query.tenant_id.as_deref()) {
        Ok(tenant) => tenant,
        Err(response) => return response.into_response(),
    };
    let mobile_optimized = query.mobile_optimized.unwrap_or(false);

    let cache_key = format!(
        "crm_opportunities:{}:mobile:{}",
        tenant_id, mobile_optimized
    );
    let cache =
        CRM_OPPORTUNITIES_CACHE.get_or_init(|| HybridCache::new(crate::distributed_cache()));

    if let Some((cached, is_stale)) = cache.get_with_swr(&cache_key).await {
        if !is_stale {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                "x-total-count",
                axum::http::HeaderValue::from_str(&cached.total_count.to_string())
                    .unwrap_or(axum::http::HeaderValue::from_static("0")),
            );
            return (headers, Json(cached.rows)).into_response();
        }

        let db_bg = db.clone();
        let t_bg = tenant_id.clone();
        let cache_key_bg = cache_key.clone();

        tokio::spawn(async move {
            if let (Ok((rows, total_count)), Some(c)) = (
                fetch_opportunities_and_count(&db_bg, &t_bg, mobile_optimized).await,
                CRM_OPPORTUNITIES_CACHE.get(),
            ) {
                let _ = c
                    .set(
                        &cache_key_bg,
                        CachedOpportunities { rows, total_count },
                        std::time::Duration::from_secs(10),
                    )
                    .await;
            }
        });

        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "x-total-count",
            axum::http::HeaderValue::from_str(&cached.total_count.to_string())
                .unwrap_or(axum::http::HeaderValue::from_static("0")),
        );
        return (headers, Json(cached.rows)).into_response();
    }

    let (rows, total_count) =
        match fetch_opportunities_and_count(&db, &tenant_id, mobile_optimized).await {
            Ok(result) => result,
            Err(e) => {
                tracing::error!("Failed to fetch opportunities: {:?}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(Vec::<Opportunity>::new()),
                )
                    .into_response();
            }
        };

    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "x-total-count",
        axum::http::HeaderValue::from_str(&total_count.to_string())
            .unwrap_or(axum::http::HeaderValue::from_static("0")),
    );
    let _ = cache
        .set(
            &cache_key,
            CachedOpportunities {
                rows: rows.clone(),
                total_count,
            },
            std::time::Duration::from_secs(10),
        )
        .await;

    (headers, Json(rows)).into_response()
}

pub async fn update_opportunity_stage_handler(
    State(db): State<Arc<crate::db::DB>>,
    Extension(claims): Extension<Claims>,
    Json(payload): Json<UpdateOpportunityStageRequest>,
) -> impl IntoResponse {
    let tenant_id = match resolve_authenticated_tenant(&claims, None) {
        Ok(tenant) => tenant,
        Err(response) => return response.into_response(),
    };

    let rows_affected = match &db.store {
        crate::db::DbStore::Postgres => {
            let mut tx = match db.pool.begin().await {
                Ok(tx) => tx,
                Err(e) => {
                    tracing::error!("Failed to begin transaction: {:?}", e);
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": "internal error"})),
                    )
                        .into_response();
                }
            };

            if let Err(e) = ::server_common::auth_utils::set_org_context(&mut *tx, &tenant_id).await
            {
                tracing::error!("Failed to set org context: {:?}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "internal error"})),
                )
                    .into_response();
            }

            let res = match sqlx::query(
                "UPDATE opportunities SET stage = $1, updated_at = CURRENT_TIMESTAMP WHERE id = $2 AND tenant_id = $3",
            )
            .bind(&payload.stage)
            .bind(&payload.opportunity_id)
            .bind(&tenant_id)
            .execute(&mut *tx)
            .await
            {
                Ok(res) => res,
                Err(e) => {
                    tracing::error!("Failed to update opportunity stage: {:?}", e);
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": "Failed to update"})),
                    )
                        .into_response();
                }
            };

            if let Err(e) = tx.commit().await {
                tracing::error!("Failed to commit transaction: {:?}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "failed to commit"})),
                )
                    .into_response();
            }

            res.rows_affected()
        }
        crate::db::DbStore::Sqlite(sqlite_pool) => match sqlx::query(
            "UPDATE opportunities SET stage = ?1, updated_at = CURRENT_TIMESTAMP WHERE id = ?2 AND tenant_id = ?3",
        )
        .bind(&payload.stage)
        .bind(&payload.opportunity_id)
        .bind(&tenant_id)
        .execute(sqlite_pool)
        .await
        {
            Ok(res) => res.rows_affected(),
            Err(e) => {
                tracing::error!("Failed to update opportunity stage: {:?}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "Failed to update"})),
                )
                    .into_response();
            }
        },
    };

    if rows_affected == 0 {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "opportunity not found"})),
        )
            .into_response();
    }

    if let Some(c) = CRM_OPPORTUNITIES_CACHE.get() {
        let _ = c
            .invalidate(&format!("crm_opportunities:{}:mobile:true", tenant_id))
            .await;
        let _ = c
            .invalidate(&format!("crm_opportunities:{}:mobile:false", tenant_id))
            .await;
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({"status": "success"})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        http::Request,
        routing::{get, post},
    };
    use tower::ServiceExt;

    fn claims_for(org: Option<&str>) -> Claims {
        Claims {
            sub: "user-1".to_string(),
            exp: chrono::Utc::now().timestamp() + 3600,
            iat: chrono::Utc::now().timestamp(),
            organization_id: org.map(str::to_string),
            username: "owner".to_string(),
            email: "owner@example.test".to_string(),
            roles: vec!["ADMIN".to_string()],
            session_id: None,
            jti: uuid::Uuid::new_v4().to_string(),
        }
    }

    async fn sqlite_crm_db() -> (Arc<crate::db::DB>, sqlx::SqlitePool) {
        let sqlite_pool = crate::db::create_sqlite_pool_for_test().await;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS opportunities (
                id TEXT PRIMARY KEY,
                tenant_id TEXT NOT NULL,
                lead_id TEXT,
                title TEXT NOT NULL,
                stage TEXT NOT NULL DEFAULT 'Qualified',
                estimated_value INTEGER,
                priority TEXT,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                updated_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
            )",
        )
        .execute(&sqlite_pool)
        .await
        .unwrap();
        let db = Arc::new(crate::db::DB {
            pool: crate::db::create_dummy_pg_pool().await,
            store: crate::db::DbStore::Sqlite(sqlite_pool.clone()),
        });
        (db, sqlite_pool)
    }

    fn crm_router(db: Arc<crate::db::DB>, claims: Claims) -> Router {
        Router::new()
            .route("/api/v1/ui/opportunities", get(list_opportunities_handler))
            .route(
                "/api/v1/ui/opportunities/stage",
                post(update_opportunity_stage_handler),
            )
            .layer(Extension(claims))
            .with_state(db)
    }

    async fn insert_opportunity(
        pool: &sqlx::SqlitePool,
        id: &str,
        tenant_id: &str,
        title: &str,
        stage: &str,
    ) {
        sqlx::query(
            "INSERT INTO opportunities (id, tenant_id, lead_id, title, stage, estimated_value, priority) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(id)
        .bind(tenant_id)
        .bind(format!("lead-{id}"))
        .bind(title)
        .bind(stage)
        .bind(1500_i64)
        .bind("HIGH")
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn missing_empty_or_system_org_claim_fails_closed() {
        let (db, sqlite_pool) = sqlite_crm_db().await;
        let tenant_a = format!("tenant-a-{}", uuid::Uuid::new_v4());
        insert_opportunity(
            &sqlite_pool,
            "opp-a",
            &tenant_a,
            "Tenant A Deal",
            "Qualified",
        )
        .await;
        insert_opportunity(
            &sqlite_pool,
            "opp-default",
            "default",
            "Default Deal",
            "Qualified",
        )
        .await;

        for (org_claim, expected_status) in [
            (None, StatusCode::UNAUTHORIZED),
            (Some(""), StatusCode::UNAUTHORIZED),
            (Some("   "), StatusCode::UNAUTHORIZED),
            (Some("system"), StatusCode::FORBIDDEN),
            (Some("SYSTEM"), StatusCode::FORBIDDEN),
        ] {
            let app = crm_router(db.clone(), claims_for(org_claim));
            let res = app
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri(format!("/api/v1/ui/opportunities?tenant_id={tenant_a}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(res.status(), expected_status);

            let stage_app = crm_router(db.clone(), claims_for(org_claim));
            let stage_res = stage_app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/ui/opportunities/stage")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({
                                "opportunity_id": "opp-a",
                                "stage": "Won"
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(stage_res.status(), expected_status);
        }

        let stage: String =
            sqlx::query_scalar("SELECT stage FROM opportunities WHERE id = 'opp-a'")
                .fetch_one(&sqlite_pool)
                .await
                .unwrap();
        assert_eq!(stage, "Qualified");
    }

    #[tokio::test]
    async fn query_param_tenant_spoofing_is_rejected() {
        let (db, sqlite_pool) = sqlite_crm_db().await;
        let tenant_a = format!("tenant-a-{}", uuid::Uuid::new_v4());
        let tenant_b = format!("tenant-b-{}", uuid::Uuid::new_v4());
        insert_opportunity(
            &sqlite_pool,
            "opp-a",
            &tenant_a,
            "Tenant A Deal",
            "Qualified",
        )
        .await;
        insert_opportunity(
            &sqlite_pool,
            "opp-b",
            &tenant_b,
            "Tenant B Secret",
            "Proposal",
        )
        .await;

        let app = crm_router(db.clone(), claims_for(Some(&tenant_a)));
        let res = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/api/v1/ui/opportunities?tenant_id={tenant_b}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn tenant_a_cannot_read_or_mutate_tenant_b_opportunities() {
        let (db, sqlite_pool) = sqlite_crm_db().await;
        let tenant_a = format!("tenant-a-{}", uuid::Uuid::new_v4());
        let tenant_b = format!("tenant-b-{}", uuid::Uuid::new_v4());
        insert_opportunity(
            &sqlite_pool,
            "opp-a",
            &tenant_a,
            "Tenant A Deal",
            "Qualified",
        )
        .await;
        insert_opportunity(
            &sqlite_pool,
            "opp-b",
            &tenant_b,
            "Tenant B Deal",
            "Proposal",
        )
        .await;

        // Tenant A sees only its own opportunity.
        let res_a = crm_router(db.clone(), claims_for(Some(&tenant_a)))
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(format!("/api/v1/ui/opportunities?tenant_id={tenant_a}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res_a.status(), StatusCode::OK);
        assert_eq!(
            res_a
                .headers()
                .get("x-total-count")
                .unwrap()
                .to_str()
                .unwrap(),
            "1"
        );
        let body_a = axum::body::to_bytes(res_a.into_body(), usize::MAX)
            .await
            .unwrap();
        let rows_a: Vec<Opportunity> = serde_json::from_slice(&body_a).unwrap();
        assert_eq!(rows_a.len(), 1);
        assert_eq!(rows_a[0].id, "opp-a");
        assert_eq!(rows_a[0].tenant_id, tenant_a);

        // Tenant A cannot mutate Tenant B's opportunity.
        let cross_mutate = crm_router(db.clone(), claims_for(Some(&tenant_a)))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/ui/opportunities/stage")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "opportunity_id": "opp-b",
                            "stage": "ClosedLost"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cross_mutate.status(), StatusCode::NOT_FOUND);
        let stage_b: String =
            sqlx::query_scalar("SELECT stage FROM opportunities WHERE id = 'opp-b'")
                .fetch_one(&sqlite_pool)
                .await
                .unwrap();
        assert_eq!(stage_b, "Proposal");

        // Tenant A can update its own opportunity and cache reflects the change.
        let own_mutate = crm_router(db.clone(), claims_for(Some(&tenant_a)))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/ui/opportunities/stage")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::json!({
                            "opportunity_id": "opp-a",
                            "stage": "ClosedWon"
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(own_mutate.status(), StatusCode::OK);

        let res_a_after = crm_router(db.clone(), claims_for(Some(&tenant_a)))
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v1/ui/opportunities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res_a_after.status(), StatusCode::OK);
        let body_a_after = axum::body::to_bytes(res_a_after.into_body(), usize::MAX)
            .await
            .unwrap();
        let rows_a_after: Vec<Opportunity> = serde_json::from_slice(&body_a_after).unwrap();
        assert_eq!(rows_a_after.len(), 1);
        assert_eq!(rows_a_after[0].stage, "ClosedWon");

        // Mobile optimization strips tenant_id while preserving lead_id and isolation.
        let res_mobile = crm_router(db.clone(), claims_for(Some(&tenant_b)))
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/v1/ui/opportunities?mobile_optimized=true")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res_mobile.status(), StatusCode::OK);
        let body_mobile = axum::body::to_bytes(res_mobile.into_body(), usize::MAX)
            .await
            .unwrap();
        let rows_mobile: Vec<Opportunity> = serde_json::from_slice(&body_mobile).unwrap();
        assert_eq!(rows_mobile.len(), 1);
        assert_eq!(rows_mobile[0].id, "opp-b");
        assert_eq!(rows_mobile[0].tenant_id, "");
        assert_eq!(rows_mobile[0].lead_id.as_deref(), Some("lead-opp-b"));
    }
}
