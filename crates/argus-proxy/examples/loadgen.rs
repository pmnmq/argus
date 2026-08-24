//! 对照法压测的客户端（`docs/21` §5.3）。
//!
//! 为什么不用 oha / k6：它们量的是「HTTP 请求什么时候完成」，而我们要的是
//! **TTFT** —— 请求写出到**第一个 body 字节**到达之间的时间。对一条 SSE 流，
//! 前者约等于流的总长度，跟代理开销几乎无关。用错了指标，压测再漂亮也是白测。
//!
//! 也因此这里是手写 HTTP/1.1 而不是套一层客户端库：库会缓冲、会合并读，
//! 「第一个字节」的时刻就被抹掉了。顺带避开了 `clippy.toml` 里禁用的
//! `HttpConnector`（它会自己再解析一次 DNS，见 `upstream/connect.rs`）。
//!
//! 对照组与实验组**跑的是同一个二进制、同一份负载**，只改 `--url`——
//! 这是 `docs/21` §5.3 对差值可信度的硬要求。
//!
//! ```bash
//! cargo run --release -p argus-proxy --features testkit --example loadgen -- \
//!     --url http://127.0.0.1:8080/provider/mock/v1/messages \
//!     --streams 100 --duration-s 30 --label experiment
//! ```

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::Mutex,
};

/// 单条流最多跑这么久。压测的流都在秒级，超过这个数说明卡住了。
const STREAM_DEADLINE: Duration = Duration::from_secs(60);

const REQUEST_BODY: &str = r#"{"model":"claude-x","max_tokens":256,"stream":true,"messages":[{"role":"user","content":"bench"}]}"#;

struct Args {
    url: String,
    streams: usize,
    duration: Duration,
    label: String,
}

fn parse_args() -> Args {
    let mut url = String::new();
    let mut streams = 100_usize;
    let mut duration_s = 30_u64;
    let mut label = "run".to_owned();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        let value = args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "--url" => url = value,
            "--streams" => streams = value.parse().unwrap_or(streams),
            "--duration-s" => duration_s = value.parse().unwrap_or(duration_s),
            "--label" => label = value,
            other => {
                eprintln!("未知参数：{other}");
                std::process::exit(2);
            }
        }
        i += 2;
    }
    if url.is_empty() {
        eprintln!("--url 是必填的");
        std::process::exit(2);
    }
    Args {
        url,
        streams,
        duration: Duration::from_secs(duration_s),
        label,
    }
}

/// `http://host:port/path` → (host:port, path)。
///
/// 只认明文 http：压测要量的是代理开销，掺进 TLS 握手会把对照组与实验组
/// 的差值染上一层与 Argus 无关的成本。
fn split_url(url: &str) -> (String, String) {
    let rest = url.strip_prefix("http://").unwrap_or_else(|| {
        eprintln!("只支持 http:// 的目标（理由见源码注释）");
        std::process::exit(2);
    });
    match rest.find('/') {
        Some(i) => (rest[..i].to_owned(), rest[i..].to_owned()),
        None => (rest.to_owned(), "/".to_owned()),
    }
}

#[derive(Default)]
struct Samples {
    ttft_micros: Vec<u64>,
}

#[tokio::main]
async fn main() {
    let args = parse_args();
    let (authority, path) = split_url(&args.url);

    let samples = Arc::new(Mutex::new(Samples::default()));
    let errors = Arc::new(AtomicUsize::new(0));
    let error_kinds: Arc<Mutex<std::collections::BTreeMap<String, usize>>> =
        Arc::new(Mutex::new(std::collections::BTreeMap::new()));
    let completed = Arc::new(AtomicUsize::new(0));
    let bytes = Arc::new(AtomicU64::new(0));

    let deadline = Instant::now() + args.duration;
    let mut workers = Vec::with_capacity(args.streams);

    for _ in 0..args.streams {
        let authority = authority.clone();
        let path = path.clone();
        let samples = Arc::clone(&samples);
        let errors = Arc::clone(&errors);
        let error_kinds = Arc::clone(&error_kinds);
        let completed = Arc::clone(&completed);
        let bytes = Arc::clone(&bytes);

        workers.push(tokio::spawn(async move {
            let mut local: Vec<u64> = Vec::new();
            while Instant::now() < deadline {
                // 单条流的硬时限。没有它，一条卡住的流就能让整轮压测永远
                // 不结束 —— 而现象是「跑了半小时还没出结果」，不是报错
                let attempt =
                    tokio::time::timeout(STREAM_DEADLINE, one_stream(&authority, &path)).await;
                match attempt {
                    Ok(Ok((ttft, read))) => {
                        local.push(ttft);
                        completed.fetch_add(1, Ordering::Relaxed);
                        bytes.fetch_add(read, Ordering::Relaxed);
                    }
                    other => {
                        errors.fetch_add(1, Ordering::Relaxed);
                        let kind = match other {
                            Err(_) => "stream_deadline".to_owned(),
                            // Other 是我们自己造的（非 2xx），消息里就是类别；
                            // 其余是真正的 IO 错误，用 ErrorKind
                            Ok(Err(e)) if e.kind() == std::io::ErrorKind::Other => e.to_string(),
                            Ok(Err(e)) => format!("{:?}", e.kind()),
                            Ok(Ok(_)) => unreachable!(),
                        };
                        *error_kinds.lock().await.entry(kind).or_insert(0) += 1;
                        // 失败之后立刻重试会把出错的那一侧变成忙等，
                        // 顺便污染另一侧的 CPU 读数
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
            }
            samples.lock().await.ttft_micros.extend(local);
        }));
    }

    for w in workers {
        let _ = w.await;
    }

    let mut ttft = std::mem::take(&mut samples.lock().await.ttft_micros);
    ttft.sort_unstable();

    let report = format!(
        concat!(
            "{{\n",
            "  \"label\": \"{label}\",\n",
            "  \"streams\": {streams},\n",
            "  \"duration_s\": {duration},\n",
            "  \"completed\": {completed},\n",
            "  \"errors\": {errors},\n",
            "  \"error_kinds\": {{{kinds}}},\n",
            "  \"bytes_read\": {bytes},\n",
            "  \"ttft_us\": {{ \"p50\": {p50}, \"p95\": {p95}, \"p99\": {p99}, \"samples\": {n} }}\n",
            "}}"
        ),
        label = args.label,
        streams = args.streams,
        duration = args.duration.as_secs(),
        completed = completed.load(Ordering::Relaxed),
        errors = errors.load(Ordering::Relaxed),
        kinds = error_kinds
            .lock()
            .await
            .iter()
            .map(|(k, v)| format!("\"{k}\": {v}"))
            .collect::<Vec<_>>()
            .join(", "),
        bytes = bytes.load(Ordering::Relaxed),
        p50 = percentile(&ttft, 50.0),
        p95 = percentile(&ttft, 95.0),
        p99 = percentile(&ttft, 99.0),
        n = ttft.len(),
    );
    println!("{report}");
}

/// 跑一条流，返回 (TTFT 微秒, 读到的字节数)。
///
/// 每条流一条新连接：连接复用会把 TCP 握手从测量里拿掉，那正是
/// `docs/21` §6.1「连接复用」那一行的条件。冷连接的数字由上层脚本
/// 另外跑一轮 —— 混在一起会得到一个两者都不是的中间值。
async fn one_stream(authority: &str, path: &str) -> std::io::Result<(u64, u64)> {
    let mut sock = TcpStream::connect(authority).await?;
    sock.set_nodelay(true)?;

    // `connection: close` 是必须的，不是随手加的：读到 EOF 才算这条流结束，
    // 而开着 keep-alive 的话 Argus 发完终止 chunk 也不会关连接 —— 客户端会
    // 永远卡在 read 上。（直连 mock 时不会，因为它一条连接只服务一次，
    // 于是这个 bug 只在实验组出现，症状是「对照组正常、实验组永远不结束」。）
    //
    // 每条流本来就是新连接（见函数头），所以关掉 keep-alive 不影响要量的东西。
    let req = format!(
        "POST {path} HTTP/1.1\r\nhost: {authority}\r\nconnection: close\r\n\
         content-type: application/json\r\n\
         anthropic-version: 2023-06-01\r\ncontent-length: {}\r\n\r\n{REQUEST_BODY}",
        REQUEST_BODY.len()
    );

    let started = Instant::now();
    sock.write_all(req.as_bytes()).await?;
    sock.flush().await?;

    let mut buf = vec![0_u8; 16 * 1024];
    let mut seen = Vec::new();
    let mut ttft = None;
    let mut total = 0_u64;

    loop {
        let n = sock.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if ttft.is_none() {
            seen.extend_from_slice(&buf[..n]);
            // 「第一个 body 字节」而不是「第一个字节」：响应头是 Argus
            // 一口气写出的，把它算进去会系统性地低估 TTFT
            if let Some(pos) = find_head_end(&seen)
                && seen.len() > pos
            {
                // 状态码不是 2xx 就当失败。少了这一句，一个 404 会被记成
                // 「一条极快的流」，把实验组的分位数拉得比对照组还好看 ——
                // 压测里最坏的一类假绿：数字漂亮，但根本没在测要测的东西
                if let Some(code) = status_code(&seen)
                    && !(200..300).contains(&code)
                {
                    // 错误类别里带上状态码：只报「失败 4 次」的话，
                    // 「上游 502」和「客户端端口耗尽」长得一模一样，
                    // 而两者的处置完全相反
                    return Err(std::io::Error::other(format!("http_{code}")));
                }
                // 一次请求不可能跑满 u64 微秒（58 万年）
                ttft = u64::try_from(started.elapsed().as_micros()).ok().or(Some(u64::MAX));
            }
        }
    }

    ttft.map_or_else(
        || {
            Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "响应里没有 body",
            ))
        },
        |t| Ok((t, total)),
    )
}

/// 从状态行里取状态码。
fn status_code(head: &[u8]) -> Option<u16> {
    String::from_utf8_lossy(&head[..head.len().min(64)])
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

/// 最近秩百分位。样本为空时返回 0 —— 调用方靠 `samples` 字段判断有效性，
/// 而不是靠一个假的分位数。
fn percentile(sorted: &[u64], pct: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "样本数远小于 f64 的整数精度上限，且 rank 已被 min 夹住"
    )]
    let rank = (pct / 100.0 * (sorted.len() - 1) as f64).round() as usize;
    sorted[rank.min(sorted.len() - 1)]
}
