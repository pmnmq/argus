//! Argus 网关的唯一可执行入口。
//!
//! 本 crate 只做三件事：解析 CLI、按配置组装各层、编排优雅停机。
//! 业务逻辑一律不在这里。见 `docs/19-rust-project-structure.md` §5.3。
//!
//! F06 阶段有两个命令：
//!
//! - 无参数：自检，证明五个 crate 真的被链接进了同一个二进制
//! - `serve`：起一个**能真跑的透传代理**，Profile 从环境变量读
//!
//! 配置文件、认证、路由分别是 F11 / F13 / F14 的活。这里写死一条 Profile
//! 不是偷懒，是为了让 M0 的架构假设能被端到端地验一遍：客户端 → Argus →
//! provider，同时旁路观测，两条路互不影响。

#![forbid(unsafe_code)]

use std::{env, sync::Arc};

use anyhow::Context as _;
use argus_core::{
    CredentialKind, CredentialMode, ObserverKind, ResolvedCredential, Timeouts, ids::CredentialId,
};
use argus_proxy::{
    body::CancelHandle,
    ingress::{ProxyConfig, ProxyState, StaticRegistry, router},
    observer::ObserverRules,
    sink::LogSink,
    target::{CheckedResolver, SystemResolver, TargetPolicy, TargetPolicyConfig},
    upstream::UpstreamClient,
};
use http::HeaderName;

fn main() -> anyhow::Result<()> {
    if env::args().nth(1).as_deref() == Some("serve") {
        return tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(serve());
    }
    self_check();
    Ok(())
}

fn self_check() {
    println!("argus {}", env!("CARGO_PKG_VERSION"));

    let observers: Vec<&str> = argus_core::ObserverKind::ALL.iter().map(|k| k.as_str()).collect();
    println!("observers: {}", observers.join(", "));

    let providers: Vec<&str> = argus_core::ProviderType::ALL.iter().map(|t| t.as_str()).collect();
    println!("provider types: {}", providers.join(", "));

    println!(
        "linked:    argus-core, {}, {}, {}",
        argus_protocol::CRATE_NAME,
        argus_proxy::CRATE_NAME,
        argus_storage::CRATE_NAME,
    );
    println!("usage:     argus serve   （需要 ARGUS_UPSTREAM）");
}

async fn serve() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let listen = env::var("ARGUS_LISTEN").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let upstream = env::var("ARGUS_UPSTREAM")
        .context("需要 ARGUS_UPSTREAM，例如 https://api.anthropic.com")?;
    let profile = env::var("ARGUS_PROFILE").unwrap_or_else(|_| "default".into());

    let registry = StaticRegistry::new(profile.clone(), upstream.clone())
        .with_observer(observer_from_env())
        .with_credential(credential_from_env()?);

    let policy = Arc::new(target_policy()?);
    let resolver = Arc::new(CheckedResolver::new(Arc::new(SystemResolver::default())));
    let timeouts = Timeouts::default();
    let client = UpstreamClient::new(Arc::clone(&policy), resolver, timeouts)
        .map_err(|e| anyhow::anyhow!("上游客户端构建失败：{e}"))?;

    let shutdown = CancelHandle::new();
    let state = ProxyState {
        profiles: Arc::new(registry),
        client,
        sink: LogSink::new(),
        rules: Arc::new(ObserverRules::default()),
        cfg: ProxyConfig::default(),
        shutdown: shutdown.clone(),
    };

    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("绑定 {listen} 失败"))?;
    tracing::info!(%listen, %upstream, %profile, "argus 已启动");
    tracing::info!("试试： curl -X POST http://{listen}/provider/{profile}/v1/messages -d '{{}}'");

    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal(shutdown))
        .await
        .context("服务退出")
}

/// Ctrl-C 之后**先停止接受新连接**，在飞的流由各自的 `Cancellable` 收尾。
///
/// 完整的优雅停机编排（等待在飞请求、spool 落盘、超时强杀）是 F15 的活。
async fn shutdown_signal(cancel: CancelHandle) {
    if tokio::signal::ctrl_c().await.is_err() {
        return;
    }
    tracing::info!("收到停机信号");
    cancel.cancel(argus_core::CancelReason::Shutdown);
}

/// 默认策略把回环地址拒在门外（SSRF 防护，`docs/13` §5）。
///
/// 但 F06 的验收命令就是 `ARGUS_UPSTREAM=http://127.0.0.1:9999` —— 本地
/// 对着 mock provider 跑。所以留一个**必须显式打开**的开关，而不是把回环
/// 从默认拒绝列表里拿掉：默认安全，开口子要留痕。
fn target_policy() -> anyhow::Result<TargetPolicy> {
    let mut cfg = TargetPolicyConfig::default();
    if env::var("ARGUS_ALLOW_LOOPBACK").is_ok() {
        tracing::warn!(
            "ARGUS_ALLOW_LOOPBACK 已打开：回环地址不再被 SSRF 策略拦截。\
             这只该用于本地开发与 mock provider"
        );
        cfg.allow_cidrs.push("127.0.0.0/8".into());
        cfg.allow_cidrs.push("::1/128".into());
    }
    TargetPolicy::compile(&cfg).map_err(|e| anyhow::anyhow!("Target Policy 配置无效：{e}"))
}

fn observer_from_env() -> ObserverKind {
    match env::var("ARGUS_OBSERVER").as_deref() {
        Ok("openai_chat") => ObserverKind::OpenAiChat,
        Ok("openai_responses") => ObserverKind::OpenAiResponses,
        Ok("generic_sse") => ObserverKind::GenericSse,
        Ok("generic_json") => ObserverKind::GenericJson,
        // 默认 Anthropic：M0 的端到端验证走的就是它（F05 已落地）
        _ => ObserverKind::AnthropicMessages,
    }
}

/// `ARGUS_API_KEY` 有就注入，没有就当上游不需要认证（本地模型很常见）。
fn credential_from_env() -> anyhow::Result<CredentialMode> {
    let Ok(secret) = env::var("ARGUS_API_KEY") else {
        return Ok(CredentialMode::None);
    };
    let header = env::var("ARGUS_API_KEY_HEADER").unwrap_or_else(|_| "x-api-key".into());
    let name = HeaderName::try_from(header.as_str())
        .with_context(|| format!("ARGUS_API_KEY_HEADER 不是合法的 header 名：{header}"))?;
    // 存的是**完整的 header 值**：`Authorization` 要带上 `Bearer ` 前缀，
    // 自定义 header 承载的是裸 key。搞反了上游会回 401，而错误信息里
    // 又不会有凭据（它是 <redacted> 的），排查会很难受
    let (kind, value) = if name == http::header::AUTHORIZATION {
        (CredentialKind::Bearer, format!("Bearer {secret}"))
    } else {
        (CredentialKind::ApiKeyHeader, secret)
    };
    let cred = ResolvedCredential::new(CredentialId::generate(), kind, name, value)
        .map_err(|e| anyhow::anyhow!("ARGUS_API_KEY 不可用：{e}"))?;
    Ok(CredentialMode::Injected(Arc::new(cred)))
}
