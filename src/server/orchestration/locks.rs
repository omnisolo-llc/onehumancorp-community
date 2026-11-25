use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, OwnedMutexGuard};

#[async_trait::async_trait]
pub trait DistributedLock: Send + Sync {
    async fn acquire(&self, task_id: &str) -> Result<LockGuard, String>;
    async fn acquire_resource(
        &self,
        tenant_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<LockGuard, String>;
}

pub struct LockGuard {
    _local_guard: Option<OwnedMutexGuard<()>>,
    lock_key: Option<String>,
    lock_val: Option<String>,
    sqlite_pool: Option<sqlx::SqlitePool>,
    released: bool,
}

impl LockGuard {
    pub async fn release(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        if let (Some(pool), Some(key), Some(val)) =
            (&self.sqlite_pool, &self.lock_key, &self.lock_val)
        {
            let _ = sqlx::query("DELETE FROM distributed_locks WHERE id = $1 AND lock_val = $2")
                .bind(key)
                .bind(val)
                .execute(pool)
                .await;
        }
        self._local_guard.take();
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if !self.released && self.sqlite_pool.is_some() {
            // Warning: Dropped without release. The distributed lock will expire via TTL naturally.
        }
    }
}

pub struct StandaloneLock {
    pool: Option<sqlx::SqlitePool>,
    pub local_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl Default for StandaloneLock {
    fn default() -> Self {
        Self::new()
    }
}

impl StandaloneLock {
    pub fn new() -> Self {
        Self {
            pool: None,
            local_locks: Mutex::new(HashMap::new()),
        }
    }

    pub fn with_pool(pool: sqlx::SqlitePool) -> Self {
        Self {
            pool: Some(pool),
            local_locks: Mutex::new(HashMap::new()),
        }
    }

    async fn do_acquire(&self, key: &str, tenant_id: &str) -> Result<LockGuard, String> {
        let val = uuid::Uuid::new_v4().to_string();

        if let Some(pool) = &self.pool {
            // First cleanup expired locks
            let _ =
                sqlx::query("DELETE FROM distributed_locks WHERE expires_at < CURRENT_TIMESTAMP")
                    .execute(pool)
                    .await;

            let result = sqlx::query("INSERT INTO distributed_locks (id, tenant_id, lock_val, expires_at) VALUES ($1, $2, $3, datetime('now', '+15 seconds'))")
                .bind(key)
                .bind(tenant_id)
                .bind(&val)
                .execute(pool)
                .await;

            if result.is_ok() {
                return Ok(LockGuard {
                    _local_guard: None,
                    lock_key: Some(key.to_string()),
                    lock_val: Some(val),
                    sqlite_pool: Some(pool.clone()),
                    released: false,
                });
            } else {
                return Err("Failed to acquire SQLite lock".to_string());
            }
        }

        // Fallback to local memory lock if no pool is provided
        let task_mutex = {
            let mut locks = self.local_locks.lock().await;
            locks
                .entry(key.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };

        let guard = task_mutex.lock_owned().await;
        Ok(LockGuard {
            _local_guard: Some(guard),
            lock_key: None,
            lock_val: None,
            sqlite_pool: None,
            released: false,
        })
    }
}

#[async_trait::async_trait]
impl DistributedLock for StandaloneLock {
    async fn acquire(&self, task_id: &str) -> Result<LockGuard, String> {
        self.do_acquire(&format!("ohc:lock:task:{}", task_id), "system")
            .await
    }

    async fn acquire_resource(
        &self,
        tenant_id: &str,
        resource_type: &str,
        resource_id: &str,
    ) -> Result<LockGuard, String> {
        self.do_acquire(
            &format!("ohc:lock:{}:{}:{}", tenant_id, resource_type, resource_id),
            tenant_id,
        )
        .await
    }
}
