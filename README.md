# voice-input（fcitx5 插件版）

按住 **Ctrl** 说话，松开后自动识别并**以输入法方式提交**到当前光标处（fcitx5，X11 / Wayland 均可）。

- 按住 Ctrl → 开始录音 → 松开 → 音频发送到火山引擎豆包语音 **Seed ASR 2.0**
  （单向流式 `bigmodel_nostream`）→ 识别结果通过 fcitx5 `commitString` 提交到当前输入上下文
- 不依赖剪贴板、不模拟按键，应用视角等同于正常打字

## 架构

```
fcitx5（C++ 插件 libvoiceinput.so）
  │  热键触发 / commitString 提交
  ▼
Rust 核心（libvoiceinput_core.so，C ABI）
  │  vi_init / vi_start / vi_stop / vi_set_callback
  ├─ recorder.rs   ALSA 录音（16kHz 单声道 i16）
  ├─ asr.rs        Seed ASR V3 协议客户端
  ├─ ws.rs         极简 WebSocket（wss + gzip 帧）
  └─ config.rs     配置 ~/.config/voice-input/config.toml
```

- `src/`：Rust 核心库（`cargo build --release` → `target/release/libvoiceinput_core.so`）
- `fcitx5-addon/`：C++ 插件（cmake 工程），链接 Rust 核心，导出 `libvoiceinput.so`
- 回调线程模型：Rust 在 tokio 线程触发回调 → C++ 侧只拷贝文本到互斥锁保护的缓冲 →
  fcitx5 主线程 100ms 定时器轮询缓冲（sd-event 定时器单次触发，需 `setEnabled(true)`
  重挂）→ `reset()` 清 preedit → `commitString()` 提交
- `fcitx5-addon/test_vi.c`：核心库 C ABI 冒烟测试（改 Rust 侧后跑一遍回归）：

  ```bash
  gcc -I fcitx5-addon fcitx5-addon/test_vi.c -L target/release -lvoiceinput_core -o /tmp/test_vi
  LD_LIBRARY_PATH=target/release /tmp/test_vi
  ```

## 构建

### 1. 系统依赖

Ubuntu/Debian（需 sudo 一次）：

```bash
sudo apt install build-essential cmake pkg-config libasound2-dev \
  libfcitx5-dev fcitx5-modules-dev extra-cmake-modules
```

### 2. 构建

```bash
cargo build --release          # Rust 核心
cmake -S fcitx5-addon -B fcitx5-addon/build
cmake --build fcitx5-addon/build   # 会自动调 cargo build --release
```

### 3. 安装（把两个 .so 装进 fcitx5 插件目录）

```bash
sudo cmake --install fcitx5-addon/build
```

安装内容：

| 文件 | 目标位置 |
|---|---|
| `libvoiceinput.so` | `/usr/lib/x86_64-linux-gnu/fcitx5/` |
| `libvoiceinput_core.so` | 同上（`$ORIGIN` rpath 互找） |
| `voiceinput.conf` | `/usr/share/fcitx5/addon/` |

重启 fcitx5（`fcitx5 -r`）后生效。可用 `fcitx5-diagnose` 检查插件加载状态。

## 配置

首次运行生成 `~/.config/voice-input/config.toml` 模板，填入火山引擎 **API Key**：

```toml
[asr]
api_key = "你的 API Key"
resource_id = "volc.seedasr.sauc.duration"  # 2.0 小时版
rate = 16000
```

API Key 在[火山引擎控制台](https://console.volcengine.com/speech/new/setting/apikeys)「API Key 管理」创建。

## 使用

- 按住**右 Ctrl** 说话，松开后识别结果出现在当前输入位置；录音/识别中候选框旁有状态提示
- 热键暂为硬编码，TODO：做成 fcitx5 配置项
- 调试日志：`~/.local/share/voice-input/debug.log`（Rust 核心）、`plugin.log`（插件）
- 日志保留策略：只留最近 **7 天**，单文件超 **1MB** 自动轮转到 `.1`（每小时检查一次）

## 说明

- 服务端协议为豆包 Seed ASR V3 协议（4 字节头 + seq + gzip payload），
  握手 Header 鉴权（X-Api-Key / X-Api-Resource-Id / X-Api-Request-Id / X-Api-Sequence），
  单向流式接口 `bigmodel_nostream`，音频结束以「负 seq 最后一包」标记
- 更多参数（热词、上下文、语种检测等）参考 https://docs.volcengine.com/docs/6561/2628951
