//! 独立跑起来的 `MockProvider`，给 `ci/bench-e2e.sh` 用。
//!
//! 端口不给参数，起来之后把自己的 `base_url` 打到 stdout 第一行 ——
//! 压测脚本读那一行。写死端口的话，两次压测叠在一起就会撞端口，
//! 而症状是「第二次的数字莫名其妙地好」（它连到了上一次的进程）。
//!
//! ```bash
//! cargo run -p argus-proxy --features testkit --example mock-provider -- \
//!     --tokens 200 --interval-ms 5
//! ```

use std::time::Duration;

use argus_proxy::testkit::{MockProvider, Scenario};

#[tokio::main]
async fn main() {
    let mut tokens = 200_usize;
    let mut interval_ms = 5_u64;
    let mut tls = false;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tokens" => {
                i += 1;
                tokens = args.get(i).and_then(|v| v.parse().ok()).unwrap_or(tokens);
            }
            "--interval-ms" => {
                i += 1;
                interval_ms = args.get(i).and_then(|v| v.parse().ok()).unwrap_or(interval_ms);
            }
            "--tls" => tls = true,
            other => {
                eprintln!("未知参数：{other}");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    // Synthetic 是压测主力：不依赖录制，token 数与间隔都可扫（F07 §5）
    let scenario = Scenario::Synthetic {
        tokens,
        interval: Duration::from_millis(interval_ms),
    };
    let provider = if tls {
        MockProvider::tls(scenario).await
    } else {
        MockProvider::raw(scenario).await
    };

    println!("{}", provider.base_url());
    eprintln!(
        "mock-provider 就绪：{} tokens，间隔 {interval_ms}ms，{}",
        tokens,
        if tls { "TLS" } else { "明文" }
    );

    // stdout 已经刷出去了，剩下的就是待命
    std::future::pending::<()>().await;
}
