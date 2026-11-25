use super::User;
use super::user_repository::UserRepository;
use ::server_common::auth_utils::set_org_context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};

macro_rules! validate_org_id {
    ($org_id:expr) => {
        if $org_id.trim().eq_ignore_ascii_case("system") {
            return Err("tenant_id 'system' cannot be queried directly".into());
        }
        if $org_id.trim().is_empty() {
            return Err("empty tenant_id is not allowed".into());
        }
    };
}

pub struct PgUserRepository {
    pool: PgPool,
}

impl PgUserRepository {
    pub fn new(pool: PgPool) -> Self {
        PgUserRepository { pool }
    }
}

fn roles_from_row(row: &sqlx::postgres::PgRow) -> Result<Vec<String>, String> {
    row.try_get::<Vec<String>, _>("roles")
        .map_err(|error| error.to_string())
}

fn user_from_row(row: &sqlx::postgres::PgRow) -> Result<User, String> {
    Ok(User {
        id: row.get("id"),
        username: row.get("username"),
        email: row.get("email"),
        password_hash: row.get("password_hash"),
        roles: roles_from_row(row)?,
        active: row.get("active"),
        organization_id: row.get("tenant_id"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

#[async_trait]
impl UserRepository for PgUserRepository {
    async fn create_user(&self, user: User, org_id: &str) -> Result<(), String> {
        validate_org_id!(org_id);
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        set_org_context(&mut *tx, org_id)
            .await
            .map_err(|e| e.to_string())?;

        sqlx::query(
            "INSERT INTO users (id, username, email, password_hash, roles, active, tenant_id, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        )
        .bind(&user.id)
        .bind(&user.username)
        .bind(&user.email)
        .bind(&user.password_hash)
        .bind(&user.roles)
        .bind(user.active)
        .bind(org_id)
        .bind(user.created_at)
        .bind(user.updated_at)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn get_by_id(&self, id: &str, org_id: &str) -> Result<User, String> {
        validate_org_id!(org_id);
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        set_org_context(&mut *tx, org_id)
            .await
            .map_err(|e| e.to_string())?;

        let row = sqlx::query(
            "SELECT id, username, email, password_hash, roles, active, tenant_id, created_at, updated_at
             FROM users WHERE id = $1 AND tenant_id = current_setting('app.current_tenant')::text",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "user not found".to_string())?;

        let user = user_from_row(&row)?;
        tx.rollback().await.map_err(|e| e.to_string())?;
        Ok(user)
    }

    async fn get_by_username(&self, username: &str, org_id: &str) -> Result<User, String> {
        validate_org_id!(org_id);
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        set_org_context(&mut *tx, org_id)
            .await
            .map_err(|e| e.to_string())?;

        let row = sqlx::query(
            "SELECT id, username, email, password_hash, roles, active, tenant_id, created_at, updated_at
             FROM users WHERE username = $1 AND tenant_id = current_setting('app.current_tenant')::text",
        )
        .bind(username)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "user not found".to_string())?;

        let user = user_from_row(&row)?;
        tx.rollback().await.map_err(|e| e.to_string())?;
        Ok(user)
    }

    async fn get_by_email(&self, email: &str, org_id: &str) -> Result<User, String> {
        validate_org_id!(org_id);
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        set_org_context(&mut *tx, org_id)
            .await
            .map_err(|e| e.to_string())?;

        let row = sqlx::query(
            "SELECT id, username, email, password_hash, roles, active, tenant_id, created_at, updated_at
             FROM users WHERE email = $1 AND tenant_id = current_setting('app.current_tenant')::text",
        )
        .bind(email)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "user not found".to_string())?;

        let user = user_from_row(&row)?;
        tx.rollback().await.map_err(|e| e.to_string())?;
        Ok(user)
    }

    async fn get_by_login_identifier(
        &self,
        identifier: &str,
        org_id: &str,
    ) -> Result<Option<User>, String> {
        validate_org_id!(org_id);
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        set_org_context(&mut *tx, org_id)
            .await
            .map_err(|e| e.to_string())?;
        let rows = sqlx::query(
            "SELECT id, username, email, password_hash, roles, active, tenant_id, created_at, updated_at
             FROM users
             WHERE (username = $1 OR email = $1)
               AND tenant_id = current_setting('app.current_tenant')::text
               AND active = TRUE
             LIMIT 2",
        )
        .bind(identifier)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
        tx.rollback().await.map_err(|e| e.to_string())?;

        if rows.len() != 1 {
            return Ok(None);
        }
        Ok(Some(user_from_row(&rows[0])?))
    }

    async fn list_users(&self, org_id: &str) -> Result<Vec<User>, String> {
        validate_org_id!(org_id);
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        set_org_context(&mut *tx, org_id)
            .await
            .map_err(|e| e.to_string())?;

        let rows = sqlx::query(
            "SELECT id, username, email, password_hash, roles, active, tenant_id, created_at, updated_at
             FROM users WHERE tenant_id = current_setting('app.current_tenant')::text ORDER BY created_at",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

        let mut users = Vec::with_capacity(rows.len());
        for row in &rows {
            users.push(user_from_row(row)?);
        }
        tx.rollback().await.map_err(|e| e.to_string())?;
        Ok(users)
    }

    async fn update_user(&self, user: User, org_id: &str) -> Result<(), String> {
        validate_org_id!(org_id);
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        set_org_context(&mut *tx, org_id)
            .await
            .map_err(|e| e.to_string())?;

        let res = sqlx::query(
            "UPDATE users SET username=$2, email=$3, password_hash=$4, roles=$5, active=$6, updated_at=$7
             WHERE id=$1 AND tenant_id = current_setting('app.current_tenant')::text RETURNING id",
        )
        .bind(&user.id)
        .bind(&user.username)
        .bind(&user.email)
        .bind(&user.password_hash)
        .bind(&user.roles)
        .bind(user.active)
        .bind(user.updated_at)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

        if res.is_none() {
            return Err("user not found or unauthorized".to_string());
        }

        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn delete_user(&self, id: &str, org_id: &str) -> Result<(), String> {
        validate_org_id!(org_id);
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        set_org_context(&mut *tx, org_id)
            .await
            .map_err(|e| e.to_string())?;

        let res = sqlx::query(
            "DELETE FROM users WHERE id = $1 AND tenant_id = current_setting('app.current_tenant')::text RETURNING id",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

        if res.is_none() {
            return Err("user not found or unauthorized".to_string());
        }

        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn revoke_token(
        &self,
        jti: String,
        exp: DateTime<Utc>,
        org_id: &str,
    ) -> Result<(), String> {
        validate_org_id!(org_id);
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        set_org_context(&mut *tx, org_id)
            .await
            .map_err(|e| e.to_string())?;

        sqlx::query(
            "INSERT INTO revoked_tokens (jti, expires_at, tenant_id) VALUES ($1, $2, $3)
             ON CONFLICT (jti, tenant_id) DO NOTHING",
        )
        .bind(jti)
        .bind(exp)
        .bind(org_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

        let now = chrono::Utc::now();
        sqlx::query(
            "DELETE FROM revoked_tokens WHERE expires_at < $1 AND tenant_id = current_setting('app.current_tenant')::text",
        )
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn is_revoked(&self, jti: &str, org_id: &str) -> Result<bool, String> {
        validate_org_id!(org_id);
        let mut tx = self.pool.begin().await.map_err(|e| e.to_string())?;
        set_org_context(&mut *tx, org_id)
            .await
            .map_err(|e| e.to_string())?;

        let row = sqlx::query(
            "SELECT COUNT(*) FROM revoked_tokens WHERE jti = $1 AND expires_at >= $2 AND tenant_id = current_setting('app.current_tenant')::text",
        )
        .bind(jti)
        .bind(chrono::Utc::now())
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

        let count: i64 = row.get(0);
        tx.rollback().await.map_err(|e| e.to_string())?;
        Ok(count > 0)
    }
}

#[cfg(test)]
mod auth_utils_tests {
    use super::*;

    #[test]
    fn postgres_role_storage_matches_text_array_schema() {
        let source = include_str!("postgres_store.rs");
        let production_source = source
            .split_once("\n#[cfg(test)]\nmod auth_utils_tests")
            .expect("PostgreSQL repository must retain its test boundary")
            .0;
        assert!(!production_source.contains("roles_json"));
        assert!(!production_source.contains("::jsonb"));
        assert!(!production_source.contains("serde_json::from_value"));
        assert!(production_source.contains("row.try_get::<Vec<String>, _>(\"roles\")"));
        assert_eq!(production_source.matches(".bind(&user.roles)").count(), 2);
    }

    #[tokio::test]
    async fn pg_repository_rejects_system_and_empty_workspace_ids() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(std::time::Duration::from_millis(50))
            .connect_lazy("postgres://postgres:postgres@127.0.0.1:5432/nonexistent")
            .unwrap();
        let repo = PgUserRepository::new(pool);
        assert_eq!(
            repo.get_by_id("u1", "system").await.unwrap_err(),
            "tenant_id 'system' cannot be queried directly"
        );
        assert_eq!(
            repo.get_by_id("u1", "").await.unwrap_err(),
            "empty tenant_id is not allowed"
        );
    }
}
