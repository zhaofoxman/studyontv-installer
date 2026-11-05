//! StudyOnTV 一键安装助手
//!
//! Rust(tokio) + 内嵌 HTML/JS 前端。启动后在本机打开网页：
//!   教程 → 异步扫描局域网已开启 ADB 的电视 → 点击把 TV 版 APK 安装到电视 → 打开家长管理页
//!
//! 依据：https://github.com/btcfoxman/StudyOnTV/blob/main/docs/install.md
//!   adb connect <TV IP>:5555
//!   adb install -r StudyOnTV.apk
//!   adb shell monkey -p com.studyontv.tv -c android.intent.category.LAUNCHER 1
//!   家长管理页：http://<TV IP>:8099/admin/

use axum::{
    extract::{Multipart, Path, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr},
    path::{Path as FsPath, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    process::Command,
    sync::{RwLock, Semaphore},
    time::timeout,
};

const TV_PACKAGE: &str = "com.studyontv.tv";
const ADMIN_PORT: u16 = 8099;
const DEFAULT_ADB_PORT: u16 = 5555;
const HTML: &str = include_str!("../static/index.html");

// ---------- 数据结构 ----------

#[derive(Clone, Serialize, Default)]
struct Device {
    ip: String,
    adb_port: u16,
    adb_open: bool,
    adb_state: String, // device / unauthorized / offline / open(未验证) / unknown
    manufacturer: String,
    model: String,
    android: String,
    characteristics: String,
    likely_tv: bool,
    studyontv_admin: bool,
    manage_url: String,
}

#[derive(Clone, Serialize)]
struct Job {
    id: String,
    kind: String,
    status: String, // running / success / failed
    progress_done: u32,
    progress_total: u32,
    logs: Vec<String>,
    devices: Vec<Device>,
}

#[derive(Clone)]
struct AppState {
    jobs: Arc<RwLock<HashMap<String, Job>>>,
    current_apk: Arc<RwLock<Option<PathBuf>>>,
    adb_cache: Arc<RwLock<Option<(String, String)>>>, // (path, version)
    data_dir: PathBuf,
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

async fn job_new(state: &AppState, kind: &str) -> String {
    let id = format!("{}-{}", now_ms(), rand_suffix());
    let job = Job {
        id: id.clone(),
        kind: kind.to_string(),
        status: "running".into(),
        progress_done: 0,
        progress_total: 0,
        logs: vec![],
        devices: vec![],
    };
    state.jobs.write().await.insert(id.clone(), job);
    id
}

fn rand_suffix() -> u32 {
    // 不引入 rand crate：用时间纳秒做简单后缀足够做本地 job id
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    n % 100000
}

async fn job_log(state: &AppState, id: &str, msg: impl Into<String>) {
    if let Some(j) = state.jobs.write().await.get_mut(id) {
        j.logs.push(msg.into());
    }
}

async fn job_finish(state: &AppState, id: &str, ok: bool) {
    if let Some(j) = state.jobs.write().await.get_mut(id) {
        j.status = if ok { "success" } else { "failed" }.into();
    }
}

// ---------- 路径 / 环境 ----------

fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn data_dir() -> PathBuf {
    let cand = exe_dir().join("data");
    if std::fs::create_dir_all(cand.join("uploads")).is_ok()
        && std::fs::create_dir_all(cand.join("downloads")).is_ok()
    {
        return cand;
    }
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_else(|_| ".".into());
    let cand = PathBuf::from(home).join(".studyontv-installer");
    let _ = std::fs::create_dir_all(cand.join("uploads"));
    let _ = std::fs::create_dir_all(cand.join("downloads"));
    cand
}

fn local_ipv4() -> Option<Ipv4Addr> {
    // 用 UDP connect 探主出口网卡地址，不会真的发数据
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    match sock.local_addr().ok()?.ip() {
        IpAddr::V4(v4) => Some(v4),
        _ => None,
    }
}

fn parse_targets(subnet: &Option<String>) -> Result<Vec<Ipv4Addr>, String> {
    let s = match subnet {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            let ip = local_ipv4().ok_or("无法确定本机局域网 IP，请手动填写网段，如 192.168.1.0/24")?;
            format!("{}.{}.{}.0/24", ip.octets()[0], ip.octets()[1], ip.octets()[2])
        }
    };
    if let Ok(ip) = s.parse::<Ipv4Addr>() {
        return Ok(vec![ip]);
    }
    if let Some((base, prefix)) = s.split_once('/') {
        let base: Ipv4Addr = base.parse().map_err(|_| format!("网段格式不对：{s}"))?;
        let prefix: u32 = prefix.parse().map_err(|_| format!("网段格式不对：{s}"))?;
        if !(16..=30).contains(&prefix) {
            return Err("只支持 /16–/30，家用一般用 /24".into());
        }
        let host_bits = 32 - prefix;
        let count = 1u32 << host_bits;
        if count > 4096 {
            return Err("网段太大，请缩小到 /22 或更小".into());
        }
        let base_u = u32::from(base) & (!0u32 << host_bits);
        let mut out = Vec::new();
        for i in 1..count - 1 {
            out.push(Ipv4Addr::from(base_u + i));
        }
        return Ok(out);
    }
    Err(format!("无法解析网段：{s}（示例：192.168.1.0/24 或 192.168.1.100）"))
}

fn check_private(ip: &Ipv4Addr) -> Result<(), String> {
    if ip.is_private() || ip.is_loopback() {
        Ok(())
    } else {
        Err(format!("{ip} 不是局域网私有地址，出于安全只允许扫描/安装局域网设备"))
    }
}

// ---------- ADB ----------

fn adb_candidates() -> Vec<String> {
    let mut out: Vec<String> = vec![];
    if let Ok(p) = std::env::var("ADB_PATH") {
        if !p.is_empty() {
            out.push(p);
        }
    }
    let exe_name = if cfg!(windows) { "adb.exe" } else { "adb" };
    // 程序同目录 / platform-tools 子目录（给小白用户的推荐放法）
    for base in [exe_dir(), std::env::current_dir().unwrap_or_default()] {
        out.push(base.join(exe_name).to_string_lossy().into_owned());
        out.push(
            base.join("platform-tools")
                .join(exe_name)
                .to_string_lossy()
                .into_owned(),
        );
    }
    // PATH
    out.push("adb".into());
    out
}

async fn run_cmd(prog: &str, args: &[&str], secs: u64) -> Result<(bool, String), String> {
    let fut = Command::new(prog)
        .args(args)
        .kill_on_drop(true)
        .output();
    match timeout(Duration::from_secs(secs), fut).await {
        Ok(Ok(o)) => {
            let mut text = String::from_utf8_lossy(&o.stdout).to_string();
            text.push_str(&String::from_utf8_lossy(&o.stderr));
            Ok((o.status.success(), text.trim().to_string()))
        }
        Ok(Err(e)) => Err(format!("无法执行 {prog}：{e}")),
        Err(_) => Err(format!("执行 {prog} 超时（{secs}s）")),
    }
}

async fn find_adb(state: &AppState) -> Result<(String, String), String> {
    if let Some(v) = state.adb_cache.read().await.clone() {
        return Ok(v);
    }
    for cand in adb_candidates() {
        if cand.contains(std::path::MAIN_SEPARATOR) && !FsPath::new(&cand).exists() {
            continue;
        }
        if let Ok((ok, text)) = run_cmd(&cand, &["version"], 5).await {
            if ok {
                let ver = text.lines().next().unwrap_or("adb").to_string();
                let found = (cand, ver);
                *state.adb_cache.write().await = Some(found.clone());
                return Ok(found);
            }
        }
    }
    Err("未找到 adb。请安装 Android platform-tools，并把 adb 放到本程序同目录，或加入系统 PATH；教程页第 0 步有下载链接。".into())
}

async fn run_adb(state: &AppState, args: &[&str], secs: u64) -> Result<(bool, String), String> {
    let (adb, _) = find_adb(state).await?;
    run_cmd(&adb, args, secs).await
}

/// 对一台已确认 5555 开放的主机做 adb connect + getprop 富化
async fn enrich_device(state: &AppState, ip: Ipv4Addr, adb_port: u16) -> Device {
    let mut dev = Device {
        ip: ip.to_string(),
        adb_port,
        adb_open: true,
        adb_state: "open".into(),
        manage_url: format!("http://{}:{}/admin/", ip, ADMIN_PORT),
        ..Default::default()
    };
    let serial = format!("{}:{}", ip, adb_port);
    if let Ok((_, text)) = run_adb(state, &["connect", &serial], 6).await {
        if text.contains("unauthorized") {
            dev.adb_state = "unauthorized".into();
        } else if text.contains("connected") {
            dev.adb_state = "device".into();
        }
    }
    if dev.adb_state == "device" {
        if let Ok((_, st)) = run_adb(state, &["-s", &serial, "get-state"], 5).await {
            if !st.is_empty() {
                dev.adb_state = st.lines().next().unwrap_or("device").trim().to_string();
            }
        }
    }
    if dev.adb_state == "device" {
        let script = "getprop ro.product.manufacturer; getprop ro.product.model; getprop ro.build.version.release; getprop ro.build.characteristics; getprop ro.product.device";
        if let Ok((_, out)) = run_adb(state, &["-s", &serial, "shell", script], 8).await {
            let mut lines = out.lines().map(|l| l.trim().to_string());
            dev.manufacturer = lines.next().unwrap_or_default();
            dev.model = lines.next().unwrap_or_default();
            dev.android = lines.next().unwrap_or_default();
            dev.characteristics = lines.next().unwrap_or_default();
            let device = lines.next().unwrap_or_default();
            let hay = format!(
                "{} {} {} {}",
                dev.manufacturer, dev.model, dev.characteristics, device
            )
            .to_lowercase();
            dev.likely_tv = dev.characteristics.contains("tv")
                || hay.contains("tv")
                || hay.contains("bravia")
                || hay.contains("mitv")
                || hay.contains("emulator") == false && hay.contains("television");
        }
    }
    dev.studyontv_admin = probe_admin(ip).await;
    dev
}

async fn probe_admin(ip: Ipv4Addr) -> bool {
    let addr = format!("{}:{}", ip, ADMIN_PORT);
    let stream = timeout(Duration::from_millis(500), TcpStream::connect(&addr)).await;
    let mut s = match stream {
        Ok(Ok(s)) => s,
        _ => return false,
    };
    let req = format!(
        "GET /admin/ HTTP/1.0\r\nHost: {}\r\n\r\n",
        ip
    );
    if s.write_all(req.as_bytes()).await.is_err() {
        return false;
    }
    let mut buf = vec![0u8; 256];
    match timeout(Duration::from_millis(700), s.read(&mut buf)).await {
        Ok(Ok(n)) if n > 0 => {
            let text = String::from_utf8_lossy(&buf[..n]);
            text.starts_with("HTTP/")
        }
        _ => false,
    }
}

async fn tcp_open(ip: Ipv4Addr, port: u16, ms: u64) -> bool {
    timeout(
        Duration::from_millis(ms),
        TcpStream::connect((ip, port)),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false)
}

// ---------- APK ----------

async fn find_apk(state: &AppState) -> Option<PathBuf> {
    if let Some(p) = state.current_apk.read().await.clone() {
        if p.exists() {
            return Some(p);
        }
    }
    let mut dirs = vec![
        state.data_dir.join("downloads"),
        state.data_dir.join("uploads"),
        exe_dir(),
        exe_dir().join("apk"),
    ];
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd);
    }
    for dir in dirs {
        if let Ok(rd) = std::fs::read_dir(&dir) {
            let mut apks: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.extension()
                        .map(|e| e.eq_ignore_ascii_case("apk"))
                        .unwrap_or(false)
                })
                .collect();
            // 优先名字带 tv 的，其次任意 apk
            apks.sort_by_key(|p| {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_lowercase())
                    .unwrap_or_default();
                if name.contains("tv") {
                    0
                } else {
                    1
                }
            });
            if let Some(first) = apks.into_iter().next() {
                return Some(first);
            }
        }
    }
    None
}

fn apk_info(p: &FsPath) -> serde_json::Value {
    let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    serde_json::json!({
        "name": p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        "path": p.to_string_lossy(),
        "size_mb": (size as f64 / 1_000_000.0 * 10.0).round() / 10.0,
    })
}

// ---------- 请求结构 ----------

#[derive(Deserialize)]
struct ScanReq {
    subnet: Option<String>,
    extra_ports: Option<Vec<u16>>,
}

#[derive(Deserialize)]
struct TargetReq {
    ip: String,
    port: Option<u16>,
}

#[derive(Deserialize)]
struct PairReq {
    ip: String,
    pair_port: u16,
    code: String,
}

#[derive(Deserialize)]
struct InstallReq {
    ip: String,
    port: Option<u16>,
}

// ---------- HTTP 处理 ----------

async fn index() -> Html<&'static str> {
    Html(HTML)
}

async fn status(State(state): State<AppState>) -> Json<serde_json::Value> {
    let adb = find_adb(&state).await;
    let apk = find_apk(&state).await;
    let local = local_ipv4();
    Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "tv_package": TV_PACKAGE,
        "admin_port": ADMIN_PORT,
        "local_ip": local.map(|i| i.to_string()),
        "default_subnet": local.map(|i| { let o=i.octets(); format!("{}.{}.{}.0/24", o[0],o[1],o[2]) }),
        "adb": match adb {
            Ok((path, ver)) => serde_json::json!({"found": true, "path": path, "version": ver}),
            Err(e) => serde_json::json!({"found": false, "error": e}),
        },
        "apk": apk.as_ref().map(|p| apk_info(p)),
        "data_dir": state.data_dir.to_string_lossy(),
    }))
}

async fn scan_start(
    State(state): State<AppState>,
    Json(req): Json<ScanReq>,
) -> impl IntoResponse {
    let targets = match parse_targets(&req.subnet) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": e})),
            )
        }
    };
    for ip in &targets {
        if let Err(e) = check_private(ip) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": e})),
            );
        }
    }
    let mut ports: Vec<u16> = vec![DEFAULT_ADB_PORT, ADMIN_PORT];
    if let Some(extra) = &req.extra_ports {
        for p in extra {
            if *p != 0 && !ports.contains(p) {
                ports.push(*p);
            }
        }
    }
    let id = job_new(&state, "scan").await;
    {
        let mut jobs = state.jobs.write().await;
        if let Some(j) = jobs.get_mut(&id) {
            j.progress_total = targets.len() as u32;
        }
    }
    let st = state.clone();
    let jid = id.clone();
    tokio::spawn(async move {
        job_log(&st, &jid, format!("开始扫描 {} 台主机，端口 {:?}", targets.len(), ports)).await;
        let sem = Arc::new(Semaphore::new(96));
        let found: Arc<RwLock<Vec<Device>>> = Arc::new(RwLock::new(vec![]));
        let mut handles = vec![];
        for ip in targets {
            let permit = sem.clone().acquire_owned().await.unwrap();
            let st2 = st.clone();
            let jid2 = jid.clone();
            let found2 = found.clone();
            let ports2 = ports.clone();
            handles.push(tokio::spawn(async move {
                let _p = permit;
                // 先探 8099（已装 StudyOnTV 的电视也要显示出来）
                let admin = tcp_open(ip, ADMIN_PORT, 350).await;
                let mut adb_port_found: Option<u16> = None;
                for p in &ports2 {
                    if *p == ADMIN_PORT {
                        continue;
                    }
                    if tcp_open(ip, *p, 350).await {
                        adb_port_found = Some(*p);
                        break;
                    }
                }
                if adb_port_found.is_some() || admin {
                    let mut dev = if let Some(ap) = adb_port_found {
                        enrich_device(&st2, ip, ap).await
                    } else {
                        Device {
                            ip: ip.to_string(),
                            adb_port: 0,
                            adb_open: false,
                            adb_state: "not-open".into(),
                            manage_url: format!("http://{}:{}/admin/", ip, ADMIN_PORT),
                            ..Default::default()
                        }
                    };
                    dev.studyontv_admin = admin;
                    job_log(
                        &st2,
                        &jid2,
                        format!(
                            "发现 {}（ADB {}，StudyOnTV 管理页 {}）",
                            dev.ip,
                            if dev.adb_open { dev.adb_state.clone() } else { "未开".into() },
                            if admin { "可达" } else { "不可达" }
                        ),
                    )
                    .await;
                    found2.write().await.push(dev);
                }
                let mut jobs = st2.jobs.write().await;
                if let Some(j) = jobs.get_mut(&jid2) {
                    j.progress_done += 1;
                    j.devices = found2.read().await.clone();
                }
            }));
        }
        for h in handles {
            let _ = h.await;
        }
        let n = found.read().await.len();
        job_log(&st, &jid, format!("扫描完成，共发现 {n} 台候选设备")).await;
        job_finish(&st, &jid, true).await;
    });
    (
        StatusCode::OK,
        Json(serde_json::json!({"job_id": id})),
    )
}

async fn job_get(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    match state.jobs.read().await.get(&id) {
        Some(j) => (StatusCode::OK, Json(serde_json::to_value(j).unwrap())),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": "任务不存在"})),
        ),
    }
}

fn parse_target(req_ip: &str, port: Option<u16>) -> Result<(Ipv4Addr, u16), String> {
    let ip: Ipv4Addr = req_ip.trim().parse().map_err(|_| "IP 格式不对".to_string())?;
    check_private(&ip)?;
    Ok((ip, port.unwrap_or(DEFAULT_ADB_PORT)))
}

async fn connect(
    State(state): State<AppState>,
    Json(req): Json<TargetReq>,
) -> impl IntoResponse {
    let (ip, port) = match parse_target(&req.ip, req.port) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": e}))),
    };
    let serial = format!("{}:{}", ip, port);
    match run_adb(&state, &["connect", &serial], 8).await {
        Ok((_, text)) => {
            let dev = enrich_device(&state, ip, port).await;
            (
                StatusCode::OK,
                Json(serde_json::json!({"output": text, "device": dev})),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        ),
    }
}

async fn pair(
    State(state): State<AppState>,
    Json(req): Json<PairReq>,
) -> impl IntoResponse {
    let ip: Ipv4Addr = match req.ip.trim().parse() {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "IP 格式不对"})),
            )
        }
    };
    if let Err(e) = check_private(&ip) {
        return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": e})));
    }
    let target = format!("{}:{}", ip, req.pair_port);
    match run_adb(&state, &["pair", &target, req.code.trim()], 15).await {
        Ok((ok, text)) => (
            if ok { StatusCode::OK } else { StatusCode::BAD_REQUEST },
            Json(serde_json::json!({"output": text})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        ),
    }
}

async fn launch(
    State(state): State<AppState>,
    Json(req): Json<TargetReq>,
) -> impl IntoResponse {
    let (ip, port) = match parse_target(&req.ip, req.port) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": e}))),
    };
    let serial = format!("{}:{}", ip, port);
    let _ = run_adb(&state, &["connect", &serial], 6).await;
    match run_adb(
        &state,
        &[
            "-s",
            &serial,
            "shell",
            "monkey",
            "-p",
            TV_PACKAGE,
            "-c",
            "android.intent.category.LAUNCHER",
            "1",
        ],
        15,
    )
    .await
    {
        Ok((_, text)) => (
            StatusCode::OK,
            Json(serde_json::json!({"output": text})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": e})),
        ),
    }
}

async fn install_start(
    State(state): State<AppState>,
    Json(req): Json<InstallReq>,
) -> impl IntoResponse {
    let (ip, port) = match parse_target(&req.ip, req.port) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": e})),
            )
        }
    };
    let id = job_new(&state, "install").await;
    let st = state.clone();
    let jid = id.clone();
    tokio::spawn(async move {
        let serial = format!("{}:{}", ip, port);
        // 1) adb
        match find_adb(&st).await {
            Ok((_, ver)) => job_log(&st, &jid, format!("使用 adb：{ver}")).await,
            Err(e) => {
                job_log(&st, &jid, e).await;
                job_finish(&st, &jid, false).await;
                return;
            }
        }
        // 2) apk
        let apk = match find_apk(&st).await {
            Some(p) => p,
            None => {
                job_log(&st, &jid, "没有找到 TV 版 APK。请先在网页上方「下载最新 TV 版」或上传 APK 文件，再重试安装。".to_string()).await;
                job_finish(&st, &jid, false).await;
                return;
            }
        };
        job_log(&st, &jid, format!("使用安装包：{}", apk.display())).await;
        // 3) connect
        job_log(&st, &jid, format!("adb connect {serial} …")).await;
        match run_adb(&st, &["connect", &serial], 8).await {
            Ok((_, t)) => job_log(&st, &jid, t).await,
            Err(e) => {
                job_log(&st, &jid, e).await;
                job_finish(&st, &jid, false).await;
                return;
            }
        }
        // 状态检查：unauthorized 要先去电视上点允许
        let dev = enrich_device(&st, ip, port).await;
        if dev.adb_state == "unauthorized" {
            job_log(&st, &jid, "电视尚未授权：请看电视屏幕，点「允许 USB/网络调试」，然后重新点安装。".to_string()).await;
            job_finish(&st, &jid, false).await;
            return;
        }
        if dev.adb_state != "device" {
            job_log(&st, &jid, format!("设备状态异常：{}。请确认电视已开网络调试、且与电脑同一 WiFi。", dev.adb_state)).await;
            job_finish(&st, &jid, false).await;
            return;
        }
        job_log(&st, &jid, format!("已连接：{} {}（Android {}）", dev.manufacturer, dev.model, dev.android)).await;
        // 4) install -r
        job_log(&st, &jid, "adb install -r 开始推送安装（大文件需等一会儿）…".to_string()).await;
        let apk_str = apk.to_string_lossy().to_string();
        match run_adb(&st, &["-s", &serial, "install", "-r", &apk_str], 300).await {
            Ok((_, t)) => {
                job_log(&st, &jid, t.clone()).await;
                if !t.contains("Success") {
                    job_log(&st, &jid, "安装未返回 Success。常见原因：系统低于 Android 5.0、APK 不是 TV 版、签名冲突（先在电视上卸载旧版再装）。".to_string()).await;
                    job_finish(&st, &jid, false).await;
                    return;
                }
            }
            Err(e) => {
                job_log(&st, &jid, e).await;
                job_finish(&st, &jid, false).await;
                return;
            }
        }
        job_log(&st, &jid, "安装成功，正在电视上打开 StudyOnTV …".to_string()).await;
        // 5) launch
        let _ = run_adb(
            &st,
            &["-s", &serial, "shell", "monkey", "-p", TV_PACKAGE, "-c", "android.intent.category.LAUNCHER", "1"],
            15,
        )
        .await;
        job_log(&st, &jid, format!("完成！请在电视上：① 设置管理密码 ② 按需开启悬浮窗/使用情况访问/自启动。装好并完成首次设置后，家长管理页：http://{}:{}/admin/", ip, ADMIN_PORT)).await;
        job_finish(&st, &jid, true).await;
    });
    (StatusCode::OK, Json(serde_json::json!({"job_id": id})))
}

async fn download_apk_start(State(state): State<AppState>) -> impl IntoResponse {
    let id = job_new(&state, "download-apk").await;
    let st = state.clone();
    let jid = id.clone();
    tokio::spawn(async move {
        // 通过系统 curl 调 GitHub Releases API（Windows 10+/macOS 都自带 curl）
        let repos = ["btcfoxman/StudyOnTV", "chadyi/StudyOnTV"];
        let mut download_url: Option<String> = None;
        for repo in repos {
            let api = format!("https://api.github.com/repos/{repo}/releases/latest");
            job_log(&st, &jid, format!("查询 {repo} 最新 Release …")).await;
            match run_cmd("curl", &["-fsSL", "-H", "User-Agent: studyontv-installer", &api], 30).await {
                Ok((true, text)) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Some(assets) = v.get("assets").and_then(|a| a.as_array()) {
                            for a in assets {
                                let name = a.get("name").and_then(|n| n.as_str()).unwrap_or("");
                                if name.to_lowercase().ends_with(".apk")
                                    && name.to_lowercase().contains("tv")
                                {
                                    download_url = a
                                        .get("browser_download_url")
                                        .and_then(|u| u.as_str())
                                        .map(|s| s.to_string());
                                    job_log(&st, &jid, format!("找到 TV 版安装包：{name}")).await;
                                    break;
                                }
                            }
                        }
                    }
                }
                Ok((_, t)) => job_log(&st, &jid, format!("{repo} 查询失败：{}", t.chars().take(200).collect::<String>())).await,
                Err(e) => job_log(&st, &jid, e).await,
            }
            if download_url.is_some() {
                break;
            }
        }
        let url = match download_url {
            Some(u) => u,
            None => {
                job_log(&st, &jid, "GitHub Release 里没找到 TV 版 APK。请手动打开 https://www.pgyer.com/StudyOnTVTV 下载 TV 版，再用上方「上传 APK」选进来，然后重试安装。".to_string()).await;
                job_finish(&st, &jid, false).await;
                return;
            }
        };
        let dest = st.data_dir.join("downloads").join("StudyOnTV-TV.apk");
        let dest_str = dest.to_string_lossy().to_string();
        job_log(&st, &jid, "开始下载（走系统 curl）…".to_string()).await;
        match run_cmd("curl", &["-fL", "--progress-bar", "-o", &dest_str, &url], 600).await {
            Ok((true, _)) => {
                *st.current_apk.write().await = Some(dest.clone());
                job_log(&st, &jid, format!("下载完成：{}", apk_info(&dest))).await;
                job_finish(&st, &jid, true).await;
            }
            Ok((_, t)) => {
                job_log(&st, &jid, format!("下载失败：{}", t.chars().take(300).collect::<String>())).await;
                job_finish(&st, &jid, false).await;
            }
            Err(e) => {
                job_log(&st, &jid, e).await;
                job_finish(&st, &jid, false).await;
            }
        }
    });
    (StatusCode::OK, Json(serde_json::json!({"job_id": id})))
}

async fn upload_apk(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    while let Ok(Some(field)) = multipart.next_field().await {
        if field.name() == Some("apk") {
            let filename = field
                .file_name()
                .map(|s| s.to_string())
                .unwrap_or_else(|| "StudyOnTV-TV.apk".into());
            let safe: String = filename
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || ".-_".contains(c) {
                        c
                    } else {
                        '_'
                    }
                })
                .collect();
            if !safe.to_lowercase().ends_with(".apk") {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "只接受 .apk 文件"})),
                );
            }
            let dest = state.data_dir.join("uploads").join(&safe);
            match field.bytes().await {
                Ok(bytes) => {
                    if tokio::fs::write(&dest, &bytes).await.is_ok() {
                        *state.current_apk.write().await = Some(dest.clone());
                        return (
                            StatusCode::OK,
                            Json(serde_json::json!({"apk": apk_info(&dest)})),
                        );
                    }
                }
                Err(e) => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({"error": e.to_string()})),
                    )
                }
            }
        }
    }
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": "没有收到 apk 字段"})),
    )
}

async fn manage_check(
    State(state): State<AppState>,
    Json(req): Json<TargetReq>,
) -> impl IntoResponse {
    let _ = state;
    let ip: Ipv4Addr = match req.ip.trim().parse() {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "IP 格式不对"})),
            )
        }
    };
    let reachable = probe_admin(ip).await;
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "reachable": reachable,
            "manage_url": format!("http://{}:{}/admin/", ip, ADMIN_PORT),
            "hint": if reachable { "管理页可达，点开输入首次设置的管理密码即可" } else { "管理页不可达：请先在电视上打开 StudyOnTV 并完成首次设置（设密码），且保证同一局域网" }
        })),
    )
}

// ---------- 启动 ----------

fn open_browser(url: &str) {
    if std::env::var("NO_OPEN").is_ok() {
        return;
    }
    let url = url.to_string();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        #[cfg(target_os = "windows")]
        let _ = Command::new("cmd")
            .args(["/C", "start", "", &url])
            .spawn();
        #[cfg(target_os = "macos")]
        let _ = Command::new("open").arg(&url).spawn();
        #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
        let _ = Command::new("xdg-open").arg(&url).spawn();
    });
}

async fn version_info() -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "name": "studyontv-installer",
        "display_name": "StudyOnTV 一键安装助手",
        "version": env!("CARGO_PKG_VERSION"),
        "version_label": format!("v{}", env!("CARGO_PKG_VERSION")),
    }))
}

#[tokio::main]
async fn main() {
    // studyontv-installer --version / -V ：只打印版本号退出，方便脚本和用户核对
    if std::env::args()
        .skip(1)
        .any(|a| a == "--version" || a == "-V" || a == "version")
    {
        println!(
            "StudyOnTV 一键安装助手 v{}",
            env!("CARGO_PKG_VERSION")
        );
        return;
    }
    let state = AppState {
        jobs: Arc::new(RwLock::new(HashMap::new())),
        current_apk: Arc::new(RwLock::new(None)),
        adb_cache: Arc::new(RwLock::new(None)),
        data_dir: data_dir(),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/api/version", get(version_info))
        .route("/api/status", get(status))
        .route("/api/scan", post(scan_start))
        .route("/api/jobs/:id", get(job_get))
        .route("/api/connect", post(connect))
        .route("/api/pair", post(pair))
        .route("/api/launch", post(launch))
        .route("/api/install", post(install_start))
        .route("/api/download-apk", post(download_apk_start))
        .route("/api/upload-apk", post(upload_apk))
        .route("/api/manage-check", post(manage_check))
        .with_state(state);

    // 端口：INSTALLER_PORT 指定，否则 18099 起顺延找空闲
    let preferred: u16 = std::env::var("INSTALLER_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(18099);
    let mut listener = None;
    let mut chosen = preferred;
    for p in preferred..preferred + 20 {
        match tokio::net::TcpListener::bind(("127.0.0.1", p)).await {
            Ok(l) => {
                listener = Some(l);
                chosen = p;
                break;
            }
            Err(_) => continue,
        }
    }
    let listener = listener.expect("18099-18118 端口都被占用，无法启动");
    let url = format!("http://127.0.0.1:{chosen}/");
    println!(
        "StudyOnTV 一键安装助手 v{} 已启动：{url}",
        env!("CARGO_PKG_VERSION")
    );
    println!("保持此窗口开着，在网页里扫描并安装；关闭窗口即退出。");
    open_browser(&url);
    axum::serve(listener, app).await.expect("服务器异常退出");
}
