use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub asr: AsrConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AsrConfig {
    pub api_key: String,
    #[serde(default = "default_resource_id")]
    pub resource_id: String,
    #[serde(default = "default_rate")]
    pub rate: u32,
}

fn default_resource_id() -> String {
    "volc.seedasr.sauc.duration".into()
}
fn default_rate() -> u32 {
    16000
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path();
        if !path.exists() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, TEMPLATE)?;
            eprintln!("已创建配置文件模板: {}", path.display());
            eprintln!("请编辑该文件，填入火山引擎 API Key 后重启 fcitx5。");
        }
        let s = fs::read_to_string(&path)
            .with_context(|| format!("读取配置失败: {}", path.display()))?;
        let cfg: Config = toml::from_str(&s).map_err(|e| anyhow!("解析配置失败: {e}"))?;
        Ok(cfg)
    }
}

fn config_path() -> PathBuf {
    let dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    dir.join("voice-input").join("config.toml")
}

const TEMPLATE: &str = r#"# voice-input 配置
# 火山引擎「豆包语音」流式语音识别（Seed ASR 2.0）:
# https://console.volcengine.com/speech/new/setting/apikeys
# 在控制台「API Key 管理」创建 API Key 后填入下方。

[asr]
# API Key（新版控制台鉴权，只需这一个）
api_key = ""
# 模型资源 ID：2.0 小时版
resource_id = "volc.seedasr.sauc.duration"
# 音频采样率（仅支持 16000）
rate = 16000
"#;
