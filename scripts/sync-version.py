#!/usr/bin/env python3
"""把 git tag 的版本号同步写入 Cargo.toml 与 tauri.conf.json（CI release 构建调用）。

用法: python3 scripts/sync-version.py v0.1.21
tag 前缀 v 会被剥掉；版本串不合法时跳过（不阻断构建）。
"""
import json
import re
import sys


def main() -> int:
    tag = sys.argv[1].strip() if len(sys.argv) > 1 else ""
    version = re.sub(r"^v", "", tag)
    if not re.fullmatch(r"\d+\.\d+\.\d+(?:[-+].+)?", version):
        # 输出保持纯 ASCII：Windows runner 控制台为 cp1252，非 ASCII 会 UnicodeEncodeError
        print(f"sync-version: invalid tag version {tag!r}, skip")
        return 0

    cargo_path = "src-tauri/Cargo.toml"
    text = open(cargo_path, encoding="utf-8").read()
    text, n = re.subn(r'(?m)^version = "[^"]+"', f'version = "{version}"', text, count=1)
    if n != 1:
        print("sync-version: package version line not found in Cargo.toml")
        return 1
    open(cargo_path, "w", encoding="utf-8").write(text)

    conf_path = "src-tauri/tauri.conf.json"
    with open(conf_path, encoding="utf-8") as f:
        conf = json.load(f)
    conf["version"] = version
    with open(conf_path, "w", encoding="utf-8") as f:
        json.dump(conf, f, indent=2, ensure_ascii=False)
        f.write("\n")

    print(f"sync-version: synced -> {version}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
