//! Argus 网关的唯一可执行入口。
//!
//! 本 crate 只做三件事：解析 CLI、按配置组装各层、编排优雅停机。
//! 业务逻辑一律不在这里。见 `docs/19-rust-project-structure.md` §5.3。
//!
//! F02 阶段这里仍然只有一个自检命令。它同时证明两件事：五个 crate 真的被
//! 链接进了同一个二进制，且 `argus-core` 的类型在组装层可用。

#![forbid(unsafe_code)]

fn main() {
    // F03 起替换为 clap 的子命令（serve / migrate / config / keys / doctor / keygen）。
    println!("argus {}", env!("CARGO_PKG_VERSION"));

    let protocols: Vec<&str> = argus_core::Protocol::ALL
        .iter()
        .map(|p| p.as_str())
        .collect();
    println!("protocols: {}", protocols.join(", "));

    let providers: Vec<&str> = argus_core::ProviderKind::ALL
        .iter()
        .map(|k| k.as_str())
        .collect();
    println!("providers: {}", providers.join(", "));

    println!(
        "linked:    argus-core, {}, {}, {}",
        argus_protocol::CRATE_NAME,
        argus_proxy::CRATE_NAME,
        argus_storage::CRATE_NAME,
    );
}
