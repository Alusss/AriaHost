//! 端到端流程测试：线程内假 HTTP 服务器（`127.0.0.1` 随机端口）→ [`Fetcher`] → [`VersionStore`]。
//!
//! **不依赖真实网络**：所有请求都打到本进程内起的 `TcpListener`，测试还断言 `hits` 里
//! 确实出现过 `/api`、`/asset.zip` 这些路径（证明确实走的是本地回环，而不是被某个
//! 真地址兜住）。
//!
//! 覆盖（task-30 交付 5）：API 正常解析 / BadPayload / RateLimited / 白名单过滤 debug 包 /
//! 镜像模板拼接 / 哈希不匹配拒绝并保留旧版本 / active.json 切换与回滚 / 非 zip 载荷拒绝。

use std::io::{Cursor, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use ariahost_aria2::Aria2Version;
use ariahost_common::ErrorCode;
use ariahost_core::{
    ci_lines, default_lines, DownloadLine, Fetcher, HashState, HttpClient, HttpOptions,
    UpdateState, VersionStore, CORE_EXE, FILE_PLACEHOLDER,
};
use serde_json::json;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

// ---------------------------------------------------------------------------
// 假 HTTP 服务器（只服务本机回环）
// ---------------------------------------------------------------------------

/// 一条静态路由：`prefix` 命中请求路径就返回 `response`。
struct Route {
    prefix: String,
    status: u16,
    content_type: Option<String>,
    body: Vec<u8>,
}

impl Route {
    fn new(prefix: &str, status: u16, content_type: Option<&str>, body: Vec<u8>) -> Self {
        Self {
            prefix: prefix.to_owned(),
            status,
            content_type: content_type.map(str::to_owned),
            body,
        }
    }

    fn json(prefix: &str, body: &str) -> Self {
        Self::new(
            prefix,
            200,
            Some("application/json"),
            body.as_bytes().to_vec(),
        )
    }

    fn file(prefix: &str, body: Vec<u8>) -> Self {
        Self::new(prefix, 200, Some("application/zip"), body)
    }

    fn html(prefix: &str, body: &str) -> Self {
        Self::new(prefix, 200, Some("text/html"), body.as_bytes().to_vec())
    }
}

/// 假服务器句柄。线程故意 detach（不 join）：测试结束时若线程还阻塞在 `accept` 上，
/// join 会把测试挂死；这里改为把服务端异常记进 `errors`，由测试断言为空。
struct FakeServer {
    addr: SocketAddr,
    hits: Arc<Mutex<Vec<String>>>,
    errors: Arc<Mutex<Vec<String>>>,
}

impl FakeServer {
    fn start<F>(build: F) -> Self
    where
        F: FnOnce(SocketAddr) -> Vec<Route>,
    {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("绑定假服务器失败");
        let addr = listener.local_addr().expect("取地址失败");
        let routes = Arc::new(build(addr));
        let hits = Arc::new(Mutex::new(Vec::new()));
        let errors = Arc::new(Mutex::new(Vec::new()));

        let hits_thread = Arc::clone(&hits);
        let errors_thread = Arc::clone(&errors);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    errors_thread
                        .lock()
                        .expect("锁")
                        .push("accept 失败".to_owned());
                    continue;
                };
                match read_request_path(&mut stream) {
                    Ok(path) => {
                        hits_thread.lock().expect("锁").push(path.clone());
                        let route = routes.iter().find(|route| path.starts_with(&route.prefix));
                        match route {
                            Some(route) => write_response(&mut stream, route),
                            None => write_response(
                                &mut stream,
                                &Route::new("/", 404, Some("text/plain"), b"not found".to_vec()),
                            ),
                        }
                    }
                    Err(error) => errors_thread
                        .lock()
                        .expect("锁")
                        .push(format!("读请求失败：{error}")),
                }
            }
        });

        Self { addr, hits, errors }
    }

    fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base())
    }

    /// 已收到的请求路径（按顺序）。
    fn hits(&self) -> Vec<String> {
        self.hits.lock().expect("锁").clone()
    }

    /// 服务端异常（应为空）。
    fn errors(&self) -> Vec<String> {
        self.errors.lock().expect("锁").clone()
    }
}

/// 读请求行，返回路径（忽略 query）。
fn read_request_path(stream: &mut TcpStream) -> std::io::Result<String> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
            let line = head.lines().next().unwrap_or_default();
            let mut parts = line.split_whitespace();
            let _method = parts.next().unwrap_or_default();
            let path = parts.next().unwrap_or_default();
            return Ok(path.split('?').next().unwrap_or_default().to_owned());
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "请求头未收全",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.len() > 1 << 20 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "请求头过大",
            ));
        }
    }
}

fn write_response(stream: &mut TcpStream, route: &Route) {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        route.status,
        reason(route.status),
        route.body.len()
    );
    if let Some(content_type) = &route.content_type {
        head.push_str(&format!("Content-Type: {content_type}\r\n"));
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&route.body);
    let _ = stream.flush();
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

// ---------------------------------------------------------------------------
// 测试夹具
// ---------------------------------------------------------------------------

/// 每个测试一个临时版本库目录。
struct TestDir(PathBuf);

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

impl TestDir {
    fn new(tag: &str) -> Self {
        let sequence = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ariahost-core-flow-{tag}-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("建临时目录");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 在内存里造一个「像 aria2 官方包」的 zip。
fn build_core_zip(payload: &[u8]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    writer.start_file(CORE_EXE, options).expect("start_file");
    writer.write_all(payload).expect("写条目");
    writer.finish().expect("finish").into_inner()
}

/// 与生产选择器同源的架构标签（`32bit` / `64bit`）。
///
/// **为什么不能写死 64**：`windows / x86` job 真跑时，fixture 里只有 `win-64bit` 资产，
/// 生产代码正确地判「该版本没有匹配本机架构的安装包」→ 9 个用例集体判红。
/// **产品行为是对的，夹具是错的**：夹具必须跟着目标架构走。
fn arch_tag() -> &'static str {
    match std::env::consts::ARCH {
        "x86" => "32bit",
        "x86_64" => "64bit",
        other => panic!("fetch_flow 夹具未覆盖的目标架构：{other}"),
    }
}

/// 资产名（按本机目标架构 / 任意版本）。
fn asset_name(version: &str) -> String {
    format!("aria2-{version}-win-{}-build1.zip", arch_tag())
}

/// 构造一份「一个版本 + 一个 64 位资产」的 API JSON，资产 URL 指向假服务器。
fn api_json(version: &str, asset_url: &str, size: usize, asset_file: &str) -> String {
    json!([{
        "tag_name": format!("release-{version}"),
        "published_at": "2023-11-15T13:05:48Z",
        "assets": [
            {
                "name": asset_file,
                "size": size,
                "browser_download_url": asset_url
            }
        ]
    }])
    .to_string()
}

/// 允许明文 HTTP 的客户端（只用于指向本机假服务器）。
fn test_client() -> HttpClient {
    HttpClient::with_options(HttpOptions {
        allow_plain_http: true,
        ..HttpOptions::default()
    })
}

/// 直连线路（`{url}` 原样）。
fn direct_line() -> DownloadLine {
    DownloadLine::direct()
}

/// 指向一个**已关闭**端口的线路（连接必被拒）。
fn dead_line(name: &str) -> DownloadLine {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("占端口");
    let addr = listener.local_addr().expect("取地址");
    drop(listener);
    DownloadLine::new(name, format!("http://{addr}/dead/{URL}"))
}

const URL: &str = "{url}";

// ---------------------------------------------------------------------------
// 正常路径
// ---------------------------------------------------------------------------

#[test]
fn fetch_latest_installs_and_activates_with_unverified_hash() {
    // 1.36.0 没有已知哈希 → 必须走 Unverified + 告警这条路。
    let payload = b"fake-aria2c-1.36.0";
    let zip = build_core_zip(payload);
    let server = FakeServer::start(|addr| {
        let asset_url = format!("http://{addr}/asset.zip");
        vec![
            Route::json(
                "/api",
                &api_json("1.36.0", &asset_url, zip.len(), &asset_name("1.36.0")),
            ),
            Route::file("/asset.zip", zip),
        ]
    });

    let dir = TestDir::new("latest");
    let store = VersionStore::new(dir.path());
    let fetcher =
        Fetcher::with_parts(test_client(), vec![direct_line()]).with_api_url(server.url("/api"));

    let outcome = fetcher.fetch(&store, None).expect("应成功获取最新版");
    assert_eq!(outcome.version, Aria2Version::new(1, 36, 0));
    assert_eq!(outcome.line, "直连 GitHub");
    assert!(
        !outcome.hash.is_verified(),
        "1.36.0 无已知哈希，不得声称校验过"
    );
    assert!(
        outcome.warnings.iter().any(|w| w.contains("未校验哈希")),
        "warnings: {:?}",
        outcome.warnings
    );
    assert_eq!(
        std::fs::read(&outcome.exe_path).expect("读 exe"),
        payload,
        "exe 内容应与 zip 内的 aria2c.exe 一致"
    );
    assert_eq!(outcome.pointer.active, "1.36.0");
    assert_eq!(outcome.pointer.previous, None);

    // 证明确实走的是本机回环上的假服务器。
    let hits = server.hits();
    assert_eq!(hits, vec!["/api".to_owned(), "/asset.zip".to_owned()]);
    assert!(server.errors().is_empty(), "{:?}", server.errors());
}

#[test]
fn fetch_specific_version_selects_asset_by_arch() {
    let payload = b"fake-aria2c";
    let zip = build_core_zip(payload);
    let server = FakeServer::start(|addr| {
        let asset_url = format!("http://{addr}/asset.zip");
        vec![
            Route::json(
                "/api",
                &api_json("1.36.0", &asset_url, zip.len(), &asset_name("1.36.0")),
            ),
            Route::file("/asset.zip", zip),
        ]
    });

    let dir = TestDir::new("specific");
    let store = VersionStore::new(dir.path());
    let fetcher =
        Fetcher::with_parts(test_client(), vec![direct_line()]).with_api_url(server.url("/api"));

    let outcome = fetcher
        .fetch(&store, Some(Aria2Version::new(1, 36, 0)))
        .expect("指定版本应成功");
    assert_eq!(outcome.version, Aria2Version::new(1, 36, 0));
    assert!(store.is_installed("1.36.0"));
}

#[test]
fn check_update_reports_both_states_against_fake_api() {
    let server = FakeServer::start(|addr| {
        let asset_url = format!("http://{addr}/asset.zip");
        vec![Route::json(
            "/api",
            &api_json("1.36.0", &asset_url, 10, &asset_name("1.36.0")),
        )]
    });
    let fetcher =
        Fetcher::with_parts(test_client(), vec![direct_line()]).with_api_url(server.url("/api"));

    assert_eq!(
        fetcher
            .check_update(Aria2Version::new(1, 35, 0))
            .expect("检查"),
        UpdateState::UpdateAvailable {
            current: Aria2Version::new(1, 35, 0),
            latest: Aria2Version::new(1, 36, 0)
        }
    );
    assert_eq!(
        fetcher
            .check_update(Aria2Version::new(1, 36, 0))
            .expect("检查"),
        UpdateState::NoUpdate {
            current: Aria2Version::new(1, 36, 0),
            latest: Aria2Version::new(1, 36, 0)
        }
    );
}

// ---------------------------------------------------------------------------
// API 侧的四态错误
// ---------------------------------------------------------------------------

#[test]
fn html_response_is_bad_payload_with_e6001() {
    let server =
        FakeServer::start(|_addr| vec![Route::html("/api", "<html>mirror is down</html>")]);
    let dir = TestDir::new("badpayload");
    let store = VersionStore::new(dir.path());
    let fetcher =
        Fetcher::with_parts(test_client(), vec![direct_line()]).with_api_url(server.url("/api"));

    let error = fetcher.fetch(&store, None).expect_err("html 必须报错");
    assert!(
        matches!(error, ariahost_core::CoreError::BadPayload { .. }),
        "{error}"
    );
    assert_eq!(error.code(), Some(ErrorCode::UpdateNetworkFailed));
    assert!(store.list().expect("列版本").is_empty(), "不得留下任何版本");
}

#[test]
fn rate_limited_api_maps_to_e6002() {
    let server = FakeServer::start(|_addr| {
        vec![Route::new(
            "/api",
            403,
            Some("application/json"),
            json!({"message": "API rate limit exceeded for 1.2.3.4"})
                .to_string()
                .into_bytes(),
        )]
    });
    let dir = TestDir::new("ratelimit");
    let store = VersionStore::new(dir.path());
    let fetcher =
        Fetcher::with_parts(test_client(), vec![direct_line()]).with_api_url(server.url("/api"));

    let error = fetcher.fetch(&store, None).expect_err("限流必须报错");
    assert!(
        matches!(error, ariahost_core::CoreError::RateLimited { .. }),
        "{error}"
    );
    assert_eq!(error.code(), Some(ErrorCode::UpdateRateLimited));
}

#[test]
fn http_500_maps_to_update_network_failed() {
    let server = FakeServer::start(|_addr| {
        vec![Route::new(
            "/api",
            500,
            Some("text/plain"),
            b"boom".to_vec(),
        )]
    });
    let dir = TestDir::new("http500");
    let store = VersionStore::new(dir.path());
    let fetcher =
        Fetcher::with_parts(test_client(), vec![direct_line()]).with_api_url(server.url("/api"));

    let error = fetcher.fetch(&store, None).expect_err("500 必须报错");
    assert_eq!(
        error.code(),
        Some(ErrorCode::UpdateNetworkFailed),
        "{error}"
    );
}

#[test]
fn requested_version_missing_from_releases_is_reported() {
    let server = FakeServer::start(|addr| {
        let asset_url = format!("http://{addr}/asset.zip");
        vec![Route::json(
            "/api",
            &api_json("1.36.0", &asset_url, 10, &asset_name("1.36.0")),
        )]
    });
    let dir = TestDir::new("norelease");
    let store = VersionStore::new(dir.path());
    let fetcher =
        Fetcher::with_parts(test_client(), vec![direct_line()]).with_api_url(server.url("/api"));

    let error = fetcher
        .fetch(&store, Some(Aria2Version::new(1, 30, 0)))
        .expect_err("上游没有该版本");
    assert!(
        matches!(error, ariahost_core::CoreError::ReleaseNotFound { .. }),
        "{error}"
    );
    assert_eq!(error.code(), None, "既有码里没有「上游没有该版本」");
}

#[test]
fn too_old_version_is_rejected_without_any_request() {
    let server = FakeServer::start(|_addr| vec![Route::json("/api", "[]")]);
    let dir = TestDir::new("tooold");
    let store = VersionStore::new(dir.path());
    let fetcher =
        Fetcher::with_parts(test_client(), vec![direct_line()]).with_api_url(server.url("/api"));

    let error = fetcher
        .fetch(&store, Some(Aria2Version::new(1, 18, 0)))
        .expect_err("低于 1.19.1 必须拒绝");
    assert_eq!(error.code(), Some(ErrorCode::CoreVersionTooOld), "{error}");
    assert!(server.hits().is_empty(), "低于最低版本不应发任何请求");
}

#[test]
fn whitelist_filters_debug_packages() {
    // API 只给了 -debug 包：白名单必须拒绝，且**不发起下载**。
    // 资产名按本机架构生成，好让「被拒的唯一理由」就是 `-debug` 后缀（而不是架构不匹配）。
    let debug_asset = format!("aria2-1.36.0-win-{}-build1-debug.zip", arch_tag());
    let server = FakeServer::start(|addr| {
        let asset_url = format!("http://{addr}/asset.zip");
        vec![Route::json(
            "/api",
            &api_json("1.36.0", &asset_url, 10, &debug_asset),
        )]
    });
    let dir = TestDir::new("debug");
    let store = VersionStore::new(dir.path());
    let fetcher =
        Fetcher::with_parts(test_client(), vec![direct_line()]).with_api_url(server.url("/api"));

    let error = fetcher
        .fetch(&store, Some(Aria2Version::new(1, 36, 0)))
        .expect_err("debug 包不应被选中");
    assert!(
        matches!(error, ariahost_core::CoreError::NoAssetForArch { .. }),
        "{error}"
    );
    assert_eq!(server.hits(), vec!["/api".to_owned()], "不应下载 debug 包");
}

// ---------------------------------------------------------------------------
// 下载/校验侧
// ---------------------------------------------------------------------------

#[test]
fn hash_mismatch_rejects_and_keeps_old_version() {
    // 1.37.0 有硬哈希：假 zip 必然不匹配 → 必须在装库前失败，旧版本与指针原封不动。
    let zip = build_core_zip(b"not-the-real-1.37.0");
    let server = FakeServer::start(|addr| {
        let asset_url = format!("http://{addr}/asset.zip");
        vec![
            Route::json(
                "/api",
                &api_json("1.37.0", &asset_url, zip.len(), &asset_name("1.37.0")),
            ),
            Route::file("/asset.zip", zip),
        ]
    });

    let dir = TestDir::new("hash");
    let store = VersionStore::new(dir.path());
    // 先装一个旧版并激活。
    let old = ariahost_core::ExtractedCore {
        aria2c: b"old-core".to_vec(),
        dlls: Vec::new(),
        skipped: Vec::new(),
    };
    store.install_core("1.35.0", &old).expect("装旧版");
    store.activate("1.35.0").expect("激活旧版");

    let fetcher =
        Fetcher::with_parts(test_client(), vec![direct_line()]).with_api_url(server.url("/api"));
    let error = fetcher
        .fetch(&store, Some(Aria2Version::new(1, 37, 0)))
        .expect_err("哈希不符必须拒绝");

    assert_eq!(
        error.code(),
        Some(ErrorCode::UpdatePackageInvalid),
        "{error}"
    );
    assert!(error.to_string().contains("SHA-256 不符"), "{error}");
    assert!(!store.is_installed("1.37.0"), "坏包不得进版本库");
    let pointer = store.active().expect("读指针").expect("有指针");
    assert_eq!(pointer.active, "1.35.0", "指针必须留在旧版");
}

#[test]
fn non_zip_payload_is_rejected() {
    let body = b"<html>not a zip</html>".to_vec();
    let server = FakeServer::start(|addr| {
        let asset_url = format!("http://{addr}/asset.zip");
        vec![
            Route::json(
                "/api",
                &api_json("1.36.0", &asset_url, body.len(), &asset_name("1.36.0")),
            ),
            // Content-Type 故意不是 html（否则会先被判 BadPayload），让魔数检查生效。
            Route::new("/asset.zip", 200, Some("application/octet-stream"), body),
        ]
    });
    let dir = TestDir::new("nonzip");
    let store = VersionStore::new(dir.path());
    let fetcher =
        Fetcher::with_parts(test_client(), vec![direct_line()]).with_api_url(server.url("/api"));

    let error = fetcher.fetch(&store, None).expect_err("非 zip 必须拒绝");
    assert_eq!(
        error.code(),
        Some(ErrorCode::UpdatePackageInvalid),
        "{error}"
    );
    assert!(store.list().expect("列版本").is_empty());
}

#[test]
fn size_mismatch_between_release_and_body_is_rejected() {
    let zip = build_core_zip(b"payload");
    let declared = zip.len() + 100; // API 声明的大小与实际不符
    let server = FakeServer::start(|addr| {
        let asset_url = format!("http://{addr}/asset.zip");
        vec![
            Route::json(
                "/api",
                &api_json("1.36.0", &asset_url, declared, &asset_name("1.36.0")),
            ),
            Route::file("/asset.zip", zip),
        ]
    });
    let dir = TestDir::new("size");
    let store = VersionStore::new(dir.path());
    let fetcher =
        Fetcher::with_parts(test_client(), vec![direct_line()]).with_api_url(server.url("/api"));

    let error = fetcher.fetch(&store, None).expect_err("大小不符必须拒绝");
    assert_eq!(
        error.code(),
        Some(ErrorCode::UpdatePackageInvalid),
        "{error}"
    );
    assert!(error.to_string().contains("不一致"), "{error}");
}

#[test]
fn first_line_failure_is_visible_in_warnings() {
    let zip = build_core_zip(b"core-bytes");
    let server = FakeServer::start(|addr| {
        let asset_url = format!("http://{addr}/asset.zip");
        vec![
            Route::json(
                "/api",
                &api_json("1.36.0", &asset_url, zip.len(), &asset_name("1.36.0")),
            ),
            Route::file("/asset.zip", zip),
        ]
    });
    let dir = TestDir::new("fallback");
    let store = VersionStore::new(dir.path());
    let lines = vec![dead_line("坏镜像"), direct_line()];
    let fetcher = Fetcher::with_parts(test_client(), lines).with_api_url(server.url("/api"));

    let outcome = fetcher.fetch(&store, None).expect("第二条线路应兜住");
    assert_eq!(outcome.line, "直连 GitHub");
    assert!(
        outcome.warnings.iter().any(|w| w.contains("坏镜像")),
        "降级必须可见：{:?}",
        outcome.warnings
    );
}

#[test]
fn all_lines_failed_reports_e6007() {
    let zip = build_core_zip(b"core-bytes");
    let server = FakeServer::start(|addr| {
        let asset_url = format!("http://{addr}/asset.zip");
        vec![
            Route::json(
                "/api",
                &api_json("1.36.0", &asset_url, zip.len(), &asset_name("1.36.0")),
            ),
            Route::file("/asset.zip", zip),
        ]
    });
    let dir = TestDir::new("allfail");
    let store = VersionStore::new(dir.path());
    let lines = vec![dead_line("线路 A"), dead_line("线路 B")];
    let fetcher = Fetcher::with_parts(test_client(), lines).with_api_url(server.url("/api"));

    let error = fetcher.fetch(&store, None).expect_err("两条线路都坏");
    assert_eq!(
        error.code(),
        Some(ErrorCode::AllSourcesUnavailable),
        "{error}"
    );
    match &error {
        ariahost_core::CoreError::AllLinesFailed { attempts } => {
            assert_eq!(attempts.len(), 2, "必须逐条保留失败线路");
            assert_eq!(attempts[0].line, "线路 A");
            assert_eq!(attempts[1].line, "线路 B");
            assert!(
                attempts[0].detail.contains("AH-E6001") || attempts[0].detail.contains("AH-E6003"),
                "detail 应带既有错误码：{:?}",
                attempts[0]
            );
        }
        other => panic!("期望 AllLinesFailed，实际 {other:?}"),
    }
}

#[test]
fn mirror_template_line_is_actually_used() {
    // 把镜像模板指向假服务器的 /mirror 前缀：证明 join() 出来的地址真的被请求到。
    let zip = build_core_zip(b"mirror-core");
    let server = FakeServer::start(|addr| {
        let asset_url = format!("http://{addr}/asset.zip");
        vec![
            Route::json(
                "/api",
                &api_json("1.36.0", &asset_url, zip.len(), &asset_name("1.36.0")),
            ),
            Route::file("/asset.zip", zip.clone()),
            // 镜像形态是「前缀 + 完整 GitHub URL」，这里注册 /mirror/ 前缀的等价路由。
            Route::file("/mirror/", zip),
        ]
    });
    let dir = TestDir::new("mirror");
    let store = VersionStore::new(dir.path());
    let lines = vec![
        dead_line("直连不通"),
        DownloadLine::new("镜像 /mirror", format!("{}/mirror/{URL}", server.base())),
    ];
    let fetcher = Fetcher::with_parts(test_client(), lines).with_api_url(server.url("/api"));

    let outcome = fetcher.fetch(&store, None).expect("镜像线路应命中");
    assert_eq!(outcome.line, "镜像 /mirror");
    assert!(
        server
            .hits()
            .iter()
            .any(|path| path.starts_with("/mirror/")),
        "镜像前缀必须真的出现在请求路径里：{:?}",
        server.hits()
    );
}

/// 合法 zip，但条目里**没有** `aria2c.exe`（替换包 / 错包形态）。
fn zip_without_core() -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    writer
        .start_file("README.txt", options)
        .expect("start_file");
    writer.write_all(b"no core here").expect("write");
    writer.finish().expect("finish").into_inner()
}

/// M4-0 验收第 5 条：CDN 线路吃到坏包必须**被拦下并自动降到下一线路**。
///
/// 五种形态逐条走同一个 fetch：前四种都在 CDN 上坏掉、必须落到 GitHub 直连成功；
/// 第五种（完全正常）则**留在 CDN**、不发生降级。
///
/// 「Content-Length 与实际字节不符」由 [`size_mismatch_between_release_and_body_is_rejected`]
/// 单独覆盖（那需要假服务器故意谎报长度，`Route` 造不出来），这里用**截断的 zip** 替代表「下了一半」。
#[test]
fn bad_cdn_responses_all_degrade_to_github_line() {
    let good = build_core_zip(b"core-bytes-ok");
    // Cloudflare 404 页的量级（27,150 B，text/html）——正是 fail-closed 预检要拦的形态。
    let html_page = vec![b'<'; 27_150];
    // 合法魔数但被截断：等价于「下了一半」。
    let mut truncated = good.clone();
    truncated.truncate(good.len() / 2);

    // (标签, CDN 上的坏响应)
    let bad_forms: Vec<(&str, Route)> = vec![
        (
            "html404",
            Route::new("/cdn/asset.zip", 404, Some("text/html"), html_page),
        ),
        (
            "badmagic",
            Route::new(
                "/cdn/asset.zip",
                200,
                Some("application/octet-stream"),
                b"not a zip at all".to_vec(),
            ),
        ),
        (
            "truncated",
            Route::new(
                "/cdn/asset.zip",
                200,
                Some("application/octet-stream"),
                truncated,
            ),
        ),
        (
            "noexe",
            Route::new(
                "/cdn/asset.zip",
                200,
                Some("application/octet-stream"),
                zip_without_core(),
            ),
        ),
    ];

    for (tag, bad) in bad_forms {
        let serving = good.clone();
        let declared = good.len();
        let server = FakeServer::start(move |addr| {
            let asset_url = format!("http://{addr}/asset.zip");
            vec![
                Route::json(
                    "/api",
                    &api_json("1.36.0", &asset_url, declared, &asset_name("1.36.0")),
                ),
                bad,
                Route::file("/asset.zip", serving),
            ]
        });

        let (lines, order_warnings) = ariahost_core::lines_from("cdn,github", &server.url("/cdn/"));
        assert!(
            order_warnings.is_empty(),
            "{tag}: 合法顺序不该有告警：{order_warnings:?}"
        );
        assert_eq!(lines.len(), 2, "{tag}: 顺序解析应给出两条线路");

        let dir = TestDir::new(&format!("degrade-{tag}"));
        let store = VersionStore::new(dir.path());
        let fetcher = Fetcher::with_parts(test_client(), lines).with_api_url(server.url("/api"));

        let outcome = fetcher
            .fetch(&store, None)
            .unwrap_or_else(|error| panic!("{tag}: 坏 CDN 应降级成功，实际 {error}"));

        assert_eq!(
            outcome.line,
            ariahost_core::DIRECT_LINE_NAME,
            "{tag}: 必须已降级到 GitHub 直连线路"
        );
        assert!(
            !outcome.warnings.is_empty(),
            "{tag}: 降级必须留下可见告警（试过哪条线路、为什么失败）"
        );
        assert!(
            std::fs::read(&outcome.exe_path).expect("读 exe") == b"core-bytes-ok".to_vec(),
            "{tag}: 装上的必须是 GitHub 那份好包"
        );
    }

    // 第五种：CDN 正常 → 不得降级。
    let serving = good.clone();
    let declared = good.len();
    let server = FakeServer::start(move |addr| {
        let asset_url = format!("http://{addr}/asset.zip");
        vec![
            Route::json(
                "/api",
                &api_json("1.36.0", &asset_url, declared, &asset_name("1.36.0")),
            ),
            Route::file("/cdn/asset.zip", serving),
        ]
    });
    let (lines, order_warnings) = ariahost_core::lines_from("cdn,github", &server.url("/cdn/"));
    assert!(
        order_warnings.is_empty(),
        "合法顺序不该有告警：{order_warnings:?}"
    );
    assert_eq!(lines.len(), 2);
    let dir = TestDir::new("degrade-ok");
    let store = VersionStore::new(dir.path());
    let fetcher = Fetcher::with_parts(test_client(), lines).with_api_url(server.url("/api"));
    let outcome = fetcher.fetch(&store, None).expect("CDN 正常时应成功");
    assert_eq!(
        outcome.line,
        ariahost_core::CDN_LINE_NAME,
        "CDN 正常时不该降级"
    );
}

#[test]
fn default_lines_cdn_first_then_direct() {
    // 守住默认顺序（task-72 / Q17 拍板）：**自带 CDN 优先，官方直连兜底**。
    // 来由（真机实测）：用户网络下 `api.github.com` 与 `r.alusss.cn` 镜像都拿不到包，
    // CDN 是唯一可用线路；但 CDN 是第三方主机（可能挂/可能被劫），
    // 一旦它失败必须能退回官方直连，所以两条都在、顺序固定。
    let lines = default_lines();
    assert_eq!(lines.len(), 2, "默认只应有 cdn + github 两条：{lines:?}");
    assert_eq!(lines[0].name, "CDN ah.335772.xyz");
    assert_eq!(
        lines[0].template,
        format!("{}{FILE_PLACEHOLDER}", ariahost_core::DEFAULT_CDN_BASE)
    );
    assert_eq!(lines[1].name, "直连 GitHub");
    assert_eq!(lines[1].template, ariahost_core::URL_PLACEHOLDER);
}

#[test]
fn ci_lines_keep_direct_first() {
    // CI 侧（将来跑「应用内下载」时）的排序：直连优先、CDN 兜底（runner 在 GitHub 网络内）。
    // 注意：`release.yml` 目前**不调用** `ci_lines()`——随包核心由自带 PowerShell 步骤直连下载
    // 并对 zip 做 SHA-256 硬校验，本测试只钉住「排序契约」，不代表发布链路已接入。
    let lines = ci_lines();
    assert_eq!(lines[0].name, "直连 GitHub");
    assert_eq!(lines[1].name, "CDN ah.335772.xyz");
}

#[test]
fn hash_state_verified_only_when_known_value_matches() {
    // 直接用已知的 1.37.0 交叉参考值构造不了真包，所以这里只验证「状态语义」：
    // 已知版本 + 任意内容 → 必是拒绝（前面的 hash_mismatch 用例已覆盖），
    // 未知版本 → Unverified 且带原因。
    let (state, warnings) =
        ariahost_core::verify_hash(Aria2Version::new(1, 30, 0), 64, b"anything")
            .expect("未校验不是错误");
    assert!(matches!(state, HashState::Unverified { .. }));
    assert_eq!(warnings.len(), 1);
}
