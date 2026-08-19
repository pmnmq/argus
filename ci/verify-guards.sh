#!/usr/bin/env bash
# 护栏的护栏（F01 T10 / spec §7）。
#
# 对每一条架构护栏做：注入违规 → 断言它真的失败 → 回滚。
#
#   没验证过的护栏等于没有护栏。
#
# 一条 grep 写错了字符类、一条 cargo tree 换了输出格式，护栏就会安静地永远
# 返回 0。后面 27 个 spec 全都建立在"这些检查真的会拦"之上，所以这一步不能跳过。
#
# 用法： ./ci/verify-guards.sh
# 退出码：0 = 每条护栏都按预期拦截；1 = 至少一条护栏形同虚设
set -uo pipefail

cd "$(dirname "$0")/.."
ROOT="$PWD"

BACKUP="$(mktemp -d)"
FAKE="$(mktemp -d)"
PASS=0
FAIL=0

snapshot() { cp -a Cargo.toml Cargo.lock crates clippy.toml "$BACKUP/"; }
restore() {
  rm -rf "$ROOT/crates"
  cp -a "$BACKUP/crates"      "$ROOT/crates"
  cp -a "$BACKUP/Cargo.toml"  "$ROOT/Cargo.toml"
  cp -a "$BACKUP/Cargo.lock"  "$ROOT/Cargo.lock"
  cp -a "$BACKUP/clippy.toml" "$ROOT/clippy.toml"
}
cleanup() { restore; rm -rf "$BACKUP" "$FAKE"; }
trap cleanup EXIT INT TERM

# 把违规依赖插进 [dependencies] 段。
# 直接 >> 追加会落到文件末尾的 [dev-dependencies] 里，而 `cargo tree -e normal`
# 根本不看 dev 依赖 —— 那样注入等于没注入，护栏会被误判成"没拦住"。
add_dep() {  # add_dep <crate 名> <依赖声明行>
  python3 - "$1" "$2" <<'PYEOF'
import sys, pathlib
p = pathlib.Path("crates") / sys.argv[1] / "Cargo.toml"
s = p.read_text()
assert "[dependencies]\n" in s, f"{p} 没有 [dependencies] 段"
p.write_text(s.replace("[dependencies]\n", "[dependencies]\n" + sys.argv[2] + "\n", 1))
PYEOF
}

# run_case <用例名> <期望在输出中出现的串> -- <命令...>
run_case() {
  local name="$1" expect="$2"; shift 3   # 丢掉 '--'
  local out rc
  out="$("$@" 2>&1)"; rc=$?
  if [ "$rc" -eq 0 ]; then
    printf '  ✗ %-44s 护栏没拦住（命令返回 0）\n' "$name"
    FAIL=$((FAIL + 1))
  # 必须用 -F：期望串形如 "FAIL [C6]"，正则会把 [C6] 当成字符类，永远匹配不上
  elif ! printf '%s' "$out" | grep -qF -- "$expect"; then
    printf '  ✗ %-44s 拦住了，但输出不含 %s\n' "$name" "$expect"
    printf '%s\n' "$out" | tail -12 | sed 's/^/        /'
    FAIL=$((FAIL + 1))
  else
    printf '  ✓ %-44s 拦截，输出含 %s\n' "$name" "$expect"
    PASS=$((PASS + 1))
  fi
  restore
}

INV=./ci/check-invariants.sh

echo "=== 0. 基线：干净仓库必须全绿 ==="
if ! "$INV" > "$FAKE/baseline.log" 2>&1; then
  echo "  ✗ 干净仓库上 check-invariants.sh 就失败了，先修它："
  tail -20 "$FAKE/baseline.log" | sed 's/^/      /'
  exit 1
fi
echo "  ✓ 基线通过"
snapshot

echo ""
echo "=== 1. 依赖方向（C6） ==="

# 给 argus-proxy 加 sqlx
add_dep argus-proxy 'sqlx = { workspace = true }'
run_case "argus-proxy 依赖 sqlx" "FAIL [C6]" -- "$INV"

# 给 argus-proxy 加 argus-storage
add_dep argus-proxy 'argus-storage = { workspace = true }'
run_case "argus-proxy 依赖 argus-storage" "FAIL [C6]" -- "$INV"

# 给 argus-protocol 加 tokio
add_dep argus-protocol 'tokio = { workspace = true }'
run_case "argus-protocol 依赖 tokio" "FAIL [C6]" -- "$INV"

# 给 argus-core 加内部 crate（反向依赖 → 循环）
add_dep argus-core 'argus-protocol = { workspace = true }'
run_case "argus-core 反向依赖 argus-protocol" "FAIL [C6]" -- "$INV"

echo ""
echo "=== 2. SDK 禁入运行时（C4） ==="
# 造一个真名叫 async-openai 的本地 crate：不联网，且 cargo tree 里就是这个包名。
mkdir -p "$FAKE/async-openai/src"
cat > "$FAKE/async-openai/Cargo.toml" <<EOF
[package]
name = "async-openai"
version = "0.1.0"
edition = "2021"
EOF
echo "pub fn stub() {}" > "$FAKE/async-openai/src/lib.rs"
add_dep argus-proxy "async-openai = { path = \"$FAKE/async-openai\" }"
run_case "argus-proxy 运行时依赖 async-openai" "FAIL [C4]" -- "$INV"

echo ""
echo "=== 3. panic 策略（C7） ==="
sed -i.bak 's/panic         = "unwind"/panic         = "abort"/' Cargo.toml && rm -f Cargo.toml.bak
run_case 'profile.release 改成 panic = "abort"' "FAIL [C7]" -- "$INV"

# 删掉显式声明也要拦（默认值恰好也是 unwind，但不写出来 = 没人知道它不能改）
sed -i.bak '/panic         = "unwind"/d' Cargo.toml && rm -f Cargo.toml.bak
run_case 'profile.release 删掉 panic 声明'  "FAIL [C7]" -- "$INV"

echo ""
echo "=== 4. 无界队列（C9） ==="
cat >> crates/argus-proxy/src/lib.rs <<'EOF'

pub fn leaky() {
    let (_tx, _rx) = tokio::sync::mpsc::unbounded_channel::<u8>();
}
EOF
run_case "写了 mpsc::unbounded_channel()" "FAIL [C9]" -- "$INV"

echo ""
echo "=== 5. unsafe（C8） ==="
# 5a. grep 层：任意 crate
cat >> crates/argus-storage/src/lib.rs <<'EOF'

pub fn sneaky() {
    let x = 1u8;
    let _p = unsafe { *(&x as *const u8) };
}
EOF
run_case "argus-storage 里写 unsafe{}（grep 层）" "FAIL [C8]" -- "$INV"

# 5b. 编译层：#![forbid(unsafe_code)] 必须真的生效
cat >> crates/argus-protocol/src/lib.rs <<'EOF'

pub fn sneaky() -> u8 {
    let x = 1u8;
    unsafe { *(&x as *const u8) }
}
EOF
run_case "argus-protocol 里写 unsafe{}（编译层）" "unsafe_code" -- \
  cargo build -p argus-protocol --offline

echo ""
echo "=== 6. argus-protocol 的编译期 deny（C7 后半） ==="
cat >> crates/argus-protocol/src/lib.rs <<'EOF'

pub fn risky(v: Option<u8>) -> u8 {
    v.unwrap()
}
EOF
run_case "argus-protocol 里写 .unwrap()" "unwrap_used" -- \
  cargo clippy -p argus-protocol --all-targets --offline -- -D warnings

cat >> crates/argus-protocol/src/lib.rs <<'EOF'

pub fn risky(v: &[u8]) -> u8 {
    v[0]
}
EOF
run_case "argus-protocol 里写切片索引" "indexing_slicing" -- \
  cargo clippy -p argus-protocol --all-targets --offline -- -D warnings

cat >> crates/argus-protocol/src/lib.rs <<'EOF'

pub fn risky(v: Option<u8>) -> u8 {
    v.expect("总会有值的")
}
EOF
run_case "argus-protocol 里写 .expect()" "expect_used" -- \
  cargo clippy -p argus-protocol --all-targets --offline -- -D warnings

cat >> crates/argus-protocol/src/lib.rs <<'EOF'

pub fn risky(v: u8) -> u8 {
    if v == 0 {
        panic!("unreachable in practice");
    }
    v
}
EOF
run_case "argus-protocol 里写 panic!()" "clippy::panic" -- \
  cargo clippy -p argus-protocol --all-targets --offline -- -D warnings

echo ""
echo "=== 7. 日志敏感字段（C27） ==="
cat >> crates/argus-proxy/src/lib.rs <<'EOF'

pub fn logs(body: &str) {
    tracing::info!(prompt = %body, "forwarding");
}
EOF
run_case "tracing::info!(prompt = %body)" "FAIL [C27]" -- "$INV"

cat >> crates/argus-proxy/src/lib.rs <<'EOF'

pub fn logs2(k: &str) {
    tracing::warn!(%api_key, "rejected {}", k);
}
EOF
run_case "tracing::warn!(%api_key)" "FAIL [C27]" -- "$INV"

echo ""
echo "=== 8. 主链路不等待旁路（C5） ==="
cat > crates/argus-proxy/src/tee.rs <<'EOF'
// 故意写错：主链路只能 try_send
pub async fn offer(tx: &tokio::sync::mpsc::Sender<u8>, b: u8) {
    let _ = tx.send(b).await;
}
EOF
run_case "tee.rs 里写 .send().await" "FAIL [C5]" -- "$INV"

echo ""
echo "=== 8b. 测试命名约定（docs/15 §4.1） ==="
cat >> crates/argus-core/src/tee.rs <<'EOF'

#[cfg(test)]
mod naming_probe {
    #[test]
    fn works() {}
}
EOF
run_case "测试起名叫 works()" "FAIL [docs/15 §4.1]" -- "$INV"

cat >> crates/argus-core/src/tee.rs <<'EOF'

#[cfg(test)]
mod naming_probe {
    #[test]
    fn test_retry() {}
}
EOF
run_case "测试名带 test_ 前缀" "FAIL [docs/15 §4.1]" -- "$INV"

echo ""
echo "=== 9. 拆 crate 需要理由（C32） ==="
mkdir -p crates/argus-bogus/src
cat > crates/argus-bogus/Cargo.toml <<'EOF'
[package]
name = "argus-bogus"
version = "0.1.0"
edition = "2024"
EOF
echo "// 未经论证的新 crate" > crates/argus-bogus/src/lib.rs
run_case "凭空新增 crates/argus-bogus" "FAIL [C32]" -- "$INV"

echo ""
echo "=== 10. 回滚后必须仍然全绿 ==="
if "$INV" > "$FAKE/after.log" 2>&1; then
  echo "  ✓ 回滚干净"
  PASS=$((PASS + 1))
else
  echo "  ✗ 回滚不干净，仓库被污染了："
  tail -20 "$FAKE/after.log" | sed 's/^/      /'
  FAIL=$((FAIL + 1))
fi

echo ""
echo "==============================================="
printf '护栏验证：%d 通过，%d 失败\n' "$PASS" "$FAIL"
if [ "$FAIL" -gt 0 ]; then
  echo "有护栏形同虚设 —— 在它被修好之前，不要开始 F02。"
  exit 1
fi
echo "所有护栏均已验证会真的拦截。"
