# Vibe Coding Prompt – AutoASR

## TL;DR
- Rust + Iced 的桌面 GUI：按配置时间扫描指定目录的音/视频，借助 SiliconFlow ASR API 生成同名 `.txt` 文稿。
- Scanner 会递归查找媒体、调用 FFmpeg 将视频转 MP3，再上传 API，最后写回文本与日志。
- 配置存储于 `config.toml`，CI/CD 使用 GitHub Actions（`ci.yml` / `release.yml`）。

## 技术栈与依赖
- Rust 1.75+，edition 2021。
- GUI：`iced`
- 异步/IO：`tokio`、`reqwest`
- 配置：`serde`, `toml`, `directories`
- 文件遍历与转码：`walkdir` + 外部 `ffmpeg`。

## 常用命令
```powershell
cargo fmt
cargo clippy --all-targets --all-features
cargo test --all --all-features
cargo run --release
```

## 代码结构速览
- `src/main.rs`：`AutoAsrApp`（Iced Application）、调度逻辑、日志 UI。
- `src/config.rs`：`AppConfig` 的加载/保存，含默认值。
- `src/scanner.rs`：`process_directory` + 媒体判定 + FFmpeg 转码 + 结果写入。
- `src/api.rs`：`transcribe_file` 封装 SiliconFlow API 请求与错误格式化。
- `README.md`：中文使用说明；`example.py` 为 API 对照示例；`.github/workflows` 提供 CI/Release。

## 开发约定
- 保持 Rustfmt 默认风格，提交前跑 fmt / clippy / test。
- 文档注释使用中文 `///` / `//!`，方便未来同步到 docs.rs。
- Scanner 与 API 逻辑需异步、不可阻塞 GUI；所有错误都要写入日志数组以呈现给用户。
- 遇到新配置项请同步更新 `AppConfig` 默认值、README，以及必要的 UI 控件。

## 任务提示
- 若新增功能，需要：
	1. 修改 GUI -> `main.rs`
	2. 更新核心逻辑 -> `scanner.rs` / `api.rs`
	3. 调整配置 -> `config.rs` + README + workflow 说明
- 编写单元测试参考 `scanner.rs` 中现有测试。
- 若要改动发布流程，修改 `.github/workflows/release.yml` 并确保 `contents: write` 权限仍在。

> 本文件供 Vibe Coding 等 AI 辅助工具快速了解项目背景与约束，所有内容均保持最新提交同步。
