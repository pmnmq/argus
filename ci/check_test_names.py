#!/usr/bin/env python3
"""测试命名约定检查（`docs/15-testing-strategy.md` §4.1）。

统一格式：

    [被测方法名]_[前置条件/测试场景]_[期望结果]

机器只能检查它的形状（分段数、没有 `test_` 前缀），检查不了「第一段真的是
被测方法名」——那部分靠 review。但光是形状就能拦住 `test_retry` / `works`
这类失败时什么都说不出来的名字。

用法：
    python3 ci/check_test_names.py crates [更多路径...]

放行方式（需要写理由，会出现在 diff 里；只对紧邻其下 3 行内的测试生效）：
    // allow-test-name: 这是 golden 驱动器，用例名由 fixture 目录生成
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

TEST_ATTRS = ("#[test]", "#[tokio::test", "#[rstest]", "#[test_case")
# 至少三段：a_b_c。段内可以有数字。
SHAPE = re.compile(r"^[a-z][a-z0-9]*(?:_[a-z0-9]+){2,}$")
FN = re.compile(r"^\s*(?:pub\s+)?(?:async\s+)?fn\s+([A-Za-z0-9_]+)")
ALLOW = re.compile(r"//\s*allow-test-name\b")
# golden 驱动器：用例名由 fixture 目录生成，不适用本约定
GOLDEN = re.compile(r"#\[files\(")


def scan(path: Path) -> list[tuple[int, str, str]]:
    lines = path.read_text(encoding="utf-8", errors="replace").splitlines()

    exempt: set[int] = set()
    for i, text in enumerate(lines, start=1):
        if ALLOW.search(text):
            exempt.update(range(i, i + 4))

    hits: list[tuple[int, str, str]] = []
    for i, text in enumerate(lines):
        if not text.lstrip().startswith(TEST_ATTRS):
            continue
        # 属性和 fn 之间可能还有别的属性行
        for j in range(i + 1, min(i + 8, len(lines))):
            if GOLDEN.search(lines[j]):
                break
            m = FN.match(lines[j])
            if not m:
                continue
            name, lineno = m.group(1), j + 1
            if lineno in exempt:
                break
            if name.startswith("test_"):
                hits.append((lineno, name, "`test_` 前缀没有信息量，去掉它"))
            elif not SHAPE.match(name):
                hits.append(
                    (lineno, name, "至少要有三段：[被测方法]_[场景]_[期望结果]")
                )
            break
    return hits


def main(argv: list[str]) -> int:
    roots = [Path(a) for a in argv[1:]] or [Path("crates")]
    files = sorted(
        f
        for r in roots
        for f in ([r] if r.is_file() else r.rglob("*.rs"))
        if "target" not in f.parts
    )

    findings = [(f, *h) for f in files for h in scan(f)]
    total = sum(1 for f in files for _ in [0])
    if not findings:
        print(f"  ok   已扫描 {total} 个 .rs 文件，测试命名符合 docs/15 §4.1")
        return 0

    print("")
    print(f"FAIL [docs/15 §4.1] 测试命名不符合约定（{len(findings)} 处）")
    for path, line, name, why in findings:
        print(f"      {path}:{line}  `{name}` —— {why}")
    print("      → 格式：[被测方法名]_[前置条件/测试场景]_[期望结果]")
    print("      → 例：is_retryable_stage_is_s3_committed_returns_false")
    print("      → 约定原文：docs/15-testing-strategy.md §4.1")
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
