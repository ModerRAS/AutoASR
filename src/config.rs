//! 负责 AutoASR 的配置加载、保存与默认值。

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// GUI 层共享的运行配置，包含输入目录、API Key 以及每日调度时间。
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct AppConfig {
    /// 媒体文件根目录，`None` 表示尚未选择。
    pub directory: Option<String>,
    /// SiliconFlow 服务的 API Key。
    pub api_key: String,
    /// 每日执行时间，24 小时制 `HH:MM`。
    pub schedule_time: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            directory: None,
            api_key: String::new(),
            schedule_time: "02:00".to_string(),
        }
    }
}

impl AppConfig {
    /// 从磁盘读取 `config.toml`；若不存在则返回默认配置。
    pub fn load() -> Result<Self> {
        let config_path = Self::get_config_path()?;
        if config_path.exists() {
            let content = fs::read_to_string(config_path)?;
            let config: AppConfig = toml::from_str(&content)?;
            Ok(config)
        } else {
            Ok(Self::default())
        }
    }

    /// 将当前配置写入磁盘，必要时自动创建配置目录。
    pub fn save(&self) -> Result<()> {
        let config_path = Self::get_config_path()?;
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = toml::to_string(self)?;
        fs::write(config_path, content)?;
        Ok(())
    }

    /// 解析配置文件路径，遵循平台约定的用户配置目录。
    fn get_config_path() -> Result<PathBuf> {
        let dirs = directories::ProjectDirs::from("com", "autoasr", "app")
            .ok_or_else(|| anyhow::anyhow!("Could not determine config directory"))?;
        Ok(dirs.config_dir().join("config.toml"))
    }
}
