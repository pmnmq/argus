//! 可编程的假 provider。E2E 与压测共用的公共设施（`docs/20` §8）。
//!
//! 两种传输，缺一不可：
//!
//! | | 用途 | 为什么不能只留一个 |
//! |---|---|---|
//! | [`Transport::Raw`] | 手写 HTTP/1.1 字节 | 要验「字节怎么分片、什么时候到」。任何 HTTP 库都会替我们重新分片，把要验的东西抹掉 |
//! | [`Transport::Tls`] | 真 TLS + ALPN | 要验 h2 真的协商上了（宪法 C23）。手写字节做不出 h2 的帧层 |
//!
//! F06 那个只有 `Raw` 的 mock 已经并进这里（F07 T1）。

use std::{
    fmt::Write as _,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

use bytes::Bytes;
use core::{
    convert::Infallible,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use http_body::{Body, Frame};
use http_body_util::BodyExt;
use hyper_util::{rt::TokioExecutor, server::conn::auto};
use pin_project_lite::pin_project;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::Sleep,
};
use tokio_rustls::TlsAcceptor;

use super::{
    scenario::{Ending, Plan, Scenario},
    tls::TestCert,
};

/// 假 provider 怎么把字节送出去。
pub enum Transport {
    /// 手写 HTTP/1.1。分片与时序逐字节可控
    Raw,
    /// TLS + ALPN（h2 与 `http/1.1` 都提供），用现签的自签证书
    Tls(Box<TestCert>),
}

/// 收到过的一次请求。**存原始字节**，不存解析结果 ——
/// 透明性测试要比的就是字节（`docs/20` §5.2）。
#[derive(Clone, Debug, Default)]
pub struct RecordedRequest {
    pub head: Vec<u8>,
    pub body: Vec<u8>,
}

impl RecordedRequest {
    /// 请求头里是否出现过某个 header 名（大小写不敏感）。
    #[must_use]
    pub fn has_header(&self, name: &str) -> bool {
        let head = String::from_utf8_lossy(&self.head).to_ascii_lowercase();
        head.contains(&format!("\n{}:", name.to_ascii_lowercase()))
    }
}

/// 一个假 provider 实例。drop 时监听自动停止。
pub struct MockProvider {
    addr: SocketAddr,
    plan: Arc<Plan>,
    scheme: &'static str,
    hits: Arc<AtomicU32>,
    peer_gone: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    /// 服务端证书。客户端要把它加进信任集才连得上
    root: Option<rustls::pki_types::CertificateDer<'static>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockProvider {
    /// 起一个假 provider，反复用同一个 `scenario` 应答。
    ///
    /// # Panics
    ///
    /// 端口绑不上，或场景展开失败（录制读不出来）。两种都是测试本身的问题。
    pub async fn start(scenario: Scenario, transport: Transport) -> Self {
        // 展开会读录制文件。放进 spawn_blocking 而不是直接在 async 里读，
        // 免得一个大录制把 worker 卡住
        let plan = tokio::task::spawn_blocking(move || scenario.plan())
            .await
            .expect("展开任务不该 panic")
            .expect("场景应当能展开成发送计划");
        let listener =
            TcpListener::bind(("127.0.0.1", 0)).await.expect("环回口的临时端口应当能绑上");
        let addr = listener.local_addr().expect("刚绑上的监听应当有地址");

        let hits = Arc::new(AtomicU32::new(0));
        let peer_gone = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));

        let plan = Arc::new(plan);
        let shared = Shared {
            plan: Arc::clone(&plan),
            hits: Arc::clone(&hits),
            peer_gone: Arc::clone(&peer_gone),
            requests: Arc::clone(&requests),
        };

        let (scheme, root, task) = match transport {
            Transport::Raw => ("http", None, tokio::spawn(serve_raw(listener, shared))),
            Transport::Tls(cert) => {
                let root = cert.der.clone();
                let acceptor = TlsAcceptor::from(cert.server_config());
                (
                    "https",
                    Some(root),
                    tokio::spawn(serve_tls(listener, acceptor, shared)),
                )
            }
        };

        Self {
            addr,
            plan,
            scheme,
            hits,
            peer_gone,
            requests,
            root,
            task,
        }
    }

    /// 明文传输的快捷方式。
    pub async fn raw(scenario: Scenario) -> Self {
        Self::start(scenario, Transport::Raw).await
    }

    /// TLS 传输的快捷方式，现签一张证书。
    pub async fn tls(scenario: Scenario) -> Self {
        Self::start(scenario, Transport::Tls(Box::new(TestCert::localhost()))).await
    }

    /// 场景展开后**会发出去的全部字节**。
    ///
    /// 逐字节转发的断言拿它当期望值：比对客户端收到的字节与这里的返回值，
    /// 而不是比对解析结果 —— 后者会掩盖分片、编码、key 顺序的差异
    /// （`docs/20` §5.2）。
    #[must_use]
    pub fn expected_body(&self) -> Vec<u8> {
        self.plan.steps.iter().flat_map(|s| s.bytes.clone()).collect()
    }

    /// 明文用 IP 字面量，TLS 用 `localhost`。
    ///
    /// 两者不能统一：TLS 那边 SNI 要与证书里的名字对得上，只能用主机名；
    /// 而主机名会多走一次 DNS，`localhost` 常常同时解析到 v4 与 v6，
    /// 明文测试的 Target Policy 只放行了 v4 —— 用主机名会被整体拒掉。
    #[must_use]
    pub fn base_url(&self) -> String {
        match self.scheme {
            "https" => format!("https://localhost:{}", self.addr.port()),
            _ => format!("http://{}", self.addr),
        }
    }

    #[must_use]
    pub const fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// 服务端证书，加进客户端信任集用。明文传输时是 `None`。
    #[must_use]
    pub fn root_cert(&self) -> Option<rustls::pki_types::CertificateDer<'static>> {
        self.root.clone()
    }

    /// 收到过几次请求。
    #[must_use]
    pub fn hits(&self) -> u32 {
        self.hits.load(Ordering::Relaxed)
    }

    /// 写响应时对端已经断开 —— 客户端断开是否传播到了上游，靠它判定。
    #[must_use]
    pub fn peer_gone(&self) -> bool {
        self.peer_gone.load(Ordering::Relaxed)
    }

    /// 最后一次收到的请求。
    ///
    /// # Panics
    ///
    /// 内部锁中毒时。那意味着服务端 task 里 panic 过。
    #[must_use]
    pub fn last_request(&self) -> RecordedRequest {
        self.requests
            .lock()
            .expect("记录锁不该中毒")
            .last()
            .cloned()
            .unwrap_or_default()
    }

    /// 收到过的全部请求。
    ///
    /// # Panics
    ///
    /// 同 [`Self::last_request`]。
    #[must_use]
    pub fn requests_seen(&self) -> Vec<RecordedRequest> {
        self.requests.lock().expect("记录锁不该中毒").clone()
    }
}

#[derive(Clone)]
struct Shared {
    plan: Arc<Plan>,
    hits: Arc<AtomicU32>,
    peer_gone: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl Shared {
    fn record(&self, req: RecordedRequest) {
        self.hits.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut guard) = self.requests.lock() {
            guard.push(req);
        }
    }
}

// ── Raw：手写 HTTP/1.1 ──────────────────────────────────────────────

async fn serve_raw(listener: TcpListener, shared: Shared) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let shared = shared.clone();
        tokio::spawn(async move {
            raw_connection(stream, shared).await;
        });
    }
}

/// 一条连接上**反复**服务请求，直到对端关闭。
///
/// 早期版本一条连接只服务一次，压测时就露馅了：上游连接池每发一次就得
/// 重连，100 并发下连接建立的速率超过 `somaxconn`，于是零星出现
/// ECONNREFUSED —— 表现为 Argus 回 502。那是 mock 的毛病，不是 Argus 的，
/// 但排查时看起来一模一样。真实 provider 都是 keep-alive 的，照着做。
async fn raw_connection(mut stream: TcpStream, shared: Shared) {
    loop {
        if !raw_exchange(&mut stream, &shared).await {
            return;
        }
        // 截断与挂起都不该继续下一轮：前者连接已经不可用，后者压根没返回
        if shared.plan.ending != Ending::Complete {
            return;
        }
    }
}

/// 请求里写了 `Connection: close` 吗？
///
/// 必须认这个头：读到 EOF 才算流结束的客户端（压测用的就是一个），
/// 遇上一个不肯关连接的服务端会永远卡住。而症状是「压测跑不完」，
/// 不是任何一处报错。
fn wants_close(head: &[u8]) -> bool {
    String::from_utf8_lossy(head).lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.trim().eq_ignore_ascii_case("connection")
                && value.trim().eq_ignore_ascii_case("close")
        })
    })
}

/// 服务一次请求。返回 `false` 表示这条连接不能再用了。
async fn raw_exchange(stream: &mut TcpStream, shared: &Shared) -> bool {
    let Some(req) = read_request(stream).await else {
        return false;
    };
    let close_after = wants_close(&req.head);
    shared.record(req);

    let plan = &shared.plan;
    if plan.ending == Ending::Hang {
        // 连头都不发。对端只能等到自己的超时
        std::future::pending::<()>().await;
    }

    let mut head = format!(
        "HTTP/1.1 {} {}\r\ncontent-type: {}\r\n",
        plan.status,
        reason(plan.status),
        plan.content_type
    );
    for (name, value) in &plan.extra_headers {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    if close_after {
        head.push_str("connection: close\r\n");
    }
    // chunked 而不是 Content-Length：分片边界就是 chunk 边界，
    // 这正是 Raw 传输存在的理由
    head.push_str("transfer-encoding: chunked\r\n\r\n");
    if stream.write_all(head.as_bytes()).await.is_err() {
        shared.peer_gone.store(true, Ordering::Relaxed);
        return false;
    }

    for step in &plan.steps {
        if !step.delay.is_zero() {
            tokio::time::sleep(step.delay).await;
        }
        if step.bytes.is_empty() {
            continue;
        }
        let chunk = format!("{:x}\r\n", step.bytes.len());
        let ok = stream.write_all(chunk.as_bytes()).await.is_ok()
            && stream.write_all(&step.bytes).await.is_ok()
            && stream.write_all(b"\r\n").await.is_ok()
            && stream.flush().await.is_ok();
        if !ok {
            shared.peer_gone.store(true, Ordering::Relaxed);
            return false;
        }
    }

    match plan.ending {
        // 不写结束 chunk，直接关 —— 客户端会看到被截断的流
        Ending::Abrupt | Ending::Hang => false,
        Ending::Complete => {
            let flushed =
                stream.write_all(b"0\r\n\r\n").await.is_ok() && stream.flush().await.is_ok();
            flushed && !close_after
        }
    }
}

/// 读到请求头结束，再按 `Content-Length` 把 body 读完。
async fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut buf = Vec::new();
    let mut byte = [0_u8; 1024];
    let head_end = loop {
        let n = stream.read(&mut byte).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&byte[..n]);
        if let Some(pos) = find_head_end(&buf) {
            break pos;
        }
        if buf.len() > 1 << 20 {
            return None;
        }
    };

    let head = buf[..head_end].to_vec();
    let mut body = buf[head_end..].to_vec();
    if let Some(len) = content_length(&head) {
        while body.len() < len {
            let n = stream.read(&mut byte).await.ok()?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&byte[..n]);
        }
        body.truncate(len);
    }
    Some(RecordedRequest { head, body })
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

fn content_length(head: &[u8]) -> Option<usize> {
    String::from_utf8_lossy(head)
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().to_owned())
        })
        .and_then(|v| v.parse().ok())
}

const fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

// ── TLS：真 HTTP，h1/h2 由 ALPN 决定 ────────────────────────────────

async fn serve_tls(listener: TcpListener, acceptor: TlsAcceptor, shared: Shared) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let acceptor = acceptor.clone();
        let shared = shared.clone();
        tokio::spawn(async move {
            let Ok(tls) = acceptor.accept(stream).await else {
                return;
            };
            let io = hyper_util::rt::TokioIo::new(tls);
            let service = hyper::service::service_fn(move |req| {
                let shared = shared.clone();
                async move { Ok::<_, std::convert::Infallible>(respond(req, shared).await) }
            });
            let _ = auto::Builder::new(TokioExecutor::new()).serve_connection(io, service).await;
        });
    }
}

pin_project! {
    /// 计划的 body 实现。
    ///
    /// 不用 `StreamBody` + `async-stream`：那要多引两个依赖，而这里要的
    /// 东西只是「等一会儿、发一段」——手写一个 `Body` 比多两个 crate 划算。
    struct PlanBody {
        plan: Arc<Plan>,
        idx: usize,
        peer_gone: Arc<AtomicBool>,
        #[pin]
        sleep: Option<Sleep>,
    }
}

impl Body for PlanBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        let mut this = self.project();
        loop {
            let Some(step) = this.plan.steps.get(*this.idx) else {
                if this.plan.ending == Ending::Abrupt {
                    // h2 下没有「不打招呼关连接」这回事，最接近的是提前收尾。
                    // 字节级的截断只能靠 Transport::Raw 去验
                    this.peer_gone.store(true, Ordering::Relaxed);
                }
                return Poll::Ready(None);
            };

            if !step.delay.is_zero() {
                if this.sleep.is_none() {
                    this.sleep.set(Some(tokio::time::sleep(step.delay)));
                }
                if let Some(sleep) = this.sleep.as_mut().as_pin_mut() {
                    core::task::ready!(sleep.poll(cx));
                }
                this.sleep.set(None);
            }

            *this.idx += 1;
            if step.bytes.is_empty() {
                continue;
            }
            return Poll::Ready(Some(Ok(Frame::data(Bytes::from(step.bytes.clone())))));
        }
    }
}

async fn respond(
    req: hyper::Request<hyper::body::Incoming>,
    shared: Shared,
) -> hyper::Response<PlanBody> {
    let head = render_head(&req);
    let body = req
        .into_body()
        .collect()
        .await
        .map(|c| c.to_bytes().to_vec())
        .unwrap_or_default();
    shared.record(RecordedRequest { head, body });

    if shared.plan.ending == Ending::Hang {
        std::future::pending::<()>().await;
    }

    let mut resp = hyper::Response::builder()
        .status(shared.plan.status)
        .header("content-type", shared.plan.content_type);
    for (name, value) in &shared.plan.extra_headers {
        resp = resp.header(*name, value);
    }
    let plan_body = PlanBody {
        plan: Arc::clone(&shared.plan),
        idx: 0,
        peer_gone: Arc::clone(&shared.peer_gone),
        sleep: None,
    };
    resp.body(plan_body).unwrap_or_else(|_| {
        hyper::Response::new(PlanBody {
            plan: Arc::clone(&shared.plan),
            idx: usize::MAX,
            peer_gone: Arc::clone(&shared.peer_gone),
            sleep: None,
        })
    })
}

fn render_head(req: &hyper::Request<hyper::body::Incoming>) -> Vec<u8> {
    let mut head = format!(
        "{} {} {:?}\r\n",
        req.method(),
        req.uri().path_and_query().map_or_else(|| "/".to_owned(), ToString::to_string),
        req.version()
    );
    for (name, value) in req.headers() {
        let _ = write!(
            head,
            "\n{}: {}\r\n",
            name.as_str(),
            String::from_utf8_lossy(value.as_bytes())
        );
    }
    head.push_str("\r\n");
    head.into_bytes()
}
