# StudyOnTV 一键安装助手

根据 [StudyOnTV 官方安装文档](https://github.com/btcfoxman/StudyOnTV/blob/main/docs/install.md) 编写的
**Rust（tokio 异步）+ 内嵌 HTML/JS** 小工具：在 Windows / macOS（或 Linux）电脑上双击运行，
自动打开一个可视化网页，在网页上就能——

1. **看教程**：各品牌电视开 ADB 的路径、配对码说明、权限说明、装不上对照表，都在网页里；
2. **动态扫描局域网**：异步并发扫描已开启 ADB（5555）的电视，同时探测已装 StudyOnTV 的管理页（8099）；
3. **看设备信息**：连接后自动读取品牌、型号、Android 版本，识别疑似电视、授权状态、是否已装 StudyOnTV；
4. **一键安装**：点按钮即可 `adb connect → adb install -r TV版APK → 自动打开应用`，全程日志实时回显；
5. **安装管理**：一键打开应用、检测并跳转家长管理页 `http://<电视IP>:8099/admin/`。

## 快速使用（普通用户）

1. 下载对应平台的完整安装包 zip（由 GitHub Actions 在打包时组装，见下「发布」），**完整解压**到一个文件夹；
2. 安装包已自带 **adb**（`platform-tools` 文件夹）和**打包时最新的 TV 版 APK**（`apk` 文件夹），不用再单独下载任何东西；
3. 双击运行本程序 → 浏览器自动打开 `http://127.0.0.1:18099/`（端口被占用时会顺延到 18118）；
4. 按网页第 1 步在电视上开网络调试 → 第 2 步确认自带 APK（一般可跳过）→ 第 3 步扫描 → 点「一键安装到这台」；
5. 去电视上完成首次设置（设管理密码、按需开权限），之后家长手机同 WiFi 扫码或打开管理页即可管理。

关闭运行窗口即退出。服务只监听 `127.0.0.1`，且只允许扫描/操作局域网私有 IP。

## 自己编译

需要 [Rust](https://rustup.rs)：

| 平台 | 命令 |
|---|---|
| Windows | 双击 `build-windows.bat`，产物在 `dist\StudyOnTV安装助手.exe` |
| macOS | `bash build-macos.sh`，产物在 `dist/StudyOnTV安装助手` |
| Linux | `bash build-linux.sh`，产物在 `dist/StudyOnTV安装助手` |
| 通用 | `cargo build --release`，产物在 `target/release/studyontv-installer` |

> 说明：Rust 是原生编译，**Windows 的 .exe 要在 Windows 上（或 CI 上）编译，macOS 版同理**，
> 不能在一台机器上直接变出另一个系统的可执行文件。本仓库已带 GitHub Actions 工作流（见下），
> 在 GitHub 上一次就能同时产出三个平台的包。

## 发布（GitHub Actions）

把本项目推到 GitHub 后：

- 手动：Actions → “Build installers” → Run workflow，三个平台的 zip 在 Artifacts 里下载；
- 发版：打标签 `git tag v0.1.0 && git push --tags`，zip 会自动附到 Release。

最终用户拿到的是一个完整安装包 zip：可执行文件（网页已内嵌编译进二进制）+ `platform-tools`（adb）+ `apk`（最新 TV 版 APK），解压双击即用，不用装 Rust、不用装运行时、不用另下 adb 和 APK。打包时 workflow 会从 Google 官方地址下载对应平台的 platform-tools，并从 StudyOnTV 的 GitHub Release（btcfoxman 优先，其次 chadyi）取最新 TV 版 APK 一并放进 zip。

## 工作原理

```
电脑上的本程序（Rust / tokio / axum，只监听 127.0.0.1:18099 起）
 ├─ GET  /                内嵌的教程+安装+管理网页（HTML/JS，fetch 异步调用）
 ├─ POST /api/scan        并发 TCP 探测网段内 5555/8099（信号量限流 96），命中后 adb 富化设备信息
 ├─ POST /api/download-apk 经系统 curl 从 GitHub Releases 找名字带 TV 的 APK 下载到 data/downloads/
 ├─ POST /api/upload-apk  上传 APK 到 data/uploads/
 ├─ POST /api/install     后台任务：adb connect → 状态检查（unauthorized 会提示去电视点允许）
 │                        → adb install -r → adb shell monkey -p com.studyontv.tv … 打开应用
 ├─ POST /api/pair        adb pair <ip>:<配对端口> <6位码>（Android 11+ 无线调试）
 ├─ POST /api/connect /api/launch /api/manage-check
 └─ GET  /api/jobs/:id    前端轮询任务进度、日志与设备列表
```

对应官方文档命令：`adb connect <IP>:5555`、`adb install -r StudyOnTV.apk`、
`adb shell monkey -p com.studyontv.tv -c android.intent.category.LAUNCHER 1`；
家长管理页由电视上的 StudyOnTV 在局域网 8099 端口提供，本助手只负责检测和跳转，不保存管理密码。

## 注意事项与限制

- **无线调试的随机端口**：Android 11+「无线调试」主页显示的连接端口每次可能变化，又无法全端口扫描整个网段；
  请把该端口填到扫描区的「附加端口」，或先配对后用手动连接。传统电视的网络调试一般固定 5555，不受影响。
- **APK 自动下载源**：优先 GitHub Releases（btcfoxman / chadyi 两个仓库）中名字含 TV 的 APK；
  若 Release 里没有，会在日志里提示从蒲公英 `https://www.pgyer.com/StudyOnTVTV` 手动下载后上传。
- **只支持 Android 5.0+ 的电视/盒子**；鸿蒙 NEXT / Tizen / webOS 与官方文档一致，暂不支持。
- 扫描范围默认本机所在 /24 网段，可在网页改成其它网段（如 `192.168.0.0/24`）或单个 IP；单次上限 /22。
- 路由器开了 AP/客户端隔离时，电脑扫不到电视，请到路由器设置里关闭隔离。
- 本工具在 Linux 上完成了编译与接口实测（模拟电视 + 假 adb 走通扫描、安装、管理检测全流程）；
  Windows / macOS 的可执行文件请用上面的脚本或 CI 在对应系统上构建后，在真机+真电视上再验一次。

## 目录

```
├── Cargo.toml
├── src/main.rs            # Rust 后端：扫描 / ADB / 安装任务 / HTTP API
├── static/index.html      # 前端：教程 + 设备列表 + 异步任务轮询（编译时内嵌）
├── build-windows.bat / build-macos.sh / build-linux.sh
└── .github/workflows/release.yml
```

StudyOnTV 本体版权与使用限制见其仓库 License（仅限个人家庭使用）；本助手仅调用其公开的安装方式。
