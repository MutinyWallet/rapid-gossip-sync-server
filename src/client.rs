use ureq::Agent;
use crate::{SerializedResponse, config};

#[derive(Debug, Clone)]
pub struct Client {
    pub base_url: String,
    agent: Agent,
}

impl Client {
    /// build a blocking client from a [`Builder`]
    pub fn new() -> Self {
        let agent_builder = ureq::AgentBuilder::new();

        Self::from_agent(config::upload_url(), agent_builder.build())
    }

    /// build a blocking client from an [`Agent`]
    pub fn from_agent(base_url: String, agent: Agent) -> Self {
        Client { base_url, agent }
    }

    pub fn post_snapshot(
        &self,
        snapshot: SerializedResponse,
        timestamp: u64,
        token: String,
    ) -> anyhow::Result<()> {
        let resp = self
            .agent
            .post(&format!("{}/v1/rgs/snapshot/{}", self.base_url, timestamp))
            .set("X-API-KEY", &token)
            .send_json(snapshot);

        match resp {
            Ok(_resp) => Ok(()),
            Err(ureq::Error::Status(code, resp)) => {
                let str = resp.into_string().ok();
                Err(anyhow::anyhow!("{}: {}", code, str.unwrap_or_default()))
            }
            Err(e) => Err(e.into()),
        }
    }
}
