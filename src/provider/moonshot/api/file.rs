use serde::Deserialize;

use super::super::MoonshotClient;

#[derive(Debug, Clone, Deserialize)]
pub struct FileInfo {
    pub id: String,
    pub filename: String,
    pub bytes: u64,
    pub purpose: String,
    pub created_at: u64,
}

#[derive(Deserialize)]
struct FileList {
    data: Vec<FileInfo>,
}

impl MoonshotClient {
    pub async fn upload_file(
        &self,
        filename: &str,
        data: Vec<u8>,
        purpose: &str,
    ) -> Result<FileInfo, crate::error::Error> {
        let part = reqwest::multipart::Part::bytes(data)
            .file_name(filename.to_owned())
            .mime_str("application/octet-stream")
            .map_err(|e| crate::error::Error::Provider(e.to_string()))?;

        let form = reqwest::multipart::Form::new()
            .text("purpose", purpose.to_owned())
            .part("file", part);

        let response = self
            .http()
            .post(self.url("/files"))
            .header("Authorization", self.auth_header())
            .multipart(form)
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

        let info: FileInfo = response.json().await?;
        Ok(info)
    }

    pub async fn list_files(&self) -> Result<Vec<FileInfo>, crate::error::Error> {
        let response = self
            .http()
            .get(self.url("/files"))
            .header("Authorization", self.auth_header())
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

        let list: FileList = response.json().await?;
        Ok(list.data)
    }

    pub async fn get_file(&self, file_id: &str) -> Result<FileInfo, crate::error::Error> {
        let response = self
            .http()
            .get(self.url(&format!("/files/{}", file_id)))
            .header("Authorization", self.auth_header())
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

        let info: FileInfo = response.json().await?;
        Ok(info)
    }

    pub async fn get_file_content(&self, file_id: &str) -> Result<String, crate::error::Error> {
        let response = self
            .http()
            .get(self.url(&format!("/files/{}/content", file_id)))
            .header("Authorization", self.auth_header())
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

        let text = response.text().await?;
        Ok(text)
    }

    pub async fn delete_file(&self, file_id: &str) -> Result<(), crate::error::Error> {
        let response = self
            .http()
            .delete(self.url(&format!("/files/{}", file_id)))
            .header("Authorization", self.auth_header())
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

        Ok(())
    }
}
