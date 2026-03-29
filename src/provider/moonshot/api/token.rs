use serde::{Deserialize, Serialize};

use super::super::{Message, MoonshotClient};

#[derive(Serialize)]
struct EstimateRequest {
    model: String,
    messages: Vec<Message>,
}

#[derive(Deserialize)]
struct EstimateResponse {
    data: EstimateData,
}

#[derive(Deserialize)]
struct EstimateData {
    total_tokens: usize,
}

impl MoonshotClient {
    pub async fn estimate_tokens(
        &self,
        messages: &[Message],
    ) -> Result<usize, crate::error::Error> {
        let request = EstimateRequest {
            model: self.model().to_owned(),
            messages: messages.to_vec(),
        };

        let response = self
            .http()
            .post(self.url("/tokenizers/estimate-token-count"))
            .header("Authorization", self.auth_header())
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(crate::error::Error::ProviderApi {
                status: status.as_u16(),
                body,
            });
        }

        let estimate: EstimateResponse = response.json().await?;
        Ok(estimate.data.total_tokens)
    }
}
