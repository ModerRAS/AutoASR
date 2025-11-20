//! 调用 SiliconFlow 语音转写 API 的封装。

use anyhow::{anyhow, Result};
use reqwest::{Client, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;
use tokio::fs::File;
use tokio_util::codec::{BytesCodec, FramedRead};

/// SiliconFlow 返回的成功响应结构。
#[derive(Deserialize, Debug)]
pub struct SuccessResponse {
    /// 服务端返回的完整转写文本。
    pub text: String,
}

/// 上传单个音频文件并返回识别文本，自动推断常见 MIME 类型。
pub async fn transcribe_file(api_key: &str, file_path: &Path) -> Result<String> {
    let client = Client::new();
    let url = "https://api.siliconflow.cn/v1/audio/transcriptions";

    let file_name = file_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    // Simple mime type detection
    let mime_type = if let Some(ext) = file_path.extension() {
        let ext_str = ext.to_string_lossy().to_lowercase();
        match ext_str.as_str() {
            "wav" => "audio/wav",
            "ogg" | "opus" => "audio/ogg",
            "mp3" => "audio/mpeg",
            "m4a" => "audio/mp4",
            _ => "audio/mpeg", // Fallback
        }
    } else {
        "audio/mpeg"
    };

    let file = File::open(file_path).await?;
    let stream = FramedRead::new(file, BytesCodec::new());
    let file_part = reqwest::multipart::Part::stream(reqwest::Body::wrap_stream(stream))
        .file_name(file_name)
        .mime_str(mime_type)?;

    let form = reqwest::multipart::Form::new()
        .text("model", "FunAudioLLM/SenseVoiceSmall")
        .part("file", file_part);

    let response = client
        .post(url)
        .header("Authorization", format!("Bearer {}", api_key))
        .multipart(form)
        .timeout(std::time::Duration::from_secs(3600)) // Long timeout for large files
        .send()
        .await?;

    let status = response.status();
    let text = response.text().await?;

    if status.is_success() {
        return serde_json::from_str::<SuccessResponse>(&text)
            .map(|succ| succ.text)
            .map_err(|_| anyhow!("Failed to parse success response: {}", text));
    }

    Err(anyhow!(format_api_error(status, &text)))
}

/// 将 API 错误响应格式化为易读的日志文本。
fn format_api_error(status: StatusCode, body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if let Some(obj) = value.as_object() {
            let code = obj.get("code").and_then(|v| v.as_i64());
            let message = obj.get("message").and_then(|v| v.as_str());
            let data = obj.get("data").and_then(|v| v.as_str());

            if code.is_some() || message.is_some() || data.is_some() {
                return format!(
                    "API Error (HTTP {}, code {:?}): {} {}",
                    status,
                    code,
                    message.unwrap_or(""),
                    data.unwrap_or("")
                )
                .trim()
                .to_string();
            }
        } else if let Some(text) = value.as_str() {
            return format!("API Error (HTTP {}): {}", status, text);
        }
    }

    // 429 specific plain message
    if status == StatusCode::TOO_MANY_REQUESTS {
        return format!("Rate limited (HTTP 429): {}", body);
    }

    format!("API Error (HTTP {}): {}", status, body)
}
