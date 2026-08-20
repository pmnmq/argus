//! Provider 凭据在内存中的形态。
//!
//! 设计依据：`docs/07-control-plane.md` §7.3、宪法 C27。

use core::fmt;

use http::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::ids::CredentialId;

/// 凭据的形态，决定它怎么变成一个 header。
#[non_exhaustive]
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialKind {
    /// `Authorization: Bearer <k>` —— OpenAI 及大多数兼容服务
    Bearer,
    /// 自定义 header 名承载裸 key —— Azure 的 `api-key`、Anthropic 的 `x-api-key`
    ApiKeyHeader,
    /// AWS SigV4：没有静态 header，签名在发起请求时按请求内容计算
    AwsSigV4,
    /// OAuth2 Bearer，token 会过期，需要刷新（Vertex）
    OAuth2Bearer,
}

impl CredentialKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bearer => "bearer",
            Self::ApiKeyHeader => "api_key_header",
            Self::AwsSigV4 => "aws_sigv4",
            Self::OAuth2Bearer => "oauth2_bearer",
        }
    }

    /// 是否可以预构造成一个固定 header。SigV4 不行 —— 它每个请求都不一样。
    #[must_use]
    pub const fn is_static_header(self) -> bool {
        matches!(self, Self::Bearer | Self::ApiKeyHeader | Self::OAuth2Bearer)
    }
}

/// 构造 `ResolvedCredential` 失败。
///
/// 刻意不携带任何原始内容：这个错误会被打印。
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InvalidCredential {
    #[error("credential value is empty")]
    Empty,
    #[error("credential value contains bytes that are not valid in an HTTP header")]
    NotAHeaderValue,
}

/// 已解密、可直接使用的 provider 凭据。
///
/// # 为什么存 `Zeroizing<Vec<u8>>` 而不是 `HeaderValue`
///
/// `docs/07` §7.3 建议直接存预构造好的 `HeaderValue`，理由是每请求 `clone()`
/// 只是引用计数、不分配。但 `HeaderValue` 内部是 `Bytes`，**没有可变访问**，
/// 意味着 Drop 时无法清零 —— 轮换掉的 key 会一直躺在堆上直到分配器复用。
///
/// 这里选择自己持有可清零的缓冲，代价是每个上游请求多一次约 60 字节的
/// 分配 + memcpy（`header_value()`）。注意这是**每请求**一次，不是每帧一次，
/// 相对于一次 TLS 往返可以忽略；而 §7.3 真正想避免的
/// `format!("Bearer {}", key)`（格式化 + 分配）已经在构造期做掉了。
///
/// 如果 F07 的基线显示这一次分配确实可见，再回来重新权衡。
pub struct ResolvedCredential {
    pub id: CredentialId,
    pub kind: CredentialKind,
    header_name: HeaderName,
    /// 完整的 header 值，**含 `Bearer ` 之类的前缀**。Drop 时清零。
    value: Zeroizing<Vec<u8>>,
}

impl ResolvedCredential {
    /// 构造并校验。校验放在这里，`header_value()` 就不会失败。
    ///
    /// # Errors
    /// 值为空，或含有不能出现在 HTTP header 里的字节。
    pub fn new(
        id: CredentialId,
        kind: CredentialKind,
        header_name: HeaderName,
        value: impl Into<Vec<u8>>,
    ) -> Result<Self, InvalidCredential> {
        let value = Zeroizing::new(value.into());
        if value.is_empty() {
            return Err(InvalidCredential::Empty);
        }
        // 只为校验，构造出来的 HeaderValue 立刻丢弃
        HeaderValue::from_bytes(&value).map_err(|_| InvalidCredential::NotAHeaderValue)?;
        Ok(Self {
            id,
            kind,
            header_name,
            value,
        })
    }

    #[must_use]
    pub const fn header_name(&self) -> &HeaderName {
        &self.header_name
    }

    /// 构造本次请求要用的 header 值。
    ///
    /// 结果被标为 sensitive：HTTP/2 下 hyper 因此不会把它放进 HPACK 动态表。
    /// 凭据被索引后会在连接上长期驻留，还可能被压缩侧信道观测到。
    #[must_use]
    pub fn header_value(&self) -> HeaderValue {
        // new() 已经校验过，这里不可能失败。真要失败也返回一个合法的空值：
        // 上游会回 401，是一个响亮但安全的失败，好过 panic。
        let mut v =
            HeaderValue::from_bytes(&self.value).unwrap_or_else(|_| HeaderValue::from_static(""));
        v.set_sensitive(true);
        v
    }

    /// 明文长度。用于指标与自检，不泄漏任何内容。
    #[must_use]
    pub fn secret_len(&self) -> usize {
        self.value.len()
    }
}

// Debug 与 Display 都必须是 <redacted>。
// 这两个实现是手写的而不是 derive 的 —— 一旦有人给这个 struct 加上
// #[derive(Debug)]，编译器会因为重复实现而报错，而不是安静地开始打印密钥。
impl fmt::Debug for ResolvedCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedCredential")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("header_name", &self.header_name)
            .field("value", &"<redacted>")
            .finish()
    }
}

impl fmt::Display for ResolvedCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "sk-live-9f3a2b71c0d84e5fa6b7c8d9e0f1a2b3";

    fn credential() -> ResolvedCredential {
        ResolvedCredential::new(
            CredentialId::generate(),
            CredentialKind::Bearer,
            HeaderName::from_static("authorization"),
            format!("Bearer {SECRET}"),
        )
        .expect("固定的合法值")
    }

    fn assert_no_secret(rendered: &str) {
        assert!(!rendered.contains(SECRET), "凭据明文泄漏：{rendered}");
        assert!(!rendered.contains("9f3a2b71"), "凭据片段泄漏：{rendered}");
        assert!(
            !rendered.contains("Bearer sk-"),
            "带前缀的凭据泄漏：{rendered}"
        );
    }

    /// 宪法 C27：凭据不得出现在任何输出里。
    ///
    /// `Debug` 是手写的。如果有人给 `ResolvedCredential` 加上 `#[derive(Debug)]`，
    /// 编译期就会因为重复实现而报错 —— 而不是安静地开始把密钥打进日志。
    #[test]
    fn debug_credential_holding_a_secret_renders_redacted() {
        let rendered = format!("{:?}", credential());
        assert_no_secret(&rendered);
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn display_credential_holding_a_secret_renders_only_redacted() {
        let rendered = format!("{}", credential());
        assert_no_secret(&rendered);
        assert_eq!(rendered, "<redacted>");
    }

    /// 实际泄漏几乎都是这样发生的：没人直接打印 credential，
    /// 而是打印了包含它的更大的结构。
    #[test]
    fn debug_credential_nested_in_another_structure_renders_redacted() {
        let wrapper = (credential(), "context");
        assert_no_secret(&format!("{wrapper:?}"));
    }

    #[test]
    fn header_value_valid_credential_returns_the_original_bytes() {
        let v = credential().header_value();
        assert_eq!(v.as_bytes(), format!("Bearer {SECRET}").as_bytes());
    }

    /// sensitive 会阻止 hyper 把它放进 HTTP/2 的 HPACK 动态表 ——
    /// 被索引的凭据会在连接上长期驻留，还可能被压缩侧信道观测到。
    #[test]
    fn header_value_any_credential_marks_the_value_sensitive() {
        let v = credential().header_value();
        assert!(v.is_sensitive());
        assert!(!format!("{v:?}").contains(SECRET));
    }

    #[test]
    fn new_value_is_empty_returns_empty_error() {
        assert_eq!(
            ResolvedCredential::new(
                CredentialId::generate(),
                CredentialKind::Bearer,
                HeaderName::from_static("authorization"),
                Vec::new(),
            )
            .unwrap_err(),
            InvalidCredential::Empty
        );
    }

    /// 换行符会造成 header 注入。
    #[test]
    fn new_value_contains_crlf_returns_not_a_header_value_error() {
        assert_eq!(
            ResolvedCredential::new(
                CredentialId::generate(),
                CredentialKind::Bearer,
                HeaderName::from_static("authorization"),
                "Bearer x\r\nX-Injected: 1",
            )
            .unwrap_err(),
            InvalidCredential::NotAHeaderValue
        );
    }

    #[test]
    fn display_invalid_credential_error_contains_no_secret_fragment() {
        assert!(
            !InvalidCredential::NotAHeaderValue
                .to_string()
                .contains("sk-")
        );
    }

    #[test]
    fn secret_len_bearer_credential_returns_full_header_value_length() {
        assert_eq!(credential().secret_len(), SECRET.len() + "Bearer ".len());
    }

    /// SigV4 每个请求的签名都不一样，没法预构造成固定 header。
    #[test]
    fn is_static_header_kind_is_aws_sigv4_returns_false() {
        assert!(!CredentialKind::AwsSigV4.is_static_header());
    }

    #[test]
    fn is_static_header_kind_is_header_based_returns_true() {
        for k in [
            CredentialKind::Bearer,
            CredentialKind::ApiKeyHeader,
            CredentialKind::OAuth2Bearer,
        ] {
            assert!(k.is_static_header(), "{} 应该可预构造", k.as_str());
        }
    }
}
