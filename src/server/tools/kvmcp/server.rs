use crate::db::{DB, DbStore};
use crate::ohc::orchestration::{McpInvokeRequest, McpInvokeResponse, McpToolProto};
use std::sync::Arc;
use tracing::Instrument;

pub struct KvMcpServer {
    db: Arc<DB>,
}

impl KvMcpServer {
    pub fn new(db: Arc<DB>) -> Self {
        Self { db }
    }

    pub fn get_tools(&self) -> Vec<McpToolProto> {
        vec![
            McpToolProto {
                id: "kv_get".to_string(),
                name: "KV Get".to_string(),
                description: "Get a value from the KV store. Input schema: {\"type\":\"object\",\"properties\":{\"key\":{\"type\":\"string\"}}}".to_string(),
                category: "kv".to_string(),
                status: "active".to_string(),
            },
            McpToolProto {
                id: "kv_set".to_string(),
                name: "KV Set".to_string(),
                description: "Set a value in the KV store. Input schema: {\"type\":\"object\",\"properties\":{\"key\":{\"type\":\"string\"},\"value\":{\"type\":\"string\"}}}".to_string(),
                category: "kv".to_string(),
                status: "active".to_string(),
            },
            McpToolProto {
                id: "kv_delete".to_string(),
                name: "KV Delete".to_string(),
                description: "Delete a value from the KV store. Input schema: {\"type\":\"object\",\"properties\":{\"key\":{\"type\":\"string\"}}}".to_string(),
                category: "kv".to_string(),
                status: "active".to_string(),
            },
            McpToolProto {
                id: "kv_list".to_string(),
                name: "KV List".to_string(),
                description: "List keys in the KV store with an optional prefix. Input schema: {\"type\":\"object\",\"properties\":{\"prefix\":{\"type\":\"string\"}}}".to_string(),
                category: "kv".to_string(),
                status: "active".to_string(),
            },
        ]
    }

    pub fn get_tenant_id(&self, identity_str: &str) -> Result<String, crate::rpc_error::RpcError> {
        let tenant_id = identity_str
            .split("/org/")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| tonic::Status::unauthenticated("missing workspace identity"))?
            .to_string();
        Ok(tenant_id)
    }

    pub async fn invoke_tool(
        &self,
        req: &McpInvokeRequest,
    ) -> Result<McpInvokeResponse, tonic::Status> {
        let params: serde_json::Value = serde_json::from_str(&req.params)
            .map_err(|e| tonic::Status::invalid_argument(format!("invalid JSON params: {}", e)))?;

        let tenant_id = self.get_tenant_id(&req.spiffe_id)?;

        match req.tool_id.as_str() {
            "kv_get" => {
                let key = params["key"]
                    .as_str()
                    .ok_or_else(|| tonic::Status::invalid_argument("key is required"))?;
                async {
                    let row: Result<(String,), sqlx::Error> = match &self.db.store {
                        DbStore::Sqlite(pool) => {
                            sqlx::query_as(
                                "SELECT kv_value FROM agent_kv_store WHERE tenant_id = ? AND kv_key = ?",
                            )
                            .bind(&tenant_id)
                            .bind(key)
                            .fetch_one(pool)
                            .await
                        }
                        DbStore::Postgres => {
                            sqlx::query_as(
                                "SELECT kv_value FROM agent_kv_store WHERE tenant_id = $1 AND kv_key = $2",
                            )
                            .bind(&tenant_id)
                            .bind(key)
                            .fetch_one(&self.db.pool)
                            .await
                        }
                    };

                    match row {
                        Ok((val,)) => {
                            let resp = serde_json::json!({"value": val});
                            Ok(McpInvokeResponse {
                                payload: serde_json::to_string(&resp).unwrap(),
                            })
                        }
                        Err(sqlx::Error::RowNotFound) => {
                            let resp = serde_json::json!({"value": null});
                            Ok(McpInvokeResponse {
                                payload: serde_json::to_string(&resp).unwrap(),
                            })
                        }
                        Err(e) => Err(tonic::Status::internal(format!("db error: {}", e))),
                    }
                }
                .instrument(tracing::info_span!("kv_get"))
                .await
            }
            "kv_set" => {
                let key = params["key"]
                    .as_str()
                    .ok_or_else(|| tonic::Status::invalid_argument("key is required"))?;
                let value = params["value"]
                    .as_str()
                    .ok_or_else(|| tonic::Status::invalid_argument("value is required"))?;
                async {
                    match &self.db.store {
                        DbStore::Sqlite(pool) => {
                            sqlx::query("INSERT INTO agent_kv_store (tenant_id, kv_key, kv_value, updated_at) VALUES (?, ?, ?, CURRENT_TIMESTAMP) ON CONFLICT(tenant_id, kv_key) DO UPDATE SET kv_value = excluded.kv_value, updated_at = CURRENT_TIMESTAMP")
                                .bind(&tenant_id)
                                .bind(key)
                                .bind(value)
                                .execute(pool)
                                .await
                                .map_err(|e| tonic::Status::internal(format!("db error: {}", e)))?;
                        }
                        DbStore::Postgres => {
                            sqlx::query("INSERT INTO agent_kv_store (tenant_id, kv_key, kv_value, updated_at) VALUES ($1, $2, $3, CURRENT_TIMESTAMP) ON CONFLICT (tenant_id, kv_key) DO UPDATE SET kv_value = EXCLUDED.kv_value, updated_at = CURRENT_TIMESTAMP")
                                .bind(&tenant_id)
                                .bind(key)
                                .bind(value)
                                .execute(&self.db.pool)
                                .await
                                .map_err(|e| tonic::Status::internal(format!("db error: {}", e)))?;
                        }
                    }
                    let resp = serde_json::json!({"status": "success"});
                    Ok(McpInvokeResponse {
                        payload: serde_json::to_string(&resp).unwrap(),
                    })
                }
                .instrument(tracing::info_span!("kv_set"))
                .await
            }
            "kv_delete" => {
                let key = params["key"]
                    .as_str()
                    .ok_or_else(|| tonic::Status::invalid_argument("key is required"))?;
                async {
                    match &self.db.store {
                        DbStore::Sqlite(pool) => {
                            sqlx::query(
                                "DELETE FROM agent_kv_store WHERE tenant_id = ? AND kv_key = ?",
                            )
                            .bind(&tenant_id)
                            .bind(key)
                            .execute(pool)
                            .await
                            .map_err(|e| tonic::Status::internal(format!("db error: {}", e)))?;
                        }
                        DbStore::Postgres => {
                            sqlx::query(
                                "DELETE FROM agent_kv_store WHERE tenant_id = $1 AND kv_key = $2",
                            )
                            .bind(&tenant_id)
                            .bind(key)
                            .execute(&self.db.pool)
                            .await
                            .map_err(|e| tonic::Status::internal(format!("db error: {}", e)))?;
                        }
                    }
                    let resp = serde_json::json!({"status": "success"});
                    Ok(McpInvokeResponse {
                        payload: serde_json::to_string(&resp).unwrap(),
                    })
                }
                .instrument(tracing::info_span!("kv_delete"))
                .await
            }
            "kv_list" => {
                let prefix = params["prefix"].as_str().unwrap_or("");
                async {
                    let like_pattern = format!("{}%", prefix);
                    let rows: Result<Vec<(String,)>, sqlx::Error> = match &self.db.store {
                        DbStore::Sqlite(pool) => {
                            sqlx::query_as(
                                "SELECT kv_key FROM agent_kv_store WHERE tenant_id = ? AND kv_key LIKE ?",
                            )
                            .bind(&tenant_id)
                            .bind(&like_pattern)
                            .fetch_all(pool)
                            .await
                        }
                        DbStore::Postgres => {
                            sqlx::query_as(
                                "SELECT kv_key FROM agent_kv_store WHERE tenant_id = $1 AND kv_key LIKE $2",
                            )
                            .bind(&tenant_id)
                            .bind(&like_pattern)
                            .fetch_all(&self.db.pool)
                            .await
                        }
                    };

                    match rows {
                        Ok(results) => {
                            let keys: Vec<String> = results.into_iter().map(|r| r.0).collect();
                            let resp = serde_json::json!({"keys": keys});
                            Ok(McpInvokeResponse {
                                payload: serde_json::to_string(&resp).unwrap(),
                            })
                        }
                        Err(e) => Err(tonic::Status::internal(format!("db error: {}", e))),
                    }
                }
                .instrument(tracing::info_span!("kv_list"))
                .await
            }
            _ => Err(tonic::Status::not_found(format!(
                "tool {} not implemented",
                req.tool_id
            ))),
        }
    }
}
