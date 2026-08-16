use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

/// 极简 WebSocket 客户端。
///
/// 只支持 wss://、binary 数据帧，且**不校验 Text 帧的 UTF-8**——火山 ASR 用 Text 帧
/// 发送二进制协议头 + JSON，tungstenite 等完整库会直接报错，因此这里手写帧层。
pub struct WsStream {
    stream: TlsStream<TcpStream>,
    /// 分片消息拼接缓冲
    recv_buf: Vec<u8>,
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

        // HTTP 升级握手（客户端不校验 Sec-WebSocket-Accept）
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
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
        while !resp.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                return Err(anyhow!("握手时连接关闭"));
            }
            resp.extend_from_slice(&tmp[..n]);
            if resp.len() > 8192 {
                return Err(anyhow!("握手响应过大"));
            }
        }
        if !resp.starts_with(b"HTTP/1.1 101") {
            let head = String::from_utf8_lossy(&resp[..resp.len().min(200)]);
            return Err(anyhow!("WebSocket 握手失败: {head}"));
        }

        Ok(Self {
            stream,
            recv_buf: Vec::new(),
        })
    }

    /// 发送一个二进制数据帧（客户端帧带掩码）。
    pub async fn send_binary(&mut self, payload: &[u8]) -> Result<()> {
        let n = payload.len();
        let mut hdr = vec![0x82u8]; // FIN + binary opcode
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

    /// 读取一帧原始 payload（Text/Binary 数据帧，不做 UTF-8 校验）。
    /// 收到 close 帧或连接关闭时返回 None。
    pub async fn recv(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            let Some(frame) = self.read_frame().await? else {
                return Ok(None);
            };
            match frame.opcode {
                0x0 | 0x1 | 0x2 => {
                    self.recv_buf.extend_from_slice(&frame.payload);
                    if frame.fin {
                        let data = std::mem::take(&mut self.recv_buf);
                        return Ok(Some(data));
                    }
                }
                0x8 => return Ok(None), // close
                0x9 | 0xa => {}         // ping/pong：忽略
                _ => {}
            }
        }
    }

    async fn read_frame(&mut self) -> Result<Option<WsFrame>> {
        let Some(b0) = read_u8(&mut self.stream).await? else {
            return Ok(None);
        };
        let Some(b1) = read_u8(&mut self.stream).await? else {
            return Err(anyhow!("帧头不完整"));
        };
        let fin = b0 & 0x80 != 0;
        let opcode = b0 & 0x0f;
        let masked = b1 & 0x80 != 0;
        let mut len = (b1 & 0x7f) as u64;
        if len == 126 {
            let mut b = [0u8; 2];
            self.stream.read_exact(&mut b).await.context("帧长不完整")?;
            len = u16::from_be_bytes(b) as u64;
        } else if len == 127 {
            let mut b = [0u8; 8];
            self.stream.read_exact(&mut b).await.context("帧长不完整")?;
            len = u64::from_be_bytes(b);
        }
        let mut mask = [0u8; 4];
        if masked {
            self.stream
                .read_exact(&mut mask)
                .await
                .context("掩码不完整")?;
        }
        let mut payload = vec![0u8; len as usize];
        if len > 0 {
            self.stream
                .read_exact(&mut payload)
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
}

async fn read_u8(stream: &mut (impl AsyncRead + Unpin)) -> Result<Option<u8>> {
    let mut b = [0u8; 1];
    let n = stream.read(&mut b).await?;
    if n == 0 {
        Ok(None)
    } else {
        Ok(Some(b[0]))
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

/// 简单的伪随机掩码（非安全关键）。
fn random_mask() -> [u8; 4] {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0) as u64;
    let mut x = t.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    let mut m = [0u8; 4];
    for b in m.iter_mut() {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *b = (x >> 33) as u8;
    }
    m
}
