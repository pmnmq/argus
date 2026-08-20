//! Resolver 的性质测试（R02 T7）。
//!
//! 两条性质：
//!
//! 1. **对任意输入不 panic**（R11）。Resolver 处理的是完全不受控的客户端
//!    输入，而 panic 在这一层等于把一个畸形 URL 变成一次可用的拒绝服务。
//! 2. **两种 Raw 形式产出同一个 target**（R8）。这两条写法的存在理由就是
//!    「换个部署方式也能用」，一旦它们分叉，表现是「过 nginx 之后打到了
//!    别的地方」—— 没有比这更难查的 bug。

mod common;

mod resolver_properties {
    use argus_core::ProxyError;
    use argus_proxy::ingress::{
        detect_raw_form, is_valid_profile_name, resolve_target, split_first_segment,
    };
    use http::Request;
    use proptest::prelude::*;

    use crate::common::TestProfiles;

    /// 试着按 request-target 造 `Parts` 并解析，造不出来就返回 `None`。
    ///
    /// hyper 只会把**它自己认得的** request-target 交上来，所以造不出来的
    /// 那些输入本来也到不了 Resolver。
    fn try_resolve(target: &str) -> Option<Result<String, ProxyError>> {
        let parts = Request::builder().uri(target).body(()).ok()?.into_parts().0;
        let profiles = TestProfiles::one("openai", "https://api.openai.com");
        Some(resolve_target(&parts, &profiles).map(|t| t.uri.to_string()))
    }

    proptest! {
        /// 纯函数层：任意字节都不能 panic，切分也不能凭空造出或丢掉字节。
        #[test]
        fn split_first_segment_arbitrary_bytes_never_panics(
            bytes in prop::collection::vec(any::<u8>(), 0..512),
        ) {
            let s = String::from_utf8_lossy(&bytes);
            if let Some((head, rest)) = split_first_segment(&s) {
                prop_assert_eq!(head.len() + rest.len(), s.len());
                prop_assert!(rest.starts_with('/'));
                prop_assert!(!head.contains('/'));
            }
        }

        /// 合法就意味着「原样可以当 path 段用」：不含 `/`，也不含 `%`。
        #[test]
        fn is_valid_profile_name_arbitrary_bytes_never_panics(
            bytes in prop::collection::vec(any::<u8>(), 0..512),
        ) {
            let s = String::from_utf8_lossy(&bytes);
            if is_valid_profile_name(&s) {
                prop_assert!(!s.contains('/'));
                prop_assert!(!s.contains('%'));
                prop_assert!(s.len() <= 63);
            }
        }

        #[test]
        fn detect_raw_form_arbitrary_bytes_never_panics(
            bytes in prop::collection::vec(any::<u8>(), 0..512),
        ) {
            let s = String::from_utf8_lossy(&bytes);
            let _ = detect_raw_form(&s);
        }

        /// 整条链路：任意可解析的 request-target 都不能 panic。
        #[test]
        fn resolve_target_arbitrary_printable_paths_never_panics(tail in "[ -~]{0,120}") {
            for prefix in ["/provider/", "/raw/", "/", ""] {
                let _ = try_resolve(&format!("{prefix}{tail}"));
            }
        }

        /// R8：同一个 target 的两种写法产出 byte-identical 的 uri。
        #[test]
        fn resolve_target_both_raw_forms_agree_on_arbitrary_targets(
            host in "[a-z][a-z0-9.-]{0,30}",
            port in prop::option::of(1_u16..=65535),
            path in "(/[A-Za-z0-9%._~-]{0,12}){0,5}",
            query in prop::option::of("[A-Za-z0-9=&%_-]{0,40}"),
        ) {
            for scheme in ["http", "https"] {
                let authority = port.map_or_else(|| host.clone(), |p| format!("{host}:{p}"));
                let suffix = query.as_ref().map_or_else(String::new, |q| format!("?{q}"));

                let direct = format!("/raw/{scheme}://{authority}{path}{suffix}");
                let segmented = format!("/raw/{scheme}/{authority}{path}{suffix}");

                let (Some(a), Some(b)) = (try_resolve(&direct), try_resolve(&segmented)) else {
                    continue;
                };
                prop_assert_eq!(a, b, "两种形式分叉了：{} vs {}", direct, segmented);
            }
        }
    }

    /// 从表里按伪随机数挑一格。`% len` 之后一定落在 `usize` 范围内。
    fn pick<T: Copy>(table: &[T], r: u64) -> T {
        let idx = usize::try_from(r % table.len() as u64).unwrap_or(0);
        table[idx]
    }

    /// `specs/R02` §7 点名的那条：10 万随机 path 不 panic。
    ///
    /// 用固定种子而不是随机的 —— 失败时能原样复现。
    ///
    /// 随机串会拼在**真实前缀**后面（`openai/`、`https://`、`https/`…）而不是
    /// 纯随机：纯随机字节几乎构不成一条走得进解析分支的 path，那样跑十万次
    /// 也只是在反复验证「404 不 panic」。末尾的断言就是防止悄悄退化成那样。
    #[test]
    fn resolve_target_one_hundred_thousand_random_paths_never_panics() {
        const INTERESTING: &[u8] = b"/%:.[]@-_abcdehnoprstuvwxyz0123456789";
        const SHAPES: [&str; 8] = [
            "/provider/",
            "/provider/openai/",
            "/provider/openai/v1/",
            "/raw/",
            "/raw/https://",
            "/raw/https/",
            "/raw/https://api.openai.com/",
            "/",
        ];

        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        let mut resolved = 0_u32;
        for _ in 0..100_000_u32 {
            let len = usize::try_from(next() % 48).unwrap_or(0);
            let tail: String = (0..len).map(|_| char::from(pick(INTERESTING, next()))).collect();

            let shape = pick(&SHAPES, next());
            if matches!(try_resolve(&format!("{shape}{tail}")), Some(Ok(_))) {
                resolved += 1;
            }
        }

        // 光「不 panic」还不够：一条都没解析成功的话，这个测试就是在空转
        assert!(
            resolved > 1_000,
            "只有 {resolved} 条解析成功，随机 path 的字节表大概是废了"
        );
    }
}
