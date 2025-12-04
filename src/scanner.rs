//! 目录扫描与媒体处理逻辑，包含递归遍历、FFmpeg 转码与结果落盘。

use crate::api::transcribe_file;
use anyhow::{anyhow, Context, Result};
use std::env;
use std::ffi::OsString;
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
const MIN_EXPORT_DURATION_SEC: f64 = 0.25;
const MIN_SEGMENT_EPS: f64 = 1e-3;

fn resolve_tool_path(tool: &str) -> OsString {
    fn candidate_name(tool: &str) -> String {
        if cfg!(windows) {
            format!("{tool}.exe")
        } else {
            tool.to_string()
        }
    }

    if let Ok(mut current) = env::current_exe() {
        if current.pop() {
            let candidate = current.join(candidate_name(tool));
            if candidate.exists() {
                return candidate.into_os_string();
            }
        }
    }

    OsString::from(tool)
}

fn ffmpeg_program() -> OsString {
    resolve_tool_path("ffmpeg")
}

fn ffprobe_program() -> OsString {
    resolve_tool_path("ffprobe")
}

#[derive(Clone)]
pub struct ScannerOptions {
    pub api_key: String,
    pub api_url: String,
    pub model_name: String,
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

struct MaterializedAudio {
    path: PathBuf,
    cleanup: bool,
}

#[derive(Clone)]
struct AudioSource {
    original_path: PathBuf,
    track_index: Option<u32>,
    kind: AudioSourceKind,
}

#[derive(Clone)]
enum AudioSourceKind {
    DirectAudio {
        audio_path: PathBuf,
    },
    VideoTrack {
        video_path: PathBuf,
        track_index: u32,
    },
}

impl AudioSource {
    fn from_audio_file(path: PathBuf) -> Self {
        Self {
            original_path: path.clone(),
            track_index: None,
            kind: AudioSourceKind::DirectAudio { audio_path: path },
        }
    }

    fn from_video_track(path: PathBuf, track_index: u32) -> Self {
        Self {
            original_path: path.clone(),
            track_index: Some(track_index),
            kind: AudioSourceKind::VideoTrack {
                video_path: path,
                track_index,
            },
        }
    }

    fn original_path(&self) -> &Path {
        &self.original_path
    }

    fn track_index(&self) -> Option<u32> {
        self.track_index
    }

    fn display_name(&self) -> String {
        format!(
            "{:?}{}",
            self.original_path,
            track_suffix(self.track_index, None)
        )
    }

    fn input_path(&self) -> &Path {
        match &self.kind {
            AudioSourceKind::DirectAudio { audio_path } => audio_path,
            AudioSourceKind::VideoTrack { video_path, .. } => video_path,
        }
    }

    fn map_arg(&self) -> Option<String> {
        match (&self.kind, self.track_index) {
            (AudioSourceKind::VideoTrack { .. }, Some(track)) => Some(format!("0:{}", track)),
            _ => None,
        }
    }

    async fn materialize_full_audio(&self) -> Result<MaterializedAudio> {
        match &self.kind {
            AudioSourceKind::DirectAudio { audio_path } => Ok(MaterializedAudio {
                path: audio_path.clone(),
                cleanup: false,
            }),
            AudioSourceKind::VideoTrack {
                video_path,
                track_index,
            } => {
                let output = audio_track_path(video_path, *track_index);
                if output.exists() {
                    let _ = fs::remove_file(&output).await;
                }
                convert_track_to_mp3(video_path, *track_index, &output).await?;
                Ok(MaterializedAudio {
                    path: output,
                    cleanup: true,
                })
            }
        }
    }

    async fn convert_to_pcm16(&self) -> Result<PathBuf> {
        let output = vad_audio_path(&self.original_path, self.track_index);
        if output.exists() {
            let _ = fs::remove_file(&output).await;
        }

        let mut cmd = Command::new(ffmpeg_program());
        cmd.arg("-i").arg(self.input_path());
        if let Some(map) = self.map_arg() {
            cmd.arg("-map").arg(map);
        }
        cmd.arg("-ac")
            .arg("1")
            .arg("-ar")
            .arg(VAD_SAMPLE_RATE.to_string())
            .arg("-sample_fmt")
            .arg("s16")
            .arg("-y")
            .arg(&output);

        let status = cmd.status().await?;
        if status.success() {
            Ok(output)
        } else {
            Err(anyhow!(
                "FFmpeg 转换音频用于 VAD 时失败，退出状态：{}",
                status
            ))
        }
    }

    async fn export_segment_audio(
        &self,
        segment_idx: usize,
        segment: &SpeechSegment,
    ) -> Result<PathBuf> {
        let output = segment_audio_path(&self.original_path, self.track_index, segment_idx);
        if output.exists() {
            let _ = fs::remove_file(&output).await;
        }

        let duration = (segment.end_sec - segment.start_sec).max(MIN_EXPORT_DURATION_SEC);
        let mut cmd = Command::new(ffmpeg_program());
        cmd.arg("-ss")
            .arg(format!("{:.3}", segment.start_sec))
            .arg("-i")
            .arg(self.input_path());
        if let Some(map) = self.map_arg() {
            cmd.arg("-map").arg(map);
        }
        cmd.arg("-t")
            .arg(format!("{:.3}", duration))
            .arg("-acodec")
            .arg("libmp3lame")
            .arg("-y")
            .arg(&output);

        let status = cmd.status().await?;
        if status.success() {
            Ok(output)
        } else {
            Err(anyhow!("FFmpeg 裁剪语音片段失败，退出状态：{}", status))
        }
    }
}

async fn cleanup_materialized(audio: MaterializedAudio) -> Result<()> {
    if audio.cleanup {
        fs::remove_file(&audio.path).await?;
    }
    Ok(())
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
        return Err(anyhow!("API Key 为空，请在设置中填写后再运行。"));
    }

    if !dir.exists() {
        return Err(anyhow!("目录不存在：{:?}", dir));
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
                let source = AudioSource::from_audio_file(path);
                process_audio_source(options.clone(), source, &mut logger).await;
            }
            PendingJob::Video { path, tracks } => {
                for track in tracks {
                    let source = AudioSource::from_video_track(path.clone(), track);
                    process_audio_source(options.clone(), source, &mut logger).await;
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
    let status = Command::new(ffmpeg_program())
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
        Err(anyhow!("FFmpeg 转码音轨失败，退出状态：{}", status))
    }
}

/// 基于原始文件名生成转写结果 `.srt` 路径，可附带音轨编号。
fn transcript_result_path(original: &Path, track_index: Option<u32>) -> PathBuf {
    let base_name = original
        .file_stem()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "result".to_string());

    let target_name = match track_index {
        Some(idx) => format!("{}.轨道{}.srt", base_name, idx),
        None => format!("{}.srt", base_name),
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
    let track_suffix = track_file_suffix(track_index);
    original.with_file_name(format!(
        "{}{}-seg{}.mp3",
        file_name, track_suffix, segment_idx
    ))
}

fn vad_audio_path(original: &Path, track_index: Option<u32>) -> PathBuf {
    let file_name = original
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "segment".to_string());
    let track_suffix = track_file_suffix(track_index);
    original.with_file_name(format!("{}{}-vad.wav", file_name, track_suffix))
}

fn track_file_suffix(track_index: Option<u32>) -> String {
    track_index
        .map(|idx| format!("-track{}", idx))
        .unwrap_or_default()
}

async fn process_audio_source(
    options: Arc<ScannerOptions>,
    source: AudioSource,
    logger: &mut ScanLogger,
) {
    let mut handled = false;

    if let Some(vad_cfg) = options.vad.clone() {
        match process_with_vad(
            &options.api_key,
            &options.api_url,
            &options.model_name,
            &source,
            &vad_cfg,
            logger,
        )
        .await
        {
            Ok(_) => handled = true,
            Err(err) => {
                logger.info(format!(
                    "VAD 分段失败（{}），回退整段上传：{}",
                    err,
                    source.display_name()
                ));
            }
        }
    }

    if !handled {
        process_without_vad(
            &options.api_key,
            &options.api_url,
            &options.model_name,
            &source,
            logger,
        )
        .await;
    }
}

async fn process_without_vad(
    api_key: &str,
    api_url: &str,
    model_name: &str,
    source: &AudioSource,
    logger: &mut ScanLogger,
) {
    let target_name = source.display_name();
    let materialized = match source.materialize_full_audio().await {
        Ok(audio) => audio,
        Err(err) => {
            logger.error(format!("准备 {} 音频失败：{}", target_name, err));
            return;
        }
    };

    logger.info(format!(
        "开始转写 {}，音频源 {:?}",
        target_name, materialized.path
    ));

    match transcribe_file(api_key, api_url, model_name, &materialized.path).await {
        Ok(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                logger.error(format!("{} 的识别结果为空，跳过写入。", target_name));
                let _ = cleanup_materialized(materialized).await;
                return;
            }

            let duration = match media_duration(&materialized.path).await {
                Ok(value) => value.max(0.5),
                Err(e) => {
                    logger.info(format!(
                        "无法获取 {:?} 的时长（{}），使用估算值。",
                        materialized.path, e
                    ));
                    estimate_duration_from_text(trimmed)
                }
            };

            let srt_content = build_srt_entry(1, 0.0, duration, trimmed);
            let srt_path = transcript_result_path(source.original_path(), source.track_index());
            match fs::write(&srt_path, srt_content).await {
                Ok(_) => logger.success(format!("完成 {}，结果输出 {:?}", target_name, srt_path)),
                Err(e) => logger.error(format!("写入 {} 失败：{}", target_name, e)),
            }
        }
        Err(e) => logger.error(format!("调用 API 转写 {} 失败：{}", target_name, e)),
    }

    if let Err(err) = cleanup_materialized(materialized).await {
        logger.info(format!("清理临时音轨失败：{}", err));
    }
}

async fn process_with_vad(
    api_key: &str,
    api_url: &str,
    model_name: &str,
    source: &AudioSource,
    vad_cfg: &VadConfig,
    logger: &mut ScanLogger,
) -> Result<()> {
    let display_name = source.display_name();
    logger.info(format!("{} 启用 VAD，准备语音分段。", display_name));

    let pcm_path = source.convert_to_pcm16().await?;
    let samples = read_wav_samples(&pcm_path).await?;
    let _ = fs::remove_file(&pcm_path).await;
    let total_duration = samples.len() as f64 / VAD_SAMPLE_RATE as f64;

    let speech_segments = detect_speech_segments(&samples, vad_cfg)?;
    if speech_segments.is_empty() {
        return Err(anyhow!("未检测到有效语音"));
    }

    let segments = expand_segments_with_gaps(&speech_segments, total_duration);
    let extra_gaps = segments
        .iter()
        .filter(|seg| seg.kind == SegmentKind::Gap)
        .count();
    if extra_gaps > 0 {
        logger.info(format!(
            "检测到 {} 段语音，额外包含 {} 个静音覆盖区。",
            speech_segments.len(),
            extra_gaps
        ));
    } else {
        logger.info(format!(
            "检测到 {} 段语音，逐段上传。",
            speech_segments.len()
        ));
    }

    let mut entries: Vec<String> = Vec::new();
    for (idx, segment) in segments.iter().enumerate() {
        let segment_audio = source.export_segment_audio(idx + 1, segment).await?;
        match transcribe_file(api_key, api_url, model_name, &segment_audio).await {
            Ok(text) => {
                let trimmed = text.trim();
                if trimmed.is_empty() {
                    logger.info(format!("分段 {} 结果为空，已跳过。", idx + 1));
                    let _ = fs::remove_file(&segment_audio).await;
                    continue;
                }
                let label = match segment.kind {
                    SegmentKind::Speech => "语音",
                    SegmentKind::Gap => "补间",
                };
                logger.success(format!(
                    "分段 {} [{}] 完成（{} - {}）。",
                    idx + 1,
                    label,
                    format_timestamp(segment.start_sec),
                    format_timestamp(segment.end_sec)
                ));
                entries.push(build_srt_entry(
                    entries.len() + 1,
                    segment.start_sec,
                    segment.end_sec,
                    trimmed,
                ));
            }
            Err(e) => {
                logger.error(format!("分段 {} 调用 API 失败：{}", idx + 1, e));
            }
        }
        let _ = fs::remove_file(&segment_audio).await;
    }

    if entries.is_empty() {
        return Err(anyhow!("所有分段均转写失败"));
    }

    let srt_path = transcript_result_path(source.original_path(), source.track_index());
    let srt_content: String = entries.concat();
    fs::write(&srt_path, srt_content).await?;
    logger.success(format!(
        "{} VAD 分段完成，结果输出 {:?}",
        display_name, srt_path
    ));
    Ok(())
}

async fn read_wav_samples(path: &Path) -> Result<Vec<i16>> {
    let path = path.to_path_buf();
    task::spawn_blocking(move || {
        let mut reader = hound::WavReader::open(&path)?;
        let spec = reader.spec();
        if spec.sample_rate != VAD_SAMPLE_RATE || spec.channels != 1 || spec.bits_per_sample != 16 {
            return Err(anyhow!("生成的 WAV 格式不符合 VAD 要求"));
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SegmentKind {
    Speech,
    Gap,
}

#[derive(Clone, Debug)]
struct SpeechSegment {
    start_sec: f64,
    end_sec: f64,
    kind: SegmentKind,
}

impl SpeechSegment {
    fn new(start_sec: f64, end_sec: f64, kind: SegmentKind) -> Self {
        Self {
            start_sec,
            end_sec,
            kind,
        }
    }

    fn from_chunks(start_chunk: usize, end_chunk: usize) -> Self {
        Self::new(
            chunk_to_time(start_chunk),
            chunk_to_time(end_chunk),
            SegmentKind::Speech,
        )
    }

    fn try_new(start_sec: f64, end_sec: f64, kind: SegmentKind) -> Option<Self> {
        if end_sec - start_sec <= MIN_SEGMENT_EPS {
            None
        } else {
            Some(Self::new(start_sec, end_sec, kind))
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
        .context("语音活动检测器初始化失败")?;

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

fn expand_segments_with_gaps(
    speech_segments: &[SpeechSegment],
    total_duration: f64,
) -> Vec<SpeechSegment> {
    if speech_segments.is_empty() {
        return Vec::new();
    }

    let mut sorted = speech_segments.to_vec();
    sorted.sort_by(|a, b| {
        a.start_sec
            .partial_cmp(&b.start_sec)
            .unwrap_or(std::cmp::Ordering::Less)
    });

    let mut expanded = Vec::new();
    let mut cursor = 0.0f64;

    for segment in sorted {
        if let Some(gap) = SpeechSegment::try_new(cursor, segment.start_sec, SegmentKind::Gap) {
            expanded.push(gap);
        }
        let end = segment.end_sec;
        expanded.push(segment);
        cursor = end;
    }

    if let Some(tail) = SpeechSegment::try_new(cursor, total_duration, SegmentKind::Gap) {
        expanded.push(tail);
    }

    expanded
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

fn format_srt_timestamp(seconds: f64) -> String {
    let total_ms = (seconds * 1000.0).round().max(0.0) as u64;
    let hours = total_ms / 3_600_000;
    let minutes = (total_ms % 3_600_000) / 60_000;
    let secs = (total_ms % 60_000) / 1000;
    let millis = total_ms % 1000;
    format!("{:02}:{:02}:{:02},{:03}", hours, minutes, secs, millis)
}

fn sanitize_srt_text(input: &str) -> String {
    input.replace("\r\n", "\n").trim().to_string()
}

fn build_srt_entry(index: usize, start: f64, end: f64, text: &str) -> String {
    let safe_end = if end <= start { start + 0.5 } else { end };
    format!(
        "{idx}\n{start} --> {end}\n{body}\n\n",
        idx = index,
        start = format_srt_timestamp(start),
        end = format_srt_timestamp(safe_end),
        body = sanitize_srt_text(text)
    )
}

fn estimate_duration_from_text(text: &str) -> f64 {
    let chars = text.chars().count() as f64;
    (chars / 15.0).max(5.0)
}

async fn audio_stream_indices(path: &Path) -> Result<Vec<u32>> {
    let output = Command::new(ffprobe_program())
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
        return Err(anyhow!("ffprobe 解析音轨失败，退出状态：{}", output.status));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let indices = stdout
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect();

    Ok(indices)
}

async fn media_duration(path: &Path) -> Result<f64> {
    let output = Command::new(ffprobe_program())
        .arg("-v")
        .arg("error")
        .arg("-show_entries")
        .arg("format=duration")
        .arg("-of")
        .arg("default=noprint_wrappers=1:nokey=1")
        .arg(path)
        .output()
        .await?;

    if !output.status.success() {
        return Err(anyhow!(
            "ffprobe 读取 {:?} 时长失败，退出状态：{}",
            path,
            output.status
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .find_map(|line| line.trim().parse::<f64>().ok())
        .ok_or_else(|| anyhow!("无法解析 {:?} 的时长", path))
}

fn track_suffix(track_index: Option<u32>, segment_index: Option<usize>) -> String {
    match (track_index, segment_index) {
        (Some(track), Some(segment)) => format!("（音轨 {} · 片段 {}）", track, segment),
        (Some(track), None) => format!("（音轨 {}）", track),
        (None, Some(segment)) => format!("（片段 {}）", segment),
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
        assert_eq!(txt, PathBuf::from("C:/tmp/input/video.srt"));

        let track_txt = transcript_result_path(path, Some(2));
        assert_eq!(track_txt, PathBuf::from("C:/tmp/input/video.轨道2.srt"));

        let no_ext = Path::new("/tmp/audio");
        let txt2 = transcript_result_path(no_ext, None);
        assert_eq!(txt2, PathBuf::from("/tmp/audio.srt"));
    }

    #[test]
    fn audio_track_path_includes_track_id() {
        let path = Path::new("/media/sample.mkv");
        let mp3 = audio_track_path(path, 1);
        assert_eq!(mp3, PathBuf::from("/media/sample.mkv-track1.mp3"));
    }

    #[test]
    fn expand_segments_adds_gap_coverage() {
        let speech_segments = vec![
            SpeechSegment::new(0.0, 2.0, SegmentKind::Speech),
            SpeechSegment::new(4.0, 6.0, SegmentKind::Speech),
        ];
        let expanded = expand_segments_with_gaps(&speech_segments, 8.0);
        assert_eq!(expanded.len(), 4);
        assert_eq!(expanded[0].kind, SegmentKind::Speech);
        assert_eq!(expanded[1].kind, SegmentKind::Gap);
        assert!((expanded[1].start_sec - 2.0).abs() < 1e-6);
        assert!((expanded[1].end_sec - 4.0).abs() < 1e-6);
        assert_eq!(expanded[2].kind, SegmentKind::Speech);
        assert_eq!(expanded[3].kind, SegmentKind::Gap);
        assert!((expanded[3].start_sec - 6.0).abs() < 1e-6);
        assert!((expanded[3].end_sec - 8.0).abs() < 1e-6);
    }
}
