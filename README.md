# AutoASR

AutoASR 是一个基于 Rust + Iced GUI 的定时语音转写桌面工具，内置调度器可以每天定时扫描指定目录，自动调用 SiliconFlow ASR API（FunAudioLLM/SenseVoiceSmall 模型）完成音视频转写，并将结果保存为同名 `.srt` 字幕文件。适合需要无人值守批量处理播客、会议录音、课程视频等场景。

## ✨ 功能亮点

- **跨平台 GUI**：使用 Iced 构建，界面默认中文，提供目录选择、API 密钥输入、定时器控制与实时日志查看。
- **即时扫描**：除定时任务外，还可在 GUI 中点击“立即扫描”立刻触发一次扫描，便于临时补录或测试配置。
- **计划任务调度**：精确到分钟的 HH:MM 配置，自动记录每日执行状态，避免同日重复运行。
- **多媒体支持**：内置媒体扫描器，自动跳过已转写的文件；视频会通过 FFmpeg 转为 MP3 后再上传。
- **临时音轨自动清理**：为视频音轨生成的中间 MP3 仅用于上传，任务结束后将立即删除，确保磁盘不被临时文件占用。
- **多音轨转写**：同一视频的每条音轨都会单独生成临时 MP3 并输出对应的 `.srt` 字幕，文件名包含 `轨道X` 以示区分。
- **语音活动检测（VAD）**：可选的 `voice_activity_detector` 分段流程，先将音频拆成多段语音后再上传，显著降低静音/噪声带来的时长浪费，并在结果中附上分段时间戳。
- **健壮的 API 处理**：针对 SiliconFlow API 的成功/失败响应、限流（429）等情况提供详细日志。
- **持久化配置**：配置保存在 `config.toml`（用户目录下），重启仍然有效。
- **CI/CD 自动化**：GitHub Actions 覆盖 fmt/clippy/test/build 以及自动打包 Windows 版本并发布 Release。

## 📦 目录结构

```
AutoASR/
├── Cargo.toml
├── README.md
├── agents.md              # Agent 架构说明（本文档在后续小节中也会介绍）
├── src/
│   ├── main.rs            # Iced GUI、调度器与状态管理
│   ├── config.rs          # 配置加载与保存
│   ├── api.rs             # SiliconFlow API 封装
│   └── scanner.rs         # 目录遍历、媒体判定、FFmpeg 转码
└── .github/workflows/
	├── ci.yml             # fmt/clippy/test/build + artifact
	└── release.yml        # Windows 构建 + 压缩 + 发布 Release
```

## 🚀 快速开始

### 环境依赖

- Rust 工具链（推荐 `rustup` 安装，最低 1.75+）
- FFmpeg/FFprobe（确保 `ffmpeg`、`ffprobe` 命令可用；通常同捆提供）
- Windows/Mac/Linux 任意桌面环境

### 构建与运行

```powershell
git clone https://github.com/ModerRAS/AutoASR.git
cd AutoASR
cargo run --release
```

首次启动后：

1. 点击 **选择目录** 选择待监控的根目录；子目录会被递归扫描。
2. 输入 SiliconFlow 的 **API 密钥**（需要具备音频转写权限）。
3. 设定每日执行时间（24 小时制，例如 `02:00`）。
4. 需要时勾选 **启用 VAD 语音分段**，并通过“VAD 阈值”“最短片段（秒）”滑块微调触发阈值与最短片段长度。
5. 想立即跑一次可以点击 **立即扫描**；若要进入定时模式则点击 **启动定时**，状态栏会切换为“停止定时”。
6. 点击 **保存设置** 可立即将当前配置写入 `config.toml`。

### 配置文件说明

配置保存在：`%AppData%/autoasr/app/config/config.toml`（Windows 示例）。

```toml
directory = "D:/recordings"
api_key = "sk-xxxxxxxx"
schedule_time = "02:00"
vad_enabled = true
vad_threshold = 0.6
vad_min_segment_secs = 2.0
```

若需重置，可删除该文件或直接修改内容。

### 语音活动检测（VAD）

- 本项目集成了 [voice_activity_detector](https://crates.io/crates/voice_activity_detector) crate（Silero V5 模型），默认勾选开启。
- FFmpeg 会先将音频转成 16kHz/Mono PCM，再在本地进行语音片段检测；每个片段单独上传并带上时间戳，最终合并回单个 `.srt` 字幕文件。
- 如果 VAD 检测失败或没有语音，系统会自动回退到整段音频上传，因此无需担心误判导致任务中断。
- 当录音存在长时间静音或背景噪声时，建议保持 VAD 开启，可显著缩短 API 处理时长、减少无效 token 消耗。
- **阈值/最短片段可调**：`VAD 阈值`（0.3~0.9）越高越保守，只有更强烈的语音才会触发；`最短片段（秒）`（0.5~6.0）控制最短合并长度，可避免过多 1 秒内的小段。

## 🔄 工作流与发布

| Workflow | 触发条件 | 说明 |
| --- | --- | --- |
| `ci.yml` | push / PR 到 master | 运行 `cargo fmt`, `cargo clippy`, `cargo test`, `cargo build`，并上传 Windows 构建产物作为 artifact。 |
| `release.yml` | 推送 `v*` 标签或手动触发 | 在 Windows runner 上构建 release，打包 `auto_asr.exe + README + LICENSE` 为 zip，通过 softprops/action-gh-release 发布到 GitHub Release。 |

发布新版本步骤：

```powershell
git tag -a v0.x.y -m "AutoASR v0.x.y"
git push origin v0.x.y
```

GitHub Actions 会自动完成构建和发布，产物可在 Release 页面下载。

## 🧩 组件概览

- `AutoAsrApp`：GUI 状态机，负责表单输入、日志输出、订阅 tick 事件。
- `scanner` 模块：遍历目录 -> 筛选媒体 -> 视频转码 -> 调用 API -> 写入结果。
- `api` 模块：封装 SiliconFlow 上传流程，处理响应、限流与错误信息。
- `config` 模块：负责配置默认值、加载/保存以及持久化路径推导。

更多关于 agent 设计理念，请参阅 `agents.md`。

## ❓ 常见问题

- **FFmpeg 未找到**：请确认系统 PATH 中包含 `ffmpeg`，或在命令行运行 `ffmpeg -version` 验证。
- **API 密钥报错**：检查密钥是否有效、账单是否正常；遇到 429 代表频率限制，可稍后重试。
- **定时任务未触发**：确保应用保持运行状态，且系统时间与设置时间一致；同一天只会执行一次，若需再次执行可停止后手动启动。

## 🤝 贡献指南

1. Fork 项目并创建特性分支。
2. 提交前运行 `cargo fmt`、`cargo clippy --all-targets --all-features` 与 `cargo test --all --all-features`。
3. 提交 PR 时描述改动背景，并附带必要的截图或日志。

欢迎 Issue 反馈和 PR 贡献，让 AutoASR 更加好用！