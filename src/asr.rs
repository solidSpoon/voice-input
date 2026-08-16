use crate::config::AsrConfig;
use crate::ws::WsStream;
use anyhow::{anyhow, Context, Result};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde_json::json;
use std::io::{Read, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::Receiver;
use tokio::time::timeout;

/// 火山引擎（豆包）Seed ASR 2.0 流式语音识别客户端（V3 协议，单向流式 bigmodel_nostream）。
///
/// 协议要点（官方文档 https://docs.volcengine.com/docs/6561/2628951）：
/// - 鉴权：WebSocket 握手 Header 传 X-Api-Key（新版控制台只此一个）、
///   X-Api-Resource-Id（模型资源，2.0 小时版 = volc.seedasr.sauc.duration）、
///   X-Api-Request-Id（UUID）、X-Api-Sequence（固定 -1）
/// - 请求帧：4 字节头 + 4 字节 sequence + 4 字节 payload 长度 + gzip(body)
///   - byte0 = 0x11（version=1, header_size=1）
///   - byte1 = message_type<<4 | flags（0b0001=带正 seq，0b0011=带负 seq 的最后一包）
///   - byte2 = serialization<<4 | compression（JSON=1, GZIP=1）
/// - 响应帧：4 字节头；flags 低 1 位=带 sequence，低 2 位=最后一包；payload 为 gzip(JSON)
/// - 单向流式：发完音频后发最后一包（负 seq），服务端返回最终识别结果
const MSG_FULL_REQUEST: u8 = 0b0001;
const MSG_AUDIO_ONLY: u8 = 0b0010;
const MSG_SERVER_FULL_RESPONSE: u8 = 0b1001;
const MSG_SERVER_ERROR: u8 = 0b1111;
const FLAG_POS_SEQUENCE: u8 = 0b0001;
const FLAG_NEG_WITH_SEQUENCE: u8 = 0b0011;
const SERIALIZATION_JSON: u8 = 0b0001;
const COMPRESSION_GZIP: u8 = 0b0001;

pub struct AsrClient {
    api_key: String,
    resource_id: String,
    rate: u32,
}

/// 服务端响应帧解析结果。
enum ServerMsg {
    /// 识别结果帧（已解压的 JSON body）
    Result { is_last: bool, body: Vec<u8> },
    /// 协议错误帧
    Error(String),
}

impl AsrClient {
    pub fn new(cfg: &AsrConfig) -> Self {
        Self {
            api_key: cfg.api_key.clone(),
            resource_id: cfg.resource_id.clone(),
            rate: cfg.rate,
        }
    }

    fn gzip(data: &[u8]) -> Result<Vec<u8>> {
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(data)?;
        enc.finish().context("gzip 压缩失败")
    }

    fn gunzip(data: &[u8]) -> Result<Vec<u8>> {
        let mut dec = GzDecoder::new(data);
        let mut out = Vec::new();
        dec.read_to_end(&mut out).context("gzip 解压失败")?;
        Ok(out)
    }

    fn header(msg_type: u8, flags: u8) -> [u8; 4] {
        [0x11, (msg_type << 4) | flags, (SERIALIZATION_JSON << 4) | COMPRESSION_GZIP, 0x00]
    }

    /// 客户端请求帧：4 字节头 + seq(4B) + size(4B) + gzip(payload)
    fn request_pkg(msg_type: u8, flags: u8, seq: i32, payload: &[u8]) -> Result<Vec<u8>> {
        let body = Self::gzip(payload)?;
        let mut out = Vec::with_capacity(12 + body.len());
        out.extend_from_slice(&Self::header(msg_type, flags));
        out.extend_from_slice(&seq.to_be_bytes());
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// 单向流式识别：消费音频帧，发完最后一包后返回最终文本。
    pub async fn transcribe(&self, mut audio: Receiver<Vec<i16>>) -> Result<String> {
        let request_id = format!(
            "{:x}{:x}",
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            std::process::id()
        );
        let url = "wss://openspeech.bytedance.com/api/v3/sauc/bigmodel_nostream";
        let headers = [
            ("X-Api-Key", self.api_key.as_str()),
            ("X-Api-Resource-Id", self.resource_id.as_str()),
            ("X-Api-Request-Id", request_id.as_str()),
            ("X-Api-Sequence", "-1"),
        ];
        let mut ws = WsStream::connect(url, &headers)
            .await
            .context("连接 ASR 服务失败")?;

        // 1. Full client request（含音频参数与识别配置）
        let full = json!({
            "user": { "uid": "voice-input" },
            "audio": {
                "format": "pcm",
                "codec": "raw",
                "rate": self.rate,
                "bits": 16,
                "channel": 1,
            },
            "request": {
                "model_name": "bigmodel",
                "enable_itn": true,
                "enable_punc": true,
                "enable_ddc": true,
            }
        });
        let mut seq: i32 = 1;
        ws.send_binary(&Self::request_pkg(
            MSG_FULL_REQUEST,
            FLAG_POS_SEQUENCE,
            seq,
            full.to_string().as_bytes(),
        )?)
        .await?;
        seq += 1;

        // 2. 音频帧：边录边发
        while let Some(chunk) = audio.recv().await {
            let mut pcm = Vec::with_capacity(chunk.len() * 2);
            for s in &chunk {
                pcm.extend_from_slice(&s.to_le_bytes());
            }
            ws.send_binary(&Self::request_pkg(
                MSG_AUDIO_ONLY,
                FLAG_POS_SEQUENCE,
                seq,
                &pcm,
            )?)
            .await?;
            seq += 1;
        }

        // 3. 结束标志：最后一包（负 seq + 空音频）
        ws.send_binary(&Self::request_pkg(MSG_AUDIO_ONLY, FLAG_NEG_WITH_SEQUENCE, -seq, &[])?)
            .await?;

        // 4. 收集结果，直到最后一包或超时
        let mut text = String::new();
        let deadline = Duration::from_secs(20);
        loop {
            let frame = match timeout(deadline, ws.recv()).await {
                Ok(Ok(Some(f))) => f,
                Ok(Ok(None)) => { crate::logger::debug("[debug] 服务端关闭连接"); break; }
                Ok(Err(e)) => return Err(anyhow!("接收 ASR 结果失败: {e}")),
                Err(_) => { crate::logger::debug(&format!("[debug] 等待响应超时({}s)", deadline.as_secs())); break; }
            };
            crate::logger::debug(&format!("[debug] 收到帧 {} 字节: {}", frame.len(),
                frame.iter().take(16).map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")));
            match parse_server_frame(&frame) {
                Some(ServerMsg::Error(detail)) => {
                    return Err(anyhow!("ASR 协议错误: {detail}"))
                }
                Some(ServerMsg::Result { is_last, body }) => {
                    crate::logger::debug(&format!("[debug] ASR 响应(is_last={is_last}): {}",
                        String::from_utf8_lossy(&body)));
                    let v: serde_json::Value = match serde_json::from_slice(&body) {
                        Ok(v) => v,
                        Err(e) => return Err(anyhow!("解析 ASR 响应失败: {e}")),
                    };
                    if let Some(code) = v.get("code").and_then(|c| c.as_i64()) {
                        if code != 0 {
                            let msg = v
                                .get("message")
                                .and_then(|m| m.as_str())
                                .unwrap_or("unknown");
                            return Err(anyhow!("ASR 错误 {code}: {msg}"));
                        }
                    }
                    if let Some(t) = v.pointer("/result/text").and_then(|x| x.as_str()) {
                        text.push_str(t);
                    }
                    if is_last
                        || v.get("is_last_package")
                            .and_then(|x| x.as_bool())
                            .unwrap_or(false)
                    {
                        crate::logger::debug("[debug] 收到最后包，结束");
                        break;
                    }
                }
                None => { crate::logger::debug("[debug] 帧解析失败，跳过"); }
            }
        }
        Ok(text)
    }
}

/// 解析服务端响应帧，返回解压后的 JSON body 或错误信息。
fn parse_server_frame(frame: &[u8]) -> Option<ServerMsg> {
    if frame.len() < 4 {
        return None;
    }
    let header_size = (frame[0] & 0x0f) as usize;
    let msg_type = frame[1] >> 4;
    let flags = frame[1] & 0x0f;
    let compression = frame[2] & 0x0f;
    let mut off = header_size * 4;
    if flags & 0x01 != 0 {
        off += 4; // sequence
    }

    if msg_type == MSG_SERVER_ERROR {
        // 错误帧：error code(4B) + size(4B) + JSON payload（不压缩）
        if frame.len() < off + 8 {
            return None;
        }
        let code = i32::from_be_bytes(frame[off..off + 4].try_into().ok()?);
        off += 4;
        let size = u32::from_be_bytes(frame[off..off + 4].try_into().ok()?) as usize;
        off += 4;
        let end = (off + size).min(frame.len());
        let detail = String::from_utf8_lossy(&frame[off..end]).to_string();
        return Some(ServerMsg::Error(format!("错误码 {code}: {detail}")));
    }

    if msg_type != MSG_SERVER_FULL_RESPONSE || frame.len() < off + 4 {
        return None;
    }
    let size = u32::from_be_bytes(frame[off..off + 4].try_into().ok()?) as usize;
    off += 4;
    let end = (off + size).min(frame.len());
    let payload = &frame[off..end];
    // 服务端响应可能不压缩（compression=0），按帧头字段决定
    let body = if compression == COMPRESSION_GZIP {
        AsrClient::gunzip(payload).ok()?
    } else {
        payload.to_vec()
    };
    Some(ServerMsg::Result {
        is_last: flags & 0x02 != 0,
        body,
    })
}
