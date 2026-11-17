#!/usr/bin/env python3
"""把 git tag 的版本号同步写入 Cargo.toml 与 tauri.conf.json（CI release 构建调用）。

用法: python3 scripts/sync-version.py v0.1.21
tag 前缀 v 会被剥掉；版本串不合法时跳过（不阻断构建）。

写入路径锚定仓库根并做包含校验（Mimosa 路径穿越规则：变量路径写文件必须
canonicalize + 白名单校验；本脚本的路径虽是常量，仍按规范显式自证）。
"""
import json
import re
import sys
from pathlib import Path

# 仓库根 = 本脚本 parent 的 parent（scripts/ 在根下）
ROOT = Path(__file__).resolve().parent.parent


def _inside_root(p: Path) -> bool:
    """解析后的路径必须落在仓库根内（防符号链接/相对段逃逸）。"""
    resolved = Path(p).resolve()
    return resolved == ROOT or ROOT in resolved.parents


def _sync(rel_parts: list, mutate) -> int:
    target = (ROOT.joinpath(*rel_parts)).resolve()
    if not _inside_root(target):
        print(f"sync-version: resolved path escapes repo root: {target}, skip")
        return 1
    return mutate(target)


def main() -> int:
    tag = sys.argv[1].strip() if len(sys.argv) > 1 else ""
    version = re.sub(r"^v", "", tag)
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:[-+].+)?", version):
        # 输出保持纯 ASCII：Windows runner 控制台为 cp1252，非 ASCII 会 UnicodeEncodeError
        print(f"sync-version: invalid tag version {tag!r}, skip")
        return 0

    def cargo(target: Path) -> int:
        text = target.read_text(encoding="utf-8")
        text, n = re.subn(r'(?m)^version = "[^"]+"', f'version = "{version}"', text, count=1)
        if n != 1:
            print("sync-version: package version line not found in Cargo.toml")
            return 1
        target.write_text(text, encoding="utf-8")
        return 0

    def conf(target: Path) -> int:
        conf = json.loads(target.read_text(encoding="utf-8"))
        conf["version"] = version
        with target.open("w", encoding="utf-8") as f:
            json.dump(conf, f, indent=2, ensure_ascii=False)
            f.write("\n")
        return 0

    rc = _sync(["src-tauri", "Cargo.toml"], cargo)
    if rc != 0:
        return rc
    rc = _sync(["src-tauri", "tauri.conf.json"], conf)
    if rc != 0:
        return rc

    print(f"sync-version: synced -> {version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
