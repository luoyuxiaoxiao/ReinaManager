use super::{LaunchResult, StopResult, load_game, validate_and_open_steam, validate_local_launch};
use crate::game::monitor::{TimeTrackingMode, monitor_game, stop_game_session};
use log::{debug, info};
use sea_orm::DatabaseConnection;
use std::process::Command;
use tauri::{AppHandle, Manager, Runtime, State, command};
use tauri_plugin_store::StoreExt;

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
            game.localpath.as_deref(),
            args.as_deref(),
        )?;

        return Ok(LaunchResult::delegated(format!(
            "已交由 Steam 启动游戏 ({})",
            steam_launch.steam_launch_id
        )));
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
    let _ = check_scope_or_reset_failed(&systemd_unit_name).await;

    // 构造 systemd-run 启动命令。proton_profile 语义:
    //   None      = 不使用 proton-autogen,走 settings 里的 Linux 启动命令(也是回退目标)
    //   "auto"    = 裸 proton-autogen <exe> —— 一切由 proton-autogen 的
    //               游戏级配置(games/*.json)决定:prefix、Proton 版本、env。
    //               与 proton-autogen 自己生成的 .desktop 行为完全一致
    //   其他值    = proton-autogen --profile <name>,显式环境预设覆盖
    //               (会强制覆盖游戏的 exe_type,仅在需要时使用)
    let build_command = |profile: Option<&str>| -> Command {
        let linux_launch_command = match profile {
            Some("auto") => "proton-autogen".to_string(),
            Some(profile) => format!("proton-autogen --profile {profile}"),
            None => app_handle
                .store("settings.json")
                .ok()
                .and_then(|store| store.get("linux_launch_command"))
                .and_then(|v| v.as_str().map(|s| s.to_string()))
                .unwrap_or_else(|| "wine".to_string()),
        };
        let linux_launch_command = expand_path(&app_handle, &linux_launch_command);
        debug!("使用的 Linux 启动命令: {:?}", linux_launch_command);

        let mut cmd = Command::new("systemd-run");
        cmd.arg("--scope");
        cmd.arg("--user");
        cmd.arg("-p");
        cmd.arg("Delegate=yes");
        cmd.arg("--unit");
        cmd.arg(&systemd_unit_name);

        if exe_name.to_string_lossy().ends_with(".exe") {
            // 按空白拆分为多个参数，支持 "proton-autogen --profile dx11" 这类多词命令
            // ponytail: 不处理带空格的 profile 名，proton-autogen 的 profile 是文件名，不含空格
            for part in linux_launch_command.split_whitespace() {
                cmd.arg(part);
            }
        }
        cmd.arg(&game_path);
        cmd.current_dir(&game_dir);
        cmd
    };

    let mut command = build_command(game.proton_profile.as_deref());

    let args_clone = args.clone();
    if let Some(arguments) = &args_clone {
        command.args(arguments);
    }

    debug!(
        "准备启动游戏 game_id={} scope={} command={} arg_count={} cwd={}",
        game_id,
        systemd_unit_name,
        if exe_name.to_string_lossy().ends_with(".exe") {
            "systemd-run+wine"
        } else {
            "systemd-run"
        },
        args_clone.as_ref().map_or(0, Vec::len),
        game_dir.display()
    );

    match command.spawn() {
        Ok(mut child) => {
            // proton 快速失败回退:启动后短时间内非零退出(游戏配置缺失、
            // Proton 运行时不可用等)则改用 settings 里的 Linux 启动命令重试。
            // ponytail: 8s 窗口内活着就认为启动成功;首次运行下载 GE-Proton
            // 属于"活着",不会误触;8s 后才崩的游戏属游戏自身问题,不回退
            let mut need_fallback = false;
            if game.proton_profile.is_some() && exe_name.to_string_lossy().ends_with(".exe") {
                let deadline =
                    tokio::time::Instant::now() + std::time::Duration::from_secs(8);
                loop {
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            need_fallback = !status.success();
                            break;
                        }
                        Ok(None) if tokio::time::Instant::now() < deadline => {
                            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                        }
                        Ok(None) | Err(_) => break,
                    }
                }
            }

            if need_fallback {
                info!(
                    "proton-autogen 快速失败 game_id={}，回退 Linux 启动命令",
                    game_id
                );
                let _ = check_scope_or_reset_failed(&systemd_unit_name).await;
                let mut fallback = build_command(None);
                if let Some(arguments) = &args_clone {
                    fallback.args(arguments);
                }
                let child = fallback.spawn().map_err(|e| {
                    format!("proton 启动失败且回退也失败: {e}，目录: {game_dir:?}")
                })?;
                let process_id = child.id();
                info!(
                    "回退启动成功 game_id={} pid={} scope={}",
                    game_id, process_id, systemd_unit_name
                );

                monitor_game(
                    app_handle.clone(),
                    db.inner().clone(),
                    time_tracking_mode,
                    game_id,
                    process_id,
                    systemd_unit_name.clone(),
                )
                .await;

                return Ok(LaunchResult::tracking(
                    format!(
                        "proton-autogen 启动失败，已回退 Linux 启动命令启动: {}",
                        exe_name.to_string_lossy()
                    ),
                    Some(process_id),
                ));
            }

            let process_id = child.id();
            info!(
                "游戏启动成功 game_id={} pid={} scope={}",
                game_id, process_id, systemd_unit_name
            );

            monitor_game(
                app_handle.clone(),
                db.inner().clone(),
                time_tracking_mode,
                game_id,
                process_id,
                systemd_unit_name.clone(),
            )
            .await;

            Ok(LaunchResult::tracking(
                format!(
                    "成功启动游戏: {}，工作目录: {:?}",
                    exe_name.to_string_lossy(),
                    game_dir
                ),
                Some(process_id),
            ))
        }
        Err(e) => Err(format!("启动游戏失败: {}，目录: {:?}", e, game_dir)),
    }
}

#[command]
pub async fn stop_game(game_id: u32) -> Result<StopResult, String> {
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
pub async fn install_proton_autogen<R: Runtime>(
    app_handle: AppHandle<R>,
) -> Result<(), String> {
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
        // 使用 Tauri 提供的内置路径解析
        if let Ok(home_dir) = app_handle.path().home_dir() {
            path.replacen('~', &home_dir.to_string_lossy(), 1)
        } else {
            path.to_string()
        }
    } else {
        path.to_string()
    }
}

/// 在 Linux 上检查 systemd scope 的状态，如果是 failed 则重置它
/// 返回bool值表示scope是否已经存在
/// # Arguments
/// * `systemd_unit_name` - systemd 单元名称
///
/// # Returns
/// bool - 如果 scope 已存在则返回 true，否则返回 false
async fn check_scope_or_reset_failed(systemd_unit_name: &str) -> Result<bool, String> {
    use crate::game::monitor::{get_connection, get_manager_proxy};
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
