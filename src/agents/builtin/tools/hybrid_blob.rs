#![allow(clippy::collapsible_if)]
use super::{Tool, ToolExecutor, tenant::TenantContext, workspace_path};
use omnisolo_builtin_agent_core::types::ToolError;
use serde_json::{Value, json};
use std::env;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[async_trait::async_trait]
pub trait BlobManager: Send + Sync {
    async fn write_blob(&self, key: &str, data: &[u8]) -> Result<(), String>;
    async fn read_blob(&self, key: &str) -> Result<Vec<u8>, String>;
}

fn sanitize_tenant_segment(tenant_id: &str) -> String {
    let mut out = String::with_capacity(tenant_id.len());
    for ch in tenant_id.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "default".to_string()
    } else {
        out
    }
}

pub struct HybridBlobManager {
    is_cloud: bool,
    tenant_prefix: String,
    local_dir: Option<PathBuf>,
    cloud_store: dashmap::DashMap<String, Vec<u8>>,
}

impl HybridBlobManager {
    pub async fn new_for_workspace(working_dir: Option<PathBuf>, tenant: &TenantContext) -> Self {
        let s3_endpoint = env::var("S3_ENDPOINT").unwrap_or_default();
        let is_cloud = !s3_endpoint.is_empty();
        let tenant_segment = sanitize_tenant_segment(tenant.as_str());

        let local_dir = if !is_cloud {
            resolve_tenant_blob_dir(working_dir, &tenant_segment)
        } else {
            None
        };

        let manager = Self {
            is_cloud,
            tenant_prefix: tenant_segment,
            local_dir: local_dir.clone(),
            cloud_store: dashmap::DashMap::new(),
        };

        if let Some(dir) = local_dir {
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
                loop {
                    interval.tick().await;
                    Self::cleanup_old_blobs(&dir, std::time::Duration::from_secs(86400)).await;
                }
            });
        }

        manager
    }

    pub async fn cleanup_old_blobs(dir: &std::path::Path, max_age: std::time::Duration) {
        if let Ok(mut entries) = tokio::fs::read_dir(dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                let path = entry.path();
                if let Ok(metadata) = tokio::fs::symlink_metadata(&path).await {
                    if metadata.file_type().is_symlink() {
                        continue;
                    }
                    if metadata.is_file() {
                        if let Ok(modified) = metadata.modified()
                            && let Ok(elapsed) = modified.elapsed()
                            && elapsed > max_age
                        {
                            let _ = tokio::fs::remove_file(&path).await;
                        }
                    } else if metadata.is_dir() {
                        Box::pin(Self::cleanup_old_blobs(&path, max_age)).await;
                    }
                }
            }
        }
    }

    pub async fn new_for_test(local_dir: PathBuf) -> Self {
        if let Err(e) = fs::create_dir_all(&local_dir).await {
            tracing::error!("Failed to create test local blob directory: {}", e);
        }
        Self {
            is_cloud: false,
            tenant_prefix: "test-tenant".to_string(),
            local_dir: Some(local_dir),
            cloud_store: dashmap::DashMap::new(),
        }
    }

    pub fn new_cloud_for_test() -> Self {
        Self {
            is_cloud: true,
            tenant_prefix: "test-tenant".to_string(),
            local_dir: None,
            cloud_store: dashmap::DashMap::new(),
        }
    }

    fn sanitize_key(key: &str) -> Result<String, String> {
        if key.trim().is_empty()
            || key.contains("..")
            || key.starts_with('/')
            || key.starts_with('\\')
            || key.contains('\0')
        {
            return Err("Invalid blob key: path traversal not allowed".to_string());
        }
        Ok(key.to_string())
    }
}

fn resolve_tenant_blob_dir(working_dir: Option<PathBuf>, tenant_segment: &str) -> Option<PathBuf> {
    let base = if let Some(wd) = working_dir {
        let canonical_wd = std::fs::canonicalize(&wd).ok()?;
        canonical_wd.join(".omnisolo-blobs")
    } else if let Ok(configured) = env::var("OMNISOLO_BLOB_DIR")
        && !configured.trim().is_empty()
    {
        PathBuf::from(configured.trim())
    } else {
        // Fail closed: never default to a world-shared /tmp/omnisolo_blobs directory across tenants.
        return None;
    };

    let tenant_dir = base.join(tenant_segment);
    if let Err(e) = std::fs::create_dir_all(&tenant_dir) {
        tracing::error!("Failed to create tenant blob directory: {}", e);
        return None;
    }
    std::fs::canonicalize(&tenant_dir).ok()
}

#[async_trait::async_trait]
impl BlobManager for HybridBlobManager {
    async fn write_blob(&self, key: &str, data: &[u8]) -> Result<(), String> {
        let safe_key = Self::sanitize_key(key)?;

        if self.is_cloud {
            let scoped_key = format!("{}/{}", self.tenant_prefix, safe_key);
            tracing::debug!("Cloud: Writing to scoped key {}", scoped_key); // pii-safe
            self.cloud_store.insert(scoped_key, data.to_vec());
            Ok(())
        } else {
            let local_dir = self
                .local_dir
                .as_ref()
                .ok_or("Local tenant blob directory is not configured")?;
            let path = workspace_path::resolve_workspace_path(local_dir, &safe_key, false)
                .await
                .map_err(|e| e.to_string())?;

            if let Some(parent) = path.parent() {
                if let Err(e) = fs::create_dir_all(parent).await {
                    return Err(format!("Failed to create directories for blob: {}", e));
                }
            }
            let verified = workspace_path::resolve_workspace_path(local_dir, &safe_key, false)
                .await
                .map_err(|e| e.to_string())?;

            let mut file = fs::File::create(&verified)
                .await
                .map_err(|e| format!("Failed to create file: {}", e))?;
            file.write_all(data)
                .await
                .map_err(|e| format!("Failed to write data: {}", e))?;

            Ok(())
        }
    }

    async fn read_blob(&self, key: &str) -> Result<Vec<u8>, String> {
        let safe_key = Self::sanitize_key(key)?;

        if self.is_cloud {
            let scoped_key = format!("{}/{}", self.tenant_prefix, safe_key);
            tracing::debug!("Cloud: Reading from scoped key {}", scoped_key); // pii-safe
            if let Some(data) = self.cloud_store.get(&scoped_key) {
                Ok(data.clone())
            } else {
                Err("Blob not found in cloud store".to_string())
            }
        } else {
            let local_dir = self
                .local_dir
                .as_ref()
                .ok_or("Local tenant blob directory is not configured")?;
            let path = workspace_path::resolve_workspace_path(local_dir, &safe_key, true)
                .await
                .map_err(|e| e.to_string())?;

            let mut file = fs::File::open(&path)
                .await
                .map_err(|e| format!("Failed to open file: {}", e))?;
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer)
                .await
                .map_err(|e| format!("Failed to read data: {}", e))?;

            Ok(buffer)
        }
    }
}

struct HybridBlobExecutor {
    manager: Arc<dyn BlobManager>,
}

#[async_trait::async_trait]
impl ToolExecutor for HybridBlobExecutor {
    async fn execute(&self, args: Value) -> Result<String, ToolError> {
        let action = args["Action"].as_str().ok_or_else(|| {
            ToolError::LlmRecoverable("hybrid_blob: Action is required".to_string())
        })?;
        let key = args["Key"]
            .as_str()
            .ok_or_else(|| ToolError::LlmRecoverable("hybrid_blob: Key is required".to_string()))?;

        match action {
            "read" => {
                let data = self
                    .manager
                    .read_blob(key)
                    .await
                    .map_err(ToolError::LlmRecoverable)?;
                let content = if let Ok(s) = String::from_utf8(data.clone()) {
                    s
                } else {
                    use base64::{Engine as _, engine::general_purpose};
                    general_purpose::STANDARD.encode(&data)
                };

                Ok(json!({
                    "status": "read",
                    "key": key,
                    "data": content
                })
                .to_string())
            }
            "write" => {
                let data_str = args["Data"].as_str().ok_or_else(|| {
                    ToolError::LlmRecoverable("hybrid_blob: Data is required for write".to_string())
                })?;
                let data_bytes = data_str.as_bytes();

                self.manager
                    .write_blob(key, data_bytes)
                    .await
                    .map_err(ToolError::LlmRecoverable)?;

                Ok(json!({
                    "status": "written",
                    "key": key
                })
                .to_string())
            }
            _ => Err(ToolError::LlmRecoverable("invalid action".to_string())),
        }
    }
}

pub fn hybrid_blob_tool(working_dir: Option<PathBuf>, tenant: TenantContext) -> Tool {
    let s3_endpoint = env::var("S3_ENDPOINT").unwrap_or_default();
    let is_cloud = !s3_endpoint.is_empty();
    let tenant_segment = sanitize_tenant_segment(tenant.as_str());

    let local_dir = if !is_cloud {
        resolve_tenant_blob_dir(working_dir, &tenant_segment)
    } else {
        None
    };

    let manager = Arc::new(HybridBlobManager {
        is_cloud,
        tenant_prefix: tenant_segment,
        local_dir,
        cloud_store: dashmap::DashMap::new(),
    });

    Tool {
        name: "hybrid_blob".to_string(),
        description: "Reads and writes tenant-scoped blobs inside the workspace".to_string(),
        is_read_only: false,
        parameters: json!({
            "type": "object",
            "properties": {
                "Action": {
                    "type": "string",
                    "description": "Action to perform: read, write"
                },
                "Key": {
                    "type": "string",
                    "description": "Blob key"
                },
                "Data": {
                    "type": "string",
                    "description": "Data to write (required for write action)"
                }
            },
            "required": ["Action", "Key"]
        }),
        execute: Arc::new(HybridBlobExecutor { manager }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_local_blob_write_read() {
        let dir = tempdir().unwrap();
        let manager = HybridBlobManager::new_for_test(dir.path().to_path_buf()).await;

        let key = "test/image.png";
        let data = b"png bytes";

        manager.write_blob(key, data).await.unwrap();
        let read_data = manager.read_blob(key).await.unwrap();

        assert_eq!(read_data, data);

        let path = dir.path().join(key);
        assert!(path.exists());
    }

    #[tokio::test]
    async fn test_path_traversal_and_symlink_escape_prevention() {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let outside_file = outside.path().join("secret.txt");
        tokio::fs::write(&outside_file, b"secret").await.unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside_file, dir.path().join("escape.txt")).unwrap();

        let manager = HybridBlobManager::new_for_test(dir.path().to_path_buf()).await;

        let res = manager.write_blob("../../../etc/passwd", b"hack").await;
        assert!(res.is_err());
        assert!(res.unwrap_err().contains("path traversal not allowed"));

        assert!(manager.read_blob("escape.txt").await.is_err());
        assert!(
            manager
                .write_blob("escape.txt", b"overwrite")
                .await
                .is_err()
        );
        assert_eq!(tokio::fs::read(&outside_file).await.unwrap(), b"secret");
    }

    #[tokio::test]
    async fn test_hybrid_blob_isolates_tenants_and_fails_closed_without_root() {
        temp_env::async_with_vars(
            [
                ("S3_ENDPOINT", None::<&str>),
                ("OMNISOLO_BLOB_DIR", None::<&str>),
            ],
            async {
                let unconfined = hybrid_blob_tool(None, TenantContext::new("tenant-a").unwrap());
                assert!(
                    unconfined
                        .execute
                        .execute(json!({"Action": "write", "Key": "k", "Data": "v"}))
                        .await
                        .is_err()
                );

                let workspace = tempdir().unwrap();
                let tool_a = hybrid_blob_tool(
                    Some(workspace.path().to_path_buf()),
                    TenantContext::new("tenant-a").unwrap(),
                );
                let tool_b = hybrid_blob_tool(
                    Some(workspace.path().to_path_buf()),
                    TenantContext::new("tenant-b").unwrap(),
                );
                tool_a
                    .execute
                    .execute(json!({"Action": "write", "Key": "shared.txt", "Data": "alpha"}))
                    .await
                    .unwrap();
                assert!(
                    tool_b
                        .execute
                        .execute(json!({"Action": "read", "Key": "shared.txt"}))
                        .await
                        .is_err()
                );
            },
        )
        .await;
    }

    #[tokio::test]
    async fn test_cleanup_old_blobs() {
        let dir = tempdir().unwrap();
        let path1 = dir.path().join("new.txt");
        let path2 = dir.path().join("old.txt");

        tokio::fs::write(&path1, "new").await.unwrap();
        tokio::fs::write(&path2, "old").await.unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        HybridBlobManager::cleanup_old_blobs(dir.path(), std::time::Duration::from_millis(0)).await;

        assert!(!path1.exists());
        assert!(!path2.exists());
    }

    #[tokio::test]
    async fn test_cloud_store_roundtrip() {
        let manager = HybridBlobManager::new_cloud_for_test();

        let key = "org-1/test.txt";
        let data = b"cloud data";

        manager.write_blob(key, data).await.unwrap();
        let read_data = manager.read_blob(key).await.unwrap();

        assert_eq!(read_data, b"cloud data");
    }
}
