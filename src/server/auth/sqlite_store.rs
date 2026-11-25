use super::User;
use super::user_repository::UserRepository;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::Row;
use sqlx::SqlitePool;

macro_rules! validate_org_id {
    ($org_id:expr) => {
        if $org_id.trim().eq_ignore_ascii_case("system") {
            return Err("tenant_id 'system' cannot be queried directly".to_string());
        }
        if $org_id.trim().is_empty() {
            return Err("empty tenant_id is not allowed".to_string());
        }
    };
}

pub struct SqliteUserRepository {
    pool: SqlitePool,
}

impl SqliteUserRepository {
    pub fn new(pool: SqlitePool) -> Self {
        SqliteUserRepository { pool }
    }
}

fn user_from_row(row: &sqlx::sqlite::SqliteRow) -> User {
    let roles_json: serde_json::Value = row
        .try_get("roles")
        .unwrap_or_else(|_| serde_json::Value::Null);
    let roles: Vec<String> = serde_json::from_value(roles_json).unwrap_or_default();

    User {
        id: row.get("id"),
        username: row.get("username"),
        email: row.get("email"),
        password_hash: row.get("password_hash"),
        roles,
        active: row.get("active"),
        organization_id: row.get("tenant_id"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

#[async_trait]
impl UserRepository for SqliteUserRepository {
    async fn create_user(&self, user: User, org_id: &str) -> Result<(), String> {
        validate_org_id!(org_id);
        let roles_json = serde_json::to_string(&user.roles).unwrap_or_default();

        let query = r#"
        INSERT INTO users (id, username, email, password_hash, roles, active, tenant_id, created_at, updated_at)
        VALUES ($1, $2, $3, $4, json($5), $6, $7, $8, $9)
        "#;

        sqlx::query(query)
            .bind(&user.id)
            .bind(&user.username)
            .bind(&user.email)
            .bind(&user.password_hash)
            .bind(roles_json)
            .bind(user.active)
            .bind(org_id)
            .bind(user.created_at)
            .bind(user.updated_at)
            .execute(&self.pool)
            .await
            .map_err(|e: sqlx::Error| e.to_string())?;

        Ok(())
    }

    async fn get_by_id(&self, id: &str, org_id: &str) -> Result<User, String> {
        validate_org_id!(org_id);
        let query = "SELECT id, username, email, password_hash, roles, active, tenant_id, created_at, updated_at FROM users WHERE id = $1 AND tenant_id = $2";
        let row = sqlx::query(query)
            .bind(id)
            .bind(org_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "user not found".to_string())?;

        Ok(user_from_row(&row))
    }

    async fn get_by_username(&self, username: &str, org_id: &str) -> Result<User, String> {
        validate_org_id!(org_id);
        let query = "SELECT id, username, email, password_hash, roles, active, tenant_id, created_at, updated_at FROM users WHERE username = $1 AND tenant_id = $2";
        let row = sqlx::query(query)
            .bind(username)
            .bind(org_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "user not found".to_string())?;

        Ok(user_from_row(&row))
    }

    async fn get_by_email(&self, email: &str, org_id: &str) -> Result<User, String> {
        validate_org_id!(org_id);
        let query = "SELECT id, username, email, password_hash, roles, active, tenant_id, created_at, updated_at FROM users WHERE email = $1 AND tenant_id = $2";
        let row = sqlx::query(query)
            .bind(email)
            .bind(org_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "user not found".to_string())?;

        Ok(user_from_row(&row))
    }

    async fn get_by_login_identifier(
        &self,
        identifier: &str,
        org_id: &str,
    ) -> Result<Option<User>, String> {
        validate_org_id!(org_id);
        let rows = sqlx::query(
            "SELECT id, username, email, password_hash, roles, active, tenant_id, created_at, updated_at
             FROM users
             WHERE (username = $1 OR email = $1) AND tenant_id = $2 AND active = TRUE
             LIMIT 2",
        )
        .bind(identifier)
        .bind(org_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| e.to_string())?;

        if rows.len() != 1 {
            return Ok(None);
        }
        Ok(Some(user_from_row(&rows[0])))
    }

    async fn list_users(&self, org_id: &str) -> Result<Vec<User>, String> {
        validate_org_id!(org_id);
        let query = "SELECT id, username, email, password_hash, roles, active, tenant_id, created_at, updated_at FROM users WHERE tenant_id = $1 ORDER BY created_at";
        let rows = sqlx::query(query)
            .bind(org_id)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| e.to_string())?;

        Ok(rows.iter().map(user_from_row).collect())
    }

    async fn update_user(&self, user: User, org_id: &str) -> Result<(), String> {
        validate_org_id!(org_id);
        let roles_json = serde_json::to_string(&user.roles).unwrap_or_default();

        let query = r#"
            UPDATE users SET username=$2, email=$3, password_hash=$4, roles=json($5), active=$6,
            updated_at=$7
            WHERE id=$1 AND tenant_id = $8 RETURNING id
            "#;

        let res = sqlx::query(query)
            .bind(&user.id)
            .bind(&user.username)
            .bind(&user.email)
            .bind(&user.password_hash)
            .bind(roles_json)
            .bind(user.active)
            .bind(user.updated_at)
            .bind(org_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| e.to_string())?;

        if res.is_none() {
            return Err("user not found or unauthorized".to_string());
        }
        Ok(())
    }

    async fn delete_user(&self, id: &str, org_id: &str) -> Result<(), String> {
        validate_org_id!(org_id);
        let query = "DELETE FROM users WHERE id = $1 AND tenant_id = $2 RETURNING id";
        let res = sqlx::query(query)
            .bind(id)
            .bind(org_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e: sqlx::Error| e.to_string())?;

        if res.is_none() {
            return Err("user not found or unauthorized".to_string());
        }
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

        sqlx::query(
            r#"
            INSERT INTO revoked_tokens (jti, expires_at, tenant_id) VALUES ($1, $2, $3)
            ON CONFLICT (jti, tenant_id) DO NOTHING
            "#,
        )
        .bind(jti)
        .bind(exp)
        .bind(org_id)
        .execute(&mut *tx)
        .await
        .map_err(|e: sqlx::Error| e.to_string())?;

        let now = chrono::Utc::now();
        sqlx::query("DELETE FROM revoked_tokens WHERE expires_at < $1 AND tenant_id = $2")
            .bind(now)
            .bind(org_id)
            .execute(&mut *tx)
            .await
            .map_err(|e: sqlx::Error| e.to_string())?;

        tx.commit().await.map_err(|e| e.to_string())?;

        Ok(())
    }

    async fn is_revoked(&self, jti: &str, org_id: &str) -> Result<bool, String> {
        validate_org_id!(org_id);

        let row = sqlx::query("SELECT COUNT(*) FROM revoked_tokens WHERE jti = $1 AND expires_at >= $3 AND tenant_id = $2")
            .bind(jti)
            .bind(org_id)
            .bind(chrono::Utc::now())
            .fetch_one(&self.pool)
            .await
            .map_err(|e: sqlx::Error| e.to_string())?;

        let count: i32 = row.get(0);
        Ok(count > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn login_lookup_repo() -> SqliteUserRepository {
        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE users (
                id TEXT PRIMARY KEY, username TEXT NOT NULL, email TEXT NOT NULL,
                password_hash TEXT NOT NULL, roles TEXT NOT NULL, active BOOLEAN NOT NULL,
                tenant_id TEXT NOT NULL,
                created_at TIMESTAMPTZ NOT NULL, updated_at TIMESTAMPTZ NOT NULL
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        for (id, username, email, active, tenant_id) in [
            ("active", "alice", "alice@example.com", true, "tenant-a"),
            (
                "inactive",
                "disabled",
                "disabled@example.com",
                false,
                "tenant-a",
            ),
            ("other", "other", "other@example.com", true, "tenant-b"),
        ] {
            sqlx::query(
                "INSERT INTO users
                 (id, username, email, password_hash, roles, active, tenant_id, created_at, updated_at)
                 VALUES ($1, $2, $3, 'hash', '[]', $4, $5, $6, $6)",
            )
            .bind(id)
            .bind(username)
            .bind(email)
            .bind(active)
            .bind(tenant_id)
            .bind(Utc::now())
            .execute(&pool)
            .await
            .unwrap();
        }
        SqliteUserRepository::new(pool)
    }

    #[tokio::test]
    async fn sqlite_login_identifier_lookup_requires_active_tenant_user() {
        let repo = login_lookup_repo().await;
        assert_eq!(
            repo.get_by_login_identifier("alice", "tenant-a")
                .await
                .unwrap()
                .map(|user| user.id),
            Some("active".to_string())
        );
        assert_eq!(
            repo.get_by_login_identifier("alice@example.com", "tenant-a")
                .await
                .unwrap()
                .map(|user| user.id),
            Some("active".to_string())
        );
        assert!(
            repo.get_by_login_identifier("disabled@example.com", "tenant-a")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            repo.get_by_login_identifier("other@example.com", "tenant-a")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn test_sqlite_create_user_organization_id_parity() {
        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();

        sqlx::query(
            "CREATE TABLE users (
                id TEXT PRIMARY KEY,
                username TEXT NOT NULL,
                email TEXT NOT NULL,
                password_hash TEXT,
                roles TEXT,
                active BOOLEAN,
                tenant_id TEXT,
                created_at TIMESTAMPTZ,
                updated_at TIMESTAMPTZ
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        let repo = SqliteUserRepository::new(pool.clone());
        let user = User {
            id: "test-id".to_string(),
            username: "test-user".to_string(),
            email: "test@example.com".to_string(),
            password_hash: "".to_string(),
            roles: vec!["admin".to_string()],
            active: true,
            organization_id: Some("user-org-id".to_string()),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };

        repo.create_user(user, "function-arg-org-id").await.unwrap();

        let row = sqlx::query("SELECT tenant_id FROM users WHERE id = 'test-id'")
            .fetch_one(&pool)
            .await
            .unwrap();

        let fetched_org_id: String = sqlx::Row::get(&row, "tenant_id");
        assert_eq!(fetched_org_id, "function-arg-org-id");
    }

    #[tokio::test]
    async fn test_sqlite_revoke_token_tenant_isolation_and_gc() {
        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();

        sqlx::query(
            "CREATE TABLE revoked_tokens (
                jti TEXT NOT NULL,
                expires_at TIMESTAMPTZ NOT NULL,
                tenant_id TEXT NOT NULL,
                PRIMARY KEY (jti, tenant_id)
            )",
        )
        .execute(&pool)
        .await
        .unwrap();

        let repo = SqliteUserRepository::new(pool.clone());
        let now = Utc::now();
        let exp1 = now - chrono::Duration::hours(1);
        let exp2 = now + chrono::Duration::hours(1);

        sqlx::query("INSERT INTO revoked_tokens (jti, expires_at, tenant_id) VALUES ($1, $2, $3)")
            .bind("jti-1")
            .bind(exp1)
            .bind("tenant-1")
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("INSERT INTO revoked_tokens (jti, expires_at, tenant_id) VALUES ($1, $2, $3)")
            .bind("jti-2")
            .bind(exp1)
            .bind("tenant-2")
            .execute(&pool)
            .await
            .unwrap();

        let _ = repo
            .revoke_token("jti-3".to_string(), exp2, "tenant-1")
            .await;

        let count: i64 =
            sqlx::query("SELECT COUNT(*) FROM revoked_tokens WHERE tenant_id = 'tenant-2'")
                .fetch_one(&pool)
                .await
                .unwrap()
                .get(0);
        assert_eq!(count, 1, "GC leak across tenants");

        let count_tenant_1: i64 = sqlx::query(
            "SELECT COUNT(*) FROM revoked_tokens WHERE tenant_id = 'tenant-1' AND jti != 'jti-3'",
        )
        .fetch_one(&pool)
        .await
        .unwrap()
        .get(0);
        assert_eq!(
            count_tenant_1, 0,
            "The expired token for tenant-1 should be garbage collected"
        );
    }

    #[tokio::test]
    async fn test_sqlite_system_and_empty_workspace_rejected() {
        let pool = SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();

        let repo = SqliteUserRepository::new(pool);
        let res = repo.get_by_id("dummy_id", "system").await;
        assert_eq!(
            res.unwrap_err(),
            "tenant_id 'system' cannot be queried directly"
        );
        let res_empty = repo.get_by_id("dummy_id", "  ").await;
        assert_eq!(res_empty.unwrap_err(), "empty tenant_id is not allowed");
    }
}
