//! Iced GUI 入口，负责状态管理、调度以及用户交互。

use crate::config::AppConfig;
use crate::scanner::{process_directory, ScanLog, ScanLogLevel, ScannerOptions, VadConfig};
use chrono::{Local, NaiveTime, Timelike};
use iced::{
    executor, time,
    widget::{button, checkbox, scrollable, slider, text, text_input, Column, Container, Row},
    Alignment, Application, Color, Command, Element, Font, Length, Settings, Subscription, Theme,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{mpsc, Mutex};

mod api;
mod config;
mod scanner;

/// 程序入口，启动 Iced 应用。
pub fn main() -> iced::Result {
    AutoAsrApp::run(Settings::default())
}

/// GUI 主体，封装配置、调度状态与日志输出。
struct AutoAsrApp {
    config: AppConfig,
    is_running: bool,
    logs: Vec<ScanLog>,
    last_run_date: Option<String>,
    is_processing: bool,
    scan_progress_rx: Option<Arc<Mutex<mpsc::UnboundedReceiver<ScanLog>>>>,
}

/// Iced 消息枚举，覆盖用户交互与后台任务回调。
#[derive(Debug, Clone)]
enum Message {
    DirectorySelected(Option<PathBuf>),
    SelectDirectory,
    ApiKeyChanged(String),
    ScheduleTimeChanged(String),
    VadToggled(bool),
    VadThresholdChanged(f32),
    VadMinDurationChanged(f32),
    ToggleRunning,
    Tick(chrono::DateTime<chrono::Local>),
    ScanFinished(Result<Vec<ScanLog>, String>),
    ScanProgress(Option<ScanLog>),
    SaveConfig,
    ConfigSaved(Result<(), String>),
}

impl Application for AutoAsrApp {
    type Executor = executor::Default;
    type Message = Message;
    type Theme = Theme;
    type Flags = ();

    fn new(_flags: ()) -> (Self, Command<Message>) {
        let config = AppConfig::load().unwrap_or_default();
        (
            Self {
                config,
                is_running: false,
                logs: vec![ScanLog::new(ScanLogLevel::Info, "Application started.")],
                last_run_date: None,
                is_processing: false,
                scan_progress_rx: None,
            },
            Command::none(),
        )
    }

    fn title(&self) -> String {
        String::from("AutoASR - SiliconFlow")
    }

    fn update(&mut self, message: Message) -> Command<Message> {
        match message {
            Message::SelectDirectory => {
                return Command::perform(
                    async {
                        rfd::AsyncFileDialog::new()
                            .pick_folder()
                            .await
                            .map(|h| h.path().to_path_buf())
                    },
                    Message::DirectorySelected,
                );
            }
            Message::DirectorySelected(path) => {
                if let Some(p) = path {
                    self.config.directory = Some(p.to_string_lossy().to_string());
                    self.log_info(format!("Directory selected: {:?}", p));
                }
            }
            Message::ApiKeyChanged(key) => {
                self.config.api_key = key;
            }
            Message::ScheduleTimeChanged(time) => {
                self.config.schedule_time = time;
            }
            Message::VadToggled(enabled) => {
                self.config.vad_enabled = enabled;
                let note = if enabled {
                    "Voice Activity Detection enabled."
                } else {
                    "Voice Activity Detection disabled."
                };
                self.log_info(note);
            }
            Message::VadThresholdChanged(value) => {
                self.config.vad_threshold = value;
            }
            Message::VadMinDurationChanged(value) => {
                self.config.vad_min_segment_secs = value;
            }
            Message::ToggleRunning => {
                if self.is_running {
                    self.is_running = false;
                    self.log_info("Scheduler stopped.");
                } else {
                    match self.validate_ready_state() {
                        Ok(_) => {
                            self.is_running = true;
                            self.last_run_date = None;
                            self.log_success("Scheduler started.");
                        }
                        Err(err) => {
                            self.log_error(format!("Cannot start scheduler: {}", err));
                        }
                    }
                }
            }
            Message::SaveConfig => {
                let config = self.config.clone();
                return Command::perform(
                    async move { config.save().map_err(|e| e.to_string()) },
                    Message::ConfigSaved,
                );
            }
            Message::ConfigSaved(res) => match res {
                Ok(_) => self.log_success("Configuration saved."),
                Err(e) => self.log_error(format!("Failed to save config: {}", e)),
            },
            Message::Tick(now) => {
                if self.is_running && !self.is_processing {
                    let target_time =
                        match NaiveTime::parse_from_str(&self.config.schedule_time, "%H:%M") {
                            Ok(t) => t,
                            Err(_) => {
                                self.log_error("Invalid schedule time format. Scheduler stopped.");
                                self.is_running = false;
                                return Command::none();
                            }
                        };

                    let now_time = now.time();
                    let current_date = now.format("%Y-%m-%d").to_string();

                    if now_time.hour() == target_time.hour()
                        && now_time.minute() == target_time.minute()
                        && self.last_run_date.as_deref() != Some(&current_date)
                    {
                        if let Some(dir) = self.config.directory.clone() {
                            self.is_processing = true;
                            self.last_run_date = Some(current_date);
                            self.log_info("Starting scheduled scan...");

                            let dir_path = PathBuf::from(dir);
                            let api_key = self.config.api_key.clone();
                            let vad = if self.config.vad_enabled {
                                Some(VadConfig::from_user_settings(
                                    self.config.vad_threshold,
                                    self.config.vad_min_segment_secs,
                                ))
                            } else {
                                None
                            };

                            let (progress_tx, progress_rx) = mpsc::unbounded_channel();
                            let progress_handle = Arc::new(Mutex::new(progress_rx));
                            self.scan_progress_rx = Some(progress_handle.clone());

                            let options = ScannerOptions { api_key, vad };
                            let scan_cmd = Command::perform(
                                process_directory(dir_path, options, Some(progress_tx)),
                                |res| Message::ScanFinished(res.map_err(|e| e.to_string())),
                            );
                            let progress_cmd = AutoAsrApp::listen_scan_progress(progress_handle);

                            return Command::batch(vec![scan_cmd, progress_cmd]);
                        } else {
                            self.log_error("Scheduled time reached but no directory selected.");
                        }
                    }
                }
            }
            Message::ScanFinished(res) => {
                self.is_processing = false;
                self.scan_progress_rx = None;
                match res {
                    Ok(new_logs) => {
                        self.logs.extend(new_logs);
                        self.log_success("Scan completed.");
                    }
                    Err(e) => {
                        self.log_error(format!("Scan error: {}", e));
                    }
                }
            }
            Message::ScanProgress(Some(log)) => {
                self.logs.push(log);
                if let Some(rx) = &self.scan_progress_rx {
                    return AutoAsrApp::listen_scan_progress(rx.clone());
                }
            }
            Message::ScanProgress(None) => {
                self.scan_progress_rx = None;
            }
        }
        Command::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let font = Self::preferred_font();

        let title = text("AutoASR - SiliconFlow").font(font).size(30);

        let dir_display = text(
            self.config
                .directory
                .as_deref()
                .unwrap_or("No directory selected"),
        )
        .font(font);
        let dir_btn =
            button(text("Select Directory").font(font)).on_press(Message::SelectDirectory);

        let api_key_input = text_input("Enter API Key", &self.config.api_key)
            .on_input(Message::ApiKeyChanged)
            .padding(10)
            .font(font);

        let schedule_input = text_input("Schedule Time (HH:MM)", &self.config.schedule_time)
            .on_input(Message::ScheduleTimeChanged)
            .padding(10)
            .font(font);

        let vad_toggle = checkbox("Enable VAD-based segmentation", self.config.vad_enabled)
            .on_toggle(Message::VadToggled)
            .spacing(10)
            .text_size(16)
            .font(font);

        let vad_threshold_slider = slider(
            0.3..=0.9,
            self.config.vad_threshold,
            Message::VadThresholdChanged,
        )
        .step(0.01);
        let vad_min_duration_slider = slider(
            0.5..=6.0,
            self.config.vad_min_segment_secs,
            Message::VadMinDurationChanged,
        )
        .step(0.1);

        let vad_controls = Column::new()
            .spacing(10)
            .push(vad_toggle)
            .push(
                Row::new()
                    .spacing(10)
                    .align_items(Alignment::Center)
                    .push(text("VAD Threshold").font(font))
                    .push(vad_threshold_slider)
                    .push(text(format!("{:.2}", self.config.vad_threshold)).font(font)),
            )
            .push(
                Row::new()
                    .spacing(10)
                    .align_items(Alignment::Center)
                    .push(text("Min Segment (s)").font(font))
                    .push(vad_min_duration_slider)
                    .push(text(format!("{:.1}s", self.config.vad_min_segment_secs)).font(font)),
            );

        let toggle_btn = button(if self.is_running {
            text("Stop Scheduler").font(font)
        } else {
            text("Start Scheduler").font(font)
        })
        .on_press(Message::ToggleRunning)
        .padding(10)
        .style(if self.is_running {
            iced::theme::Button::Destructive
        } else {
            iced::theme::Button::Primary
        });

        let save_btn = button(text("Save Settings").font(font))
            .on_press(Message::SaveConfig)
            .padding(10);

        let controls = Column::new()
            .spacing(20)
            .push(title)
            .push(
                Row::new()
                    .spacing(10)
                    .push(dir_btn)
                    .push(dir_display)
                    .align_items(Alignment::Center),
            )
            .push(
                Column::new()
                    .spacing(5)
                    .push(text("API Key:").font(font))
                    .push(api_key_input),
            )
            .push(
                Column::new()
                    .spacing(5)
                    .push(text("Schedule Time:").font(font))
                    .push(schedule_input),
            )
            .push(vad_controls)
            .push(Row::new().spacing(20).push(toggle_btn).push(save_btn));

        const MAX_LOGS: usize = 500;
        let logs_content =
            self.logs
                .iter()
                .rev()
                .take(MAX_LOGS)
                .fold(Column::new().spacing(5), |col, log| {
                    let (label, color) = Self::log_visuals(log.level);
                    let display = format!("[{}] {}", label, log.message);
                    col.push(
                        text(display)
                            .font(Self::preferred_font())
                            .style(iced::theme::Text::Color(color)),
                    )
                });

        let logs_scroll = scrollable(logs_content)
            .height(Length::Fill)
            .width(Length::Fill);

        let content = Column::new()
            .spacing(20)
            .padding(20)
            .push(controls)
            .push(text("Logs:").font(font).size(20))
            .push(
                Container::new(logs_scroll)
                    .style(iced::theme::Container::Box)
                    .padding(10),
            );

        Container::new(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x()
            .into()
    }

    fn subscription(&self) -> Subscription<Message> {
        time::every(std::time::Duration::from_secs(1)).map(|_| Message::Tick(Local::now()))
    }
}

impl AutoAsrApp {
    fn preferred_font() -> Font {
        #[cfg(target_os = "windows")]
        {
            Font::with_name("Microsoft YaHei")
        }

        #[cfg(target_os = "macos")]
        {
            Font::with_name("PingFang SC")
        }

        #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
        {
            Font::with_name("Noto Sans CJK SC")
        }
    }

    fn listen_scan_progress(
        receiver: Arc<Mutex<mpsc::UnboundedReceiver<ScanLog>>>,
    ) -> Command<Message> {
        Command::perform(
            async move {
                let mut rx = receiver.lock().await;
                rx.recv().await
            },
            Message::ScanProgress,
        )
    }

    fn push_log(&mut self, level: ScanLogLevel, message: impl Into<String>) {
        self.logs.push(ScanLog::new(level, message));
    }

    fn log_info(&mut self, message: impl Into<String>) {
        self.push_log(ScanLogLevel::Info, message);
    }

    fn log_success(&mut self, message: impl Into<String>) {
        self.push_log(ScanLogLevel::Success, message);
    }

    fn log_error(&mut self, message: impl Into<String>) {
        self.push_log(ScanLogLevel::Error, message);
    }

    fn log_visuals(level: ScanLogLevel) -> (&'static str, Color) {
        match level {
            ScanLogLevel::Info => ("INFO", Color::from_rgb(0.75, 0.75, 0.78)),
            ScanLogLevel::Success => ("OK", Color::from_rgb(0.3, 0.75, 0.4)),
            ScanLogLevel::Error => ("ERR", Color::from_rgb(0.92, 0.32, 0.32)),
        }
    }

    /// 校验调度启动前的必要条件，避免无效配置触发任务。
    fn validate_ready_state(&self) -> Result<(), String> {
        let dir = self
            .config
            .directory
            .as_ref()
            .ok_or_else(|| "Please select a directory.".to_string())?;

        if !Path::new(dir).exists() {
            return Err("Selected directory does not exist.".to_string());
        }

        if self.config.api_key.trim().is_empty() {
            return Err("API key is required.".to_string());
        }

        if NaiveTime::parse_from_str(&self.config.schedule_time, "%H:%M").is_err() {
            return Err("Schedule time must be in HH:MM format.".to_string());
        }

        Ok(())
    }
}
