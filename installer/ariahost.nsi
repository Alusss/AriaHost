; ============================================================================
;  AriaHost 安装脚本（NSIS 3.x / Unicode）
;
;  编译（**只在 Windows 上编译**；CI 与本地一致）：
;      makensis /DVERSION=0.1.0 /DVI_VERSION=0.1.0.0 /DARCH=x64 ^
;               /DSRCDIR=<repo>\dist\x64 /DOUTFILE=<repo>\installer\AriaHost-0.1.0-x64-setup.exe ^
;               installer\ariahost.nsi
;  产物（per-user，首版唯一发布形态）：installer\AriaHost-<ver>-x64-setup.exe
;
;  ---- 编译期路径规则（改动前必读；2026-11-25 实测结论）---------------------
;  Windows 版 makensis 的 `File` **只接受全反斜杠的绝对路径**。实测探针
;  （windows-latest，run 45452855606）：
;    File "D:\a\w\dist\x64\ariahost.exe"   → ✅ 出包
;    File "D:/a/w/dist/x64/ariahost.exe"   → ❌ File: ... -> no files found
;    File "dist\x64\ariahost.exe"（相对）   → ❌ 找不到（相对路径按**脚本所在目录**解析）
;  因此：
;    * **编译期路径（File、!if /FileExists）一律用 "\"，且优先用绝对路径**（来自 ${SRCDIR}）。
;    * 早期版本写的「用 / 分隔以便 Linux 也能编」是**已被证伪的错误结论**：Linux 版 makensis
;      接受 "/"，Windows 版不接受 → 造成「Linux stub 编译绿、Windows 真实打包红」的假绿。
;      本脚本现在只在 Windows 上编译（ci.yml 的 windows job、release.yml 的 build job），
;      ubuntu 的 nsi-syntax stub job 已于 2026-11-25 删除。
;    * 运行期字符串（$INSTDIR\…、$APPDATA\…）保持 "\"，它们只在 Windows 上执行。
;
;  !! 验证状态：**已在 Windows 上真实编译通过**（windows-latest + 预装 NSIS 3.x，
;     见 ci.yml 的 windows job「安装脚本编译门禁」与 release.yml 的 build job；
;     Release run 45454312274 用它出过包）。
;     已验证：语法/宏/MUI2 页面、Section ID 前向引用、UTF-8 无 BOM + /INPUTCHARSET UTF8
;     的中文文案、${SRCDIR} 载荷路径、编译通过即可出 setup.exe。
;     **未验证（M3 批次全部在此列）**：真机安装/卸载、UAC 提权路径（runas 的
;     回执文件能否读到）、服务注册/注销的真实结果、AUMID 是否真的让 Toast 显示、
;     /S 静默安装、per-user 与 /DPER_MACHINE 的运行时差异。
;     人工自检清单见 docs/dev/release.md §5。
;
;  ---- M3 批次（2026-11-25）改了什么 --------------------------------------
;     A. 组件页新增可选「同时注册为 Windows 服务」：默认不勾；勾选后 per-user 包
;        用 runas 调 --install-service（单次 UAC），**等真实退出码**，失败只提示
;        不中止安装（Function RegisterServiceIfRequested）。
;     B. 每个快捷方式建完都写 AUMID（!macro WriteAumidFor），失败只记日志。
;     C. 卸载顺序固定为：注销服务 → 停 ariahost（等 2s 收 aria2c）→ 删快捷方式/
;        注册表 → 删 $INSTDIR → 用户数据默认保留（问过才删）。
;     D. 升级停进程改成「--quit + 每 250ms 真实复查，最多约 10s」，不再 Sleep 假同步。
;     另：删除 A6 的 HidePerUserServiceSection（per-user 现在可以注册服务）。
;
;     历史修复（2026-11-23 独立审计 .scratch/wasm-design/review-exec-v2.md，静态）：
;     ① ${SEC_SERVICE} 曾在 .onInit（Section 之前）被引用；② 卸载段曾 Call 无 un.
;     前缀的普通函数；③ WinVer 下限从 ${AtLeastWin10}（10.0.10240）改为
;     ${AtLeastBuild} 17763（Server 2016 必须拒装，AH-E1001）。
;     2026-11-25 实测修复：File 的正斜杠路径在 Windows makensis 下必然失败 → 改回 "\"
;     （首发 tag v0.1.0 的 release run 45451494558 就是死在这里）。
;
;  ---- 两种编译变体（架构 §4.10）------------------------------------------
;  默认 per-user：RequestExecutionLevel user、装到 %LOCALAPPDATA%\Programs\AriaHost、
;               全程免 UAC；服务组件在组件页**隐藏**（R9：per-user 建不了服务）。
;  加 /DPER_MACHINE：admin、装到 %PROGRAMFILES64%\AriaHost、HKLM，
;               服务组件可勾选并直接注册（已是管理员，不再额外 UAC）。
;               首版不发布此变体，M4 进 release matrix；产物名带 -machine- 后缀。
;
;  ---- CLI 契约（架构 §7.1 R9，由 M1 的 ariahost-cli 实现）----------------
;    ariahost.exe --install-service          一次 UAC 提权助手：建 ProgramData、
;                                            复制版本库、CreateService（幂等）
;    ariahost.exe --uninstall-service        一次 UAC：StopService→DeleteService→清 ProgramData
;    ariahost.exe --write-aumid <lnk>        自写 IPropertyStore 写 System.AppUserModel.ID
;                                            （AUMID 值 AriaHost.Desktop，实现在 exe 内）
;    ariahost.exe --quit                     通知已运行实例退出（停止链内完成）
;  本脚本假设的 --quit 退出码（M1 定稿时确认，若不同只改 QuitRunningInstance）：
;    0=目标实例已退出 / 1=没有运行中的实例 / 2=超时未退出（其余非 0 同样按失败处理）
;
;  注意：本脚本**不弹** aria2 来源页（v2.0 决定），也不分发 aria2c.exe；
;  dist\<arch>\ 里存在 aria2c.exe 时才会一并安装（打包口径见 docs/dev/release.md §7）。
;
;  图标管线（2026-11-25 落地）：installer\ariahost.ico 是 7 尺寸
;  （16/24/32/48/64/128/256）、32bpp、**非 PNG 压缩**（BMP 条目）的 ICO，由
;  assets/branding/ariahost-icon-256.png 经 Pillow 生成（见 docs/dev/build.md）。编译期一律以
;  ${SRCDIR}\ariahost.ico 引用（CI 会先把 ico 拷进 SRCDIR）；运行期落盘到 $INSTDIR，
;  开始菜单/桌面快捷方式与 setup.exe 自身图标都指向它。
; ============================================================================

Unicode true
ManifestDPIAware true
ManifestSupportedOS all
SetCompressor /SOLID lzma
SetCompressorDictSize 32

!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"
!include "x64.nsh"
!include "WinVer.nsh"
; Sections.nsh 提供 SectionSetText / SectionSetFlags 等（M3 起本脚本已不再隐藏
; 任何组件，per-user 也能注册服务，故这里**没有** SectionSetText 调用；include
; 保留是因为后续若要做「组件置灰」仍会用到，且该文件有 include guard、成本为 0）。
!include "Sections.nsh"

; ---- 版本注入（全部由 makensis /D 传入，脚本内不硬编码版本）---------------
!ifndef VERSION
  !define VERSION "0.0.0-dev"
!endif
!ifndef VI_VERSION
  !define VI_VERSION "0.0.0.0"
!endif
!ifndef ARCH
  !define ARCH "x64"
!endif
!ifndef SRCDIR
  ; 默认值仅供本地手工编译：相对路径按**本脚本所在目录**解析，故用 "..\dist\x64"。
  ; CI 一律用 /DSRCDIR 传绝对路径（见文件头「编译期路径规则」）。
  !define SRCDIR "..\dist\x64"
!endif

; ---- 产品常量（改名/改路径只需改这一段）----------------------------------
!define APP_NAME       "AriaHost"
!define APP_EXE        "ariahost.exe"
!define APP_PUBLISHER  "AriaHost"
!define SVC_NAME       "AriaHost"
!define REG_APP_KEY    "Software\AriaHost"
!define REG_UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\AriaHost"
!define SVC_REG_KEY    "SYSTEM\CurrentControlSet\Services\${SVC_NAME}"

!ifdef PER_MACHINE
  RequestExecutionLevel admin
  !define INSTALL_ROOT "$PROGRAMFILES64"          ; x86 全机变体（M4）应改 $PROGRAMFILES32
  !define REG_ROOT     HKLM
!else
  RequestExecutionLevel user
  !define INSTALL_ROOT "$LOCALAPPDATA\Programs"
  !define REG_ROOT     HKCU
!endif

; A7：两个变体同为 x64 时不能同名，否则发布的包互相覆盖（架构 §3.3 安装包命名）
!ifndef OUTFILE
  !ifdef PER_MACHINE
    !define OUTFILE "AriaHost-${VERSION}-${ARCH}-machine-setup.exe"
  !else
    !define OUTFILE "AriaHost-${VERSION}-${ARCH}-setup.exe"
  !endif
!endif

Name "${APP_NAME} ${VERSION}"
OutFile "${OUTFILE}"
InstallDir "${INSTALL_ROOT}\${APP_NAME}"
InstallDirRegKey ${REG_ROOT} "${REG_APP_KEY}" "InstallDir"
ShowInstDetails show
ShowUninstDetails show
BrandingText "${APP_NAME} ${VERSION} (${ARCH})"

; 版本信息资源块。
; - LegalCopyright 已按 MIT OR Apache-2.0 双许可填写（架构 §13 S1 已定：2026-11）。
; - 首版安装包**不含许可页 / EULA 页**：双许可不需要用户先接受条款，属 Q13/S3 决定，
;   勿自行新增 MUI_LICENSEPAGE（加了会改变安装流程与验收 #1 口径）。
; - 版权行只放所有者署名，不要拼 aria2 的 GPLv2 声明（aria2 不由本安装包分发，见 release.md §7）。
VIProductVersion "${VI_VERSION}"
VIAddVersionKey "ProductName"     "${APP_NAME}"
VIAddVersionKey "FileDescription" "${APP_NAME} 安装程序"
VIAddVersionKey "FileVersion"     "${VERSION}"
VIAddVersionKey "ProductVersion"  "${VERSION}"
VIAddVersionKey "CompanyName"     "${APP_PUBLISHER}"
VIAddVersionKey "LegalCopyright"  "Copyright (c) 2026 Alusss"

; ---- 界面 -----------------------------------------------------------------
!define MUI_ABORTWARNING

; 安装器/卸载器图标：与 ariahost.exe 用的是**同一份**多尺寸 ICO
; （16/24/32/48/64/128/256，32bpp，由 assets/branding/ariahost-icon-256.png 生成）。
; 路径走 ${SRCDIR}（编译期绝对路径，CI 会先把 installer\ariahost.ico 拷进 SRCDIR）：
; 与下面 File 的载荷路径同一套规则，见文件头「编译期路径规则」。
!define MUI_ICON   "${SRCDIR}\ariahost.ico"
!define MUI_UNICON "${SRCDIR}\ariahost.ico"

!define MUI_WELCOMEPAGE_TITLE "${APP_NAME} 安装向导"
!define MUI_WELCOMEPAGE_TEXT "将安装 ${APP_NAME} ${VERSION}（${ARCH}）。$\r$\n$\r$\n${APP_NAME} 是 aria2 的 Windows 服务管理器：只通过子进程与 RPC 调度 aria2，不修改其源码、不链接其二进制。$\r$\n$\r$\n下一步可确认安装目录与可选组件。"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES

!ifndef PER_MACHINE
  !define MUI_FINISHPAGE_RUN "$INSTDIR\${APP_EXE}"
  !define MUI_FINISHPAGE_RUN_TEXT "立即启动 ${APP_NAME}"
!endif
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "SimpChinese"
!insertmacro MUI_LANGUAGE "English"

; setup.exe / uninst.exe 自身的图标。放在所有 MUI 宏**之后**：NSIS 的 Icon 是「最后一次生效」，
; 而 MUI2 会按 MUI_ICON 自己发一次 Icon；两处指向同一文件，这里放最后只是为了让结果确定。
Icon          "${SRCDIR}\ariahost.ico"
UninstallIcon "${SRCDIR}\ariahost.ico"

; ---- 通用：进程探测（tasklist，不依赖第三方插件）---------------------------
; 为什么用 tasklist 而不是 FindWindow：托盘实例没有可靠窗口类名/标题，
; 而"是否在跑"这件事只需要映像名。tasklist 找不到进程时**退出码仍为 0**，
; 所以只能看输出文本：/FO TABLE /NH 命中时首行就是 "<image>  <pid> ..."。
;
; 入参：!macro 参数 IMAGE（映像名）、LEN（映像名字符数，用于截取比较）
;       **传参不要带引号**（引号一律写在宏体内），避免"引号是否算参数一部分"的歧义：
;       调用写 `!insertmacro ProcessIsRunning ariahost.exe 12`。
; 出参：$R7 = "1" 在跑 / "0" 不在跑；$R4 首段文本、$R5 原始输出、$R6 退出码
; 约束：宏体不写 label（宏会被多处置入），也不改 $R0–$R3/$0–$9（调用方在用）。
!macro ProcessIsRunning IMAGE LEN
  nsExec::ExecToStack 'tasklist /FI "IMAGENAME eq ${IMAGE}" /FO TABLE /NH'
  Pop $R6
  Pop $R5
  StrCpy $R4 $R5 ${LEN}
  ${If} $R4 == "${IMAGE}"
    StrCpy $R7 1
  ${Else}
    StrCpy $R7 0
  ${EndIf}
!macroend

; ---- 通用：停掉正在运行的实例（不 Sleep 假同步）---------------------------
; MODE = "install"：安装/覆盖前调用。检测到在跑 → 提示 → --quit → **每 250ms
;        真实复查**最多约 10s；仍活着才 Abort（否则 File 必然失败）。
; MODE = "uninstall"：卸载时调用。不提示、不 Abort；停完按 C 步要求等 2s
;        让 Job Object 收掉 aria2c 子进程，再复查 aria2c 是否残留。
;
; 为什么必须是「复查」而不是「Sleep 若干秒后当它已退出」：run 45451494558 与
; 早期 A4 实现都吃过这个亏——睡眠时长与真实退出时间无关，睡完照样占用文件。
; 这里的 Sleep 只用作两次探测之间的间隔（250ms），判定依据始终是 tasklist 结果。
;
; 入参：$INSTDIR 必须已就位（--quit 要用 $INSTDIR\ariahost.exe）
; 出参：$R7 最后一次探测结果（"1"：还在跑）
; 复用：安装器（Function StopAppForInstall）、卸载器（Function un.StopAppForUninstall）
!macro StopAppBody MODE
  !insertmacro ProcessIsRunning ${APP_EXE} 12
  ${If} $R7 == "0"
    DetailPrint "没有检测到运行中的 ${APP_EXE}，跳过关闭步骤。"
  ${Else}
    !if "${MODE}" == "install"
      MessageBox MB_OK|MB_ICONINFORMATION "${APP_NAME} 正在运行，将先关闭它再继续安装。$\r$\n$\r$\n托盘里的实例也会一起关闭；退出前会保存下载会话，已下载的文件不受影响。" /SD IDOK
    !endif
    DetailPrint "通知 ${APP_NAME} 优雅退出：$INSTDIR\${APP_EXE} --quit"
    ${If} ${FileExists} "$INSTDIR\${APP_EXE}"
      nsExec::ExecToStack '"$INSTDIR\${APP_EXE}" --quit'
      Pop $R8   ; 退出码（契约：0=已退出 1=无实例 2=超时；失败也可能是 error/timeout 字符串）
      Pop $R3   ; 标准输出
      DetailPrint "--quit 返回：$R8"
    ${Else}
      ; 进程在跑但当前安装目录没有 exe（例如从别处跑的便携版）：无法代为退出，
      ; 直接给出可执行提示，不要空转 10 秒轮询。
      !if "${MODE}" == "install"
        MessageBox MB_OK|MB_ICONSTOP "检测到 ${APP_EXE} 正在运行，但它不在本次安装目录 $INSTDIR 下，安装程序无法代为关闭。$\r$\n$\r$\n请先从托盘菜单「退出」${APP_NAME}（或在任务管理器结束 ${APP_EXE}），然后重新运行安装程序。"
        Abort
      !else
        DetailPrint "警告：$INSTDIR\${APP_EXE} 不存在，且仍有 ${APP_EXE} 在运行（可能来自其他目录）；后续删除可能部分失败。"
      !endif
    ${EndIf}
    ; 真实等待：最多 40 次 × 250ms ≈ 10s，每轮都以 tasklist 结果判定
    StrCpy $R9 0
    ${Do}
      !insertmacro ProcessIsRunning ${APP_EXE} 12
      ${If} $R7 == "0"
        ${ExitDo}
      ${EndIf}
      Sleep 250
      IntOp $R9 $R9 + 1
    ${LoopWhile} $R9 < 40
    ${If} $R7 == "1"
      !if "${MODE}" == "install"
        MessageBox MB_OK|MB_ICONSTOP "${APP_NAME} 仍在运行（已等待约 10 秒），安装无法覆盖被占用的文件，已中止。$\r$\n$\r$\n请从托盘菜单「退出」${APP_NAME}，或在任务管理器结束 ${APP_EXE} 后重新运行安装程序。$\r$\n（--quit 返回：$R8）"
        Abort
      !else
        DetailPrint "警告：${APP_NAME} 仍在运行，后续删除可能因文件占用失败；请退出后手工删除残留目录。"
      !endif
    ${Else}
      DetailPrint "${APP_NAME} 已退出。"
    ${EndIf}
  ${EndIf}
  !if "${MODE}" == "uninstall"
    ; C-2：先停 ariahost，再等 2 秒让 Job Object 收掉 aria2c 子进程，然后复查。
    ; 这 2 秒是"给子进程收尾的缓冲"，判定依据仍然是随后的 tasklist 结果。
    Sleep 2000
    !insertmacro ProcessIsRunning aria2c.exe 10
    ${If} $R7 == "1"
      DetailPrint "警告：检测到仍在运行的 aria2c.exe，不做强制结束（可能是用户自己的 aria2 进程）。若 $INSTDIR 删除不完整，请结束该进程后手工删除。"
    ${Else}
      DetailPrint "aria2c.exe 已退出。"
    ${EndIf}
  !endif
!macroend

; 安装器侧实例（.onInit 调用）
Function StopAppForInstall
  !insertmacro StopAppBody install
FunctionEnd

; 卸载器侧实例：卸载段（Section "Uninstall"）只能 Call un. 前缀的函数
; （Chapter 4 §4.7.1.1 卸载器只编译 un. 前缀），故同一宏再置入一份。
Function un.StopAppForUninstall
  !insertmacro StopAppBody uninstall
FunctionEnd

; ---- 通用：给快捷方式写 AUMID（Toast 前提，失败不阻断）--------------------
; CreateShortCut 写不了 System.AppUserModel.ID，只能回调用 exe 内的自写
; IPropertyStore 实现（架构 §7.1 R9）。写失败 = Toast 不可用，运行时降级到
; 托盘气泡/面板条幅，属于设计允许的降级，绝不因此让安装失败。
; 入参：!macro 参数 LNKPATH（快捷方式完整路径）
!macro WriteAumidFor LNKPATH
  ${If} ${FileExists} "${LNKPATH}"
    ExecWait '"$INSTDIR\${APP_EXE}" --write-aumid "${LNKPATH}"' $0
    ${If} $0 == 0
      DetailPrint "AUMID 已写入：${LNKPATH}"
    ${Else}
      DetailPrint "警告：--write-aumid 返回 $0（${LNKPATH}）；Toast 将在运行时降级为托盘气泡/面板条幅。安装继续。"
    ${EndIf}
  ${Else}
    DetailPrint "警告：快捷方式不存在，跳过 AUMID：${LNKPATH}"
  ${EndIf}
!macroend

; ---- A：可选的服务注册（per-user 包，单次 UAC，等真实退出码）--------------
; 为什么不能直接 nsExec：nsExec 无法提权，非管理员下 CreateService 必失败。
; 为什么不能只 ExecShell 了事：ExecShell "runas" 是**异步**的，$0 只表示
;   "提权请求已发出"，拿不到子进程退出码，无法区分"用户取消 UAC"与"注册失败"。
; 做法：让 cmd 以提权身份执行 exe，并把 !errorlevel! 写进 %TEMP% 的回执文件，
;   安装器轮询该文件出现（真实结果，不是 Sleep 假同步），再读码判断。
; 注意点（都踩过或专门规避过）：
;   * %errorlevel% 在**整行解析阶段**就展开，拿到的是启动前的值 → 必须 `/v:on`
;     用 !errorlevel!（延迟展开）。
;   * `echo 0>file` 会把 0 当成句柄号重定向 → 用括号 `(echo !errorlevel!)>file`。
;   * 路径含空格 → 外层再包一层引号并加 `/S`（cmd /S 会恰好剥掉最外层引号）。
;   * 失败**绝不**中止安装：面板与 headless 形态仍可用。
Function RegisterServiceIfRequested
  StrCpy $R4 "$TEMP\ariahost-svc-install.rc"
  Delete "$R4"
  DetailPrint "请求提权注册 Windows 服务：$INSTDIR\${APP_EXE} --install-service（可能出现一次 UAC 确认）"
  ; 单行调用：NSIS 没有反斜杠续行（`\` 只在字符串里作字面量），长行必须保持一行。
  ExecShell "runas" "$SYSDIR\cmd.exe" "/v:on /S /c $\"$\"$INSTDIR\${APP_EXE}$\" --install-service & (echo !errorlevel!)>$\"$R4$\"$\"" SW_HIDE
  ; 轮询回执文件（UAC 交互 + CreateService，给到 60s）；不预设它一定成功
  StrCpy $R5 0
  ${Do}
    ${If} ${FileExists} "$R4"
      ${ExitDo}
    ${EndIf}
    Sleep 400
    IntOp $R5 $R5 + 1
  ${LoopWhile} $R5 < 150

  ${IfNot} ${FileExists} "$R4"
    DetailPrint "警告：未拿到 --install-service 的回执（大概率是取消了 UAC），服务未注册。"
    MessageBox MB_OK|MB_ICONEXCLAMATION "${APP_NAME} 已安装完成，但**服务未注册**。$\r$\n$\r$\n可能原因：取消或拒绝了 UAC 授权。$\r$\nAriaHost 仍可正常使用（面板会以 headless 方式托管 aria2）；需要服务时，请打开面板 → 设置 → 「一键装服务」重试。"
    Return
  ${EndIf}

  ; 读码并去掉行尾 CR/LF（cmd 的 echo 一定带换行）
  FileOpen $R6 "$R4" r
  FileRead $R6 $R7
  FileClose $R6
  Delete "$R4"
  ${Do}
    StrCpy $R8 $R7 1 -1
    ${If} $R8 == "$\r"
    ${OrIf} $R8 == "$\n"
    ${OrIf} $R8 == " "
      StrCpy $R7 $R7 -1
    ${Else}
      ${ExitDo}
    ${EndIf}
  ${Loop}

  ${If} $R7 == "0"
    DetailPrint "服务注册成功（--install-service 返回 0）。服务为手动触发：不会随开机自动运行 aria2。"
  ${ElseIf} $R7 == "1223"
    DetailPrint "服务注册被用户取消（Win32 1223 = ERROR_CANCELLED，AH-E5002）。"
    MessageBox MB_OK|MB_ICONEXCLAMATION "${APP_NAME} 已安装完成，但服务注册被取消。$\r$\n$\r$\n这不影响使用：面板会以 headless 方式托管 aria2。需要服务时，请打开面板 → 设置 → 「一键装服务」重试。"
  ${Else}
    DetailPrint "服务注册失败（--install-service 返回 $R7）。"
    MessageBox MB_OK|MB_ICONEXCLAMATION "${APP_NAME} 已安装完成，但服务注册失败（返回 $R7，参见 AH-E5003）。$\r$\n$\r$\n这不影响使用：面板会以 headless 方式托管 aria2。$\r$\n需要服务时，可用管理员权限执行：$\r$\n  $\"$INSTDIR\${APP_EXE}$\" --install-service$\r$\n或打开面板 → 设置 → 「一键装服务」。"
  ${EndIf}
FunctionEnd

; ---- 初始化 ---------------------------------------------------------------
Function .onInit
  !if "${ARCH}" == "x64"
    ${IfNot} ${RunningX64}
      MessageBox MB_ICONSTOP "这是 64 位安装包，请在 64 位 Windows 上运行。"
      Abort
    ${EndIf}
  !endif

  ; 版本基线是 10.0.17763（Win10 1809 / Server 2019），不是 10.0.10240：
  ; ${AtLeastWin10} 只比 major/minor（WinVer.nsh：WINVER_10 = 10.0.10240），
  ; 会放行 Windows Server 2016（10.0.14393），与需求 N6/Q16、架构 §4.10/§13 的
  ; 「安装器卡 10.0.17763，Server 2016 安装即拒并给 AH-E1001」冲突。
  ; 因此改用构建号比较 ${AtLeastBuild} 17763（WinVer.nsh）。
  ${IfNot} ${AtLeastBuild} 17763
    ${WinVerGetBuild} $0
    MessageBox MB_ICONSTOP "AriaHost 需要 Windows 10 1809（内部版本 17763）或更高版本，或 Windows Server 2019 及以上。$\r$\n$\r$\n当前系统内部版本：$0$\r$\n被拒绝：Windows 10 1803 及更早、Windows Server 2016（14393）。$\r$\n错误码：AH-E1001"
    Abort
  ${EndIf}

  !ifdef PER_MACHINE
    SetShellVarContext all
    SetRegView 64
  !else
    SetShellVarContext current
  !endif

  ; A6 已废弃（M3）：per-user 包**不再隐藏**服务组件——`--install-service` 自带
  ; 提权助手，per-user 也能通过单次 UAC 注册服务（见 Function RegisterServiceIfRequested）。
  ; 组件页里它仍是可选（`/o`），默认不勾。
  ;
  ; 历史：v2.x 曾在 .onInit 里 Call HidePerUserServiceSection 把该组件置空，
  ; 并因为 ${SEC_SERVICE} 只能在该 Section 之后引用而把隐藏逻辑放到 Section 之后。
  ; 该函数已随本批删除，勿再恢复（per-user 注册服务已是产品能力）。

  ; D：覆盖/升级前先让旧实例退出（真实等待进程消失，不是 Sleep 后假设已退出）
  ReadRegStr $0 ${REG_ROOT} "${REG_APP_KEY}" "InstallDir"
  ${If} $0 == ""
    StrCpy $0 "$INSTDIR"
  ${EndIf}
  ; 目标目录优先用已记录的安装位置（用户这次可能在目录页换了新目录，旧实例仍在旧目录）
  ${If} ${FileExists} "$0\${APP_EXE}"
    DetailPrint "检测到已安装的 ${APP_NAME}（$0），将按升级方式处理。"
    StrCpy $INSTDIR "$0"
  ${EndIf}
  Call StopAppForInstall
FunctionEnd

; ---- 组件 -----------------------------------------------------------------
Section "AriaHost 主程序（必需）" SEC_MAIN
  SectionIn RO
  SetOutPath "$INSTDIR"
  SetOverwrite on
  ; 编译期路径必须用 "\"，且优先绝对路径（见文件头「编译期路径规则」的实测结论）。
  File "${SRCDIR}\${APP_EXE}"
  ; 图标：与安装器自己、开始菜单/桌面快捷方式、exe 资源共用同一份 ICO
  File "${SRCDIR}\ariahost.ico"

  ; 可选：dist 中存在 aria2c.exe 时一并安装（默认不随包分发，见 docs/dev/release.md §7）
!if /FileExists "${SRCDIR}\aria2c.exe"
  File "${SRCDIR}\aria2c.exe"
!endif

  ; 文件名与架构 §4.10 安装包清单一致（uninst.exe）
  WriteUninstaller "$INSTDIR\uninst.exe"

  WriteRegStr   ${REG_ROOT} "${REG_UNINST_KEY}" "DisplayName"     "${APP_NAME}"
  WriteRegStr   ${REG_ROOT} "${REG_UNINST_KEY}" "DisplayVersion"  "${VERSION}"
  WriteRegStr   ${REG_ROOT} "${REG_UNINST_KEY}" "Publisher"       "${APP_PUBLISHER}"
  WriteRegStr   ${REG_ROOT} "${REG_UNINST_KEY}" "DisplayIcon"     "$INSTDIR\${APP_EXE}"
  WriteRegStr   ${REG_ROOT} "${REG_UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr   ${REG_ROOT} "${REG_UNINST_KEY}" "UninstallString" '"$INSTDIR\uninst.exe"'
  WriteRegStr   ${REG_ROOT} "${REG_UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninst.exe" /S'
  WriteRegDWORD ${REG_ROOT} "${REG_UNINST_KEY}" "NoModify" 1
  WriteRegDWORD ${REG_ROOT} "${REG_UNINST_KEY}" "NoRepair" 1
  WriteRegStr   ${REG_ROOT} "${REG_APP_KEY}"    "InstallDir" "$INSTDIR"

  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD ${REG_ROOT} "${REG_UNINST_KEY}" "EstimatedSize" "$0"
SectionEnd

Section "开始菜单快捷方式" SEC_SM
  CreateDirectory "$SMPROGRAMS\${APP_NAME}"
  CreateShortCut "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}" "" "$INSTDIR\ariahost.ico" 0

  ; B：建完 .lnk 立刻写 AUMID（Toast 前提）。失败只写日志，不阻断（见宏内注释）。
  !insertmacro WriteAumidFor $SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk

  CreateShortCut "$SMPROGRAMS\${APP_NAME}\卸载 ${APP_NAME}.lnk" "$INSTDIR\uninst.exe"
SectionEnd

Section /o "桌面快捷方式" SEC_DESKTOP
  CreateShortCut "$DESKTOP\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}" "" "$INSTDIR\ariahost.ico" 0
  ; 每个 .lnk 都要单独写：AUMID 是**每份快捷方式**的属性，不是全局设置。
  !insertmacro WriteAumidFor $DESKTOP\${APP_NAME}.lnk
SectionEnd

; 服务组件：可选（`/o`，默认**不勾**）。
; - per-user 包：勾选后用 runas 调 --install-service（单次 UAC，失败不阻断安装）；
; - 全机变体：安装器已是 admin，直接调，不再额外弹 UAC。
; 两种是同一份脚手架：per-user 的隐藏逻辑（A6）已在 M3 移除。
Section /o "同时注册为 Windows 服务（Aria2 随系统服务运行）" SEC_SERVICE
!ifdef PER_MACHINE
  ; 全机变体：安装器本身已是管理员上下文，直接调用提权助手，不再额外弹 UAC
  DetailPrint "注册 Windows 服务（当前已是管理员上下文）：$INSTDIR\${APP_EXE} --install-service"
  nsExec::ExecToLog '"$INSTDIR\${APP_EXE}" --install-service'
  Pop $0
  ${If} $0 != 0
    MessageBox MB_OK|MB_ICONEXCLAMATION "服务注册失败（--install-service 返回 $0，参见 AH-E5003）。$\r$\n$\r$\n${APP_NAME} 已安装，仍可用 headless 形态运行；可在面板设置页重试「一键装服务」。"
  ${EndIf}
!else
  ; per-user：不能在本进程内调 SCM（非管理员），走单次 UAC 助手并等真实退出码
  Call RegisterServiceIfRequested
!endif
SectionEnd

; ---- A6 已删除：per-user 包的隐藏逻辑（历史见 .onInit 注释）----------------

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_MAIN}    "主程序 ariahost.exe 与卸载器（必需）。"
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_SM}      "在开始菜单创建启动与卸载快捷方式（同时写入 AUMID，通知可用）。"
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_DESKTOP} "在桌面创建启动快捷方式（同样写入 AUMID）。"
  !insertmacro MUI_DESCRIPTION_TEXT ${SEC_SERVICE} "安装完成后把 AriaHost 注册为 Windows 服务（手动触发，不随机启动 aria2）。需要一次管理员授权（UAC）；取消或不勾选都不影响使用——面板会以 headless 方式托管 aria2。"
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; ---- 卸载 -----------------------------------------------------------------
; 顺序（C 步，任何一步失败都继续下一步——卸载绝不能半途 Abort）：
;   1) 注销服务（runas，单次 UAC；失败不阻断，提示残留）
;   2) 停 ariahost（--quit + 真实等待）→ 等 2s 让 Job Object 收掉 aria2c → 复查
;   3) 删快捷方式 + 注册表卸载项
;   4) 删 $INSTDIR
;   5) 用户数据（%APPDATA%、%LOCALAPPDATA%）默认**保留**，问过才删
; 为什么 1) 在 2) 之前：注销服务要用 --uninstall-service 的提权助手，它需要
; $INSTDIR\ariahost.exe 仍在；先把进程停掉会让这一步失去执行体。
Section "Uninstall"
  !ifdef PER_MACHINE
    SetShellVarContext all
    SetRegView 64
  !else
    SetShellVarContext current
  !endif

  ; === 1) 注销服务（可拒绝 UAC → 保留服务并提示残留，AH-E5003）=============
  StrCpy $R0 0   ; 0=无服务 1=已注销/无残留 2=残留待人工处理
  ReadRegStr $1 HKLM "${SVC_REG_KEY}" "ImagePath"
  ${If} $1 != ""
    ${IfNot} ${FileExists} "$INSTDIR\${APP_EXE}"
      MessageBox MB_OK|MB_ICONEXCLAMATION "检测到服务 ${SVC_NAME}，但 $INSTDIR\${APP_EXE} 已不存在，无法自动注销。$\r$\n$\r$\n请以管理员身份执行：sc delete ${SVC_NAME}"
      StrCpy $R0 2
    ${Else}
      !ifdef PER_MACHINE
        ; 已是管理员：直接执行并取退出码（A3：不再只 Sleep）
        DetailPrint "注销 Windows 服务：$INSTDIR\${APP_EXE} --uninstall-service"
        nsExec::ExecToLog '"$INSTDIR\${APP_EXE}" --uninstall-service'
        Pop $1
        ${If} $1 == "0"
          StrCpy $R0 1
        ${Else}
          MessageBox MB_OK|MB_ICONEXCLAMATION "服务注销失败（--uninstall-service 返回 $1，AH-E5003）。$\r$\n$\r$\n请以管理员身份手工执行：sc delete ${SVC_NAME}"
          StrCpy $R0 2
        ${EndIf}
      !else
        ; per-user 非管理员：nsExec 不能提权 → runas 发起，以服务注册表键是否消失为判据（≤30s）。
        DetailPrint "请求提权注销 Windows 服务：$INSTDIR\${APP_EXE} --uninstall-service"
        ExecShell "runas" "$INSTDIR\${APP_EXE}" "--uninstall-service" SW_HIDE
        StrCpy $2 0
        ${Do}
          Sleep 500
          IntOp $2 $2 + 1
          ReadRegStr $1 HKLM "${SVC_REG_KEY}" "ImagePath"
          ${If} $1 == ""
            ${ExitDo}
          ${EndIf}
        ${LoopWhile} $2 < 60
        ${If} $1 == ""
          StrCpy $R0 1
        ${Else}
          MessageBox MB_OK|MB_ICONEXCLAMATION "服务 ${SVC_NAME} 仍在注册（可能取消了 UAC 授权，AH-E5003）。$\r$\n$\r$\n服务残留不影响卸载，但下次开机仍可能拉起 aria2；请以管理员身份执行：sc delete ${SVC_NAME}"
          StrCpy $R0 2
        ${EndIf}
      !endif
    ${EndIf}
  ${EndIf}

  ; === 2) 停进程：先 ariahost，等 2s 让 Job Object 收掉 aria2c，再复查 =====
  ; 卸载段只能 Call un. 前缀函数（Chapter 4 §4.7.1.1），故用 un. 版本；
  ; 该版本不弹框、不 Abort，退出不掉也只告警并继续。
  Call un.StopAppForUninstall

  ; === 3) 快捷方式 + 注册表（先删再删文件：卸载项留着会让"程序列表"指向空路径）=
  Delete "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk"
  Delete "$SMPROGRAMS\${APP_NAME}\卸载 ${APP_NAME}.lnk"
  RMDir  "$SMPROGRAMS\${APP_NAME}"
  Delete "$DESKTOP\${APP_NAME}.lnk"

  DeleteRegKey ${REG_ROOT} "${REG_UNINST_KEY}"
  DeleteRegKey ${REG_ROOT} "${REG_APP_KEY}"

  ; === 4) 删安装目录 ========================================================
  Delete "$INSTDIR\${APP_EXE}"
  Delete "$INSTDIR\aria2c.exe"
  Delete "$INSTDIR\ariahost.ico"
  Delete "$INSTDIR\uninst.exe"
  Delete "$INSTDIR\uninstall.exe"   ; 旧版本（改名前的产物）
  RMDir "$INSTDIR"

  ; === 5) 服务平面数据 %ProgramData%\AriaHost\（仅管理员可删）==============
  ; 服务平面 = aria2.conf/session/logs/secret/aria2\versions\<ver>\ + staging\（架构 §4.11）
  ; $APPDATA 的指向由 SetShellVarContext 决定：all 时为 %ProgramData%。
  !ifdef PER_MACHINE
    RMDir /r "$APPDATA\${APP_NAME}"
  !else
    SetShellVarContext all
    ${If} ${FileExists} "$APPDATA\${APP_NAME}"
      StrCpy $R1 1
      StrCpy $R3 "$APPDATA\${APP_NAME}"
    ${Else}
      StrCpy $R1 0
    ${EndIf}
    SetShellVarContext current
  !endif

  ; ---- 用户数据：默认保留，问过才删（下载目录永不在作用范围）-------------
  ; 用户平面 = config.toml/logs/session/runtime/secret（%APPDATA%\AriaHost）
  ;            + cache（dht/update、aria2\versions\<ver>、staging）（%LOCALAPPDATA%\AriaHost）
  ; 默认按钮是「否」（/SD IDNO）：静默卸载同样保留用户数据。
  MessageBox MB_YESNO|MB_ICONQUESTION "是否同时删除 ${APP_NAME} 的用户数据？$\r$\n$\r$\n配置与日志：$APPDATA\${APP_NAME}$\r$\n版本库与缓存：$LOCALAPPDATA\${APP_NAME}\cache$\r$\n$\r$\n下载目录中的文件永远不会被删除。" /SD IDNO IDNO keep_user_data

  RMDir /r "$APPDATA\${APP_NAME}"
  RMDir /r "$LOCALAPPDATA\${APP_NAME}\cache"
  RMDir /r "$LOCALAPPDATA\${APP_NAME}\logs"
  RMDir    "$LOCALAPPDATA\${APP_NAME}"

  keep_user_data:

  !ifndef PER_MACHINE
    ${If} $R1 == 1
      MessageBox MB_OK|MB_ICONINFORMATION "服务平面数据仍在：$R3$\r$\n$\r$\n它属于 %ProgramData% 作用域，需要管理员权限才能删除。$\r$\n保留它可以在下次「一键装服务」时复用（推荐）；要立即删除请以管理员身份执行：$\r$\nrmdir /s /q $\"$R3$\""
    ${EndIf}
  !endif
SectionEnd
