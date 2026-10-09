//! 对 `larksuite-oapi-sdk-rs` 的薄封装：回复 / 更新消息、下载图片、查机器人身份。
use larksuite_oapi_sdk_rs::service::im::v1::{
    CreateMessageReqBody, PatchMessageReqBody, ReplyMessageReqBody, UpdateMessageReqBody,
};
use larksuite_oapi_sdk_rs::{AccessTokenType, ApiReq, LarkClient, LarkError, RequestOption};
use serde_json::Value;
use tracing::warn;

#[derive(Clone)]
pub struct LarkApi {
    client: LarkClient,
}

#[derive(Debug, thiserror::Error)]
pub enum LarkApiError {
    #[error("飞书 SDK 错误: {0}")]
    Sdk(#[from] LarkError),
    #[error("飞书接口返回错误 code={code} msg={msg}")]
    Api { code: i64, msg: String },
}

impl LarkApi {
    pub fn new(client: LarkClient) -> Self {
        Self { client }
    }

    pub fn client(&self) -> &LarkClient {
        &self.client
    }

    /// 以「回复」方式发文本。返回新消息 id。
    pub async fn reply_text(&self, message_id: &str, text: &str, in_thread: bool, uuid: Option<String>) -> Result<String, LarkApiError> {
        let body = ReplyMessageReqBody {
            content: Some(serde_json::json!({ "text": text }).to_string()),
            msg_type: Some("text".into()),
            reply_in_thread: Some(in_thread),
            uuid,
        };
        let resp = self.client.im().message.reply(message_id, &body, &RequestOption::default()).await?;
        if !resp.success() {
            return Err(api_err(resp.code_error.code, &resp.code_error.msg));
        }
        Ok(resp.data.and_then(|d| d.message_id).unwrap_or_default())
    }

    /// 以「回复」方式发卡片。返回新消息 id。
    pub async fn reply_card(&self, message_id: &str, card: &Value, in_thread: bool, uuid: Option<String>) -> Result<String, LarkApiError> {
        let body = ReplyMessageReqBody {
            content: Some(card.to_string()),
            msg_type: Some("interactive".into()),
            reply_in_thread: Some(in_thread),
            uuid,
        };
        let resp = self.client.im().message.reply(message_id, &body, &RequestOption::default()).await?;
        if !resp.success() {
            return Err(api_err(resp.code_error.code, &resp.code_error.msg));
        }
        Ok(resp.data.and_then(|d| d.message_id).unwrap_or_default())
    }

    /// 直接向 chat 发文本（管理员通知、排队提示）。
    pub async fn send_text(&self, chat_id: &str, text: &str) -> Result<String, LarkApiError> {
        self.send(chat_id, "chat_id", "text", &serde_json::json!({ "text": text }).to_string()).await
    }

    pub async fn send_text_to_user(&self, open_id: &str, text: &str) -> Result<String, LarkApiError> {
        self.send(open_id, "open_id", "text", &serde_json::json!({ "text": text }).to_string()).await
    }

    pub async fn send_card(&self, chat_id: &str, card: &Value) -> Result<String, LarkApiError> {
        self.send(chat_id, "chat_id", "interactive", &card.to_string()).await
    }

    async fn send(&self, receive_id: &str, id_type: &str, msg_type: &str, content: &str) -> Result<String, LarkApiError> {
        let body = CreateMessageReqBody {
            receive_id: Some(receive_id.into()),
            msg_type: Some(msg_type.into()),
            content: Some(content.into()),
            uuid: None,
        };
        let resp = self.client.im().message.create(id_type, &body, &RequestOption::default()).await?;
        if !resp.success() {
            return Err(api_err(resp.code_error.code, &resp.code_error.msg));
        }
        Ok(resp.data.and_then(|d| d.message_id).unwrap_or_default())
    }

    /// 更新卡片消息（PATCH，仅 interactive 可用）。
    pub async fn patch_card(&self, message_id: &str, card: &Value) -> Result<(), LarkApiError> {
        let body = PatchMessageReqBody { content: Some(card.to_string()) };
        let resp = self.client.im().message.patch(message_id, &body, &RequestOption::default()).await?;
        if !resp.success() {
            return Err(api_err(resp.code_error.code, &resp.code_error.msg));
        }
        Ok(())
    }

    /// 更新文本消息（PUT）。
    pub async fn update_text(&self, message_id: &str, text: &str) -> Result<(), LarkApiError> {
        let body = UpdateMessageReqBody {
            msg_type: Some("text".into()),
            content: Some(serde_json::json!({ "text": text }).to_string()),
        };
        let resp = self.client.im().message.update(message_id, &body, &RequestOption::default()).await?;
        if !resp.success() {
            return Err(api_err(resp.code_error.code, &resp.code_error.msg));
        }
        Ok(())
    }

    /// 下载消息里的图片，返回 (bytes, mime)。
    pub async fn download_image(&self, message_id: &str, image_key: &str) -> Result<(Vec<u8>, String), LarkApiError> {
        let resp = self
            .client
            .im()
            .message_resource
            .get(message_id, image_key, "image", &RequestOption::default())
            .await?;
        if resp.api_resp.status_code != 200 {
            return Err(api_err(resp.api_resp.status_code as i64, "下载图片失败"));
        }
        let mime = sniff_mime(&resp.data).to_string();
        Ok((resp.data, mime))
    }

    /// 机器人自己的 open_id（用于群聊 @ 判定）。失败时返回 None 并打 warn。
    pub async fn bot_open_id(&self) -> Option<String> {
        let mut req = ApiReq::new(http_method_get(), "/open-apis/bot/v3/info");
        req.supported_access_token_types = vec![AccessTokenType::Tenant];
        match self.client.raw_request(&req, &RequestOption::default()).await {
            Ok(resp) => {
                let v: Value = serde_json::from_slice(&resp.raw_body).unwrap_or(Value::Null);
                let id = v.pointer("/bot/open_id").and_then(Value::as_str).map(str::to_string);
                if id.is_none() {
                    warn!(status = resp.status_code, "bot/v3/info 没有返回 open_id");
                }
                id
            }
            Err(e) => {
                warn!(error = %e, "获取机器人身份失败，群聊 @ 判定将退化");
                None
            }
        }
    }
}

fn http_method_get() -> larksuite_oapi_sdk_rs::HttpMethod {
    larksuite_oapi_sdk_rs::HttpMethod::GET
}

fn api_err(code: i64, msg: &str) -> LarkApiError {
    LarkApiError::Api { code, msg: msg.to_string() }
}

pub fn sniff_mime(data: &[u8]) -> &'static str {
    if data.starts_with(&[0x89, b'P', b'N', b'G']) {
        "image/png"
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if data.starts_with(b"GIF8") {
        "image/gif"
    } else if data.len() > 12 && &data[8..12] == b"WEBP" {
        "image/webp"
    } else if data.starts_with(b"BM") {
        "image/bmp"
    } else {
        "application/octet-stream"
    }
}

#[cfg(test)]
mod tests {
    use super::sniff_mime;

    #[test]
    fn sniffs_common_formats() {
        assert_eq!(sniff_mime(&[0x89, b'P', b'N', b'G', 0, 0]), "image/png");
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), "image/jpeg");
        assert_eq!(sniff_mime(b"GIF89a"), "image/gif");
        assert_eq!(sniff_mime(b"RIFF\0\0\0\0WEBPVP8 "), "image/webp");
        assert_eq!(sniff_mime(b"zzzz"), "application/octet-stream");
    }
}
