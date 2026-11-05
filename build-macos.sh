#!/bin/bash
# StudyOnTV 一键安装助手 - macOS 一键编译脚本
# 需要先安装 Rust: 在终端执行  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
set -e
cd "$(dirname "$0")"
if ! command -v cargo >/dev/null 2>&1; then
  echo "[错误] 没找到 cargo。请先安装 Rust："
  echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
  exit 1
fi
echo "正在编译（第一次较慢，请耐心等待）..."
cargo build --release
mkdir -p dist
cp target/release/studyontv-installer "dist/StudyOnTV安装助手"
chmod +x "dist/StudyOnTV安装助手"
echo
echo "完成！可执行文件在 dist/StudyOnTV安装助手"
echo "使用前请把 Android platform-tools 的 adb 放到 dist 文件夹里（或 platform-tools 子文件夹），或确保 adb 在 PATH 中。"
echo "双击或在终端运行它，会自动打开浏览器网页，在网页里扫描电视并一键安装。"
echo "注：首次运行若被 macOS 拦截，在「系统设置 → 隐私与安全性」里点「仍要打开」，或右键 → 打开。"
