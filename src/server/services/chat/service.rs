use super::models::{ChatChannel, ChatContact, ChatConversation, ChatInbox, ChatMessage};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

pub struct ChatService {
    pool: PgPool,
}

impl ChatService {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    async fn bind_scope(
        tx: &mut Transaction<'_, Postgres>,
        tenant_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "SELECT set_config('app.current_tenant', $1, true), set_config('app.current_tenant_id', $1, true)",
        )
        .bind(tenant_id.to_string())
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    pub async fn create_inbox(
        &self,
        tenant_id: Uuid,
        name: String,
    ) -> Result<ChatInbox, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        Self::bind_scope(&mut tx, tenant_id).await?;
        let res = sqlx::query_as(
            r#"
            INSERT INTO chat_inboxes (id, tenant_id, name)
            VALUES ($1, $2, $3)
            RETURNING id::uuid AS id, tenant_id::uuid AS tenant_id, name, created_at, updated_at
            "#,
        )
        .bind(Uuid::new_v4().to_string())
        .bind(tenant_id.to_string())
        .bind(name)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(res)
    }

    pub async fn create_channel(
        &self,
        tenant_id: Uuid,
        inbox_id: Uuid,
        channel_type: String,
        config: serde_json::Value,
    ) -> Result<ChatChannel, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        Self::bind_scope(&mut tx, tenant_id).await?;
        let res = sqlx::query_as(
            r#"
            INSERT INTO chat_channels (id, tenant_id, inbox_id, channel_type, config)
            SELECT $1, $2, $3, $4, $5
            WHERE EXISTS (SELECT 1 FROM chat_inboxes WHERE id = $3 AND tenant_id = $2)
            RETURNING id::uuid AS id, tenant_id::uuid AS tenant_id, inbox_id::uuid AS inbox_id, channel_type, config, created_at, updated_at
            "#,
        )
        .bind(Uuid::new_v4().to_string())
        .bind(tenant_id.to_string())
        .bind(inbox_id.to_string())
        .bind(channel_type)
        .bind(config)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(res)
    }

    pub async fn create_contact(
        &self,
        tenant_id: Uuid,
        name: Option<String>,
        email: Option<String>,
        phone: Option<String>,
    ) -> Result<ChatContact, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        Self::bind_scope(&mut tx, tenant_id).await?;
        let res = sqlx::query_as(
            r#"
            INSERT INTO chat_contacts (id, tenant_id, name, email, phone)
            VALUES ($1, $2, $3, $4, $5)
            RETURNING id::uuid AS id, tenant_id::uuid AS tenant_id, name, email, phone, created_at, updated_at
            "#,
        )
        .bind(Uuid::new_v4().to_string())
        .bind(tenant_id.to_string())
        .bind(name)
        .bind(email)
        .bind(phone)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(res)
    }

    pub async fn start_conversation(
        &self,
        tenant_id: Uuid,
        inbox_id: Uuid,
        contact_id: Uuid,
        assignee_id: Option<Uuid>,
    ) -> Result<ChatConversation, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        Self::bind_scope(&mut tx, tenant_id).await?;
        let res = sqlx::query_as(
            r#"
            INSERT INTO chat_conversations (id, tenant_id, inbox_id, contact_id, assignee_id, status)
            SELECT $1, $2, $3, $4, $5, 'open'
            WHERE EXISTS (SELECT 1 FROM chat_inboxes WHERE id = $3 AND tenant_id = $2)
              AND EXISTS (SELECT 1 FROM chat_contacts WHERE id = $4 AND tenant_id = $2)
            RETURNING id::uuid AS id, tenant_id::uuid AS tenant_id, inbox_id::uuid AS inbox_id,
                      contact_id::uuid AS contact_id, assignee_id::uuid AS assignee_id,
                      status, created_at, updated_at
            "#,
        )
        .bind(Uuid::new_v4().to_string())
        .bind(tenant_id.to_string())
        .bind(inbox_id.to_string())
        .bind(contact_id.to_string())
        .bind(assignee_id.map(|id| id.to_string()))
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(res)
    }

    pub async fn get_open_conversations(
        &self,
        tenant_id: Uuid,
        inbox_id: Uuid,
    ) -> Result<Vec<ChatConversation>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        Self::bind_scope(&mut tx, tenant_id).await?;
        let res = sqlx::query_as(
            r#"
            SELECT c.id::uuid AS id, c.tenant_id::uuid AS tenant_id, c.inbox_id::uuid AS inbox_id,
                   c.contact_id::uuid AS contact_id, c.assignee_id::uuid AS assignee_id,
                   c.status, c.created_at, c.updated_at
            FROM chat_conversations c
            JOIN chat_inboxes i ON i.id = c.inbox_id AND i.tenant_id = c.tenant_id
            JOIN chat_contacts p ON p.id = c.contact_id AND p.tenant_id = c.tenant_id
            WHERE c.tenant_id = $1 AND c.inbox_id = $2 AND c.status = 'open'
            ORDER BY c.updated_at DESC, c.id::uuid ASC
            "#,
        )
        .bind(tenant_id.to_string())
        .bind(inbox_id.to_string())
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(res)
    }

    pub async fn send_message(
        &self,
        tenant_id: Uuid,
        conversation_id: Uuid,
        sender_type: String,
        sender_id: Option<Uuid>,
        content: String,
    ) -> Result<ChatMessage, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        Self::bind_scope(&mut tx, tenant_id).await?;
        let res = sqlx::query_as(
            r#"
            INSERT INTO chat_messages (id, tenant_id, conversation_id, sender_type, sender_id, content)
            SELECT $1, $2, $3, $4, $5, $6
            WHERE EXISTS (SELECT 1 FROM chat_conversations WHERE id = $3 AND tenant_id = $2)
            RETURNING id::uuid AS id, tenant_id::uuid AS tenant_id,
                      conversation_id::uuid AS conversation_id, sender_type, sender_id,
                      content, created_at, updated_at
            "#,
        )
        .bind(Uuid::new_v4().to_string())
        .bind(tenant_id.to_string())
        .bind(conversation_id.to_string())
        .bind(sender_type)
        .bind(sender_id.map(|id| id.to_string()))
        .bind(content)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{
        ADDITIVE_CONTENT_TYPE_VERSION, ChatFixture, fixture_migrator, test_database_url,
    };
    use super::*;

    #[tokio::test]
    async fn test_create_inbox() {
        let fixture = ChatFixture::new().await;
        let service = ChatService::new(fixture.pool.clone());
        let workspace = Uuid::new_v4();
        let inbox = service
            .create_inbox(workspace, "Support".into())
            .await
            .unwrap();
        let channel = service
            .create_channel(
                workspace,
                inbox.id,
                "widget".into(),
                serde_json::json!({"webhook_url":"https://example.test"}),
            )
            .await
            .unwrap();
        let contact = service
            .create_contact(
                workspace,
                Some("Alice".into()),
                Some("alice@example.test".into()),
                None,
            )
            .await
            .unwrap();
        let conversation = service
            .start_conversation(workspace, inbox.id, contact.id, None)
            .await
            .unwrap();
        let message = service
            .send_message(
                workspace,
                conversation.id,
                "contact".into(),
                Some(contact.id),
                "Hello!".into(),
            )
            .await
            .unwrap();
        let counts = vec![
            fixture
                .count_for_workspace(workspace, "chat_inboxes", inbox.id)
                .await,
            fixture
                .count_for_workspace(workspace, "chat_channels", channel.id)
                .await,
            fixture
                .count_for_workspace(workspace, "chat_contacts", contact.id)
                .await,
            fixture
                .count_for_workspace(workspace, "chat_conversations", conversation.id)
                .await,
            fixture
                .count_for_workspace(workspace, "chat_messages", message.id)
                .await,
        ];
        let other_workspace = Uuid::new_v4();
        let other_counts = vec![
            fixture
                .count_for_workspace(other_workspace, "chat_inboxes", inbox.id)
                .await,
            fixture
                .count_for_workspace(other_workspace, "chat_channels", channel.id)
                .await,
            fixture
                .count_for_workspace(other_workspace, "chat_contacts", contact.id)
                .await,
            fixture
                .count_for_workspace(other_workspace, "chat_conversations", conversation.id)
                .await,
            fixture
                .count_for_workspace(other_workspace, "chat_messages", message.id)
                .await,
        ];
        fixture.finish().await;
        assert_eq!(
            other_counts,
            vec![0; 5],
            "stored chat records must match their owning workspace ID"
        );
        assert_eq!(inbox.name, "Support");
        assert_eq!(channel.channel_type, "widget");
        assert_eq!(contact.name.as_deref(), Some("Alice"));
        assert_eq!(conversation.status, "open");
        assert_eq!(message.content, "Hello!");
        assert_eq!(counts, vec![1; 5]);
    }

    #[test]
    fn missing_or_unsafe_database_prerequisites_are_visible_errors() {
        assert!(test_database_url(None).is_err());
        for raw in [
            "postgres://127.0.0.1/production",
            "postgres://remote.example/ohc_chat_test",
            "postgres://127.0.0.1/ohc_chat_test?host=remote",
            "postgres://127.0.0.1/ohc_chat_test#x",
        ] {
            assert!(test_database_url(Some(raw)).is_err());
        }
        assert!(test_database_url(Some("postgres://127.0.0.1:55439/ohc_chat_test")).is_ok());
    }

    #[tokio::test]
    async fn actual_sql_failures_are_not_successful_empty_results() {
        let f = ChatFixture::new().await;
        let service = ChatService::new(f.pool.clone());
        let workspace = Uuid::new_v4();
        sqlx::query("DROP TABLE chat_messages")
            .execute(&f.pool)
            .await
            .unwrap();
        let write = service
            .send_message(
                workspace,
                Uuid::new_v4(),
                "contact".into(),
                None,
                "No schema".into(),
            )
            .await;
        let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM chat_messages")
            .fetch_one(&f.pool)
            .await;
        f.finish().await;
        assert!(write.is_err());
        assert!(count.is_err());
    }

    #[tokio::test]
    async fn open_conversation_sql_failures_are_errors_without_tenant_leakage() {
        let f = ChatFixture::new().await;
        let service = ChatService::new(f.pool.clone());
        sqlx::query("ALTER TABLE chat_conversations RENAME TO unavailable_chat_conversations")
            .execute(&f.pool)
            .await
            .unwrap();
        let result = service
            .get_open_conversations(Uuid::new_v4(), Uuid::new_v4())
            .await;
        let context: (Option<String>, Option<String>) = sqlx::query_as(
            "SELECT NULLIF(current_setting('app.current_tenant', true), ''), \
                    NULLIF(current_setting('app.current_tenant_id', true), '')",
        )
        .fetch_one(&f.pool)
        .await
        .unwrap();
        let next = service
            .create_inbox(Uuid::new_v4(), "After failed read".into())
            .await;
        f.finish().await;
        assert_eq!(
            result
                .unwrap_err()
                .as_database_error()
                .unwrap()
                .code()
                .as_deref(),
            Some("42P01")
        );
        assert_eq!(context, (None, None));
        assert!(next.is_ok(), "a failed read must roll back its transaction");
    }

    #[tokio::test]
    async fn additive_content_type_migration_preserves_old_messages_and_history() {
        assert_content_type_upgrade(false).await;
    }

    #[tokio::test]
    async fn additive_content_type_migration_preserves_an_existing_column_and_values() {
        assert_content_type_upgrade(true).await;
    }

    async fn assert_content_type_upgrade(already_has_column: bool) {
        let f = ChatFixture::legacy().await;
        let service = ChatService::new(f.pool.clone());
        let workspace = Uuid::new_v4();
        let inbox = service
            .create_inbox(workspace, "Upgrade".into())
            .await
            .unwrap();
        let contact = service
            .create_contact(workspace, None, None, None)
            .await
            .unwrap();
        let conversation = service
            .start_conversation(workspace, inbox.id, contact.id, None)
            .await
            .unwrap();
        let message = service
            .send_message(
                workspace,
                conversation.id,
                "contact".into(),
                Some(contact.id),
                "Preserve old bytes ✉".into(),
            )
            .await
            .unwrap();
        let before: Vec<(i64, Vec<u8>)> = sqlx::query_as(
            "SELECT version,checksum FROM _sqlx_migrations WHERE success ORDER BY version",
        )
        .fetch_all(&f.pool)
        .await
        .unwrap();
        if already_has_column {
            // A database that already received the upstream column must retain
            // both its existing values and the original migration checksums.
            sqlx::raw_sql(
                "ALTER TABLE chat_messages ADD COLUMN content_type VARCHAR(50) NOT NULL DEFAULT 'text'",
            )
            .execute(&f.pool)
            .await
            .unwrap();
            sqlx::query("UPDATE chat_messages SET content_type='image' WHERE id=$1")
                .bind(message.id.to_string())
                .execute(&f.pool)
                .await
                .unwrap();
        }
        let migrations = fixture_migrator(true);
        migrations.run(&f.pool).await.unwrap();
        migrations.run(&f.pool).await.unwrap();
        let saved: (String, String, String) =
            sqlx::query_as("SELECT content,sender_id,content_type FROM chat_messages WHERE id=$1")
                .bind(message.id.to_string())
                .fetch_one(&f.pool)
                .await
                .unwrap();
        let next = service
            .send_message(
                workspace,
                conversation.id,
                "contact".into(),
                Some(contact.id),
                "After upgrade".into(),
            )
            .await
            .unwrap();
        let next_content_type: String =
            sqlx::query_scalar("SELECT content_type FROM chat_messages WHERE id=$1")
                .bind(next.id.to_string())
                .fetch_one(&f.pool)
                .await
                .unwrap();
        let after: Vec<(i64, Vec<u8>)> = sqlx::query_as(
            "SELECT version,checksum FROM _sqlx_migrations WHERE success ORDER BY version",
        )
        .fetch_all(&f.pool)
        .await
        .unwrap();
        let foreign_count = f
            .count_for_workspace(Uuid::new_v4(), "chat_messages", message.id)
            .await;
        f.finish().await;
        let expected_base_versions: Vec<i64> = ::ohc_storage::POSTGRES_MIGRATOR
            .iter()
            .map(|m| m.version)
            .collect();
        assert_eq!(
            before
                .iter()
                .map(|(version, _)| *version)
                .collect::<Vec<_>>(),
            expected_base_versions
        );
        assert_eq!(
            &after[..before.len()],
            before.as_slice(),
            "an additive upgrade must not rewrite applied migration history"
        );
        assert_eq!(after.len(), before.len() + 1);
        let added = migrations
            .iter()
            .find(|m| m.version == ADDITIVE_CONTENT_TYPE_VERSION)
            .unwrap();
        assert_eq!(
            after.last().unwrap(),
            &(ADDITIVE_CONTENT_TYPE_VERSION, added.checksum.to_vec())
        );
        assert_eq!(
            saved,
            (
                "Preserve old bytes ✉".into(),
                contact.id.to_string(),
                if already_has_column { "image" } else { "text" }.into()
            )
        );
        assert_eq!(next_content_type, "text");
        assert_eq!(foreign_count, 0);
    }
}
