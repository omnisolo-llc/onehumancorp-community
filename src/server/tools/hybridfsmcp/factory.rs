use super::provider::{FileSystemProvider, LocalFSProvider};
use std::env;
use std::path::PathBuf;
use std::sync::Arc;

pub struct FactoryConfig {
    pub workspace: String,
}

impl Default for FactoryConfig {
    fn default() -> Self {
        Self {
            workspace: env::var("OMNISOLO_LOCAL_WORKSPACE")
                .unwrap_or_else(|_| "./workspace".to_string()),
        }
    }
}

pub fn create_fs_provider_with_config(
    config: &FactoryConfig,
    _tenant_id: Option<String>,
) -> Arc<dyn FileSystemProvider> {
    Arc::new(LocalFSProvider::for_workspace(PathBuf::from(
        &config.workspace,
    )))
}

pub fn create_fs_provider(tenant_id: Option<String>) -> Arc<dyn FileSystemProvider> {
    create_fs_provider_with_config(&FactoryConfig::default(), tenant_id)
}
