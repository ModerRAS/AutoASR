//! 目录扫描与媒体处理逻辑，包含递归遍历、FFmpeg 转码与结果落盘。

use crate::api::transcribe_file;
use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use tokio::{fs, process::Command};
use walkdir::WalkDir;

enum PendingJob {
    Audio(PathBuf),
    Video { path: PathBuf, tracks: Vec<u32> },
}

/// 扫描指定目录并对尚未转写的媒体文件执行 ASR，返回日志列表。
pub async fn process_directory(dir: PathBuf, api_key: String) -> Result<Vec<String>> {
    let mut logs = Vec::new();
    let mut jobs = Vec::new();

    if api_key.trim().is_empty() {
        return Err(anyhow!(
            "API key is empty. Please configure it before running."
        ));
    }

    if !dir.exists() {
        return Err(anyhow!("Directory does not exist: {:?}", dir));
    }

    // 1. Scan directory并决定待处理任务
    for entry in WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let Some(ext) = path.extension() else {
            continue;
        };

        let ext_str = ext.to_string_lossy().to_lowercase();
        if !is_media_extension(&ext_str) {
            continue;
        }

        if is_video(path) {
            match audio_stream_indices(path).await {
                Ok(indices) => {
                    if indices.is_empty() {
                        logs.push(format!("Skip {:?}: video contains no audio stream.", path));
                        continue;
                    }

                    let mut pending_tracks = Vec::new();
                    for idx in indices {
                        let transcript_path = transcript_result_path(path, Some(idx));
                        if !transcript_path.exists() {
                            pending_tracks.push(idx);
                        }
                    }

                    if pending_tracks.is_empty() {
                        logs.push(format!(
                            "Skip {:?}: all audio tracks already transcribed.",
                            path
                        ));
                        continue;
                    }

                    jobs.push(PendingJob::Video {
                        path: path.to_path_buf(),
                        tracks: pending_tracks,
                    });
                }
                Err(e) => {
                    logs.push(format!(
                        "Failed to inspect audio tracks for {:?}: {}",
                        path, e
                    ));
                }
            }
        } else {
            let transcript_path = transcript_result_path(path, None);
            if transcript_path.exists() {
                continue;
            }
            jobs.push(PendingJob::Audio(path.to_path_buf()));
        }
    }

    if jobs.is_empty() {
        return Ok(vec!["No new files to process.".to_string()]);
    }

    let total_targets: usize = jobs
        .iter()
        .map(|job| match job {
            PendingJob::Audio(_) => 1,
            PendingJob::Video { tracks, .. } => tracks.len(),
        })
        .sum();

    logs.push(format!("Found {} audio targets to process.", total_targets));

    for job in jobs {
        match job {
            PendingJob::Audio(path) => {
                logs.push(format!("Processing audio file {:?}", path));
                process_audio_source(&api_key, &path, &path, None, &mut logs).await;
            }
            PendingJob::Video { path, tracks } => {
                for track in tracks {
                    logs.push(format!("Processing {:?} track {}", path, track));
                    match ensure_audio_track(&path, track).await {
                        Ok(audio_path) => {
                            logs.push(format!(
                                "Audio track {} prepared at {:?}",
                                track, audio_path
                            ));
                            process_audio_source(
                                &api_key,
                                &path,
                                &audio_path,
                                Some(track),
                                &mut logs,
                            )
                            .await;
                        }
                        Err(e) => logs.push(format!(
                            "Failed to prepare audio for {:?} track {}: {}",
                            path, track, e
                        )),
                    }
                }
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

/// 通过 FFmpeg 将特定音轨转为 MP3 音频，供 ASR 上传使用。
async fn convert_track_to_mp3(input: &Path, stream_index: u32, output: &Path) -> Result<()> {
    let status = Command::new("ffmpeg")
        .arg("-i")
        .arg(input)
        .arg("-map")
        .arg(format!("0:{}", stream_index))
        .arg("-c:a")
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

/// 基于原始文件名生成转写结果 `.txt` 路径，可附带音轨编号。
fn transcript_result_path(original: &Path, track_index: Option<u32>) -> PathBuf {
    let file_name = original
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "result".to_string());

    let target_name = match track_index {
        Some(idx) => format!("{}-track{}.txt", file_name, idx),
        None => format!("{}.txt", file_name),
    };

    original.with_file_name(target_name)
}

/// 基于原始视频生成指定音轨的 mp3 文件名。
fn audio_track_path(original: &Path, track_index: u32) -> PathBuf {
    let file_name = original
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "audio".to_string());
    original.with_file_name(format!("{}-track{}.mp3", file_name, track_index))
}

async fn ensure_audio_track(video_path: &Path, stream_index: u32) -> Result<PathBuf> {
    let output = audio_track_path(video_path, stream_index);
    if output.exists() {
        return Ok(output);
    }
    convert_track_to_mp3(video_path, stream_index, &output).await?;
    Ok(output)
}

async fn process_audio_source(
    api_key: &str,
    original_path: &Path,
    audio_path: &Path,
    track_index: Option<u32>,
    logs: &mut Vec<String>,
) {
    match transcribe_file(api_key, audio_path).await {
        Ok(text) => {
            let txt_path = transcript_result_path(original_path, track_index);
            if let Err(e) = fs::write(&txt_path, text).await {
                logs.push(format!(
                    "Failed to save result for {:?}{}: {}",
                    original_path,
                    track_suffix(track_index),
                    e
                ));
            } else {
                logs.push(format!("Success: Saved to {:?}", txt_path));
            }
        }
        Err(e) => logs.push(format!(
            "API Error for {:?}{}: {}",
            original_path,
            track_suffix(track_index),
            e
        )),
    }
}

async fn audio_stream_indices(path: &Path) -> Result<Vec<u32>> {
    let output = Command::new("ffprobe")
        .arg("-v")
        .arg("error")
        .arg("-select_streams")
        .arg("a")
        .arg("-show_entries")
        .arg("stream=index")
        .arg("-of")
        .arg("csv=p=0")
        .arg(path)
        .output()
        .await?;

    if !output.status.success() {
        return Err(anyhow!("ffprobe exited with status: {}", output.status));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let indices = stdout
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect();

    Ok(indices)
}

fn track_suffix(track_index: Option<u32>) -> String {
    track_index
        .map(|idx| format!(" (track {})", idx))
        .unwrap_or_default()
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
        let txt = transcript_result_path(path, None);
        assert_eq!(txt, PathBuf::from("C:/tmp/input/video.mp4.txt"));

        let track_txt = transcript_result_path(path, Some(2));
        assert_eq!(
            track_txt,
            PathBuf::from("C:/tmp/input/video.mp4-track2.txt")
        );

        let no_ext = Path::new("/tmp/audio");
        let txt2 = transcript_result_path(no_ext, None);
        assert_eq!(txt2, PathBuf::from("/tmp/audio.txt"));
    }

    #[test]
    fn audio_track_path_includes_track_id() {
        let path = Path::new("/media/sample.mkv");
        let mp3 = audio_track_path(path, 1);
        assert_eq!(mp3, PathBuf::from("/media/sample.mkv-track1.mp3"));
    }
}
