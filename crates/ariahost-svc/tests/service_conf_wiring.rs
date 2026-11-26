//! M5-f 端到端接线测试：**用户配置的下载目录**经服务侧护栏后，进到渲染出的 `aria2.conf`。
//!
//! 为什么要有这一条：`download_dir_absolute` / `render_conf` 各自都有单测，但那个 P0 的
//! 本质是「**服务侧调错了入口**」——所以真正需要钉住的是这条链路：
//! `config（用户那份）→ download_dir_for_service（护栏）→ render_conf 的 dir= 行`。
//!
//! 用的是写死的假配置与假路径，不读本机环境；Linux 门禁可跑。

use std::path::Path;

use ariahost_common::config::{
    download_dir_for_service_with, download_dir_for_service_with_guards, Config, ConfigErrorKind,
    DlSection, ServiceDirGuards,
};
use ariahost_core::runtime::{render_conf, RuntimePaths};

fn config_with_dir(dir: &str) -> Config {
    Config {
        dl: DlSection {
            dir: dir.to_owned(),
            ..DlSection::default()
        },
        ..Config::default()
    }
}

fn guards() -> ServiceDirGuards {
    ServiceDirGuards {
        system_root: Some("C:\\Windows".into()),
        program_data: Some("C:\\ProgramData".into()),
    }
}

fn paths() -> RuntimePaths {
    RuntimePaths::under(Path::new("C:\\ProgramData\\AriaHost"))
}

fn dir_line(conf: &str) -> String {
    conf.lines()
        .find(|line| line.starts_with("dir="))
        .expect("渲染结果必须含 dir=")
        .to_owned()
}

/// 验收 2 的直接证据：`dir=` 等于**用户设置**的目录（不是 systemprofile、不是默认值）。
#[test]
fn user_configured_dir_reaches_the_rendered_aria2_conf() {
    let config = config_with_dir("D:\\Downloads\\aria2");
    let resolved = download_dir_for_service_with(&config, &guards()).expect("用户目录必须放行");
    let conf = render_conf(&config, &paths(), "test-secret", &resolved);

    let line = dir_line(&conf);
    println!("CONF_DIR_LINE {line}");
    assert_eq!(
        line, "dir=D:\\Downloads\\aria2",
        "服务必须下发用户设置的目录"
    );

    // 顺手证明 conf 里没有把 secret 写成明文键（secret 由命令行注入，这是既有设计）。
    assert!(
        !conf.contains("rpc-secret="),
        "rpc-secret 不该出现在 aria2.conf 里"
    );
}

/// 用户把目录设成默认的 `~/Downloads`：展开用**注入的主目录**，并且真的进了守卫。
///
/// 这一条以前是「假绿」：它拿当前进程的 `~` 展开结果去比字符串，Linux 门禁上
/// 恒真、且**守卫根本没被执行到**（Windows 形态路径在 Linux 上不会被展开）。
/// 现在走注入式入口 `download_dir_for_service_with_guards(config, guards, home)`：
/// 注入 SYSTEM 形态的主目录 → 必须被拒；注入普通用户主目录 → 必须放行。
#[test]
fn tilde_dir_expands_through_the_injected_home_and_really_hits_the_guard() {
    let config = config_with_dir("~/Downloads/aria2");

    // ① 服务身份的主目录 = SYSTEM 的 profile → `~` 展开后必须被守卫拦下。
    let system_profile = Path::new(r"C:\WINDOWS\system32\config\systemprofile");
    let error = download_dir_for_service_with_guards(&config, &guards(), Some(system_profile))
        .expect_err("服务身份下 ~ 展开到 systemprofile，必须拒");
    assert!(
        error.to_string().contains("systemprofile"),
        "错误里要点名 systemprofile：{error}"
    );
    println!("INJECTED_HOME_REFUSED {error}");

    // ② 普通用户主目录 → 放行，且结果就是那个绝对路径。
    let user_home = Path::new(r"C:\Users\alice");
    let resolved = download_dir_for_service_with_guards(&config, &guards(), Some(user_home))
        .expect("用户主目录下的 ~ 必须放行");
    assert_eq!(
        resolved,
        std::path::PathBuf::from(r"C:\Users\alice\Downloads\aria2")
    );
    println!("INJECTED_HOME_DIR {}", resolved.display());
}

/// 反过来：护栏命中的目录在**渲染之前**就被拒，绝不会写进 conf。
#[test]
fn forbidden_dir_is_refused_before_any_conf_is_rendered() {
    let config = config_with_dir("C:\\WINDOWS\\system32\\config\\systemprofile\\Downloads");
    let error = download_dir_for_service_with(&config, &guards()).expect_err("必须拒绝");
    assert_eq!(error.kind(), ConfigErrorKind::ForbiddenDownloadDir);
    println!("REFUSED {error}");
}
