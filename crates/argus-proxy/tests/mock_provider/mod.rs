//! 一个能精确控制分片与时序的假上游。
//!
//! 直接写原始 HTTP/1.1 字节，不用 hyper server —— F06 要验的正是「字节怎么
//! 分片、什么时候到」，而任何一层 HTTP 库都会替我们重新分片，把要验的东西
//! 抹掉。真正的 `MockProvider`（带 pacing、场景回放、故障注入）是 F07 的活。
//!
//! ⚠️ 下面的 SSE 字节是**合成**的，不是 fixture：宪法 C25 要求 fixture 来自
//! 录制，而录制工具是 F10 R6 的交付物。

#![allow(dead_code, reason = "不同测试用到的子集不同")]

use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::Duration,
};

/// 一个只服务若干次请求的假 provider。
pub struct MockProvider {
    addr: SocketAddr,
    hits: Arc<AtomicU32>,
    /// 写响应时对端已经断开 —— 客户端断开是否传播到了上游，靠它判定
    peer_gone: Arc<AtomicBool>,
    /// 最后一次收到的请求（原始字节）。header 变换靠它验
    last_request: Arc<Mutex<Vec<u8>>>,
}

/// 响应怎么发。
#[derive(Clone)]
pub struct Script {
    pub status: u16,
    pub content_type: &'static str,
    /// 每片之间的间隔。`Duration::ZERO` 表示尽快发完
    pub pacing: Duration,
    pub chunks: Vec<Vec<u8>>,
}

impl Script {
    /// 一段 Anthropic Messages 的流。
    pub fn anthropic_stream() -> Self {
        let frames = [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_mock\",\"model\":\"claude-x\",\"role\":\"assistant\",\"usage\":{\"input_tokens\":11,\"output_tokens\":1}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\", world\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":7}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ];
        Self {
            status: 200,
            content_type: "text/event-stream",
            pacing: Duration::ZERO,
            chunks: frames.iter().map(|f| f.as_bytes().to_vec()).collect(),
        }
    }

    pub fn json(body: &str) -> Self {
        Self {
            status: 200,
            content_type: "application/json",
            pacing: Duration::ZERO,
            chunks: vec![body.as_bytes().to_vec()],
        }
    }

    #[must_use]
    pub fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    #[must_use]
    pub fn with_pacing(mut self, pacing: Duration) -> Self {
        self.pacing = pacing;
        self
    }

    /// 一条很长的流，用来把旁路的队列灌满。
    pub fn flood(frames: usize, filler: usize) -> Self {
        let text = "x".repeat(filler);
        let chunks = (0..frames)
            .map(|_| {
                format!(
                    "event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n"
                )
                .into_bytes()
            })
            .collect();
        Self {
            status: 200,
            content_type: "text/event-stream",
            pacing: Duration::ZERO,
            chunks,
        }
    }

    pub fn body(&self) -> Vec<u8> {
        self.chunks.concat()
    }
}

impl MockProvider {
    /// 起一个假 provider，服务 `serves` 次请求之后退出。
    pub async fn start(script: Script, serves: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("绑定回环端口");
        let addr = listener.local_addr().expect("拿到端口");
        let hits = Arc::new(AtomicU32::new(0));
        let peer_gone = Arc::new(AtomicBool::new(false));

        let last_request = Arc::new(Mutex::new(Vec::new()));
        let me = Self {
            addr,
            hits: Arc::clone(&hits),
            peer_gone: Arc::clone(&peer_gone),
            last_request: Arc::clone(&last_request),
        };
        tokio::spawn(async move {
            for _ in 0..serves {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                hits.fetch_add(1, Ordering::Relaxed);
                let script = script.clone();
                let peer_gone = Arc::clone(&peer_gone);
                let last_request = Arc::clone(&last_request);
                tokio::spawn(async move {
                    serve_one(stream, &script, &peer_gone, &last_request).await;
                });
            }
        });
        me
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn hits(&self) -> u32 {
        self.hits.load(Ordering::Relaxed)
    }

    /// 写响应途中对端就断了 —— 说明客户端断开传播到了上游。
    pub fn peer_gone(&self) -> bool {
        self.peer_gone.load(Ordering::Relaxed)
    }

    /// 上游真正收到的请求，原始字节。
    pub fn last_request(&self) -> String {
        let raw = self.last_request.lock().expect("锁没被毒化").clone();
        String::from_utf8_lossy(&raw).into_owned()
    }
}

async fn serve_one(
    mut stream: TcpStream,
    script: &Script,
    peer_gone: &AtomicBool,
    last_request: &Mutex<Vec<u8>>,
) {
    let Ok(raw) = read_request(&mut stream).await else {
        return;
    };
    *last_request.lock().expect("锁没被毒化") = raw;
    // 用 Content-Length 而不是 chunked：省掉分块编码这一层，
    // 断言的就是净荷字节本身
    let head = format!(
        "HTTP/1.1 {} OK\r\ncontent-type: {}\r\ncontent-length: {}\r\n\r\n",
        script.status,
        script.content_type,
        script.body().len(),
    );
    if stream.write_all(head.as_bytes()).await.is_err() {
        peer_gone.store(true, Ordering::Relaxed);
        return;
    }
    for chunk in &script.chunks {
        if !script.pacing.is_zero() {
            tokio::time::sleep(script.pacing).await;
        }
        if stream.write_all(chunk).await.is_err() || stream.flush().await.is_err() {
            peer_gone.store(true, Ordering::Relaxed);
            return;
        }
    }
    let _ = stream.shutdown().await;
}

/// 读到 header 结束就行。请求体一并吃掉，免得对端写阻塞。
async fn read_request(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0_u8; 1024];
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(end) = find(&buf, b"\r\n\r\n") {
            let want = content_length(buf.get(..end).unwrap_or_default()).unwrap_or(0);
            let have = buf.len().saturating_sub(end + 4);
            if have >= want {
                return Ok(buf);
            }
        }
    }
}

fn content_length(head: &[u8]) -> Option<usize> {
    core::str::from_utf8(head)
        .ok()?
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
        .and_then(|l| l.split(':').nth(1))
        .and_then(|v| v.trim().parse().ok())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
