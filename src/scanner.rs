//! 目录扫描与媒体处理逻辑，包含递归遍历、FFmpeg 转码与结果落盘。

use crate::api::transcribe_file;
use anyhow::{anyhow, Context, Result};
use std::fmt::Write as FmtWrite;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::{fs, process::Command, sync::mpsc::UnboundedSender, task};
use voice_activity_detector::VoiceActivityDetector;
use walkdir::WalkDir;

#[derive(Debug, Clone, Copy)]
pub enum ScanLogLevel {
    Info,
    Success,
    Error,
}

#[derive(Debug, Clone)]
pub struct ScanLog {
    pub level: ScanLogLevel,
    pub message: String,
}

impl ScanLog {
    pub fn new(level: ScanLogLevel, message: impl Into<String>) -> Self {
        Self {
            level,
            message: message.into(),
        }
    }
}

const VAD_SAMPLE_RATE: u32 = 16_000;
const VAD_CHUNK_SIZE: usize = 512;
const VAD_MIN_SPEECH_CHUNKS: usize = 10;
const VAD_PADDING_CHUNKS: usize = 3;
const VAD_DEFAULT_THRESHOLD: f32 = 0.6;
const VAD_DEFAULT_MIN_SEGMENT_SECS: f32 = 2.0;

#[derive(Clone)]
pub struct ScannerOptions {
    pub api_key: String,
    pub vad: Option<VadConfig>,
}

#[derive(Clone)]
pub struct VadConfig {
    pub threshold: f32,
    pub min_speech_chunks: usize,
    pub padding_chunks: usize,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            threshold: VAD_DEFAULT_THRESHOLD,
            min_speech_chunks: secs_to_chunks(VAD_DEFAULT_MIN_SEGMENT_SECS),
            padding_chunks: VAD_PADDING_CHUNKS,
        }
    }
}

impl VadConfig {
    pub fn from_user_settings(threshold: f32, min_segment_secs: f32) -> Self {
        let threshold = threshold.clamp(0.1, 0.99);
        let min_secs = min_segment_secs.clamp(0.5, 10.0);
        Self {
            threshold,
            min_speech_chunks: secs_to_chunks(min_secs),
            padding_chunks: VAD_PADDING_CHUNKS,
        }
    }
}

struct ScanLogger {
    logs: Vec<ScanLog>,
    progress: Option<UnboundedSender<ScanLog>>,
}

impl ScanLogger {
    fn new(progress: Option<UnboundedSender<ScanLog>>) -> Self {
        Self {
            logs: Vec::new(),
            progress,
        }
    }

    fn emit(&mut self, log: ScanLog) {
        if let Some(tx) = &self.progress {
            let _ = tx.send(log.clone());
        }
        self.logs.push(log);
    }

    fn info(&mut self, message: impl Into<String>) {
        self.emit(ScanLog::new(ScanLogLevel::Info, message));
    }

    fn success(&mut self, message: impl Into<String>) {
        self.emit(ScanLog::new(ScanLogLevel::Success, message));
    }

    fn error(&mut self, message: impl Into<String>) {
        self.emit(ScanLog::new(ScanLogLevel::Error, message));
    }

    fn finish(self) -> Vec<ScanLog> {
        self.logs
    }
}

enum PendingJob {
    Audio(PathBuf),
    Video { path: PathBuf, tracks: Vec<u32> },
}

/// 扫描指定目录并对尚未转写的媒体文件执行 ASR，返回日志列表。
pub async fn process_directory(
    dir: PathBuf,
    options: ScannerOptions,
    progress: Option<UnboundedSender<ScanLog>>,
) -> Result<Vec<ScanLog>> {
    let mut logger = ScanLogger::new(progress);
    let mut jobs = Vec::new();
    let api_key = options.api_key.clone();

    if api_key.trim().is_empty() {
        return Err(anyhow!(
            "API key is empty. Please configure it before running."
        ));
    }

    if !dir.exists() {
        return Err(anyhow!("Directory does not exist: {:?}", dir));
    }

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
                        logger.info(format!("跳过 {:?}：视频中未检测到音轨。", path));
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
                        logger.info(format!("跳过 {:?}：所有音轨均已转写。", path));
                        continue;
                    }

                    jobs.push(PendingJob::Video {
                        path: path.to_path_buf(),
                        tracks: pending_tracks,
                    });
                }
                Err(e) => {
                    logger.error(format!("读取 {:?} 音轨失败：{}", path, e));
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
        logger.info("没有检测到新的待转写文件。");
        return Ok(logger.finish());
    }

    let total_targets: usize = jobs
        .iter()
        .map(|job| match job {
            PendingJob::Audio(_) => 1,
            PendingJob::Video { tracks, .. } => tracks.len(),
        })
        .sum();

    logger.info(format!("待处理音轨总数：{}。", total_targets));

    let options = Arc::new(options);

    for job in jobs {
        match job {
            PendingJob::Audio(path) => {
                process_audio_source(options.clone(), &path, &path, None, &mut logger).await;
            }
            PendingJob::Video { path, tracks } => {
                for track in tracks {
                    match ensure_audio_track(&path, track).await {
                        Ok(audio_path) => {
                            process_audio_source(
                                options.clone(),
                                &path,
                                &audio_path,
                                Some(track),
                                &mut logger,
                            )
                            .await;
                        }
                        Err(e) => {
                            logger.error(format!("无法提取 {:?} 的音轨 {}：{}", path, track, e))
                        }
                    }
                }
            }
        }
    }

    Ok(logger.finish())
}
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

fn segment_audio_path(original: &Path, track_index: Option<u32>, segment_idx: usize) -> PathBuf {
    let file_name = original
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "segment".to_string());
    let track_suffix = track_index
        .map(|idx| format!("-track{}", idx))
        .unwrap_or_default();
    original.with_file_name(format!(
        "{}{}-seg{}.mp3",
        file_name, track_suffix, segment_idx
    ))
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
    options: Arc<ScannerOptions>,
    original_path: &Path,
    audio_path: &Path,
    track_index: Option<u32>,
    logger: &mut ScanLogger,
) {
    if let Some(vad_cfg) = options.vad.clone() {
        match process_with_vad(
            &options.api_key,
            original_path,
            audio_path,
            track_index,
            &vad_cfg,
            logger,
        )
        .await
        {
            Ok(_) => return,
            Err(err) => {
                logger.info(format!(
                    "VAD 分段失败（{}），回退整段上传：{:?}",
                    err, audio_path
                ));
            }
        }
    }

    process_without_vad(
        &options.api_key,
        original_path,
        audio_path,
        track_index,
        logger,
    )
    .await;
}

async fn process_without_vad(
    api_key: &str,
    original_path: &Path,
    audio_path: &Path,
    track_index: Option<u32>,
    logger: &mut ScanLogger,
) {
    let target_name = format!("{:?}{}", original_path, track_suffix(track_index, None));
    logger.info(format!("开始转写 {}，音频源 {:?}", target_name, audio_path));

    match transcribe_file(api_key, audio_path).await {
        Ok(text) => {
            let txt_path = transcript_result_path(original_path, track_index);
            match fs::write(&txt_path, text).await {
                Ok(_) => logger.success(format!("完成 {}，结果输出 {:?}", target_name, txt_path)),
                Err(e) => logger.error(format!("写入 {} 失败：{}", target_name, e)),
            }
        }
        Err(e) => logger.error(format!("调用 API 转写 {} 失败：{}", target_name, e)),
    }
}

async fn process_with_vad(
    api_key: &str,
    original_path: &Path,
    audio_path: &Path,
    track_index: Option<u32>,
    vad_cfg: &VadConfig,
    logger: &mut ScanLogger,
) -> Result<()> {
    let display_name = format!("{:?}{}", original_path, track_suffix(track_index, None));
    logger.info(format!("{} 启用 VAD，准备语音分段。", display_name));

    let pcm_path = convert_to_pcm16(audio_path).await?;
    let samples = read_wav_samples(&pcm_path).await?;
    let _ = fs::remove_file(&pcm_path).await;

    let segments = detect_speech_segments(&samples, vad_cfg)?;
    if segments.is_empty() {
        return Err(anyhow!("未检测到有效语音"));
    }

    logger.info(format!("检测到 {} 段语音，逐段上传。", segments.len()));

    let mut combined = String::new();
    for (idx, segment) in segments.iter().enumerate() {
        let segment_audio = export_segment_audio(audio_path, track_index, idx + 1, segment).await?;
        match transcribe_file(api_key, &segment_audio).await {
            Ok(text) => {
                logger.success(format!(
                    "分段 {} 完成（{} - {}）。",
                    idx + 1,
                    format_timestamp(segment.start_sec),
                    format_timestamp(segment.end_sec)
                ));
                let _ = writeln!(
                    &mut combined,
                    "[Segment {} | {} - {}]",
                    idx + 1,
                    format_timestamp(segment.start_sec),
                    format_timestamp(segment.end_sec)
                );
                let _ = writeln!(&mut combined, "{}", text.trim());
                combined.push('\n');
            }
            Err(e) => {
                logger.error(format!("分段 {} 调用 API 失败：{}", idx + 1, e));
            }
        }
        let _ = fs::remove_file(&segment_audio).await;
    }

    if combined.trim().is_empty() {
        return Err(anyhow!("所有分段均转写失败"));
    }

    let txt_path = transcript_result_path(original_path, track_index);
    fs::write(&txt_path, combined).await?;
    logger.success(format!(
        "{} VAD 分段完成，结果输出 {:?}",
        display_name, txt_path
    ));
    Ok(())
}

async fn convert_to_pcm16(audio_path: &Path) -> Result<PathBuf> {
    let output = audio_path.with_extension("vad.wav");
    let status = Command::new("ffmpeg")
        .arg("-i")
        .arg(audio_path)
        .arg("-ac")
        .arg("1")
        .arg("-ar")
        .arg(VAD_SAMPLE_RATE.to_string())
        .arg("-sample_fmt")
        .arg("s16")
        .arg("-y")
        .arg(&output)
        .status()
        .await?;

    if status.success() {
        Ok(output)
    } else {
        Err(anyhow!(
            "FFmpeg failed to convert audio for VAD: {}",
            status
        ))
    }
}

async fn read_wav_samples(path: &Path) -> Result<Vec<i16>> {
    let path = path.to_path_buf();
    task::spawn_blocking(move || {
        let mut reader = hound::WavReader::open(&path)?;
        let spec = reader.spec();
        if spec.sample_rate != VAD_SAMPLE_RATE || spec.channels != 1 || spec.bits_per_sample != 16 {
            return Err(anyhow!("Unexpected WAV format generated for VAD"));
        }

        let mut samples = Vec::new();
        for sample in reader.samples::<i16>() {
            samples.push(sample?);
        }
        Ok::<_, anyhow::Error>(samples)
    })
    .await?
}

#[derive(Clone, Debug)]
struct SegmentState {
    start_chunk: usize,
    last_active_chunk: usize,
}

impl SegmentState {
    fn new(start_chunk: usize) -> Self {
        Self {
            start_chunk,
            last_active_chunk: start_chunk,
        }
    }
}

#[derive(Clone, Debug)]
struct SpeechSegment {
    start_sec: f64,
    end_sec: f64,
}

impl SpeechSegment {
    fn from_chunks(start_chunk: usize, end_chunk: usize) -> Self {
        Self {
            start_sec: chunk_to_time(start_chunk),
            end_sec: chunk_to_time(end_chunk),
        }
    }
}

fn chunk_to_time(chunk: usize) -> f64 {
    (chunk as f64 * VAD_CHUNK_SIZE as f64) / VAD_SAMPLE_RATE as f64
}

fn secs_to_chunks(secs: f32) -> usize {
    let raw = ((secs * VAD_SAMPLE_RATE as f32) / VAD_CHUNK_SIZE as f32).ceil() as usize;
    raw.max(VAD_MIN_SPEECH_CHUNKS)
}

fn detect_speech_segments(samples: &[i16], cfg: &VadConfig) -> Result<Vec<SpeechSegment>> {
    let mut vad = VoiceActivityDetector::builder()
        .sample_rate(VAD_SAMPLE_RATE)
        .chunk_size(VAD_CHUNK_SIZE)
        .build()
        .context("Failed to initialize voice activity detector")?;

    let mut segments = Vec::new();
    let mut current: Option<SegmentState> = None;
    let mut trailing_silence = 0usize;

    let mut chunk_index = 0usize;
    let mut sample_index = 0usize;
    while sample_index < samples.len() {
        let end = usize::min(sample_index + VAD_CHUNK_SIZE, samples.len());
        let mut chunk = vec![0i16; VAD_CHUNK_SIZE];
        chunk[..(end - sample_index)].copy_from_slice(&samples[sample_index..end]);

        let probability = vad.predict(chunk);
        if probability >= cfg.threshold {
            match &mut current {
                Some(state) => state.last_active_chunk = chunk_index,
                None => current = Some(SegmentState::new(chunk_index)),
            }
            trailing_silence = 0;
        } else if let Some(state) = &mut current {
            trailing_silence += 1;
            if trailing_silence > cfg.padding_chunks {
                finalize_segment(state, cfg, &mut segments);
                current = None;
                trailing_silence = 0;
            }
        }

        sample_index = end;
        chunk_index += 1;
    }

    if let Some(state) = current {
        finalize_segment(&state, cfg, &mut segments);
    }

    Ok(segments)
}

fn finalize_segment(state: &SegmentState, cfg: &VadConfig, segments: &mut Vec<SpeechSegment>) {
    let duration_chunks = state.last_active_chunk.saturating_sub(state.start_chunk) + 1;
    if duration_chunks >= cfg.min_speech_chunks {
        segments.push(SpeechSegment::from_chunks(
            state.start_chunk,
            state.last_active_chunk + 1,
        ));
    }
}

async fn export_segment_audio(
    audio_path: &Path,
    track_index: Option<u32>,
    segment_idx: usize,
    segment: &SpeechSegment,
) -> Result<PathBuf> {
    let output = segment_audio_path(audio_path, track_index, segment_idx);
    let duration = (segment.end_sec - segment.start_sec).max(0.25);
    let status = Command::new("ffmpeg")
        .arg("-ss")
        .arg(format!("{:.3}", segment.start_sec))
        .arg("-i")
        .arg(audio_path)
        .arg("-t")
        .arg(format!("{:.3}", duration))
        .arg("-acodec")
        .arg("libmp3lame")
        .arg("-y")
        .arg(&output)
        .status()
        .await?;

    if status.success() {
        Ok(output)
    } else {
        Err(anyhow!("FFmpeg failed to cut audio segment: {}", status))
    }
}

fn format_timestamp(seconds: f64) -> String {
    let total_ms = (seconds * 1000.0).round().max(0.0) as u64;
    let hours = total_ms / 3_600_000;
    let minutes = (total_ms % 3_600_000) / 60_000;
    let secs = (total_ms % 60_000) / 1000;
    let millis = total_ms % 1000;
    if hours > 0 {
        format!("{:02}:{:02}:{:02}.{:03}", hours, minutes, secs, millis)
    } else {
        format!("{:02}:{:02}.{:03}", minutes, secs, millis)
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

fn track_suffix(track_index: Option<u32>, segment_index: Option<usize>) -> String {
    match (track_index, segment_index) {
        (Some(track), Some(segment)) => format!(" (track {} / segment {})", track, segment),
        (Some(track), None) => format!(" (track {})", track),
        (None, Some(segment)) => format!(" (segment {})", segment),
        (None, None) => String::new(),
    }
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
