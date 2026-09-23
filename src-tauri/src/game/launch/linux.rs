use super::{LaunchResult, StopResult, load_game, validate_and_open_steam, validate_local_launch};
use crate::game::monitor::TimeTrackingMode;
use crate::game::monitor::{MonitorTarget, get_connection, get_manager_proxy};
use crate::game::monitor::{monitor_game, stop_game_session, stop_steam_game, wait_for_steam_game};
use crate::game::steam::steam_app_id_from_launch_id;
use log::{debug, info};
use sea_orm::DatabaseConnection;
use std::future::poll_fn;
use std::io::{Read, Write};
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::pin::Pin;
use std::process::Command;
use std::time::Duration;
use tauri::{AppHandle, Manager, Runtime, State, command};
use tauri_plugin_store::StoreExt;
use zbus::export::futures_core::Stream;
use zbus::zvariant::{OwnedValue, Value};
use zbus_systemd::systemd1::{JobRemoved, JobRemovedStream};
/// 等待 systemd job 完成的最长时间（秒）
///
/// JobRemoved 由用户管理器异步发出，正常情况下毫秒级即可到达；
/// 超时意味着管理器异常，此时不应继续放行游戏进程。
const JOB_WAIT_TIMEOUT_SECS: u64 = 10;

/// 放行子进程 exec 的 gate 字节
///
/// gate 用一个字节传递结果：写入该值表示放行，其他字节表示父进程已放弃。
/// 必须显式写入而不是靠关闭写端：其他 fork 出来的进程会继承 gate 写端的副本，
/// 只要副本还活着子进程就读不到 EOF，会永远卡在 exec 前。
const GATE_RELEASE_BYTE: u8 = 1;

#[command]
pub async fn launch_game<R: Runtime>(
    app_handle: AppHandle<R>,
    db: State<'_, DatabaseConnection>,
    game_id: u32,
    args: Option<Vec<String>>,
    time_tracking_mode: TimeTrackingMode,
) -> Result<LaunchResult, String> {
    Ok(
        match launch_game_inner(app_handle, db, game_id, args, time_tracking_mode).await {
            Ok(result) => result,
            Err(message) => LaunchResult::failed(message),
        },
    )
}

async fn launch_game_inner<R: Runtime>(
    app_handle: AppHandle<R>,
    db: State<'_, DatabaseConnection>,
    game_id: u32,
    args: Option<Vec<String>>,
    time_tracking_mode: TimeTrackingMode,
) -> Result<LaunchResult, String> {
    let game = load_game(db.inner(), game_id).await?;

    if game.launch_type == "steam" {
        let steam_launch = validate_and_open_steam(
            &app_handle,
            game_id,
            game.steam_launch_id.as_deref(),
            args.as_deref(),
        )?;

        // Steam 在 Linux 上以 reaper 进程启动游戏，识别到它才能开始计时
        let process_id = wait_for_steam_game(steam_launch.steam_app_id)
            .await
            .map_err(|error| format!("{error}，本次游玩未开始计时"))?;

        monitor_game(
            app_handle.clone(),
            db.inner().clone(),
            time_tracking_mode,
            game_id,
            process_id,
            MonitorTarget::SteamAppId(steam_launch.steam_app_id),
        )
        .await;

        return Ok(LaunchResult::tracking(
            format!("已交由 Steam 启动游戏 ({})", steam_launch.steam_launch_id),
            Some(process_id),
        ));
    }

    let local_launch = validate_local_launch(&game)?;
    let game_dir = local_launch.game_dir;
    let executable_path = local_launch.executable_path;
    let game_path = executable_path.to_string_lossy().to_string();

    let exe_name = match executable_path.file_name() {
        Some(name) => name,
        None => return Err("无法获取游戏可执行文件名".to_string()),
    };

    let systemd_unit_name = format!("reina_game_{}.scope", game_id);
    let _ = check_unit_or_reset_failed(&systemd_unit_name).await;

    // settings 里的 Linux 启动命令：默认启动器，也是 proton 回退目标。
    let settings_launcher = app_handle
        .store("settings.json")
        .ok()
        .and_then(|store| store.get("linux_launch_command"))
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "wine".to_string());

    // 实际使用的启动器。proton_profile 语义（fork 自有功能）:
    //   None      = 不使用 proton-autogen，走 settings 命令
    //   "auto"    = 裸 proton-autogen <exe> —— 一切由 proton-autogen 的
    //               游戏级配置（games/*.json）决定：prefix、Proton 版本、env，
    //               与 proton-autogen 自己生成的 .desktop 行为完全一致
    //   其他值    = proton-autogen --profile <name>，显式环境预设覆盖
    //              （会强制覆盖游戏的 exe_type，仅在需要时使用）
    let linux_launch_command = match game.proton_profile.as_deref() {
        Some("auto") => "proton-autogen".to_string(),
        Some(profile) => format!("proton-autogen --profile {profile}"),
        None => settings_launcher.clone(),
    };
    let linux_launch_command = expand_path(&app_handle, &linux_launch_command);
    debug!("使用的 Linux 启动命令: {:?}", linux_launch_command);

    // 用选定的启动器构造实际执行路径与参数。
    // 对于 .exe 文件：用启动器作为命令，游戏文件作为参数；
    // 对于其他文件：直接用文件本身作为命令（proton 设置对其无效）。
    // 参数列表不含 argv[0]，由 spawn 时自动补上。
    let build_exec = |launcher: &str| -> (String, Vec<String>) {
        if exe_name.to_string_lossy().ends_with(".exe") {
            // .exe 文件：<launcher...> game.exe [user_args...]
            // 按空白拆分为多个参数，支持 "proton-autogen --profile dx11" 这类多词命令；
            // 不处理带空格的 profile 名，proton-autogen 的 profile 是文件名，不含空格。
            let mut parts: Vec<String> = launcher
                .split_whitespace()
                .map(|part| part.to_string())
                .collect();
            if parts.is_empty() {
                parts.push("wine".to_string());
            }
            let exec_path = parts.remove(0);
            let mut exec_args = parts;
            exec_args.push(game_path.clone());
            if let Some(arguments) = &args {
                exec_args.extend(arguments.iter().cloned());
            }
            (exec_path, exec_args)
        } else {
            // 其他文件：./game [user_args...]
            let mut exec_args = Vec::new();
            if let Some(arguments) = &args {
                exec_args.extend(arguments.iter().cloned());
            }
            (game_path.clone(), exec_args)
        }
    };

    let (exec_path, exec_args) = build_exec(&linux_launch_command);
    debug!(
        "准备启动游戏 game_id={} unit={} exec_path={:?} exec_args={:?} cwd={:?}",
        game_id, systemd_unit_name, exec_path, exec_args, game_dir
    );

    // scope 与 service 不同，它只能接管已存在的进程，因此这里自己拉起子进程
    // （上游 D-Bus transient scope 机制：fork→阻塞 exec→挂入 scope→放行，
    // 整棵进程树从第一个进程起就落在 scope 内，停止时不会漏杀）。
    let mut process_id = spawn_in_scope(
        game_id,
        &systemd_unit_name,
        &exec_path,
        &exec_args,
        &game_dir,
    )
    .await?;
    let mut fallback_used = false;

    // proton 快速失败回退（fork 自有功能）：proton-autogen 启动后短时间内进程就消失
    // （游戏配置缺失、Proton 运行时不可用等），改用 settings 里的 Linux 启动命令重试。
    // 8s 窗口内活着就认为启动成功；首次运行下载 GE-Proton 属于“活着”，不会误触；
    // 8s 后才退出的属游戏自身问题，不回退。
    // 新机制下只能拿到 PID（拿不到退出码），以“进程是否存活”代替原 try_wait 判定。
    if game.proton_profile.is_some() && exe_name.to_string_lossy().ends_with(".exe") {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
        let mut exited_early = false;
        loop {
            if !process_alive(process_id) {
                exited_early = true;
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        }
        if exited_early {
            info!(
                "proton-autogen 快速失败 game_id={}，回退 Linux 启动命令",
                game_id
            );
            let _ = check_unit_or_reset_failed(&systemd_unit_name).await;
            let settings_launcher = expand_path(&app_handle, &settings_launcher);
            let (fallback_path, fallback_args) = build_exec(&settings_launcher);
            process_id = spawn_in_scope(
                game_id,
                &systemd_unit_name,
                &fallback_path,
                &fallback_args,
                &game_dir,
            )
            .await
            .map_err(|error| format!("proton 启动失败且回退也失败: {error}，目录: {game_dir:?}"))?;
            info!(
                "回退启动成功 game_id={} pid={} scope={}",
                game_id, process_id, systemd_unit_name
            );
            fallback_used = true;
        }
    }

    info!(
        "游戏进程已加入 scope: unit={} pid={}",
        systemd_unit_name, process_id
    );

    monitor_game(
        app_handle.clone(),
        db.inner().clone(),
        time_tracking_mode,
        game_id,
        process_id,
        MonitorTarget::SystemdUnit(systemd_unit_name.clone()),
    )
    .await;

    if fallback_used {
        return Ok(LaunchResult::tracking(
            format!(
                "proton-autogen 启动失败，已回退 Linux 启动命令启动: {}",
                exe_name.to_string_lossy()
            ),
            Some(process_id),
        ));
    }

    Ok(LaunchResult::tracking(
        format!(
            "成功启动游戏: {}，工作目录: {:?}",
            exe_name.to_string_lossy(),
            game_dir
        ),
        Some(process_id),
    ))
}

/// 拉起游戏子进程，并把它交给 systemd 的 transient scope 托管。
///
/// scope 只能接管「已经存在」的进程，所以进程必须由 ReinaManager 自己 fork。
/// 但进程一旦 exec 就可能立刻 fork 出后代（wine 会拉起 wineserver），
/// 这些后代不会被事后补进 scope 的 cgroup。
///
/// 因此这里的时序是：fork 子进程 → 子进程阻塞在 exec 前 → 把它的 PID 挂进 scope
/// → 等 JobRemoved 确认 scope 就绪 → 放行 exec。
/// 这样游戏整棵进程树从第一个进程开始就落在 scope 内，停止游戏时不会漏杀。
///
/// # Arguments
/// * `exec_args` - 传给游戏的参数，不含 argv[0]（由 spawn 自动补上）
///
/// # Returns
/// 子进程 PID，同时作为监控目标 `MonitorTarget::SystemdUnit` 的起点
async fn spawn_in_scope(
    game_id: u32,
    unit_name: &str,
    exec_path: &str,
    exec_args: &[String],
    game_dir: &Path,
) -> Result<u32, String> {
    // gate: 父进程放行子进程 exec；report: 子进程回报自己的 PID
    let (mut gate_parent, gate_child) =
        UnixStream::pair().map_err(|error| format!("创建进程放行通道失败: {error}"))?;
    let (report_child, report_parent) =
        UnixStream::pair().map_err(|error| format!("创建进程 PID 回报通道失败: {error}"))?;

    // fork 出的子进程会同时持有这两个通道父进程侧的副本，需要在 exec 前关掉
    let gate_parent_fd = gate_parent.as_raw_fd();
    let report_parent_fd = report_parent.as_raw_fd();

    let mut command = tokio::process::Command::new(exec_path);
    command.args(exec_args).current_dir(game_dir);

    // 两个通道都由闭包以可变引用使用，exec 后 fd 会被 CLOEXEC 自动关闭
    let mut gate_child = gate_child;
    let mut report_child = report_child;

    // SAFETY: 闭包在 fork 之后、exec 之前运行，只调用 async-signal-safe 的
    // read/write/close 与 getpid，不分配内存、不获取锁
    unsafe {
        command.pre_exec(move || {
            // 子进程只需通道的另一端，父进程侧的副本留在手里会让对端读不到 EOF
            drop(OwnedFd::from_raw_fd(gate_parent_fd));
            drop(OwnedFd::from_raw_fd(report_parent_fd));

            // 先回报 PID，父进程据此把本进程加入 scope
            report_child.write_all(&std::process::id().to_le_bytes())?;

            // 阻塞等待放行：scope 就绪后收到放行字节，父进程放弃时会收到其他值
            let mut byte = [0u8; 1];
            gate_child.read_exact(&mut byte)?;
            if byte[0] != GATE_RELEASE_BYTE {
                return Err(std::io::Error::other("父进程已放弃启动 scope"));
            }
            Ok(())
        });
    }

    // spawn 会一直阻塞到子进程 exec 完成，而 exec 正被 gate 挡住，
    // 因此挪到阻塞线程上执行，等 scope 就绪后再放行
    let spawn_task = tokio::task::spawn_blocking(move || command.spawn());

    let process_id = match read_reported_pid(report_parent).await {
        Ok(process_id) => process_id,
        Err(error) => {
            // 子进程没能起来，gate 已无意义，顺手取回 spawn 的真实错误
            drop(gate_parent);
            let spawn_error = match spawn_task.await {
                Ok(Ok(_child)) => "子进程未能回报 PID".to_string(),
                Ok(Err(error)) => error.to_string(),
                Err(error) => error.to_string(),
            };
            return Err(format!("拉起游戏进程失败: {spawn_error}（{error}）"));
        }
    };

    if let Err(error) = create_scope(game_id, unit_name, process_id).await {
        // 通知子进程放弃 exec，等它自行退出，避免留下无主进程
        let _ = gate_parent.write_all(&[0]);
        drop(gate_parent);
        let _ = spawn_task.await;
        return Err(error);
    }

    // 放行 exec
    if let Err(error) = gate_parent.write_all(&[GATE_RELEASE_BYTE]) {
        debug!("放行游戏进程失败（子进程可能已退出）: {error}");
    }
    drop(gate_parent);

    match spawn_task.await {
        Ok(Ok(child)) => {
            // 交还 tokio 回收：进程退出时由它处理 SIGCHLD，避免留下僵尸进程
            drop(child);
        }
        Ok(Err(error)) => {
            // exec 失败意味着 scope 里其实没有进程，顺手清掉这个空 unit
            if let Ok(manager) = get_manager_proxy().await {
                let _ = manager
                    .stop_unit(unit_name.to_owned(), "replace".to_string())
                    .await;
            }
            return Err(format!("启动游戏进程失败: {error}"));
        }
        Err(error) => return Err(format!("启动游戏进程失败: {error}")),
    }

    Ok(process_id)
}

/// 读取子进程在阻塞 exec 前回报的 PID
async fn read_reported_pid(mut report_parent: UnixStream) -> Result<u32, String> {
    let reported = tokio::task::spawn_blocking(move || {
        let mut buffer = [0u8; size_of::<u32>()];
        report_parent.read_exact(&mut buffer)?;
        Ok::<u32, std::io::Error>(u32::from_le_bytes(buffer))
    })
    .await
    .map_err(|error| format!("读取子进程 PID 的任务失败: {error}"))?;

    reported.map_err(|error| format!("读取子进程 PID 失败: {error}"))
}

/// 通过 D-Bus 创建 transient scope，并把游戏进程挂进去
async fn create_scope(game_id: u32, unit_name: &str, process_id: u32) -> Result<(), String> {
    let manager = get_manager_proxy()
        .await
        .map_err(|error| format!("连接到 systemd 失败，无法启动游戏 {game_id}: {error}"))?;

    // 必须先订阅 JobRemoved 再发起请求：job 可能在方法返回前就已完成，
    // 先订阅才不会漏掉完成通知而一直等待
    let mut job_removed = manager
        .receive_job_removed()
        .await
        .map_err(|error| format!("订阅 systemd JobRemoved 信号失败: {error}"))?;

    let properties = vec![
        value_property(
            "Description",
            Value::from(format!("ReinaManager 游戏进程 (game_id={game_id})")),
        )?,
        value_property("PIDs", Value::from(vec![process_id]))?,
        value_property("Delegate", Value::from(true))?,
    ];

    let job_path = manager
        .start_transient_unit(
            unit_name.to_owned(),
            "replace".to_string(),
            properties,
            Vec::new(),
        )
        .await
        .map_err(|error| format!("创建 systemd scope {unit_name} 失败: {error}"))?;

    info!(
        "创建 scope 的请求已发送: unit={} pid={} job={:?}",
        unit_name, process_id, job_path
    );

    wait_for_job_removed(&mut job_removed, job_path.as_str())
        .await
        .map_err(|error| format!("创建 systemd scope {unit_name} 失败: {error}"))
}

/// 构造 transient unit 属性，属性值统一转换为 `OwnedValue`
fn value_property(name: &str, value: Value<'_>) -> Result<(String, OwnedValue), String> {
    OwnedValue::try_from(value)
        .map(|owned| (name.to_owned(), owned))
        .map_err(|error| format!("构建 {name} 属性失败: {error}"))
}

/// 等待指定 job 的 JobRemoved 信号，并确认执行结果为 done
async fn wait_for_job_removed(stream: &mut JobRemovedStream, job_path: &str) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(JOB_WAIT_TIMEOUT_SECS);

    loop {
        let signal = tokio::time::timeout_at(deadline, next_job_removed(stream))
            .await
            .map_err(|_| format!("等待 systemd job {job_path} 超时"))?
            .ok_or_else(|| "systemd JobRemoved 信号流已结束".to_string())?;

        let args = signal
            .args()
            .map_err(|error| format!("解析 JobRemoved 信号失败: {error}"))?;

        // manager 会广播所有 job 的完成事件，这里只关心自己发起的那一个
        if args.job().as_str() != job_path {
            continue;
        }

        return match args.result().as_str() {
            "done" => Ok(()),
            result => Err(format!("systemd job 的执行结果为 {result}")),
        };
    }
}

/// 从信号流中取出下一个 JobRemoved 信号
async fn next_job_removed(stream: &mut JobRemovedStream) -> Option<JobRemoved> {
    poll_fn(|context| Pin::new(&mut *stream).poll_next(context)).await
}

#[command]
pub async fn stop_game(
    db: State<'_, DatabaseConnection>,
    game_id: u32,
) -> Result<StopResult, String> {
    let game = load_game(db.inner(), game_id).await?;

    if game.launch_type == "steam" {
        // Steam 启动的游戏不在 ReinaManager 的 systemd unit 里，只能靠 reaper 定位
        let launch_id = game
            .steam_launch_id
            .as_deref()
            .map(str::trim)
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| "Steam 启动 ID 无效，请重新关联 Steam 启动项".to_string())?;
        let app_id = steam_app_id_from_launch_id(launch_id)
            .map_err(|error| format!("{error}，请重新关联 Steam 启动项"))?;

        let terminated_count = stop_steam_game(app_id)?;

        info!("已终止 Steam 游戏进程 game_id={game_id} app_id={app_id} count={terminated_count}");

        return Ok(StopResult::success(
            format!("成功停止游戏 {}，终止进程数: {}", game_id, terminated_count),
            terminated_count,
        ));
    }

    match stop_game_session(game_id).await {
        Ok(terminated_count) => Ok(StopResult::success(
            format!("成功停止游戏 {}，终止进程数: {}", game_id, terminated_count),
            terminated_count,
        )),
        Err(e) => Err(format!("停止游戏 {} 失败: {}", game_id, e)),
    }
}

/// 检测 proton-autogen 是否已安装在 PATH 上
#[command]
pub fn check_proton_autogen() -> bool {
    Command::new("sh")
        .args(["-c", "command -v proton-autogen"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 下载并安装 proton-autogen（系统级）。
/// 源：用户 fork 的 main 分支 tarball；授权经 pkexec（系统弹窗，可取消）。
/// 取消/失败时返回手动安装命令。
#[command]
pub async fn install_proton_autogen<R: Runtime>(app_handle: AppHandle<R>) -> Result<(), String> {
    const URL: &str =
        "https://codeload.github.com/luoyuxiaoxiao/proton-autogen/tar.gz/refs/heads/main";

    let src_dir = app_handle
        .path()
        .app_cache_dir()
        .map_err(|e| format!("无法获取缓存目录: {e}"))?
        .join("proton-autogen-src");
    std::fs::create_dir_all(&src_dir).map_err(|e| format!("无法创建缓存目录: {e}"))?;

    let bytes = crate::utils::http::get_client()
        .get(URL)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("下载 proton-autogen 失败: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("下载 proton-autogen 失败: {e}"))?;
    std::fs::write(src_dir.join("pa.tar.gz"), &bytes)
        .map_err(|e| format!("写入临时文件失败: {e}"))?;

    // GitHub tarball 有一层 <user>-<repo>-<sha>/ 顶层目录，strip 掉
    let extract = Command::new("tar")
        .arg("-xzf")
        .arg(src_dir.join("pa.tar.gz"))
        .arg("--strip-components=1")
        .arg("-C")
        .arg(&src_dir)
        .output()
        .map_err(|e| format!("解压失败: {e}"))?;
    if !extract.status.success() {
        return Err(format!(
            "解压失败: {}",
            String::from_utf8_lossy(&extract.stderr)
        ));
    }

    let manual = format!("sudo bash {}/install.sh", src_dir.display());
    let mut child = Command::new("pkexec")
        .arg("bash")
        .arg(src_dir.join("install.sh"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("无法启动授权窗口: {e}\n可手动安装: {manual}"))?;

    // 轮询等待而非阻塞 wait；600s 超时兜底（无 polkit agent 时 pkexec 会一直挂着）
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(600);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err(format!("安装未完成（取消了授权）。\n可手动安装: {manual}"))
                };
            }
            Ok(None) => {
                if tokio::time::Instant::now() > deadline {
                    let _ = child.kill();
                    return Err(format!("安装超时。\n可手动安装: {manual}"));
                }
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
            Err(e) => return Err(format!("等待安装进程失败: {e}\n可手动安装: {manual}")),
        }
    }
}

fn expand_path<R: Runtime>(app_handle: &AppHandle<R>, path: &str) -> String {
    if path.starts_with('~') {
        if let Ok(home_dir) = app_handle.path().home_dir() {
            path.replacen('~', &home_dir.to_string_lossy(), 1)
        } else {
            path.to_string()
        }
    } else {
        path.to_string()
    }
}

/// 判断进程是否仍然存活（/proc 下存在即认为存活）。
/// 用于 proton 快速失败回退：新启动机制只返回 PID，拿不到退出码。
/// 僵尸进程在 tokio 回收前仍占用 /proc 名额，会被视为存活——最坏情况只是
/// 少触发一次回退（等同于“游戏跑满 8s”），不会误杀正常游戏。
fn process_alive(process_id: u32) -> bool {
    Path::new(&format!("/proc/{process_id}")).exists()
}

/// 检查 systemd unit 的状态，如果是 failed 则重置它
/// 返回 bool 值表示 unit 是否已经存在
/// # Arguments
/// * `systemd_unit_name` - systemd 单元名称
///
/// # Returns
/// bool - 如果 unit 已存在则返回 true，否则返回 false
async fn check_unit_or_reset_failed(systemd_unit_name: &str) -> Result<bool, String> {
    let proxy = get_manager_proxy().await.map_err(|e| {
        format!(
            "连接到 systemd 失败，无法检查或重置单元 {}: {}",
            systemd_unit_name, e
        )
    })?;
    match proxy.get_unit(systemd_unit_name.to_string()).await {
        Ok(u) => {
            let conn = get_connection().await.map_err(|e| {
                format!(
                    "连接到 systemd 失败，无法检查或重置单元 {}: {}",
                    systemd_unit_name, e
                )
            })?;
            match zbus_systemd::systemd1::UnitProxy::new(conn, u).await {
                Ok(unit_proxy) => {
                    let active_state = unit_proxy
                        .active_state()
                        .await
                        .map_err(|e| format!("获取单元 {} 状态失败: {}", systemd_unit_name, e))?;
                    if active_state == "failed" {
                        proxy
                            .reset_failed_unit(systemd_unit_name.to_string())
                            .await
                            .map_err(|e| {
                                format!("重置单元 {} 状态失败: {}", systemd_unit_name, e)
                            })?;
                        info!("单元 {} 已被重置", systemd_unit_name);
                    }
                    Ok(true)
                }
                Err(_) => Ok(false),
            }
        }
        Err(_) => Ok(false),
    }
}
