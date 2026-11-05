#!/bin/bash
# StudyOnTV 一键安装助手 - Linux 一键编译脚本
set -e
cd "$(dirname "$0")"
if ! command -v cargo >/dev/null 2>&1; then
  echo "[错误] 没找到 cargo。请先安装 Rust： curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
  exit 1
fi
cargo build --release
mkdir -p dist
cp target/release/studyontv-installer "dist/StudyOnTV安装助手"
chmod +x "dist/StudyOnTV安装助手"
echo "完成！可执行文件在 dist/StudyOnTV安装助手，双击或终端运行即可。"
