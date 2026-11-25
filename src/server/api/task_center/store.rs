//! Tenant-isolated persistent store for Omni Assistant & OHC Task Center Spaces,
//! Steps, Artifacts, Approvals, and Standing Authority Policies.
use super::rows::{
    ApprovalRow, ArtifactRow, PolicyRow, SELECT_APPROVAL, SELECT_ARTIFACT, SELECT_SPACE,
    SELECT_STEP, SELECT_TASK, SpaceRow, StepRow, TaskRow, decode_task,
};
pub use super::types::*;
use crate::db::{DB, DbStore};
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

macro_rules! with_tenant_tx {
    ($store:expr, $tenant_id:expr, $tx:ident, $body:block) => {{
        match &$store.db.store {
            DbStore::Postgres => {
                let mut $tx = $store.db.pool.begin().await?;
                ::server_common::auth_utils::set_org_context(&mut *$tx, $tenant_id).await?;
                $body
            }
            DbStore::Sqlite(pool) => {
                let mut $tx = pool.begin().await?;
                $body
            }
        }
    }};
}

macro_rules! push_step_tx {
    ($tx:expr, $tenant_id:expr, $task_id:expr, $step_id:expr, $kind:expr, $title:expr, $detail:expr, $status:expr, $prov:expr, $tool:expr, $cost:expr, $now:expr) => {{
        let max_seq: Option<(i32,)> = sqlx::query_as(
            "SELECT COALESCE(MAX(seq), 0) FROM task_center_steps WHERE tenant_id = $1 AND task_id = $2",
        )
        .bind($tenant_id)
        .bind($task_id)
        .fetch_optional(&mut *$tx)
        .await?;
        let next_seq = max_seq.map(|(n,)| n).unwrap_or(0).saturating_add(1);
        sqlx::query(
            "INSERT INTO task_center_steps (id, tenant_id, task_id, seq, kind, title, detail, status, provenance, tool_name, cost_micros, created_at_unix) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)",
        )
        .bind($step_id)
        .bind($tenant_id)
        .bind($task_id)
        .bind(next_seq)
        .bind($kind)
        .bind($title)
        .bind($detail)
        .bind($status)
        .bind($prov)
        .bind($tool)
        .bind($cost)
        .bind($now)
        .execute(&mut *$tx)
        .await?;
        next_seq
    }};
}

#[derive(Clone)]
pub struct TaskCenterStore {
    db: Arc<DB>,
}

impl TaskCenterStore {
    pub fn new(db: Arc<DB>) -> Self {
        Self { db }
    }

    pub async fn list_spaces(
        &self,
        tenant_id: &str,
        include_archived: bool,
    ) -> Result<Vec<SpaceRecord>, StoreError> {
        let sql = format!(
            "{SELECT_SPACE} WHERE tenant_id = $1 AND ($2 OR archived = FALSE) ORDER BY updated_at_unix DESC, id ASC"
        );
        with_tenant_tx!(self, tenant_id, tx, {
            let rows: Vec<SpaceRow> = sqlx::query_as(&sql)
                .bind(tenant_id)
                .bind(include_archived)
                .fetch_all(&mut *tx)
                .await?;
            tx.commit().await?;
            rows.into_iter().map(TryInto::try_into).collect()
        })
    }

    pub async fn get_space(
        &self,
        tenant_id: &str,
        space_id: &str,
    ) -> Result<Option<SpaceRecord>, StoreError> {
        let sql = format!("{SELECT_SPACE} WHERE tenant_id = $1 AND id = $2");
        with_tenant_tx!(self, tenant_id, tx, {
            let row: Option<SpaceRow> = sqlx::query_as(&sql)
                .bind(tenant_id)
                .bind(space_id)
                .fetch_optional(&mut *tx)
                .await?;
            tx.commit().await?;
            row.map(TryInto::try_into).transpose()
        })
    }

    pub async fn create_space(
        &self,
        tenant_id: &str,
        s: &SpaceRecord,
    ) -> Result<SpaceRecord, StoreError> {
        let reads = serde_json::to_string(&s.reads).map_err(|_| StoreError::Invalid("reads"))?;
        let shows = serde_json::to_string(&s.shows).map_err(|_| StoreError::Invalid("shows"))?;
        let cannot = serde_json::to_string(&s.cannot).map_err(|_| StoreError::Invalid("cannot"))?;
        let pinned = serde_json::to_string(&s.pinned_artifact_ids)
            .map_err(|_| StoreError::Invalid("pinned"))?;
        with_tenant_tx!(self, tenant_id, tx, {
            sqlx::query("INSERT INTO task_center_spaces (id, tenant_id, name, description, kind, prompt_template, department, reads_json, shows_json, cannot_json, source_health, pinned_artifact_ids_json, archived, created_at_unix, updated_at_unix) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)")
                .bind(&s.id).bind(tenant_id).bind(&s.name).bind(&s.description).bind(s.kind.as_str())
                .bind(&s.prompt_template).bind(&s.department).bind(&reads).bind(&shows).bind(&cannot)
                .bind(s.source_health.as_str()).bind(&pinned).bind(s.archived)
                .bind(s.created_at_unix).bind(s.updated_at_unix).execute(&mut *tx).await?;
            tx.commit().await?;
            Ok(s.clone())
        })
    }

    pub async fn patch_space(
        &self,
        tenant_id: &str,
        space_id: &str,
        patch: SpacePatch,
        now: i64,
    ) -> Result<SpaceRecord, StoreError> {
        let sql = format!("{SELECT_SPACE} WHERE tenant_id = $1 AND id = $2");
        with_tenant_tx!(self, tenant_id, tx, {
            let row: Option<SpaceRow> = sqlx::query_as(&sql)
                .bind(tenant_id)
                .bind(space_id)
                .fetch_optional(&mut *tx)
                .await?;
            let mut space: SpaceRecord = row.ok_or(StoreError::NotFound)?.try_into()?;
            match patch {
                SpacePatch::Rename(name) => space.name = name,
                SpacePatch::Archive => space.archived = true,
                SpacePatch::Unarchive => space.archived = false,
                SpacePatch::PinArtifact(aid) => {
                    let found: Option<(String,)> = sqlx::query_as(
                        "SELECT id FROM task_center_artifacts WHERE tenant_id = $1 AND id = $2",
                    )
                    .bind(tenant_id)
                    .bind(&aid)
                    .fetch_optional(&mut *tx)
                    .await?;
                    if found.is_none() {
                        return Err(StoreError::NotFound);
                    }
                    if !space.pinned_artifact_ids.contains(&aid) {
                        space.pinned_artifact_ids.push(aid.clone());
                    }
                    sqlx::query("UPDATE task_center_artifacts SET pinned = TRUE, space_id = $3 WHERE tenant_id = $1 AND id = $2")
                        .bind(tenant_id).bind(&aid).bind(space_id).execute(&mut *tx).await?;
                }
                SpacePatch::UnpinArtifact(aid) => {
                    space.pinned_artifact_ids.retain(|id| id != &aid);
                    sqlx::query("UPDATE task_center_artifacts SET pinned = FALSE WHERE tenant_id = $1 AND id = $2")
                        .bind(tenant_id).bind(&aid).execute(&mut *tx).await?;
                }
            }
            space.updated_at_unix = now.max(space.created_at_unix);
            let pinned = serde_json::to_string(&space.pinned_artifact_ids)
                .map_err(|_| StoreError::Invalid("pinned"))?;
            sqlx::query("UPDATE task_center_spaces SET name = $3, archived = $4, pinned_artifact_ids_json = $5, updated_at_unix = $6 WHERE tenant_id = $1 AND id = $2")
                .bind(tenant_id).bind(space_id).bind(&space.name).bind(space.archived)
                .bind(&pinned).bind(space.updated_at_unix).execute(&mut *tx).await?;
            tx.commit().await?;
            Ok(space)
        })
    }

    pub async fn list_standing_policies(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<StandingPolicyRecord>, StoreError> {
        let rows: Vec<PolicyRow> = with_tenant_tx!(self, tenant_id, tx, {
            let rows = sqlx::query_as("SELECT department, auto_approve_max_micros, allow_external_send, allow_financial_commit, updated_at_unix FROM task_center_standing_policies WHERE tenant_id = $1 ORDER BY department ASC")
                .bind(tenant_id).fetch_all(&mut *tx).await?;
            tx.commit().await?;
            Ok::<_, StoreError>(rows)
        })?;
        let mut by_dept = BTreeMap::new();
        for dept in DEFAULT_DEPARTMENTS {
            by_dept.insert(
                dept.to_string(),
                StandingPolicyRecord {
                    department: dept.to_string(),
                    auto_approve_max_micros: 0,
                    allow_external_send: false,
                    allow_financial_commit: false,
                    updated_at_unix: 0,
                },
            );
        }
        for r in rows {
            by_dept.insert(
                r.department.clone(),
                StandingPolicyRecord {
                    department: r.department,
                    auto_approve_max_micros: r.auto_approve_max_micros,
                    allow_external_send: r.allow_external_send,
                    allow_financial_commit: r.allow_financial_commit,
                    updated_at_unix: r.updated_at_unix,
                },
            );
        }
        Ok(by_dept.into_values().collect())
    }

    pub async fn get_standing_policy(
        &self,
        tenant_id: &str,
        department: &str,
    ) -> Result<StandingPolicyRecord, StoreError> {
        let row: Option<PolicyRow> = with_tenant_tx!(self, tenant_id, tx, {
            let row = sqlx::query_as("SELECT department, auto_approve_max_micros, allow_external_send, allow_financial_commit, updated_at_unix FROM task_center_standing_policies WHERE tenant_id = $1 AND department = $2")
                .bind(tenant_id).bind(department).fetch_optional(&mut *tx).await?;
            tx.commit().await?;
            Ok::<_, StoreError>(row)
        })?;
        Ok(row.map_or_else(
            || StandingPolicyRecord {
                department: department.to_string(),
                auto_approve_max_micros: 0,
                allow_external_send: false,
                allow_financial_commit: false,
                updated_at_unix: 0,
            },
            |r| StandingPolicyRecord {
                department: r.department,
                auto_approve_max_micros: r.auto_approve_max_micros,
                allow_external_send: r.allow_external_send,
                allow_financial_commit: r.allow_financial_commit,
                updated_at_unix: r.updated_at_unix,
            },
        ))
    }

    pub async fn upsert_standing_policy(
        &self,
        tenant_id: &str,
        p: &StandingPolicyRecord,
    ) -> Result<StandingPolicyRecord, StoreError> {
        with_tenant_tx!(self, tenant_id, tx, {
            sqlx::query("INSERT INTO task_center_standing_policies (tenant_id, department, auto_approve_max_micros, allow_external_send, allow_financial_commit, updated_at_unix) VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT(tenant_id, department) DO UPDATE SET auto_approve_max_micros = excluded.auto_approve_max_micros, allow_external_send = excluded.allow_external_send, allow_financial_commit = excluded.allow_financial_commit, updated_at_unix = excluded.updated_at_unix")
                .bind(tenant_id).bind(&p.department).bind(p.auto_approve_max_micros)
                .bind(p.allow_external_send).bind(p.allow_financial_commit).bind(p.updated_at_unix)
                .execute(&mut *tx).await?;
            tx.commit().await?;
            Ok(p.clone())
        })
    }

    pub async fn list_tasks(&self, tenant_id: &str) -> Result<Vec<TaskCenterTask>, StoreError> {
        let q_tasks =
            format!("{SELECT_TASK} WHERE tenant_id = $1 ORDER BY updated_at_unix DESC, id ASC");
        let q_steps = format!("{SELECT_STEP} WHERE tenant_id = $1 ORDER BY seq ASC, id ASC");
        let q_arts =
            format!("{SELECT_ARTIFACT} WHERE tenant_id = $1 ORDER BY created_at_unix ASC, id ASC");
        let q_apprs = format!("{SELECT_APPROVAL} WHERE tenant_id = $1 AND status = 'pending'");
        with_tenant_tx!(self, tenant_id, tx, {
            let task_rows: Vec<TaskRow> = sqlx::query_as(&q_tasks)
                .bind(tenant_id)
                .fetch_all(&mut *tx)
                .await?;
            let step_rows: Vec<StepRow> = sqlx::query_as(&q_steps)
                .bind(tenant_id)
                .fetch_all(&mut *tx)
                .await?;
            let art_rows: Vec<ArtifactRow> = sqlx::query_as(&q_arts)
                .bind(tenant_id)
                .fetch_all(&mut *tx)
                .await?;
            let appr_rows: Vec<ApprovalRow> = sqlx::query_as(&q_apprs)
                .bind(tenant_id)
                .fetch_all(&mut *tx)
                .await?;
            tx.commit().await?;
            let mut steps_map: BTreeMap<String, Vec<TaskStepRecord>> = BTreeMap::new();
            for row in step_rows {
                let s: TaskStepRecord = row.try_into()?;
                steps_map.entry(s.task_id.clone()).or_default().push(s);
            }
            let mut arts_map: BTreeMap<String, Vec<TaskArtifactRecord>> = BTreeMap::new();
            for row in art_rows {
                let a: TaskArtifactRecord = row.try_into()?;
                arts_map.entry(a.task_id.clone()).or_default().push(a);
            }
            let mut apprs_map: BTreeMap<String, TaskApprovalPayload> = BTreeMap::new();
            for row in appr_rows {
                let tid = row.task_id.clone();
                apprs_map.insert(tid, row.try_into()?);
            }
            task_rows
                .into_iter()
                .map(|row| {
                    let id = row.id.clone();
                    decode_task(
                        row,
                        steps_map.remove(&id).unwrap_or_default(),
                        arts_map.remove(&id).unwrap_or_default(),
                        apprs_map.remove(&id),
                    )
                    .map(|m| m.task)
                })
                .collect()
        })
    }

    pub async fn get_task_meta(
        &self,
        tenant_id: &str,
        task_id: &str,
    ) -> Result<Option<StoredTaskMeta>, StoreError> {
        let q_task = format!("{SELECT_TASK} WHERE tenant_id = $1 AND id = $2");
        let q_steps =
            format!("{SELECT_STEP} WHERE tenant_id = $1 AND task_id = $2 ORDER BY seq ASC, id ASC");
        let q_arts = format!(
            "{SELECT_ARTIFACT} WHERE tenant_id = $1 AND task_id = $2 ORDER BY created_at_unix ASC, id ASC"
        );
        let q_appr = format!(
            "{SELECT_APPROVAL} WHERE tenant_id = $1 AND task_id = $2 AND status = 'pending'"
        );
        with_tenant_tx!(self, tenant_id, tx, {
            let Some(row) = sqlx::query_as::<_, TaskRow>(&q_task)
                .bind(tenant_id)
                .bind(task_id)
                .fetch_optional(&mut *tx)
                .await?
            else {
                tx.commit().await?;
                return Ok(None);
            };
            let steps: Vec<StepRow> = sqlx::query_as(&q_steps)
                .bind(tenant_id)
                .bind(task_id)
                .fetch_all(&mut *tx)
                .await?;
            let arts: Vec<ArtifactRow> = sqlx::query_as(&q_arts)
                .bind(tenant_id)
                .bind(task_id)
                .fetch_all(&mut *tx)
                .await?;
            let appr: Option<ApprovalRow> = sqlx::query_as(&q_appr)
                .bind(tenant_id)
                .bind(task_id)
                .fetch_optional(&mut *tx)
                .await?;
            tx.commit().await?;
            Ok(Some(decode_task(
                row,
                steps
                    .into_iter()
                    .map(TryInto::try_into)
                    .collect::<Result<_, _>>()?,
                arts.into_iter()
                    .map(TryInto::try_into)
                    .collect::<Result<_, _>>()?,
                appr.map(TryInto::try_into).transpose()?,
            )?))
        })
    }

    pub async fn find_task_by_idempotency(
        &self,
        tenant_id: &str,
        key: &str,
    ) -> Result<Option<StoredTaskMeta>, StoreError> {
        let found: Option<(String,)> = with_tenant_tx!(self, tenant_id, tx, {
            let f = sqlx::query_as(
                "SELECT id FROM task_center_tasks WHERE tenant_id = $1 AND idempotency_key = $2",
            )
            .bind(tenant_id)
            .bind(key)
            .fetch_optional(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<_, StoreError>(f)
        })?;
        match found {
            Some((id,)) => self.get_task_meta(tenant_id, &id).await,
            None => Ok(None),
        }
    }

    pub async fn insert_task_bundle(
        &self,
        tenant_id: &str,
        b: &NewTaskBundle<'_>,
    ) -> Result<(), StoreError> {
        let t = b.task;
        with_tenant_tx!(self, tenant_id, tx, {
            if let Some(sid) = &t.space_id {
                let ok: Option<(String,)> = sqlx::query_as(
                    "SELECT id FROM task_center_spaces WHERE tenant_id = $1 AND id = $2",
                )
                .bind(tenant_id)
                .bind(sid)
                .fetch_optional(&mut *tx)
                .await?;
                if ok.is_none() {
                    return Err(StoreError::NotFound);
                }
            }
            sqlx::query("INSERT INTO task_center_tasks (id, tenant_id, actor_id, space_id, title, prompt, department, requested_action, status, provenance, attention, risk, payer_mode, estimated_cost_micros, actual_cost_micros, idempotency_key, request_digest, receipt_id, usage_event_id, output_summary, error_message, archived, created_at_unix, updated_at_unix) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23,$24)")
                .bind(&t.id).bind(tenant_id).bind(b.actor_id).bind(&t.space_id).bind(&t.title)
                .bind(&t.prompt).bind(&t.department).bind(b.requested_action).bind(t.status.as_str())
                .bind(t.provenance.as_str()).bind(t.attention.as_str()).bind(t.risk.as_str())
                .bind(t.payer_mode.as_str()).bind(t.estimated_cost_micros).bind(t.actual_cost_micros)
                .bind(&t.idempotency_key).bind(b.request_digest).bind(&t.receipt_id).bind(b.usage_event_id)
                .bind(&t.output_summary).bind(&t.error_message).bind(t.archived)
                .bind(t.created_at_unix).bind(t.updated_at_unix).execute(&mut *tx).await?;
            for s in b.initial_steps {
                sqlx::query("INSERT INTO task_center_steps (id, tenant_id, task_id, seq, kind, title, detail, status, provenance, tool_name, cost_micros, created_at_unix) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)")
                    .bind(&s.id).bind(tenant_id).bind(&t.id).bind(s.seq).bind(s.kind.as_str())
                    .bind(&s.title).bind(&s.detail).bind(s.status.as_str()).bind(s.provenance.as_str())
                    .bind(&s.tool_name).bind(s.cost_micros).bind(s.created_at_unix).execute(&mut *tx).await?;
            }
            if let Some(a) = b.pending_approval {
                let preview = serde_json::to_string(&a.payload_preview)
                    .map_err(|_| StoreError::Invalid("preview"))?;
                sqlx::query("INSERT INTO task_center_approvals (approval_id, tenant_id, task_id, action_kind, department, risk, summary, payload_preview_json, payload_sha256, estimated_cost_micros, standing_limit_micros, status, decided_by, decided_at_unix, created_at_unix) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15)")
                    .bind(&a.approval_id).bind(tenant_id).bind(&t.id).bind(&a.action_kind)
                    .bind(&a.department).bind(a.risk.as_str()).bind(&a.summary).bind(&preview)
                    .bind(&a.payload_sha256).bind(a.estimated_cost_micros).bind(a.standing_limit_micros)
                    .bind(a.status.as_str()).bind(&a.decided_by).bind(a.decided_at_unix).bind(t.created_at_unix)
                    .execute(&mut *tx).await?;
            }
            tx.commit().await?;
            Ok(())
        })
    }

    pub async fn append_step(
        &self,
        tenant_id: &str,
        task_id: &str,
        s: &NewStep<'_>,
    ) -> Result<TaskStepRecord, StoreError> {
        let step_id = format!("stp-{}", Uuid::new_v4());
        with_tenant_tx!(self, tenant_id, tx, {
            let seq = push_step_tx!(
                tx,
                tenant_id,
                task_id,
                &step_id,
                s.kind.as_str(),
                s.title,
                s.detail,
                s.status.as_str(),
                s.provenance.as_str(),
                s.tool_name,
                s.cost_micros,
                s.now
            );
            tx.commit().await?;
            Ok(TaskStepRecord {
                id: step_id,
                task_id: task_id.into(),
                seq,
                kind: s.kind,
                title: s.title.into(),
                detail: s.detail.into(),
                status: s.status,
                provenance: s.provenance,
                tool_name: s.tool_name.map(str::to_string),
                cost_micros: s.cost_micros,
                created_at_unix: s.now,
            })
        })
    }

    pub async fn insert_artifact(
        &self,
        tenant_id: &str,
        a: &TaskArtifactRecord,
    ) -> Result<TaskArtifactRecord, StoreError> {
        if a.content.len() > MAX_ARTIFACT_CONTENT_BYTES || contains_unsafe_html(&a.content) {
            return Err(StoreError::Invalid(
                "artifact exceeds 64 KiB or contains disallowed raw HTML",
            ));
        }
        with_tenant_tx!(self, tenant_id, tx, {
            sqlx::query("INSERT INTO task_center_artifacts (id, tenant_id, task_id, space_id, title, format, content, provenance, pinned, created_at_unix) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
                .bind(&a.id).bind(tenant_id).bind(&a.task_id).bind(&a.space_id).bind(&a.title)
                .bind(a.format.as_str()).bind(&a.content).bind(a.provenance.as_str())
                .bind(a.pinned).bind(a.created_at_unix).execute(&mut *tx).await?;
            tx.commit().await?;
            Ok(a.clone())
        })
    }

    pub async fn transition_task_cas(
        &self,
        tenant_id: &str,
        task_id: &str,
        t: &TaskTransition<'_>,
    ) -> Result<bool, StoreError> {
        with_tenant_tx!(self, tenant_id, tx, {
            let changed = sqlx::query("UPDATE task_center_tasks SET status = $4, provenance = $5, attention = $6, actual_cost_micros = COALESCE($7, actual_cost_micros), receipt_id = COALESCE($8, receipt_id), output_summary = COALESCE($9, output_summary), error_message = $10, updated_at_unix = CASE WHEN $11 > updated_at_unix THEN $11 ELSE updated_at_unix END WHERE tenant_id = $1 AND id = $2 AND status = $3")
                .bind(tenant_id).bind(task_id).bind(t.expected.as_str()).bind(t.status.as_str())
                .bind(t.provenance.as_str()).bind(t.attention.as_str()).bind(t.actual_cost_micros)
                .bind(t.receipt_id).bind(t.output_summary).bind(t.error_message).bind(t.now)
                .execute(&mut *tx).await?.rows_affected();
            tx.commit().await?;
            Ok(changed == 1)
        })
    }

    pub async fn prepare_followup_cas(
        &self,
        tenant_id: &str,
        task_id: &str,
        usage_event_id: Option<&str>,
        now: i64,
    ) -> Result<bool, StoreError> {
        with_tenant_tx!(self, tenant_id, tx, {
            let changed = sqlx::query("UPDATE task_center_tasks SET status = 'queued', provenance = 'draft', attention = 'none', usage_event_id = $3, error_message = NULL, updated_at_unix = CASE WHEN $4 > updated_at_unix THEN $4 ELSE updated_at_unix END WHERE tenant_id = $1 AND id = $2 AND status IN ('completed', 'failed', 'cancelled', 'waiting_for_input', 'draft')")
                .bind(tenant_id).bind(task_id).bind(usage_event_id).bind(now)
                .execute(&mut *tx).await?.rows_affected();
            tx.commit().await?;
            Ok(changed == 1)
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn decide_approval_cas(
        &self,
        tenant_id: &str,
        task_id: &str,
        payload_sha256: &str,
        approved: bool,
        decided_by: &str,
        reason: Option<&str>,
        now: i64,
    ) -> Result<ApprovalDecisionOutcome, StoreError> {
        let step_id = format!("stp-{}", Uuid::new_v4());
        let (usage_event_id, request_digest) = with_tenant_tx!(self, tenant_id, tx, {
            let row: Option<(String, Option<String>, String)> = sqlx::query_as(
                "SELECT status, usage_event_id, request_digest FROM task_center_tasks WHERE tenant_id = $1 AND id = $2",
            )
            .bind(tenant_id)
            .bind(task_id)
            .fetch_optional(&mut *tx)
            .await?;
            let Some((st, usage_event_id, request_digest)) = row else {
                return Err(StoreError::NotFound);
            };
            if st != TaskCenterTaskStatus::AwaitingApproval.as_str() {
                return Err(StoreError::Conflict("task is not awaiting approval"));
            }
            let appr: Option<(String, String)> = sqlx::query_as(
                "SELECT approval_id, payload_sha256 FROM task_center_approvals WHERE tenant_id = $1 AND task_id = $2 AND status = 'pending'",
            )
            .bind(tenant_id)
            .bind(task_id)
            .fetch_optional(&mut *tx)
            .await?;
            let Some((aid, stored_sha)) = appr else {
                return Err(StoreError::Conflict("no pending approval"));
            };
            if stored_sha != payload_sha256 {
                return Err(StoreError::Conflict("approval payload_sha256 mismatch"));
            }
            let a_st = if approved { "approved" } else { "rejected" };
            let n1 = sqlx::query("UPDATE task_center_approvals SET status = $4, decided_by = $5, decided_at_unix = $6, decision_reason = $7 WHERE tenant_id = $1 AND approval_id = $2 AND status = 'pending' AND payload_sha256 = $3")
                .bind(tenant_id).bind(&aid).bind(payload_sha256).bind(a_st).bind(decided_by).bind(now).bind(reason)
                .execute(&mut *tx).await?.rows_affected();
            if n1 != 1 {
                return Err(StoreError::Conflict("approval already decided"));
            }
            let (t_st, prov, err) = if approved {
                ("queued", "draft", None)
            } else {
                (
                    "cancelled",
                    "cancelled",
                    Some(reason.unwrap_or("Rejected by owner")),
                )
            };
            let n2 = sqlx::query("UPDATE task_center_tasks SET status = $3, provenance = $4, attention = 'none', error_message = $5, updated_at_unix = CASE WHEN $6 > updated_at_unix THEN $6 ELSE updated_at_unix END WHERE tenant_id = $1 AND id = $2 AND status = 'awaiting_approval'")
                .bind(tenant_id).bind(task_id).bind(t_st).bind(prov).bind(err).bind(now)
                .execute(&mut *tx).await?.rows_affected();
            if n2 != 1 {
                return Err(StoreError::Conflict("task state changed concurrently"));
            }
            let title = if approved {
                "Owner approved action"
            } else {
                "Owner rejected action"
            };
            let detail = reason.unwrap_or(if approved {
                "SHA-256 verified; queued"
            } else {
                "Rejected; reservation released"
            });
            push_step_tx!(
                tx,
                tenant_id,
                task_id,
                &step_id,
                "approval_gate",
                title,
                detail,
                t_st,
                prov,
                None::<&str>,
                0_i64,
                now
            );
            tx.commit().await?;
            Ok::<_, StoreError>((usage_event_id, request_digest))
        })?;
        let meta = self
            .get_task_meta(tenant_id, task_id)
            .await?
            .ok_or(StoreError::NotFound)?;
        Ok(ApprovalDecisionOutcome {
            task: meta.task,
            usage_event_id,
            request_digest,
        })
    }

    pub async fn cancel_task_cas(
        &self,
        tenant_id: &str,
        task_id: &str,
        actor_id: &str,
        now: i64,
    ) -> Result<(TaskCenterTask, CancelOutcome, Option<String>), StoreError> {
        let step_id = format!("stp-{}", Uuid::new_v4());
        let (outcome, usage_event_id) = with_tenant_tx!(self, tenant_id, tx, {
            let row: Option<(String, Option<String>)> = sqlx::query_as(
                "SELECT status, usage_event_id FROM task_center_tasks WHERE tenant_id = $1 AND id = $2",
            )
            .bind(tenant_id)
            .bind(task_id)
            .fetch_optional(&mut *tx)
            .await?;
            let Some((raw, usage_event_id)) = row else {
                return Err(StoreError::NotFound);
            };
            let status = TaskCenterTaskStatus::parse(&raw)?;
            let outcome = match status {
                TaskCenterTaskStatus::OutcomeUnknown => CancelOutcome::PreservedOutcomeUnknown,
                TaskCenterTaskStatus::Cancelled => CancelOutcome::AlreadyCancelled,
                TaskCenterTaskStatus::Completed | TaskCenterTaskStatus::Failed => {
                    return Err(StoreError::Conflict("terminal task cannot be cancelled"));
                }
                TaskCenterTaskStatus::Running => {
                    let n = sqlx::query("UPDATE task_center_tasks SET status = 'outcome_unknown', provenance = 'outcome_unknown', attention = 'blocked', error_message = 'Cancelled while provider request was in flight; outcome is unknown — reconcile before retrying', updated_at_unix = CASE WHEN $3 > updated_at_unix THEN $3 ELSE updated_at_unix END WHERE tenant_id = $1 AND id = $2 AND status = 'running'")
                        .bind(tenant_id).bind(task_id).bind(now).execute(&mut *tx).await?.rows_affected();
                    if n != 1 {
                        return Err(StoreError::Conflict("task state changed concurrently"));
                    }
                    push_step_tx!(
                        tx,
                        tenant_id,
                        task_id,
                        &step_id,
                        "system",
                        "Cancelled in flight",
                        "Provider outcome indeterminate; hold retained for reconciliation; no auto-retry",
                        "outcome_unknown",
                        "outcome_unknown",
                        None::<&str>,
                        0_i64,
                        now
                    );
                    CancelOutcome::MarkedOutcomeUnknownInFlight
                }
                TaskCenterTaskStatus::Draft
                | TaskCenterTaskStatus::Queued
                | TaskCenterTaskStatus::AwaitingApproval
                | TaskCenterTaskStatus::WaitingForInput => {
                    sqlx::query("UPDATE task_center_approvals SET status = 'expired', decided_by = $3, decided_at_unix = $4 WHERE tenant_id = $1 AND task_id = $2 AND status = 'pending'")
                        .bind(tenant_id).bind(task_id).bind(actor_id).bind(now).execute(&mut *tx).await?;
                    let n = sqlx::query("UPDATE task_center_tasks SET status = 'cancelled', provenance = 'cancelled', attention = 'none', error_message = 'Cancelled prior to provider dispatch', updated_at_unix = CASE WHEN $4 > updated_at_unix THEN $4 ELSE updated_at_unix END WHERE tenant_id = $1 AND id = $2 AND status = $3")
                        .bind(tenant_id).bind(task_id).bind(status.as_str()).bind(now).execute(&mut *tx).await?.rows_affected();
                    if n != 1 {
                        return Err(StoreError::Conflict("task state changed concurrently"));
                    }
                    push_step_tx!(
                        tx,
                        tenant_id,
                        task_id,
                        &step_id,
                        "system",
                        "Cancelled before dispatch",
                        "No provider effect occurred; pending reservation released",
                        "cancelled",
                        "cancelled",
                        None::<&str>,
                        0_i64,
                        now
                    );
                    CancelOutcome::CancelledPreDispatch
                }
            };
            tx.commit().await?;
            Ok::<_, StoreError>((outcome, usage_event_id))
        })?;
        let meta = self
            .get_task_meta(tenant_id, task_id)
            .await?
            .ok_or(StoreError::NotFound)?;
        Ok((meta.task, outcome, usage_event_id))
    }

    pub async fn pin_artifact(
        &self,
        tenant_id: &str,
        task_id: &str,
        artifact_id: &str,
        space_id: &str,
        pinned: bool,
        now: i64,
    ) -> Result<(TaskArtifactRecord, SpaceRecord), StoreError> {
        let q_art = format!("{SELECT_ARTIFACT} WHERE tenant_id = $1 AND task_id = $2 AND id = $3");
        let q_spc = format!("{SELECT_SPACE} WHERE tenant_id = $1 AND id = $2");
        with_tenant_tx!(self, tenant_id, tx, {
            let mut art: TaskArtifactRecord = sqlx::query_as::<_, ArtifactRow>(&q_art)
                .bind(tenant_id)
                .bind(task_id)
                .bind(artifact_id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(StoreError::NotFound)?
                .try_into()?;
            let mut space: SpaceRecord = sqlx::query_as::<_, SpaceRow>(&q_spc)
                .bind(tenant_id)
                .bind(space_id)
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(StoreError::NotFound)?
                .try_into()?;
            art.pinned = pinned;
            if pinned {
                art.space_id = Some(space_id.into());
                if !space.pinned_artifact_ids.iter().any(|id| id == artifact_id) {
                    space.pinned_artifact_ids.push(artifact_id.into());
                }
            } else {
                space.pinned_artifact_ids.retain(|id| id != artifact_id);
            }
            space.updated_at_unix = now.max(space.created_at_unix);
            let pinned_json = serde_json::to_string(&space.pinned_artifact_ids)
                .map_err(|_| StoreError::Invalid("pinned"))?;
            sqlx::query("UPDATE task_center_artifacts SET pinned = $4, space_id = $5 WHERE tenant_id = $1 AND task_id = $2 AND id = $3")
                .bind(tenant_id).bind(task_id).bind(artifact_id).bind(art.pinned).bind(&art.space_id).execute(&mut *tx).await?;
            sqlx::query("UPDATE task_center_spaces SET pinned_artifact_ids_json = $3, updated_at_unix = $4 WHERE tenant_id = $1 AND id = $2")
                .bind(tenant_id).bind(space_id).bind(&pinned_json).bind(space.updated_at_unix).execute(&mut *tx).await?;
            tx.commit().await?;
            Ok((art, space))
        })
    }
}
