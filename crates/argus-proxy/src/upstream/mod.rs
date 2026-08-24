//! 上游客户端：发请求、拿 header，**不碰 body 的一个字节**。
//!
//! 用 `hyper_util` 的 legacy `Client` 而不是 `reqwest`（`docs/06` §13-A）：
//! reqwest 会自动跟随重定向、自动解压、自动 retry —— 对透明代理这三条**全是
//! 负面的**。自动解压会改变字节（宪法 C1 直接破了），重定向会跳到未经判定的
//! host（R03 的判定链绕过去了），而它把 body 包成自己的类型之后取消语义
//! 也不透明了。
//!
//! 设计依据：`docs/06-data-plane.md` §6.3–6.4、§7、§10。

pub mod headers;

pub use self::headers::{STRIP_FROM_CLIENT, build_upstream_headers};
