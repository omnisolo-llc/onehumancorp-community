use async_trait::async_trait;
use omnisolo_builtin_agent::mesh::transport::{MeshTransport, Message};
use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram};
use std::time::Instant;

/// MemoryMeshTransport uses in-process DashMap broadcasting for Teammate Mesh APIs.
/// It wraps InProcessTransport to add OpenTelemetry tracking metrics.
pub struct MemoryMeshTransport {
    inner: omnisolo_builtin_agent::mesh::transport::InProcessTransport,
    publish_counter: Counter<u64>,
    bytes_counter: Counter<u64>,
    subscribe_counter: Counter<u64>,
    latency_histogram: Histogram<u64>,
}

impl Default for MemoryMeshTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryMeshTransport {
    pub fn new() -> Self {
        let meter = global::meter("orchestration");
        let publish_counter = meter.u64_counter("mesh.publish.count").build();
        let bytes_counter = meter.u64_counter("mesh.publish.bytes").build();
        let subscribe_counter = meter.u64_counter("mesh.subscribe.count").build();
        let latency_histogram = meter.u64_histogram("mesh.publish.latency").build();

        Self {
            inner: omnisolo_builtin_agent::mesh::transport::InProcessTransport::new(),
            publish_counter,
            bytes_counter,
            subscribe_counter,
            latency_histogram,
        }
    }
}

#[async_trait]
impl MeshTransport for MemoryMeshTransport {
    async fn publish(
        &self,
        topic: &str,
        message: ::server_omnisolo::orchestration::TeammateMeshEvent,
    ) -> Result<(), String> {
        let start = Instant::now();
        let payload_size = message.payload.len() as u64;

        self.publish_counter.add(
            1,
            &[
                KeyValue::new("transport", "memory"),
                KeyValue::new("topic", topic.to_string()),
            ],
        );
        self.bytes_counter.add(
            payload_size,
            &[
                KeyValue::new("transport", "memory"),
                KeyValue::new("topic", topic.to_string()),
            ],
        );

        let res = self.inner.publish(topic, message).await;

        let duration_ms = start.elapsed().as_millis() as u64;
        self.latency_histogram.record(
            duration_ms,
            &[
                KeyValue::new("transport", "memory"),
                KeyValue::new("topic", topic.to_string()),
            ],
        );

        res
    }

    async fn subscribe(
        &self,
        topic: &str,
        handler: Box<dyn Fn(Message) + Send + Sync>,
    ) -> Result<Box<dyn Fn() + Send + Sync>, String> {
        self.subscribe_counter.add(
            1,
            &[
                KeyValue::new("transport", "memory"),
                KeyValue::new("topic", topic.to_string()),
            ],
        );
        self.inner.subscribe(topic, handler).await
    }

    async fn subscribe_pattern(
        &self,
        pattern: &str,
        handler: Box<dyn Fn(Message) + Send + Sync>,
    ) -> Result<Box<dyn Fn() + Send + Sync>, String> {
        self.subscribe_counter.add(
            1,
            &[
                KeyValue::new("transport", "memory"),
                KeyValue::new("pattern", pattern.to_string()),
            ],
        );
        self.inner.subscribe_pattern(pattern, handler).await
    }

    async fn acquire_lock(
        &self,
        resource: &str,
        owner: &str,
        ttl_seconds: u64,
    ) -> Result<bool, String> {
        self.inner.acquire_lock(resource, owner, ttl_seconds).await
    }

    async fn release_lock(&self, resource: &str, owner: &str) -> Result<(), String> {
        self.inner.release_lock(resource, owner).await
    }

    async fn register_presence(
        &self,
        agent_id: &str,
        status: &str,
        ttl_seconds: u64,
    ) -> Result<(), String> {
        self.inner
            .register_presence(agent_id, status, ttl_seconds)
            .await
    }

    async fn get_active_agents(&self) -> Result<Vec<(String, String)>, String> {
        self.inner.get_active_agents().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use omnisolo_builtin_agent::mesh::transport::Message;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn test_memory_mesh_transport_pubsub() {
        let transport = MemoryMeshTransport::new();
        let received = Arc::new(Mutex::new(Vec::new()));
        let received_clone = received.clone();

        let handler = Box::new(move |msg: Message| {
            let received = received_clone.clone();
            tokio::spawn(async move {
                received.lock().await.push(msg);
            });
        });

        let cancel = transport.subscribe("test_topic", handler).await.unwrap();

        let msg = ::server_omnisolo::orchestration::TeammateMeshEvent {
            agent_id: "agent_1".to_string(),
            action: "action_1".to_string(),
            status: "ok".to_string(),
            payload: b"hello".to_vec(),
            msg_id: "msg_1".to_string(),
        };

        transport.publish("test_topic", msg.clone()).await.unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let locked = received.lock().await;
        assert_eq!(locked.len(), 1);
        assert_eq!(locked[0].msg_id, "msg_1");
        drop(locked);

        cancel();
    }

    #[tokio::test]
    async fn test_memory_mesh_transport_locking() {
        let transport = MemoryMeshTransport::new();
        let resource_name = format!("test_resource_{}", uuid::Uuid::new_v4());

        let acq1 = transport
            .acquire_lock(&resource_name, "agent_1", 10)
            .await
            .unwrap();
        assert!(acq1);

        let acq2 = transport
            .acquire_lock(&resource_name, "agent_2", 10)
            .await
            .unwrap();
        assert!(!acq2);

        transport
            .release_lock(&resource_name, "agent_1")
            .await
            .unwrap();

        let acq3 = transport
            .acquire_lock(&resource_name, "agent_2", 10)
            .await
            .unwrap();
        assert!(acq3);
    }

    #[tokio::test]
    async fn test_memory_mesh_transport_presence() {
        let transport = MemoryMeshTransport::new();

        transport
            .register_presence("agent_1", "online", 10)
            .await
            .unwrap();
        transport
            .register_presence("agent_2", "busy", 10)
            .await
            .unwrap();

        let mut agents = transport.get_active_agents().await.unwrap();
        agents.sort();

        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0], ("agent_1".to_string(), "online".to_string()));
        assert_eq!(agents[1], ("agent_2".to_string(), "busy".to_string()));
    }

    #[tokio::test]
    async fn test_memory_mesh_transport_concurrent_subscribers() {
        let transport = MemoryMeshTransport::new();

        let mut cancels = Vec::new();
        let received_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        for _ in 0..5 {
            let rc = received_count.clone();
            let handler = Box::new(move |msg: Message| {
                if msg.msg_id == "concurrent_msg_1" {
                    rc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            });
            let cancel = transport
                .subscribe("concurrent_topic", handler)
                .await
                .unwrap();
            cancels.push(cancel);
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let msg = ::server_omnisolo::orchestration::TeammateMeshEvent {
            agent_id: "agent_concurrent".to_string(),
            action: "broadcast".to_string(),
            status: "ok".to_string(),
            payload: b"data".to_vec(),
            msg_id: "concurrent_msg_1".to_string(),
        };

        transport.publish("concurrent_topic", msg).await.unwrap();

        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        assert_eq!(received_count.load(std::sync::atomic::Ordering::SeqCst), 5);

        for cancel in cancels {
            cancel();
        }
    }

    #[tokio::test]
    async fn test_memory_mesh_transport_submillisecond_latency() {
        let transport = MemoryMeshTransport::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let tx_arc = Arc::new(tokio::sync::Mutex::new(tx));

        let handler = Box::new(move |_msg: Message| {
            let tx_clone = tx_arc.clone();
            tokio::spawn(async move {
                let tx = tx_clone.lock().await;
                let _ = tx.send(std::time::Instant::now()).await;
            });
        });

        let cancel = transport.subscribe("subms_topic", handler).await.unwrap();

        let msg = ::server_omnisolo::orchestration::TeammateMeshEvent {
            agent_id: "agent_fast".to_string(),
            action: "fast_action".to_string(),
            status: "ok".to_string(),
            payload: b"fast".to_vec(),
            msg_id: "fast_1".to_string(),
        };

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let start = std::time::Instant::now();
        transport.publish("subms_topic", msg).await.unwrap();

        if let Some(received_time) = rx.recv().await {
            let elapsed = received_time.duration_since(start);
            assert!(
                elapsed.as_millis() <= 50,
                "Latency was {} ms, expected < 50ms",
                elapsed.as_millis()
            );
        } else {
            panic!("Did not receive message");
        }
        cancel();
    }
}
