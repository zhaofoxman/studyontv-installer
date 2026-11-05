@echo off
chcp 65001 >nul
rem StudyOnTV 一键安装助手 - Windows 一键编译脚本（双击运行）
rem 需要先安装 Rust: https://rustup.rs
where cargo >nul 2>nul
if errorlevel 1 (
  echo [错误] 没找到 cargo。请先打开 https://rustup.rs 按提示安装 Rust，然后重新双击本脚本。
  pause
  exit /b 1
)
cd /d "%~dp0"
echo 正在编译（第一次较慢，请耐心等待）...
cargo build --release
if errorlevel 1 (
  echo [错误] 编译失败，请把上面的报错截图反馈。
  pause
  exit /b 1
)
if not exist dist mkdir dist
copy /y target\release\studyontv-installer.exe dist\StudyOnTV安装助手.exe >nul
echo.
echo 完成！可执行文件在 dist\StudyOnTV安装助手.exe
echo 使用前请把 Android platform-tools 的 adb.exe（连同文件夹）放到 dist 文件夹里，或确保 adb 在 PATH 中。
echo 双击 exe 后会自动打开浏览器网页，在网页里扫描电视并一键安装。
pause
