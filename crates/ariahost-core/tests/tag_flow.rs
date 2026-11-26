//! 手工 tag 取核心的端到端测试：线程内假 HTTP 服务器 → [`probe_tag_asset`] / [`fetch_by_tag`]。
//!
//! **不依赖真实网络**：所有请求打到本进程内起的 `TcpListener`，并且断言 `hits` 里
//! 确实出现过 `/https://github.com/aria2/...` 这种「代理前缀 + 完整 GitHub URL」，
//! 证明 URL 形状就是我们想要的那一条，而不是被某个真地址兜住。
//!
//! 覆盖：build1 404 → build2 命中的真实请求形状 / 全 404 的专门错误 / 5xx 立即停 /
//! 非法 tag **零请求** / HTML 假 zip 归 BadPayload（不给换代理 hint）/ 魔数不符归 BadPayload /
//! 哈希不符拒装且不留版本 / `--version` 不符拒装且不激活 / 成功路径七步全过并激活。

use std::io::{Cursor, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use ariahost_common::config::CoreProxyMode;
use ariahost_core::net::hint_of;
use ariahost_core::{
    fetch_by_tag_tracked, host_bits, probe_tag_asset, probe_tag_asset_tracked, AssetAttemptOutcome,
    CoreError, CoreVersionProbe, Fetcher, HttpClient, HttpOptions, TagError, VersionStore,
    CORE_EXE,
};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

// ---------------------------------------------------------------------------
// 假 HTTP 服务器（只服务本机回环；HEAD 只回头不写 body）
// ---------------------------------------------------------------------------

struct Route {
    prefix: String,
    status: u16,
    content_type: Option<String>,
    body: Vec<u8>,
}

impl Route {
    fn zip(prefix: &str, body: Vec<u8>) -> Self {
        Self {
            prefix: prefix.to_owned(),
            status: 200,
            content_type: Some("application/zip".to_owned()),
            body,
        }
    }

    fn html(prefix: &str) -> Self {
        Self {
            prefix: prefix.to_owned(),
            status: 200,
            content_type: Some("text/html; charset=utf-8".to_owned()),
            body: b"<html><body>404 Not Found</body></html>".to_vec(),
        }
    }

    fn plain(prefix: &str, body: &str) -> Self {
        Self {
            prefix: prefix.to_owned(),
            status: 200,
            content_type: Some("text/plain".to_owned()),
            body: body.as_bytes().to_vec(),
        }
    }

    fn status(prefix: &str, status: u16) -> Self {
        Self {
            prefix: prefix.to_owned(),
            status,
            content_type: Some("text/plain".to_owned()),
            body: format!("status {status}").into_bytes(),
        }
    }
}

struct FakeServer {
    addr: SocketAddr,
    hits: Arc<Mutex<Vec<String>>>,
    errors: Arc<Mutex<Vec<String>>>,
}

impl FakeServer {
    fn start<F>(build: F) -> Self
    where
        F: FnOnce() -> Vec<Route>,
    {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("绑定假服务器失败");
        let addr = listener.local_addr().expect("取地址失败");
        let routes = Arc::new(build());
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
                match read_request(&mut stream) {
                    Ok((method, path)) => {
                        hits_thread.lock().expect("锁").push(path.clone());
                        let route = routes.iter().find(|route| path.starts_with(&route.prefix));
                        match route {
                            Some(route) => write_response(&mut stream, &method, route),
                            None => write_response(
                                &mut stream,
                                &method,
                                &Route {
                                    prefix: "/".to_owned(),
                                    status: 404,
                                    content_type: Some("text/plain".to_owned()),
                                    body: b"not found".to_vec(),
                                },
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

    fn hits(&self) -> Vec<String> {
        self.hits.lock().expect("锁").clone()
    }

    fn errors(&self) -> Vec<String> {
        self.errors.lock().expect("锁").clone()
    }
}

fn read_request(stream: &mut TcpStream) -> std::io::Result<(String, String)> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 2048];
    loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
            let line = head.lines().next().unwrap_or_default();
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or_default().to_owned();
            let path = parts.next().unwrap_or_default();
            return Ok((
                method,
                path.split('?').next().unwrap_or_default().to_owned(),
            ));
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

fn write_response(stream: &mut TcpStream, method: &str, route: &Route) {
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
    // HEAD 只回头：ureq 对 HEAD 不读 body，这里也不发，避免「Content-Length 与实发不符」。
    if method != "HEAD" {
        let _ = stream.write_all(&route.body);
    }
    let _ = stream.flush();
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        404 => "Not Found",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

struct TestDir(PathBuf);

static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

impl TestDir {
    fn new(tag: &str) -> Self {
        let sequence = DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ariahost-core-tagflow-{tag}-{}-{sequence}",
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

fn build_core_zip(payload: &[u8]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    writer.start_file(CORE_EXE, options).expect("start_file");
    writer.write_all(payload).expect("写条目");
    writer.finish().expect("finish").into_inner()
}

/// 与生产选择器同源的架构标签（`32bit` / `64bit`）。
fn arch_tag() -> &'static str {
    match std::env::consts::ARCH {
        "x86" => "32bit",
        "x86_64" => "64bit",
        other => panic!("tag_flow 未覆盖的目标架构：{other}"),
    }
}

/// 假服务器上的一条资产路径（**代理前缀 + 完整 GitHub URL**）。
fn asset_path(version: &str, build: u32) -> String {
    format!(
        "/https://github.com/aria2/aria2/releases/download/release-{version}/aria2-{version}-win-{}-build{build}.zip",
        arch_tag()
    )
}

fn test_client() -> HttpClient {
    HttpClient::with_options(HttpOptions {
        allow_plain_http: true,
        ..HttpOptions::default()
    })
}

/// 指向假服务器的 Fetcher：`net.core_proxy_mode=proxy` + 前缀基址 = 假服务器根。
fn fetcher_for(server: &FakeServer) -> Fetcher {
    Fetcher::with_parts(test_client(), Vec::new())
        .with_core_proxy(CoreProxyMode::Proxy, format!("{}/", server.base()))
}

/// 注入的版本探测：不真跑 exe，只回一个固定版本。
struct FixedProbe(&'static str);

impl CoreVersionProbe for FixedProbe {
    fn probe_version(&self, exe: &Path) -> Result<String, String> {
        assert!(exe.is_file(), "探测前 exe 必须已落盘：{}", exe.display());
        Ok(self.0.to_owned())
    }
}

// ---------------------------------------------------------------------------
// 探测：build-N
// ---------------------------------------------------------------------------

#[test]
fn probe_hits_build2_after_build1_is_404_and_reports_the_exact_url() {
    let server = FakeServer::start(|| {
        vec![Route::zip(
            &asset_path("1.36.0", 2),
            build_core_zip(b"core"),
        )]
    });
    let fetcher = fetcher_for(&server);

    let asset = probe_tag_asset(&fetcher, "1.36.0").expect("build2 应命中");

    assert_eq!(asset.tag, "1.36.0");
    assert_eq!(asset.release_tag, "release-1.36.0");
    assert_eq!(asset.build, 2);
    assert_eq!(asset.bits, host_bits());
    assert_eq!(
        asset.url,
        format!("{}{}", server.base(), asset_path("1.36.0", 2)),
        "命中 URL 必须是「代理前缀 + 完整 GitHub URL」"
    );
    assert_eq!(asset.line.as_str(), "proxy");
    // 两个 build 都试过：build1 是 404（继续），build2 命中。
    assert_eq!(asset.attempts.len(), 2);
    assert_eq!(asset.attempts[0].build, 1);
    assert_eq!(asset.attempts[0].status, Some(404));
    assert_eq!(asset.attempts[0].outcome, AssetAttemptOutcome::Missing);
    assert_eq!(asset.attempts[1].build, 2);
    assert_eq!(asset.attempts[1].outcome, AssetAttemptOutcome::Hit);

    let hits = server.hits();
    assert_eq!(hits.len(), 2, "build1 / build2 各一次：{hits:?}");
    assert!(hits[0].ends_with("build1.zip"), "{hits:?}");
    assert!(hits[1].ends_with("build2.zip"), "{hits:?}");
    assert!(server.errors().is_empty(), "{:?}", server.errors());
}

#[test]
fn probe_stops_at_the_first_non_404_status() {
    let server = FakeServer::start(|| vec![Route::status(&asset_path("1.36.0", 1), 500)]);
    let fetcher = fetcher_for(&server);

    let error = probe_tag_asset(&fetcher, "1.36.0").expect_err("500 必须报错");
    assert!(
        matches!(error, CoreError::HttpStatus { status: 500 }),
        "{error}"
    );
    assert_eq!(
        server.hits().len(),
        1,
        "非 404 必须立即停，不许再试 build2：{:?}",
        server.hits()
    );
}

#[test]
fn probe_all_404_is_a_dedicated_error_not_a_network_one() {
    let server = FakeServer::start(Vec::new);
    let fetcher = fetcher_for(&server);

    let error = probe_tag_asset(&fetcher, "1.36.0").expect_err("全 404 必须报错");
    match &error {
        CoreError::NoReleaseAsset { tag, bits, builds } => {
            assert_eq!(tag, "1.36.0");
            assert_eq!(*bits, host_bits());
            assert_eq!(*builds, 3);
        }
        other => panic!("应是 NoReleaseAsset，实际 {other:?}"),
    }
    assert_eq!(error.code(), None, "这不是网络错误，不该套 AH-E6001");
    let message = error.to_string();
    assert!(message.contains("没有"), "{message}");
    assert!(message.contains("releases"), "要给出可核对地址：{message}");
    assert_eq!(server.hits().len(), 3, "三个 build 都要试过");
}

#[test]
fn probe_unreachable_line_is_a_network_error() {
    // 占一个端口再释放：连接必被拒。
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("占端口");
    let addr = listener.local_addr().expect("取地址");
    drop(listener);

    let mut attempts = Vec::new();
    let error = probe_tag_asset_tracked(
        &ariahost_core::net::HttpProber::with_client(test_client()),
        CoreProxyMode::Proxy,
        &format!("http://{addr}/"),
        "1.36.0",
        &mut attempts,
    )
    .expect_err("连接被拒必须报错");

    assert!(
        matches!(error, CoreError::NetworkUnreachable { .. }),
        "{error}"
    );
    assert_eq!(attempts.len(), 1, "传输失败要立即停");
    assert_eq!(attempts[0].outcome, AssetAttemptOutcome::TransportError);
    let hint = hint_of(&error, CoreProxyMode::Proxy);
    assert!(hint.contains("更换代理地址"), "{hint}");
}

#[test]
fn invalid_tag_never_sends_a_single_request() {
    let server =
        FakeServer::start(|| vec![Route::zip(&asset_path("1.37.0", 1), build_core_zip(b"x"))]);
    let dir = TestDir::new("invalid-tag");
    let store = VersionStore::new(dir.path());
    let fetcher = fetcher_for(&server);

    let mut steps = Vec::new();
    let error = fetch_by_tag_tracked(
        &fetcher,
        &store,
        "../etc/passwd",
        &FixedProbe("1.37.0"),
        &mut steps,
    )
    .expect_err("路径穿越必须拒绝");

    assert!(matches!(error, CoreError::BadTag { .. }), "{error}");
    assert!(
        server.hits().is_empty(),
        "非法 tag 绝不发起请求：{:?}",
        server.hits()
    );
    assert_eq!(
        ariahost_core::normalize_tag("../etc/passwd").expect_err("归一化也必须拒绝"),
        TagError::PathTraversal
    );
    assert!(
        steps.is_empty(),
        "第一步就失败，不该有任何 G5 步骤：{steps:?}"
    );
}

// ---------------------------------------------------------------------------
// G5 拒装
// ---------------------------------------------------------------------------

#[test]
fn html_payload_is_rejected_as_bad_payload_without_proxy_hint() {
    let server = FakeServer::start(|| vec![Route::html(&asset_path("1.36.0", 1))]);
    let dir = TestDir::new("html-payload");
    let store = VersionStore::new(dir.path());
    let fetcher = fetcher_for(&server);

    let error = fetch_by_tag_tracked(
        &fetcher,
        &store,
        "1.36.0",
        &FixedProbe("1.36.0"),
        &mut Vec::new(),
    )
    .expect_err("HTML 假 zip 必须拒装");

    assert!(matches!(error, CoreError::BadPayload { .. }), "{error}");
    // 关键：这是载荷问题，**不该建议换代理**（换了只会更糟）。
    let hint = hint_of(&error, CoreProxyMode::Proxy);
    assert!(!hint.contains("更换代理地址"), "{hint}");
    assert!(!hint.contains("切到代理"), "{hint}");
    assert!(store.list().expect("列版本").is_empty(), "拒装不得留下版本");
    assert!(store.active().expect("读指针").is_none(), "拒装不得改指针");
}

#[test]
fn non_zip_body_is_rejected_as_bad_payload() {
    let server = FakeServer::start(|| vec![Route::plain(&asset_path("1.36.0", 1), "not a zip")]);
    let dir = TestDir::new("non-zip");
    let store = VersionStore::new(dir.path());
    let fetcher = fetcher_for(&server);

    let error = fetch_by_tag_tracked(
        &fetcher,
        &store,
        "1.36.0",
        &FixedProbe("1.36.0"),
        &mut Vec::new(),
    )
    .expect_err("非 zip 必须拒装");

    assert!(
        matches!(error, CoreError::BadPayload { .. }),
        "魔数不符归 BadPayload（与线路无关）：{error}"
    );
    let message = error.to_string();
    assert!(message.contains("PK"), "错误里要写出期望的魔数：{message}");
    assert!(store.list().expect("列版本").is_empty());
}

#[test]
fn hash_mismatch_is_rejected_before_install() {
    // 1.37.0 有已知 SHA-256；假 zip 一定对不上 → 必须在装库前拒。
    let server = FakeServer::start(|| {
        vec![Route::zip(
            &asset_path("1.37.0", 1),
            build_core_zip(b"wrong"),
        )]
    });
    let dir = TestDir::new("hash-mismatch");
    let store = VersionStore::new(dir.path());
    let fetcher = fetcher_for(&server);

    let mut steps = Vec::new();
    let error = fetch_by_tag_tracked(
        &fetcher,
        &store,
        "1.37.0",
        &FixedProbe("1.37.0"),
        &mut steps,
    )
    .expect_err("哈希不符必须拒装");

    assert!(matches!(error, CoreError::PackageInvalid { .. }), "{error}");
    assert_eq!(
        error.code(),
        Some(ariahost_common::ErrorCode::UpdatePackageInvalid)
    );
    assert!(error.to_string().contains("SHA-256 不符"), "{error}");
    assert!(
        store.list().expect("列版本").is_empty(),
        "哈希不符不得留下版本"
    );
    // 步骤只到「下载后预检」，没有走到 sha256 之后的步骤。
    let names: Vec<_> = steps.iter().map(|step| step.name).collect();
    assert!(names.contains(&"1-magic"), "{names:?}");
    assert!(!names.contains(&"3-extract"), "{names:?}");
}

#[test]
fn version_mismatch_is_rejected_and_nothing_is_activated() {
    let server = FakeServer::start(|| {
        vec![Route::zip(
            &asset_path("1.36.0", 1),
            build_core_zip(b"core"),
        )]
    });
    let dir = TestDir::new("version-mismatch");
    let store = VersionStore::new(dir.path());
    let fetcher = fetcher_for(&server);

    // 包说是 1.36.0，但 exe 自己报 1.35.0 → 包被替换/名字对不上，必须拒装。
    let mut steps = Vec::new();
    let error = fetch_by_tag_tracked(
        &fetcher,
        &store,
        "1.36.0",
        &FixedProbe("1.35.0"),
        &mut steps,
    )
    .expect_err("版本不符必须拒装");

    match &error {
        CoreError::VersionMismatch { expected, actual } => {
            assert_eq!(expected, "1.36.0");
            assert_eq!(actual, "1.35.0");
        }
        other => panic!("应是 VersionMismatch，实际 {other:?}"),
    }
    assert_eq!(
        error.code(),
        Some(ariahost_common::ErrorCode::UpdatePackageInvalid)
    );
    assert!(store.list().expect("列版本").is_empty(), "版本不符不得装库");
    assert!(
        store.active().expect("读指针").is_none(),
        "版本不符不得激活"
    );
    let names: Vec<_> = steps.iter().map(|step| step.name).collect();
    assert_eq!(
        names,
        vec!["0-asset", "1-magic", "2-sha256", "3-extract"],
        "{names:?}"
    );
}

// ---------------------------------------------------------------------------
// 成功路径
// ---------------------------------------------------------------------------

#[test]
fn fetch_by_tag_installs_and_activates_after_all_steps() {
    // 1.36.0 没有已知哈希 → Unverified（带告警）继续，这是设计要的诚实口径。
    let server = FakeServer::start(|| {
        vec![Route::zip(
            &asset_path("1.36.0", 1),
            build_core_zip(b"core-bytes"),
        )]
    });
    let dir = TestDir::new("install-ok");
    let store = VersionStore::new(dir.path());
    let fetcher = fetcher_for(&server);

    let mut steps = Vec::new();
    let install = fetch_by_tag_tracked(
        &fetcher,
        &store,
        "1.36.0",
        &FixedProbe("1.36.0"),
        &mut steps,
    )
    .expect("各步全过应成功");

    assert_eq!(install.version, "1.36.0");
    assert_eq!(install.build, 1);
    assert_eq!(install.line.as_str(), "proxy");
    assert_eq!(install.probed_version, "1.36.0");
    assert!(
        !install.hash.is_verified(),
        "没有交叉参考值就不许声称校验过"
    );
    assert!(
        install
            .warnings
            .iter()
            .any(|warning| warning.contains("未校验哈希")),
        "{:?}",
        install.warnings
    );
    assert_eq!(
        steps.iter().map(|step| step.name).collect::<Vec<_>>(),
        vec![
            "0-asset",
            "1-magic",
            "2-sha256",
            "3-extract",
            "4-version",
            "5-install",
            "6-activate"
        ]
    );
    assert!(steps.iter().all(|step| step.ok), "{steps:?}");

    // 装到版本库并激活。
    assert!(store.is_installed("1.36.0"));
    assert_eq!(
        std::fs::read(store.exe_path("1.36.0")).expect("读 exe"),
        b"core-bytes"
    );
    let pointer = store.active().expect("读指针").expect("必须有指针");
    assert_eq!(pointer.active, "1.36.0");
    assert_eq!(pointer.previous, None);
    assert_eq!(install.exe_path, store.exe_path("1.36.0"));
    // 版本探测用的临时目录必须清干净。
    let leftovers: Vec<_> = std::fs::read_dir(store.root())
        .expect("读根")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".verify-"))
        .collect();
    assert!(leftovers.is_empty(), "验证目录必须删干净：{leftovers:?}");
}

#[test]
fn fetch_by_tag_all_404_leaves_the_version_store_untouched() {
    let server = FakeServer::start(Vec::new);
    let dir = TestDir::new("no-asset");
    let store = VersionStore::new(dir.path());
    let fetcher = fetcher_for(&server);

    let error = fetch_by_tag_tracked(
        &fetcher,
        &store,
        "9.9.9",
        &FixedProbe("9.9.9"),
        &mut Vec::new(),
    )
    .expect_err("没有发布包必须拒装");
    assert!(matches!(error, CoreError::NoReleaseAsset { .. }), "{error}");
    assert!(store.list().expect("列版本").is_empty());
    assert!(store.active().expect("读指针").is_none());
    assert_eq!(server.errors(), Vec::<String>::new());
}
