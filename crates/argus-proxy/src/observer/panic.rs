//! 解析链的 panic 隔离（宪法 C7）。
//!
//! 观测旁路里跑的是 `argus-protocol` 的纯计算代码。它已经 `#![forbid(unsafe_code)]`、
//! `deny(unwrap_used, indexing_slicing, panic)`，但「不会 panic」是一条**目标**，
//! 不是一条保证 —— 一个整数溢出、一个第三方 crate 的边界检查都能把它打破。
//!
//! 而 panic 打进 tokio task 的后果是：这条流的观测没了（可以接受），
//! **主链路的转发也一起没了**（不可接受）。所以这一层存在。
//!
//! ⚠️ panic 之后**绝不能再碰那三个对象**。Framer 的缓冲、Parser 的块状态、
//! Accumulator 的预算，任何一个都可能停在半路 —— 继续喂只会产出一份
//! 看起来正常、实际错乱的审计记录，而「错误的观测数据比没有观测数据更危险」
//! （ADR-009 M3）。调用方据此把 `parser_dead` 置上，之后只 drain 不解析。

use std::panic::{AssertUnwindSafe, catch_unwind};

/// 跑一段解析步骤，panic 时返回 `Err(())` 而不是把 task 带走。
///
/// 用 [`AssertUnwindSafe`] 是因为 `f` 会捕获 `&mut Framer` / `&mut Parser` /
/// `&mut Accumulator`，而 `&mut T` 天然不是 `UnwindSafe` —— 编译器在提醒的
/// 正是「panic 会让它们停在不一致的中间态」。这里断言的不是「不会不一致」，
/// 而是**不一致也没关系，因为我们再也不用它们了**。
///
/// # Errors
///
/// `f` panic 时返回 [`ParserPanic`]。**panic 的内容被刻意丢弃**：它会流进
/// `ObservedInteraction` 进而落库，而 panic message 里可能带着被断言的数据
/// （宪法 C27）。真正的消息与 backtrace 由 Rust 默认的 panic hook 打到
/// stderr，那是给运维看的地方，不是审计记录。
pub fn isolate<R>(f: impl FnOnce() -> R) -> Result<R, ParserPanic> {
    catch_unwind(AssertUnwindSafe(f)).map_err(|_| ParserPanic)
}

/// 解析链 panic 了。**刻意不携带任何信息** —— 见 [`isolate`] 的说明。
#[derive(Copy, Clone, PartialEq, Eq, Debug, thiserror::Error)]
#[error("parser panicked")]
pub struct ParserPanic;

#[cfg(test)]
#[allow(
    clippy::panic,
    clippy::expect_used,
    reason = "这个模块的全部意义就是接住 panic，测试必须制造 panic"
)]
mod tests {
    use super::*;

    #[test]
    fn isolate_a_closure_that_returns_normally_passes_the_value_through() {
        assert_eq!(isolate(|| 1_u32 + 1), Ok(2));
    }

    #[test]
    fn isolate_a_closure_that_panics_returns_err_instead_of_unwinding() {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let got = isolate(|| panic!("解析器炸了"));
        std::panic::set_hook(hook);
        assert_eq!(got, Err(ParserPanic));
    }

    /// 捕获之后 `&mut` 捕获的状态还在，只是**不可信** —— 这正是调用方
    /// 必须置 `parser_dead` 的理由。
    #[test]
    fn isolate_a_panic_leaves_the_captured_state_reachable_but_half_written() {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let mut state = Vec::new();
        let got = isolate(|| {
            state.push(1_u8);
            panic!("写到一半");
        });
        std::panic::set_hook(hook);
        assert_eq!(got, Err(ParserPanic));
        assert_eq!(state, [1], "半截状态还在，所以之后一个字节都不能再喂");
    }

    /// panic 的内容绝不能被返回出去 —— 它会一路流进落库的审计记录。
    #[test]
    fn isolate_a_panic_carrying_payload_does_not_surface_the_message() {
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let got: Result<(), ParserPanic> = isolate(|| panic!("user prompt: my private question"));
        std::panic::set_hook(hook);
        assert_eq!(format!("{got:?}"), "Err(ParserPanic)");
    }
}
