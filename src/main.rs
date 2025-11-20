use crate::config::AppConfig;
use crate::scanner::process_directory;
use chrono::{Local, NaiveTime, Timelike};
use iced::{
    executor, time,
    widget::{button, scrollable, text, text_input, Column, Container, Row},
    Alignment, Application, Command, Element, Length, Settings, Subscription, Theme,
};
use std::path::{Path, PathBuf};

mod api;
mod config;
mod scanner;

pub fn main() -> iced::Result {
    AutoAsrApp::run(Settings::default())
}

struct AutoAsrApp {
    config: AppConfig,
    is_running: bool,
    logs: Vec<String>,
    last_run_date: Option<String>,
    is_processing: bool,
}

#[derive(Debug, Clone)]
enum Message {
    DirectorySelected(Option<PathBuf>),
    SelectDirectory,
    ApiKeyChanged(String),
    ScheduleTimeChanged(String),
    ToggleRunning,
    Tick(chrono::DateTime<chrono::Local>),
    ScanFinished(Result<Vec<String>, String>),
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
                logs: vec!["Application started.".to_string()],
                last_run_date: None,
                is_processing: false,
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
                    self.logs.push(format!("Directory selected: {:?}", p));
                }
            }
            Message::ApiKeyChanged(key) => {
                self.config.api_key = key;
            }
            Message::ScheduleTimeChanged(time) => {
                self.config.schedule_time = time;
            }
            Message::ToggleRunning => {
                if self.is_running {
                    self.is_running = false;
                    self.logs.push("Scheduler stopped.".to_string());
                } else {
                    match self.validate_ready_state() {
                        Ok(_) => {
                            self.is_running = true;
                            self.last_run_date = None;
                            self.logs.push("Scheduler started.".to_string());
                        }
                        Err(err) => {
                            self.logs.push(format!("Cannot start scheduler: {}", err));
                        }
                    }
                }
            }
            Message::SaveConfig => {
                let config = self.config.clone();
                return Command::perform(
                    async move { config.save().map_err(|e| e.to_string()) },
                    |res| Message::ConfigSaved(res),
                );
            }
            Message::ConfigSaved(res) => match res {
                Ok(_) => self.logs.push("Configuration saved.".to_string()),
                Err(e) => self.logs.push(format!("Failed to save config: {}", e)),
            },
            Message::Tick(now) => {
                if self.is_running && !self.is_processing {
                    let target_time =
                        match NaiveTime::parse_from_str(&self.config.schedule_time, "%H:%M") {
                            Ok(t) => t,
                            Err(_) => {
                                self.logs.push(
                                    "Invalid schedule time format. Scheduler stopped.".to_string(),
                                );
                                self.is_running = false;
                                return Command::none();
                            }
                        };

                    let now_time = now.time();
                    let current_date = now.format("%Y-%m-%d").to_string();

                    if now_time.hour() == target_time.hour()
                        && now_time.minute() == target_time.minute()
                    {
                        if self.last_run_date.as_deref() != Some(&current_date) {
                            if let Some(dir) = &self.config.directory {
                                self.is_processing = true;
                                self.last_run_date = Some(current_date);
                                self.logs.push("Starting scheduled scan...".to_string());

                                let dir_path = PathBuf::from(dir);
                                let api_key = self.config.api_key.clone();

                                return Command::perform(
                                    process_directory(dir_path, api_key),
                                    |res| Message::ScanFinished(res.map_err(|e| e.to_string())),
                                );
                            } else {
                                self.logs.push(
                                    "Scheduled time reached but no directory selected.".to_string(),
                                );
                            }
                        }
                    }
                }
            }
            Message::ScanFinished(res) => {
                self.is_processing = false;
                match res {
                    Ok(new_logs) => {
                        for log in new_logs {
                            self.logs.push(log);
                        }
                        self.logs.push("Scan completed.".to_string());
                    }
                    Err(e) => {
                        self.logs.push(format!("Scan error: {}", e));
                    }
                }
            }
        }
        Command::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let title = text("AutoASR - SiliconFlow").size(30);

        let dir_display = text(
            self.config
                .directory
                .as_deref()
                .unwrap_or("No directory selected"),
        );
        let dir_btn = button("Select Directory").on_press(Message::SelectDirectory);

        let api_key_input = text_input("Enter API Key", &self.config.api_key)
            .on_input(Message::ApiKeyChanged)
            .padding(10);

        let schedule_input = text_input("Schedule Time (HH:MM)", &self.config.schedule_time)
            .on_input(Message::ScheduleTimeChanged)
            .padding(10);

        let toggle_btn = button(if self.is_running {
            "Stop Scheduler"
        } else {
            "Start Scheduler"
        })
        .on_press(Message::ToggleRunning)
        .padding(10)
        .style(if self.is_running {
            iced::theme::Button::Destructive
        } else {
            iced::theme::Button::Primary
        });

        let save_btn = button("Save Settings")
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
                    .push(text("API Key:"))
                    .push(api_key_input),
            )
            .push(
                Column::new()
                    .spacing(5)
                    .push(text("Schedule Time:"))
                    .push(schedule_input),
            )
            .push(Row::new().spacing(20).push(toggle_btn).push(save_btn));

        let logs_content = self
            .logs
            .iter()
            .fold(Column::new().spacing(5), |col, log| col.push(text(log)));

        let logs_scroll = scrollable(logs_content)
            .height(Length::Fill)
            .width(Length::Fill);

        let content = Column::new()
            .spacing(20)
            .padding(20)
            .push(controls)
            .push(text("Logs:").size(20))
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
