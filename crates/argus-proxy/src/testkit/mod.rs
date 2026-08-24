//! 测试与压测的公共设施。**非默认 feature，不会进 release 二进制。**
//!
//! 为什么放在 `src/` 而不是 `tests/`：它有三个调用方 —— 集成测试、
//! `examples/` 里的压测驱动、以及将来 F21 的混沌用例。`tests/` 下的模块
//! 只有同目录的测试二进制能用，剩下两个只能各抄一份。
//!
//! 为什么不单开一个 `argus-testkit` crate：宪法 C32 把 crate 集合钉死在
//! 五个，新增要先在 `docs/19` §4 给出判据。一个测试工具箱不满足那四条
//! 判据里的任何一条。
//!
//! 设计依据：`docs/20-testing-strategy.md` §8、`docs/21-performance.md` §5.3。

pub mod provider;
pub mod scenario;
pub mod tls;

pub use self::{
    provider::{MockProvider, RecordedRequest, Transport},
    scenario::{Ending, Pacing, Plan, PlanError, Scenario, Step},
    tls::TestCert,
};
