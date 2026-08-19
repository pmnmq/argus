#!/usr/bin/env python3
"""日志敏感字段扫描（F01 R8 / 宪法 C27）。

宪法 C27：日志中**绝不**出现 prompt 内容、响应内容、tool arguments、任何 key。

这类泄漏几乎总是以 `tracing` 宏字段的形式出现：

    tracing::info!(prompt = %body, "forwarding");   # ← 就是这个

所以本脚本只做一件很窄但很可靠的事：解析 tracing 宏的**字段名**，
命中黑名单就失败。它不试图理解字段的值 —— 判断 `%x` 里到底是不是
prompt 需要类型信息，那是 review 的活。

用法：
    python3 ci/check_log_fields.py crates [更多路径...]

放行方式（需要写理由，会出现在 diff 里；只对紧邻其下 3 行内的宏生效）：
    // allow-log-field: message —— 这是 upstream 的错误消息，不是 AI messages
    tracing::error!(message = %err, "upstream failed");
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

# 会记录事件字段的 tracing 宏
MACROS = (
    "trace", "debug", "info", "warn", "error", "event",
    "span", "trace_span", "debug_span", "info_span", "warn_span", "error_span",
)

# --- 黑名单：字段名一旦叫这些，几乎不可能不是用户内容或凭据 -----------------
CONTENT_FIELDS = {
    "content", "contents", "body", "request_body", "response_body",
    "req_body", "resp_body", "payload", "raw", "raw_body", "raw_json",
    "prompt", "prompts", "system_prompt", "user_prompt", "instructions",
    "message", "messages", "text", "input", "output", "completion",
    "delta", "arguments", "args", "tool_arguments", "tool_args",
    "tool_input", "tool_result", "function_arguments", "thinking",
    "reasoning", "reasoning_content", "transcript",
}
CREDENTIAL_FIELDS = {
    "key", "api_key", "apikey", "secret", "secret_key", "token",
    "access_token", "refresh_token", "bearer", "password", "passwd",
    "credential", "credentials", "authorization", "auth_header",
    "master_key", "plaintext", "private_key", "salt", "signature",
}
BANNED = {name: "用户内容" for name in CONTENT_FIELDS}
BANNED.update({name: "凭据" for name in CREDENTIAL_FIELDS})

# --- 白名单：长得像但确实安全的字段（计数、标识符、长度） --------------------
ALLOWED = {
    # usage 计数：这是网关的核心指标，必须能记
    "input_tokens", "output_tokens", "total_tokens", "token_count",
    "prompt_tokens", "completion_tokens", "cache_read_tokens",
    "cache_write_tokens", "reasoning_tokens", "tokens_per_second",
    # 标识符 / 长度 / 状态，全部不含内容
    "key_id", "api_key_id", "cache_key", "idempotency_key",
    "content_length", "body_bytes", "payload_bytes", "message_count",
    "content_type", "input_kind", "output_kind", "delta_bytes",
    "tool_name", "tool_call_id", "arguments_len", "secret_len",
    "credential_id", "credential_ref", "auth_scheme", "auth_result",
    "text_len", "prompt_hash", "content_hash", "raw_len",
}

ALLOW_RE = re.compile(r"//\s*allow-log-field\s*:\s*([A-Za-z0-9_.]+)")
MACRO_RE = re.compile(
    r"(?:^|[^A-Za-z0-9_:])(?:tracing::)?(" + "|".join(MACROS) + r")!\s*\("
)
# 字段名可以带点（tracing 支持 `http.method = ...`）
NAMED_RE = re.compile(r"(?:^|[,(\s])([A-Za-z_][A-Za-z0-9_.]*)\s*=(?!=)")
SHORTHAND_RE = re.compile(r"(?:^|[,(])\s*[%?]([A-Za-z_][A-Za-z0-9_.]*)\s*(?=[,)])")


def strip_strings_and_comments(text: str) -> str:
    """把字符串字面量与注释替换成等长空白，避免在里面误匹配字段名。"""
    out = []
    i, n = 0, len(text)
    while i < n:
        ch = text[i]
        if ch == '"':
            out.append(" ")
            i += 1
            while i < n:
                if text[i] == "\\":
                    out.append("  ")
                    i += 2
                    continue
                if text[i] == '"':
                    out.append(" ")
                    i += 1
                    break
                out.append("\n" if text[i] == "\n" else " ")
                i += 1
            continue
        if ch == "/" and i + 1 < n and text[i + 1] == "/":
            while i < n and text[i] != "\n":
                out.append(" ")
                i += 1
            continue
        out.append(ch)
        i += 1
    return "".join(out)


def macro_body(text: str, open_paren: int) -> tuple[str, int]:
    """从 '(' 开始取到配对的 ')'，返回 (内部文本, 结束下标)。"""
    depth, i, n = 0, open_paren, len(text)
    while i < n:
        c = text[i]
        if c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
            if depth == 0:
                return text[open_paren + 1 : i], i
        i += 1
    return text[open_paren + 1 :], n


def top_level(fragment: str) -> str:
    """只保留嵌套深度为 0 的字符，避免把内层调用的参数当成字段。"""
    out, depth = [], 0
    for c in fragment:
        if c in "([{":
            depth += 1
            out.append(" ")
            continue
        if c in ")]}":
            depth -= 1
            out.append(" ")
            continue
        out.append(c if depth == 0 else " ")
    return "".join(out)


def scan(path: Path) -> list[tuple[int, str, str]]:
    src = path.read_text(encoding="utf-8", errors="replace")

    # 豁免是就近生效的：只覆盖注释下方 3 行内的宏。
    # 文件级豁免会让一条 `// allow-log-field: message` 悄悄放行整个文件，
    # 那正是这个护栏要防的那种慢性腐蚀。
    exempt: dict[int, set[str]] = {}
    for lineno, text in enumerate(src.splitlines(), start=1):
        for name in ALLOW_RE.findall(text):
            for off in range(0, 4):
                exempt.setdefault(lineno + off, set()).add(name)

    clean = strip_strings_and_comments(src)

    hits: list[tuple[int, str, str]] = []
    for m in MACRO_RE.finditer(clean):
        inner, _ = macro_body(clean, clean.index("(", m.end() - 1))
        flat = top_level(inner)
        line = clean.count("\n", 0, m.start()) + 1
        allowed_here = exempt.get(line, frozenset())
        names = set(NAMED_RE.findall(flat)) | set(SHORTHAND_RE.findall(flat))
        for name in sorted(names):
            leaf = name.rsplit(".", 1)[-1].lower()
            if name in allowed_here or leaf in allowed_here:
                continue
            if leaf in ALLOWED or name.lower() in ALLOWED:
                continue
            reason = BANNED.get(leaf)
            if reason:
                hits.append((line, name, reason))
    return hits


def main(argv: list[str]) -> int:
    roots = [Path(a) for a in argv[1:]] or [Path("crates")]
    files = sorted(
        f for r in roots for f in ([r] if r.is_file() else r.rglob("*.rs"))
        if "target" not in f.parts
    )

    findings = [(f, *h) for f in files for h in scan(f)]
    if not findings:
        print(f"  ok   已扫描 {len(files)} 个 .rs 文件，tracing 字段名无敏感项")
        return 0

    print("")
    print(f"FAIL [C27] tracing 字段名命中敏感黑名单（{len(findings)} 处）")
    for path, line, name, reason in findings:
        print(f"      {path}:{line}  字段 `{name}` —— {reason}")
    print("      → 日志里不许出现 prompt/响应/tool arguments/任何 key。")
    print("      → 需要可观测性就记**元数据**：长度、哈希、计数、标识符。")
    print("      → 确属误报时，在该行上方 3 行内写 `// allow-log-field: <字段名> —— <理由>`。")
    print("      → 条款原文：specs/constitution.md  (搜索 '### C27 ')")
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
