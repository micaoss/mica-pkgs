//! The broker side, behind one trait.
//!
//! [`Transport`] is the seam the crate's tests rest on (see the crate docs):
//! everything this bridge is responsible for sits above it, and what sits
//! below it is an MQTT client library that upstream already tests. The
//! protocol tests supply their own implementation; the daemon supplies
//! [`MqttTransport`].

use async_trait::async_trait;
use rumqttc::{AsyncClient, QoS};

use crate::bridge::Publication;

/// How the bridge reaches a broker.
#[async_trait]
pub trait Transport: Send + Sync {
    async fn publish(&self, publication: &Publication) -> anyhow::Result<()>;
    async fn subscribe(&self, filter: &str) -> anyhow::Result<()>;
    async fn unsubscribe(&self, filter: &str) -> anyhow::Result<()>;
}

/// Everything this bridge sends and receives is QoS 0.
const QOS: QoS = QoS::AtMostOnce;

/// [`Transport`] over a real MQTT connection.
pub struct MqttTransport {
    client: AsyncClient,
}

impl MqttTransport {
    pub fn new(client: AsyncClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Transport for MqttTransport {
    async fn publish(&self, publication: &Publication) -> anyhow::Result<()> {
        self.client
            .publish(
                &publication.topic,
                QOS,
                publication.retain,
                publication.payload.clone(),
            )
            .await?;
        Ok(())
    }

    async fn subscribe(&self, filter: &str) -> anyhow::Result<()> {
        self.client.subscribe(filter, QOS).await?;
        Ok(())
    }

    async fn unsubscribe(&self, filter: &str) -> anyhow::Result<()> {
        self.client.unsubscribe(filter).await?;
        Ok(())
    }
}
