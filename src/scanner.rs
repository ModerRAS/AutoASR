//! 目录扫描与媒体处理逻辑，包含递归遍历、FFmpeg 转码与结果落盘。

use crate::api::transcribe_file;
use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use tokio::{fs, process::Command};
use walkdir::WalkDir;

/// 扫描指定目录并对尚未转写的媒体文件执行 ASR，返回日志列表。
pub async fn process_directory(dir: PathBuf, api_key: String) -> Result<Vec<String>> {
    let mut logs = Vec::new();
    let mut files_to_process = Vec::new();

    if api_key.trim().is_empty() {
        return Err(anyhow!(
            "API key is empty. Please configure it before running."
        ));
    }

    if !dir.exists() {
        return Err(anyhow!("Directory does not exist: {:?}", dir));
    }

    // 1. Scan directory
    for entry in WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_file() {
            if let Some(ext) = path.extension() {
                let ext_str = ext.to_string_lossy().to_lowercase();
                // Check if it's a media file
                if is_media_extension(&ext_str) {
                    let txt_path = transcript_result_path(path);
                    if !txt_path.exists() {
                        files_to_process.push(path.to_path_buf());
                    }
                }
            }
        }
    }

    if files_to_process.is_empty() {
        return Ok(vec!["No new files to process.".to_string()]);
    }

    logs.push(format!(
        "Found {} files to process.",
        files_to_process.len()
    ));

    for file_path in files_to_process {
        logs.push(format!("Processing: {:?}", file_path));

        let mut actual_file_path = file_path.clone();
        let mut generated_temp_file = None;

        // Check if video and convert
        if is_video(&file_path) {
            let audio_path = file_path.with_extension("mp3");
            logs.push(format!("Converting video to audio: {:?}", audio_path));

            match convert_to_mp3(&file_path, &audio_path).await {
                Ok(_) => {
                    actual_file_path = audio_path.clone();
                    generated_temp_file = Some(audio_path);
                }
                Err(e) => {
                    logs.push(format!("Failed to convert {:?}: {}", file_path, e));
                    continue;
                }
            }
        }

        // Transcribe
        match transcribe_file(&api_key, &actual_file_path).await {
            Ok(text) => {
                let txt_path = transcript_result_path(&file_path);
                if let Err(e) = fs::write(&txt_path, text).await {
                    logs.push(format!("Failed to save result for {:?}: {}", file_path, e));
                } else {
                    logs.push(format!("Success: Saved to {:?}", txt_path));
                }
            }
            Err(e) => {
                logs.push(format!("API Error for {:?}: {}", file_path, e));
            }
        }

        if let Some(temp_file) = generated_temp_file {
            match fs::remove_file(&temp_file).await {
                Ok(_) => logs.push(format!("Removed temporary file {:?}", temp_file)),
                Err(e) => logs.push(format!(
                    "Failed to delete temporary file {:?}: {}",
                    temp_file, e
                )),
            }
        }
    }

    Ok(logs)
}

/// 判断扩展名是否为受支持的音视频格式。
fn is_media_extension(ext: &str) -> bool {
    matches!(
        ext,
        "mkv" | "mp4" | "avi" | "mov" | "flv" | "wmv" | "wav" | "ogg" | "opus" | "mp3" | "m4a"
    )
}

/// 判断给定路径是否属于需要先转码的视频文件。
fn is_video(path: &Path) -> bool {
    if let Some(ext) = path.extension() {
        let ext = ext.to_string_lossy().to_lowercase();
        matches!(ext.as_str(), "mkv" | "mp4" | "avi" | "mov" | "flv" | "wmv")
    } else {
        false
    }
}

/// 通过 FFmpeg 将视频转为 MP3 音频，供 ASR 上传使用。
async fn convert_to_mp3(input: &Path, output: &Path) -> Result<()> {
    let status = Command::new("ffmpeg")
        .arg("-i")
        .arg(input)
        .arg("-vn")
        .arg("-acodec")
        .arg("libmp3lame")
        .arg("-y")
        .arg(output)
        .status()
        .await?;

    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("FFmpeg exited with status: {}", status))
    }
}

/// 基于原始文件名生成转写结果 `.txt` 路径。
fn transcript_result_path(original: &Path) -> PathBuf {
    let file_name = original
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "result".to_string());
    original.with_file_name(format!("{}.txt", file_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_extension_detection() {
        for ext in ["mp3", "wav", "ogg", "mp4", "mkv"] {
            assert!(is_media_extension(ext));
        }

        for ext in ["txt", "rs", "json", "zip"] {
            assert!(!is_media_extension(ext));
        }
    }

    #[test]
    fn video_detection() {
        assert!(is_video(Path::new("C:/data/sample.MP4")));
        assert!(!is_video(Path::new("C:/data/audio.mp3")));
        assert!(!is_video(Path::new("C:/data/no_ext")));
    }

    #[test]
    fn transcript_path_preserves_original_name() {
        let path = Path::new("C:/tmp/input/video.mp4");
        let txt = transcript_result_path(path);
        assert_eq!(txt, PathBuf::from("C:/tmp/input/video.mp4.txt"));

        let no_ext = Path::new("/tmp/audio");
        let txt2 = transcript_result_path(no_ext);
        assert_eq!(txt2, PathBuf::from("/tmp/audio.txt"));
    }
}
