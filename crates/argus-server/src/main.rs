//! Argus 网关的唯一可执行入口。
//!
//! 本 crate 只做三件事：解析 CLI、按配置组装各层、编排优雅停机。
//! 业务逻辑一律不在这里。见 `docs/14-rust-project-structure.md` §5.3。
//!
//! F01 阶段这里只有一个自检命令，用来证明五个 crate 真的被链接进了同一个二进制。

#![forbid(unsafe_code)]

fn main() {
    // F02 起替换为 clap 的子命令（serve / migrate / config / keys / doctor / keygen）。
    println!(
        "argus {} — crates: {}, {}, {}, {}",
        env!("CARGO_PKG_VERSION"),
        argus_core::CRATE_NAME,
        argus_protocol::CRATE_NAME,
        argus_proxy::CRATE_NAME,
        argus_storage::CRATE_NAME,
    );
}
