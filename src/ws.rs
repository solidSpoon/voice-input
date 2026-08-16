use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use sha1::{Digest, Sha1};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;
use uuid::Uuid;

const MAX_FRAME_SIZE: u64 = 16 * 1024 * 1024;
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// 极简 WebSocket 客户端。
///
/// 只支持 wss://、binary 数据帧，且不校验 Text 帧的 UTF-8。火山 ASR 的
/// Text 帧也承载二进制协议头，因此这里只把它当作原始字节处理。
pub struct WsStream {
    stream: TlsStream<TcpStream>,
    /// 握手读取时可能已经读入的首个 WebSocket 帧数据
    wire_buf: Vec<u8>,
    /// 当前分片消息拼接缓冲
    recv_buf: Vec<u8>,
    recv_opcode: Option<u8>,
}

struct WsFrame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

impl WsStream {
    pub async fn connect(url: &str, headers: &[(&str, &str)]) -> Result<Self> {
        let (host, port, path) = parse_ws_url(url)?;

        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = ClientConfig::builder()
            .with_root_certificates(Arc::new(roots))
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(config));
        let server_name =
            ServerName::try_from(host.clone()).map_err(|_| anyhow!("无效的主机名: {host}"))?;
        let tcp = TcpStream::connect((host.as_str(), port))
            .await
            .with_context(|| format!("TCP 连接 {host}:{port} 失败"))?;
        let mut stream = connector
            .connect(server_name, tcp)
            .await
            .context("TLS 连接失败")?;

        let key = base64::engine::general_purpose::STANDARD.encode(Uuid::new_v4().as_bytes());
        let mut req = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
        );
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        stream.write_all(req.as_bytes()).await?;

        let mut resp = Vec::new();
        let mut tmp = [0u8; 4096];
        let header_end = loop {
            if let Some(pos) = resp.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                return Err(anyhow!("握手时连接关闭"));
            }
            resp.extend_from_slice(&tmp[..n]);
            if resp.len() > 8192 {
                return Err(anyhow!("握手响应过大"));
            }
        };

        let header_text = String::from_utf8_lossy(&resp[..header_end]);
        if !header_text.starts_with("HTTP/1.1 101 ") {
            let head = header_text[..header_text.len().min(200)].to_string();
            return Err(anyhow!("WebSocket 握手失败: {head}"));
        }
        let accept = header_text.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            (name.eq_ignore_ascii_case("Sec-WebSocket-Accept")).then(|| value.trim())
        });
        let mut hasher = Sha1::new();
        hasher.update(key.as_bytes());
        hasher.update(WS_GUID.as_bytes());
        let expected = base64::engine::general_purpose::STANDARD.encode(hasher.finalize());
        if accept != Some(expected.as_str()) {
            return Err(anyhow!("WebSocket 握手缺少有效的 Sec-WebSocket-Accept"));
        }

        Ok(Self {
            stream,
            wire_buf: resp[header_end..].to_vec(),
            recv_buf: Vec::new(),
            recv_opcode: None,
        })
    }

    /// 发送一个二进制数据帧（客户端帧带掩码）。
    pub async fn send_binary(&mut self, payload: &[u8]) -> Result<()> {
        let n = payload.len();
        let mut hdr = vec![0x82u8];
        if n < 126 {
            hdr.push(0x80 | n as u8);
        } else if n <= 0xFFFF {
            hdr.push(0x80 | 126);
            hdr.extend_from_slice(&(n as u16).to_be_bytes());
        } else {
            hdr.push(0x80 | 127);
            hdr.extend_from_slice(&(n as u64).to_be_bytes());
        }
        let mask = random_mask();
        hdr.extend_from_slice(&mask);
        let masked: Vec<u8> = payload
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ mask[i % 4])
            .collect();
        self.stream.write_all(&hdr).await?;
        self.stream.write_all(&masked).await?;
        self.stream.flush().await?;
        Ok(())
    }

    /// 读取一条完整 WebSocket 消息，并处理控制帧。
    pub async fn recv(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            let Some(frame) = self.read_frame().await? else {
                return Ok(None);
            };
            match frame.opcode {
                0x0 => {
                    if self.recv_opcode.is_none() {
                        return Err(anyhow!("收到没有起始帧的 continuation 帧"));
                    }
                    self.recv_buf.extend_from_slice(&frame.payload);
                    if frame.fin {
                        self.recv_opcode = None;
                        return Ok(Some(std::mem::take(&mut self.recv_buf)));
                    }
                }
                0x1 | 0x2 => {
                    if self.recv_opcode.is_some() {
                        return Err(anyhow!("收到嵌套的 WebSocket 数据帧"));
                    }
                    if frame.fin {
                        return Ok(Some(frame.payload));
                    }
                    self.recv_opcode = Some(frame.opcode);
                    self.recv_buf = frame.payload;
                }
                0x8 => return Ok(None),
                0x9 => self.send_control(0xA, &frame.payload).await?,
                0xA => {}
                _ => return Err(anyhow!("未知 WebSocket opcode: {}", frame.opcode)),
            }
        }
    }

    async fn send_control(&mut self, opcode: u8, payload: &[u8]) -> Result<()> {
        if payload.len() > 125 {
            return Err(anyhow!("WebSocket 控制帧过大"));
        }
        let mut frame = vec![0x80 | opcode, 0x80 | payload.len() as u8];
        let mask = random_mask();
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.stream.write_all(&frame).await?;
        self.stream.flush().await?;
        Ok(())
    }

    async fn read_frame(&mut self) -> Result<Option<WsFrame>> {
        let Some(b0) = self.read_wire_u8().await? else {
            return Ok(None);
        };
        let Some(b1) = self.read_wire_u8().await? else {
            return Err(anyhow!("帧头不完整"));
        };
        let fin = b0 & 0x80 != 0;
        let opcode = b0 & 0x0f;
        let masked = b1 & 0x80 != 0;
        let mut len = (b1 & 0x7f) as u64;
        if len == 126 {
            let mut b = [0u8; 2];
            self.read_wire_exact(&mut b).await.context("帧长不完整")?;
            len = u16::from_be_bytes(b) as u64;
        } else if len == 127 {
            let mut b = [0u8; 8];
            self.read_wire_exact(&mut b).await.context("帧长不完整")?;
            len = u64::from_be_bytes(b);
        }
        if opcode >= 0x8 && (!fin || len > 125) {
            return Err(anyhow!("非法 WebSocket 控制帧"));
        }
        if len > MAX_FRAME_SIZE {
            return Err(anyhow!("WebSocket 帧过大: {len} 字节"));
        }

        let mut mask = [0u8; 4];
        if masked {
            self.read_wire_exact(&mut mask)
                .await
                .context("掩码不完整")?;
        }
        let mut payload = vec![0u8; len as usize];
        if len > 0 {
            self.read_wire_exact(&mut payload)
                .await
                .context("帧数据不完整")?;
        }
        if masked {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i % 4];
            }
        }
        Ok(Some(WsFrame {
            fin,
            opcode,
            payload,
        }))
    }

    async fn read_wire_u8(&mut self) -> Result<Option<u8>> {
        if !self.wire_buf.is_empty() {
            return Ok(Some(self.wire_buf.remove(0)));
        }
        let mut b = [0u8; 1];
        let n = self.stream.read(&mut b).await?;
        Ok((n != 0).then_some(b[0]))
    }

    async fn read_wire_exact(&mut self, out: &mut [u8]) -> Result<()> {
        let mut offset = 0;
        while offset < out.len() {
            if !self.wire_buf.is_empty() {
                let n = (out.len() - offset).min(self.wire_buf.len());
                out[offset..offset + n].copy_from_slice(&self.wire_buf[..n]);
                self.wire_buf.drain(..n);
                offset += n;
            } else {
                self.stream.read_exact(&mut out[offset..]).await?;
                break;
            }
        }
        Ok(())
    }
}

fn parse_ws_url(url: &str) -> Result<(String, u16, String)> {
    let rest = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("https://"))
        .context("仅支持 wss:// 地址")?;
    let (host_port, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match host_port.rfind(':') {
        Some(i) => (
            &host_port[..i],
            host_port[i + 1..].parse().context("端口无效")?,
        ),
        None => (host_port, 443),
    };
    Ok((host.to_string(), port, path.to_string()))
}

/// 使用操作系统随机源生成客户端帧掩码。
fn random_mask() -> [u8; 4] {
    let bytes = Uuid::new_v4().into_bytes();
    [bytes[0], bytes[1], bytes[2], bytes[3]]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ws_url_with_default_and_explicit_ports() {
        assert_eq!(
            parse_ws_url("wss://example.com/asr").unwrap(),
            ("example.com".to_string(), 443, "/asr".to_string())
        );
        assert_eq!(
            parse_ws_url("wss://example.com:8443").unwrap(),
            ("example.com".to_string(), 8443, "/".to_string())
        );
    }

    #[test]
    fn rejects_non_ws_urls_and_invalid_ports() {
        assert!(parse_ws_url("http://example.com").is_err());
        assert!(parse_ws_url("wss://example.com:not-a-port").is_err());
    }
}
