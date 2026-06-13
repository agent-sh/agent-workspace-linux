#[cfg(target_os = "linux")]
use mimalloc::MiMalloc;

#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

mod agent;
mod approval;
mod browser;
mod control;
mod guardrails;
mod permissions;
mod policy;
mod profile;
mod server;
mod viewer;
mod workspace;

use anyhow::{bail, Context, Result};
use policy::{AppliedWorkspacePolicy, MountMode, ProfileMount};
use profile::WorkspaceProfile;
use std::{fs, path::PathBuf};
use workspace::{DaemonOptions, EnvVar, LaunchSpec, WorkspaceStartOptions};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    let permissions = parse_global_options(&mut args)?;
    match args.first().map(String::as_str) {
        Some("doctor") => {
            let report = workspace::doctor_report();
            print_json(&report)
        }
        Some("guardrails") => print_json(&guardrails::guardrail_summary()),
        Some("mcp") => {
            args.remove(0);
            match parse_mcp_options(&args, permissions)? {
                Some(options) => server::serve_mcp(options.permissions, options.headless).await,
                None => Ok(()),
            }
        }
        Some("permissions") => {
            args.remove(0);
            handle_permissions(args)
        }
        Some("profile") => {
            args.remove(0);
            handle_profile(args, &permissions)
        }
        Some("workspace") => {
            args.remove(0);
            handle_workspace(args, &permissions)
        }
        Some("viewer") => {
            args.remove(0);
            if let Some(command) = args.first().map(String::as_str) {
                match command {
                    "list" => {
                        parse_no_options(&args[1..], "viewer list")?;
                        return print_json(&viewer::list_viewers()?);
                    }
                    "close" => {
                        let close = parse_viewer_close_options(&args[1..])?;
                        return print_json(&viewer::close_viewers(
                            close.id,
                            close.all,
                            close.dry_run,
                        )?);
                    }
                    _ => {}
                }
            }
            match parse_viewer_options(&args)? {
                Some(mut options) => {
                    if permissions.configured || permissions.restricted {
                        options.permissions = permissions;
                    }
                    viewer::run(options)
                }
                None => Ok(()),
            }
        }
        Some("daemon") => {
            args.remove(0);
            workspace::run_daemon(parse_daemon_options(args)?)
        }
        Some("--help") | Some("-h") | None => {
            print_help();
            Ok(())
        }
        Some(command) => {
            bail!(
                "unknown command '{command}'. Expected one of: doctor, guardrails, mcp, permissions, profile, workspace, viewer, --help"
            )
        }
    }
}

fn parse_global_options(args: &mut Vec<String>) -> Result<permissions::McpPermissionState> {
    let mut permissions_path = None;
    while let Some(arg) = args.first() {
        if let Some(path) = arg.strip_prefix("--permissions=") {
            if path.is_empty() {
                bail!("--permissions requires a path");
            }
            permissions_path = Some(PathBuf::from(path));
            args.remove(0);
            continue;
        }
        if arg == "--permissions" {
            if args.len() < 2 {
                bail!("--permissions requires a path");
            }
            permissions_path = Some(PathBuf::from(args.remove(1)));
            args.remove(0);
            continue;
        }
        break;
    }
    // Developers (or MCP host configs) who want to limit the server without
    // passing a flag can export AGENT_WORKSPACE_PERMISSIONS=/path/to/file.json;
    // the server starts with that ceiling and the daemon enforces it. An
    // explicit --permissions flag takes precedence over the environment.
    let permissions_path = permissions_path.or_else(env_permissions_path);
    match permissions_path {
        Some(path) => permissions::load_mcp_permission_state(path),
        None => Ok(permissions::McpPermissionState::default()),
    }
}

/// Permission ceiling path from the environment, if set and non-empty. Lets a
/// dev or host config enforce a ceiling via `AGENT_WORKSPACE_PERMISSIONS`
/// without a command-line flag. By default (unset) the MCP imposes no ceiling
/// and defers to the host tool's permission boundary.
fn env_permissions_path() -> Option<PathBuf> {
    let value = std::env::var_os("AGENT_WORKSPACE_PERMISSIONS")?;
    if value.is_empty() {
        return None;
    }
    Some(PathBuf::from(value))
}

fn handle_profile(args: Vec<String>, permissions: &permissions::McpPermissionState) -> Result<()> {
    let Some(command) = args.first().map(String::as_str) else {
        bail!("missing profile command. Expected: path, list, get, check, validate, template, put, import, export, delete");
    };
    match command {
        "path" => {
            parse_no_options(&args[1..], "profile path")?;
            print_json(&profile::profile_path())
        }
        "list" => {
            parse_no_options(&args[1..], "profile list")?;
            print_json(&profile::list_profiles()?)
        }
        "get" => {
            let id = parse_required_id_arg(&args[1..], "profile get requires an id")?;
            print_json(&profile::get_profile(&id)?)
        }
        "check" => {
            let id = parse_required_id_arg(&args[1..], "profile check requires an id")?;
            permissions.validate_profile(&profile::get_profile(&id)?)?;
            print_json(&profile::check_profile(&id)?)
        }
        "validate" => {
            let json_path = parse_profile_validate_options(&args[1..])?;
            let validation = profile::validate_profile_json_file(json_path)?;
            permissions.validate_profile(&validation.profile)?;
            print_json(&validation)
        }
        "template" => {
            let (kind, id, host_path, browser_path, user_data_dir) =
                parse_profile_template_options(&args[1..])?;
            let template =
                profile::template_profile(&kind, id, host_path, browser_path, user_data_dir)?;
            permissions.validate_profile(&template)?;
            print_json(&template)
        }
        "put" => {
            let (workspace_profile, replace, dry_run) = parse_profile_put_options(&args[1..])?;
            permissions.validate_profile(&workspace_profile)?;
            print_json(&profile::put_profile(workspace_profile, replace, dry_run)?)
        }
        "import" => {
            let (json_path, replace, dry_run) = parse_profile_import_options(&args[1..])?;
            let workspace_profile = profile::read_profile_json_file(&json_path)?;
            permissions.validate_profile(&workspace_profile)?;
            print_json(&profile::put_profile(workspace_profile, replace, dry_run)?)
        }
        "export" => {
            let (id, output_path, replace) = parse_profile_export_options(&args[1..])?;
            print_json(&profile::export_profile(&id, output_path, replace)?)
        }
        "delete" => {
            let (id, dry_run) = parse_profile_delete_options(&args[1..])?;
            print_json(&profile::delete_profile(&id, dry_run)?)
        }
        unknown => {
            bail!("unknown profile command '{unknown}'. Expected: path, list, get, check, validate, template, put, import, export, delete")
        }
    }
}

fn handle_permissions(args: Vec<String>) -> Result<()> {
    let Some(command) = args.first().map(String::as_str) else {
        bail!("missing permissions command. Expected: validate, template");
    };
    match command {
        "validate" => {
            let json_path = parse_permissions_validate_options(&args[1..])?;
            print_json(&permissions::load_mcp_permission_state(json_path)?)
        }
        "template" => {
            let (kind, allow_hosts, mounts, apps) = parse_permissions_template_options(&args[1..])?;
            print_json(&permissions::template_permission_ceiling(
                &kind,
                allow_hosts,
                mounts,
                apps,
            )?)
        }
        unknown => bail!("unknown permissions command '{unknown}'. Expected: validate, template"),
    }
}

struct McpOptions {
    permissions: permissions::McpPermissionState,
    headless: bool,
}

fn parse_mcp_options(
    args: &[String],
    default_permissions: permissions::McpPermissionState,
) -> Result<Option<McpOptions>> {
    let mut permissions_path = None;
    let mut headless = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if let Some(path) = arg.strip_prefix("--permissions=") {
            if path.is_empty() {
                bail!("--permissions requires a path");
            }
            permissions_path = Some(PathBuf::from(path));
            index += 1;
            continue;
        }
        match arg.as_str() {
            "--permissions" => {
                index += 1;
                let Some(path) = args.get(index) else {
                    bail!("--permissions requires a path");
                };
                permissions_path = Some(PathBuf::from(path));
                index += 1;
            }
            "--headless" => {
                headless = true;
                index += 1;
            }
            "--help" | "-h" => {
                print_mcp_help();
                return Ok(None);
            }
            unknown => {
                bail!(
                    "unknown mcp option '{unknown}'. Expected: --permissions PATH, --headless, --help"
                )
            }
        }
    }
    let permissions = match permissions_path {
        Some(path) => permissions::load_mcp_permission_state(path)?,
        None => default_permissions,
    };
    Ok(Some(McpOptions {
        permissions,
        headless,
    }))
}

fn parse_viewer_options(args: &[String]) -> Result<Option<viewer::ViewerOptions>> {
    let mut options = viewer::ViewerOptions::default();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if let Some(id) = arg.strip_prefix("--id=") {
            if id.is_empty() {
                bail!("--id requires a workspace id");
            }
            options.id = id.to_string();
            index += 1;
            continue;
        }
        match arg.as_str() {
            "--always-on-top" => {
                options.always_on_top = true;
                index += 1;
            }
            "--input-forwarding" => {
                options.input_forwarding = true;
                index += 1;
            }
            "--exit-when-workspace-gone" => {
                options.exit_when_workspace_gone = true;
                index += 1;
            }
            "--background" => {
                options.background = true;
                index += 1;
            }
            "--id" => {
                index += 1;
                let Some(id) = args.get(index) else {
                    bail!("--id requires a workspace id");
                };
                options.id = id.clone();
                index += 1;
            }
            "--help" | "-h" => {
                print_viewer_help();
                return Ok(None);
            }
            unknown => bail!(
                "unknown viewer option '{unknown}'. Expected: --id ID, --always-on-top, --input-forwarding, --background, --exit-when-workspace-gone, --help"
            ),
        }
    }
    Ok(Some(options))
}

struct ViewerCloseOptions {
    id: Option<String>,
    all: bool,
    dry_run: bool,
}

fn parse_viewer_close_options(args: &[String]) -> Result<ViewerCloseOptions> {
    let mut id = None;
    let mut all = false;
    let mut dry_run = false;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        if let Some(value) = arg.strip_prefix("--id=") {
            if value.is_empty() {
                bail!("--id requires a workspace id");
            }
            id = Some(value.to_string());
            index += 1;
            continue;
        }
        match arg.as_str() {
            "--id" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    bail!("--id requires a workspace id");
                };
                id = Some(value.clone());
                index += 1;
            }
            "--all" => {
                all = true;
                index += 1;
            }
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            "--help" | "-h" => {
                print_viewer_help();
                std::process::exit(0);
            }
            unknown => bail!(
                "unknown viewer close option '{unknown}'. Expected: --id ID, --all, --dry-run, --help"
            ),
        }
    }
    if id.is_some() && all {
        bail!("viewer close accepts either --id ID or --all, not both");
    }
    if id.is_none() && !all {
        bail!("viewer close requires --id ID or --all");
    }
    Ok(ViewerCloseOptions { id, all, dry_run })
}

fn handle_workspace(
    args: Vec<String>,
    permissions: &permissions::McpPermissionState,
) -> Result<()> {
    let Some(command) = args.first().map(String::as_str) else {
        bail!(
            "missing workspace command. Expected: start, open-profile, list, cleanup, status, manifest, artifacts, ipc-info, env, launch, run, launch-profile-apps, apps, browser-targets, browser-snapshot, browser-search-results, browser-navigate, windows, active-window, pointer, observe, wait-window, screenshot, screenshot-window, focus-window, close-window, move-window, resize-window, raise-window, minimize-window, show-window, click, click-window, move-pointer, move-pointer-window, drag, drag-window, scroll, scroll-window, key, key-window, type, type-window, clipboard-set, clipboard-get, paste, paste-window, logs, wait-app, events, setup, kill-app, stop"
        );
    };
    match command {
        "start" => {
            let mut start = parse_start_options(&args[1..])?;
            if let Some(profile_id) = &start.profile_id {
                permissions.validate_profile(&profile::get_profile(profile_id)?)?;
            }
            permissions.validate_start_options(&start.options)?;
            start.options.permissions_source = permissions.source.clone();
            if start.dry_run {
                print_json(&workspace::preview_workspace_start(start.options)?)
            } else if start.foreground {
                workspace::start_workspace_foreground(start.options)
            } else {
                print_json(&workspace::start_workspace(start.options)?)
            }
        }
        "open-profile" => {
            let (mut start, profile_id, open_options) = parse_open_profile_options(&args[1..])?;
            permissions.validate_profile(&profile::get_profile(&profile_id)?)?;
            permissions.validate_start_options(&start.options)?;
            start.options.permissions_source = permissions.source.clone();
            if start.dry_run {
                print_json(&profile::preview_open_profile_workspace(
                    start.options,
                    &profile_id,
                    open_options,
                )?)
            } else {
                print_json(&profile::open_profile_workspace(
                    start.options,
                    &profile_id,
                    open_options,
                )?)
            }
        }
        "status" => {
            let id = parse_id_option(&args[1..])?;
            print_json(&workspace::status_workspace(&id)?)
        }
        "manifest" => {
            let id = parse_id_option(&args[1..])?;
            print_json(&workspace::read_manifest(&id))
        }
        "artifacts" => {
            let (id, existing_only) = parse_workspace_artifacts_options(&args[1..])?;
            print_json(&workspace::artifacts(&id, existing_only))
        }
        "ipc-info" => {
            let id = parse_id_option(&args[1..])?;
            print_json(&workspace::ipc_info(&id)?)
        }
        "env" => {
            let (id, shell) = parse_workspace_env_options(&args[1..])?;
            let response = workspace::environment(&id)?;
            if shell {
                print_workspace_env_shell(&response)
            } else {
                print_json(&response)
            }
        }
        "list" => {
            parse_no_options(&args[1..], "workspace list")?;
            print_json(&workspace::list_workspaces()?)
        }
        "cleanup" => {
            let (id, dry_run) = parse_cleanup_options(&args[1..])?;
            print_json(&workspace::cleanup_stale_workspaces(id, dry_run)?)
        }
        "launch" => {
            let launch = parse_launch_options(&args[1..])?;
            permissions.validate_launch_spec(&launch.spec)?;
            let response = if launch.dry_run {
                workspace::preview_launch_app(
                    &launch.id,
                    launch.spec,
                    launch.wait_window,
                    launch.window_timeout_ms,
                    launch.screenshot_window,
                )?
            } else {
                workspace::launch_app_with_options(
                    &launch.id,
                    launch.spec,
                    launch.wait_window,
                    launch.window_timeout_ms,
                    launch.screenshot_window,
                )?
            };
            print_json(&response)
        }
        "run" => {
            let run = parse_run_options(&args[1..])?;
            permissions.validate_launch_spec(&run.spec)?;
            if run.dry_run {
                print_json(&workspace::preview_run_app_with_spec(
                    &run.id,
                    run.spec,
                    run.timeout_ms,
                    run.tail_bytes,
                    run.kill_on_timeout,
                )?)
            } else {
                print_json(&workspace::run_app_with_spec(
                    &run.id,
                    run.spec,
                    run.timeout_ms,
                    run.tail_bytes,
                    run.kill_on_timeout,
                )?)
            }
        }
        "launch-profile-apps" => {
            let (id, profile_id, options) = parse_profile_launch_options(&args[1..])?;
            permissions.validate_profile(&profile::get_profile(&profile_id)?)?;
            print_json(&profile::launch_profile_startup_apps(
                &id,
                &profile_id,
                options,
            )?)
        }
        "apps" => {
            let parsed = parse_apps_options(&args[1..])?;
            print_json(&workspace::list_apps(
                &parsed.id,
                parsed.app_id,
                parsed.name_contains,
                parsed.command_contains,
                parsed.profile_id,
                parsed.running,
            )?)
        }
        "browser-targets" => {
            let parsed = parse_browser_targets_options(&args[1..])?;
            print_json(&browser::workspace_browser_targets(
                &parsed.id,
                parsed.app_id,
                parsed.user_data_dir,
                parsed.timeout_ms,
            )?)
        }
        "browser-snapshot" => {
            let parsed = parse_browser_snapshot_options(&args[1..])?;
            print_json(&browser::workspace_browser_snapshot(
                &parsed.id,
                parsed.app_id,
                parsed.user_data_dir,
                parsed.target_id,
                parsed.title_contains,
                parsed.url_contains,
                parsed.max_text_chars,
                parsed.timeout_ms,
            )?)
        }
        "browser-search-results" => {
            let parsed = parse_browser_search_results_options(&args[1..])?;
            print_json(&browser::workspace_browser_search_results(
                &parsed.id,
                parsed.app_id,
                parsed.user_data_dir,
                parsed.target_id,
                parsed.title_contains,
                parsed.url_contains,
                parsed.max_results,
                parsed.min_vram_gb,
                parsed.timeout_ms,
            )?)
        }
        "browser-navigate" => {
            let parsed = parse_browser_navigate_options(&args[1..])?;
            print_json(&browser::workspace_browser_navigate(
                &parsed.id,
                parsed.app_id,
                parsed.user_data_dir,
                parsed.target_id,
                parsed.title_contains,
                parsed.url_contains,
                parsed.url,
                parsed.wait_ms,
                parsed.snapshot,
                parsed.max_text_chars,
                parsed.timeout_ms,
            )?)
        }
        "windows" => {
            let (id, include_hidden, title_contains, class_contains, pid, app_id) =
                parse_windows_options(&args[1..])?;
            print_json(&workspace::list_windows(
                &id,
                include_hidden,
                title_contains,
                class_contains,
                pid,
                app_id,
            )?)
        }
        "active-window" => {
            let id = parse_id_option(&args[1..])?;
            print_json(&workspace::active_window(&id)?)
        }
        "pointer" => {
            let id = parse_id_option(&args[1..])?;
            print_json(&workspace::pointer(&id)?)
        }
        "observe" => {
            let (
                id,
                screenshot,
                include_hidden,
                output_path,
                include_events,
                events_tail,
                events_since_sequence,
            ) = parse_observe_options(&args[1..])?;
            print_json(&workspace::observe(
                &id,
                screenshot,
                include_hidden,
                output_path,
                include_events,
                events_tail,
                events_since_sequence,
            )?)
        }
        "wait-window" => {
            let (id, title_contains, class_contains, pid, app_id, timeout_ms) =
                parse_wait_window_options(&args[1..])?;
            print_json(&workspace::wait_window(
                &id,
                title_contains,
                class_contains,
                pid,
                app_id,
                timeout_ms,
            )?)
        }
        "screenshot" => {
            let (id, output_path) = parse_screenshot_options(&args[1..])?;
            print_json(&workspace::screenshot(&id, output_path)?)
        }
        "screenshot-window" => {
            let (
                id,
                window_id,
                title_contains,
                class_contains,
                pid,
                app_id,
                output_path,
                timeout_ms,
            ) = parse_screenshot_window_options(&args[1..])?;
            print_json(&workspace::screenshot_window(
                &id,
                window_id,
                title_contains,
                class_contains,
                pid,
                app_id,
                output_path,
                timeout_ms,
            )?)
        }
        "focus-window" => {
            let (id, target) = parse_focus_window_options(&args[1..])?;
            match target {
                FocusWindowTarget::WindowId(window_id) => {
                    print_json(&workspace::focus_window(&id, window_id)?)
                }
                FocusWindowTarget::Match {
                    title_contains,
                    class_contains,
                    pid,
                    app_id,
                    timeout_ms,
                } => print_json(&workspace::focus_matching_window(
                    &id,
                    title_contains,
                    class_contains,
                    pid,
                    app_id,
                    timeout_ms,
                )?),
            }
        }
        "close-window" => {
            let (id, target, dry_run) = parse_close_window_options(&args[1..])?;
            match target {
                CloseWindowTarget::WindowId(window_id) => {
                    print_json(&workspace::close_window(&id, window_id, dry_run)?)
                }
                CloseWindowTarget::Match {
                    title_contains,
                    class_contains,
                    pid,
                    app_id,
                    timeout_ms,
                } => print_json(&workspace::close_matching_window(
                    &id,
                    title_contains,
                    class_contains,
                    pid,
                    app_id,
                    timeout_ms,
                    dry_run,
                )?),
            }
        }
        "move-window" => {
            let (id, window_id, title_contains, class_contains, pid, app_id, x, y, timeout_ms) =
                parse_move_window_options(&args[1..])?;
            print_json(&workspace::move_window(
                &id,
                window_id,
                title_contains,
                class_contains,
                pid,
                app_id,
                x,
                y,
                timeout_ms,
            )?)
        }
        "resize-window" => {
            let (
                id,
                window_id,
                title_contains,
                class_contains,
                pid,
                app_id,
                width,
                height,
                timeout_ms,
            ) = parse_resize_window_options(&args[1..])?;
            print_json(&workspace::resize_window(
                &id,
                window_id,
                title_contains,
                class_contains,
                pid,
                app_id,
                width,
                height,
                timeout_ms,
            )?)
        }
        "raise-window" => {
            let (id, window_id, title_contains, class_contains, pid, app_id, timeout_ms) =
                parse_targeted_window_action_options(&args[1..], "workspace raise-window")?;
            print_json(&workspace::raise_window(
                &id,
                window_id,
                title_contains,
                class_contains,
                pid,
                app_id,
                timeout_ms,
            )?)
        }
        "minimize-window" => {
            let (id, window_id, title_contains, class_contains, pid, app_id, timeout_ms) =
                parse_targeted_window_action_options(&args[1..], "workspace minimize-window")?;
            print_json(&workspace::minimize_window(
                &id,
                window_id,
                title_contains,
                class_contains,
                pid,
                app_id,
                timeout_ms,
            )?)
        }
        "show-window" => {
            let (id, window_id, title_contains, class_contains, pid, app_id, timeout_ms) =
                parse_targeted_window_action_options(&args[1..], "workspace show-window")?;
            print_json(&workspace::show_window(
                &id,
                window_id,
                title_contains,
                class_contains,
                pid,
                app_id,
                timeout_ms,
            )?)
        }
        "click" => {
            let (id, x, y, button, count) = parse_click_options(&args[1..])?;
            print_json(&workspace::click(&id, x, y, button, count)?)
        }
        "click-window" => {
            let (
                id,
                window_id,
                title_contains,
                class_contains,
                pid,
                app_id,
                x,
                y,
                button,
                count,
                timeout_ms,
            ) = parse_click_window_options(&args[1..])?;
            print_json(&workspace::click_window(
                &id,
                window_id,
                title_contains,
                class_contains,
                pid,
                app_id,
                x,
                y,
                button,
                count,
                timeout_ms,
            )?)
        }
        "move-pointer" => {
            let (id, x, y) = parse_move_pointer_options(&args[1..])?;
            print_json(&workspace::move_pointer(&id, x, y)?)
        }
        "move-pointer-window" => {
            let (id, window_id, title_contains, class_contains, pid, app_id, x, y, timeout_ms) =
                parse_move_pointer_window_options(&args[1..])?;
            print_json(&workspace::move_pointer_window(
                &id,
                window_id,
                title_contains,
                class_contains,
                pid,
                app_id,
                x,
                y,
                timeout_ms,
            )?)
        }
        "drag" => {
            let (id, from_x, from_y, to_x, to_y, button) = parse_drag_options(&args[1..])?;
            print_json(&workspace::drag(&id, from_x, from_y, to_x, to_y, button)?)
        }
        "drag-window" => {
            let (
                id,
                window_id,
                title_contains,
                class_contains,
                pid,
                app_id,
                from_x,
                from_y,
                to_x,
                to_y,
                button,
                timeout_ms,
            ) = parse_drag_window_options(&args[1..])?;
            print_json(&workspace::drag_window(
                &id,
                window_id,
                title_contains,
                class_contains,
                pid,
                app_id,
                from_x,
                from_y,
                to_x,
                to_y,
                button,
                timeout_ms,
            )?)
        }
        "scroll" => {
            let (id, x, y, direction, amount) = parse_scroll_options(&args[1..])?;
            print_json(&workspace::scroll(&id, x, y, direction, amount)?)
        }
        "scroll-window" => {
            let (
                id,
                window_id,
                title_contains,
                class_contains,
                pid,
                app_id,
                x,
                y,
                direction,
                amount,
                timeout_ms,
            ) = parse_scroll_window_options(&args[1..])?;
            print_json(&workspace::scroll_window(
                &id,
                window_id,
                title_contains,
                class_contains,
                pid,
                app_id,
                x,
                y,
                direction,
                amount,
                timeout_ms,
            )?)
        }
        "key" => {
            let (id, key) = parse_one_arg_command(&args[1..], "workspace key requires a key")?;
            print_json(&workspace::key(&id, key)?)
        }
        "key-window" => {
            let (id, window_id, title_contains, class_contains, pid, app_id, key, timeout_ms) =
                parse_key_window_options(&args[1..])?;
            print_json(&workspace::key_window(
                &id,
                window_id,
                title_contains,
                class_contains,
                pid,
                app_id,
                key,
                timeout_ms,
            )?)
        }
        "type" => {
            let (id, text) = parse_text_command(&args[1..])?;
            print_json(&workspace::type_text(&id, text)?)
        }
        "type-window" => {
            let (id, window_id, title_contains, class_contains, pid, app_id, text, timeout_ms) =
                parse_type_window_options(&args[1..])?;
            print_json(&workspace::type_window(
                &id,
                window_id,
                title_contains,
                class_contains,
                pid,
                app_id,
                text,
                timeout_ms,
            )?)
        }
        "clipboard-set" => {
            let (id, text) = parse_clipboard_set_options(&args[1..])?;
            print_json(&workspace::set_clipboard(&id, text)?)
        }
        "clipboard-get" => {
            let id = parse_id_option(&args[1..])?;
            print_json(&workspace::get_clipboard(&id)?)
        }
        "paste" => {
            let (id, text, key) = parse_paste_options(&args[1..])?;
            print_json(&workspace::paste_text(&id, text, key)?)
        }
        "paste-window" => {
            let (id, window_id, title_contains, class_contains, pid, app_id, text, key, timeout_ms) =
                parse_paste_window_options(&args[1..])?;
            print_json(&workspace::paste_window(
                &id,
                window_id,
                title_contains,
                class_contains,
                pid,
                app_id,
                text,
                key,
                timeout_ms,
            )?)
        }
        "logs" => {
            let (id, app_id, stream, tail_bytes) = parse_logs_options(&args[1..])?;
            print_json(&workspace::read_app_log(&id, app_id, stream, tail_bytes)?)
        }
        "wait-app" => {
            let (id, app_id, timeout_ms, kill_on_timeout) = parse_wait_app_options(&args[1..])?;
            print_json(&workspace::wait_app(
                &id,
                app_id,
                timeout_ms,
                kill_on_timeout,
            )?)
        }
        "events" => {
            let (id, tail, since_sequence) = parse_events_options(&args[1..])?;
            print_json(&workspace::read_events(&id, tail, since_sequence)?)
        }
        "setup" => {
            let (id, profile_id, options) = parse_workspace_setup_options(&args[1..])?;
            permissions.validate_profile(&profile::get_profile(&profile_id)?)?;
            print_json(&profile::launch_profile_setup(&id, &profile_id, options)?)
        }
        "kill-app" => {
            let (id, app_id, dry_run) = parse_kill_app_options(&args[1..])?;
            print_json(&workspace::kill_app(&id, app_id, dry_run)?)
        }
        "stop" => {
            let (id, timeout_ms, dry_run) = parse_stop_options(&args[1..])?;
            print_json(&workspace::stop_workspace(&id, timeout_ms, dry_run)?)
        }
        unknown => {
            bail!(
                "unknown workspace command '{unknown}'. Expected: {}",
                "start, open-profile, list, cleanup, status, manifest, artifacts, ipc-info, env, launch, run, launch-profile-apps, apps, browser-targets, browser-snapshot, browser-search-results, browser-navigate, windows, active-window, pointer, observe, wait-window, screenshot, screenshot-window, focus-window, close-window, move-window, resize-window, raise-window, minimize-window, show-window, click, click-window, move-pointer, move-pointer-window, drag, drag-window, scroll, scroll-window, key, key-window, type, type-window, clipboard-set, clipboard-get, paste, paste-window, logs, wait-app, events, setup, kill-app, stop"
            )
        }
    }
}

struct ParsedStartOptions {
    options: WorkspaceStartOptions,
    profile_id: Option<String>,
    foreground: bool,
    dry_run: bool,
}

fn parse_start_options(args: &[String]) -> Result<ParsedStartOptions> {
    let mut options = WorkspaceStartOptions::default();
    let mut foreground = false;
    let mut dry_run = false;
    let mut profile_id = None;
    let mut width_explicit = false;
    let mut height_explicit = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--foreground" => {
                foreground = true;
                index += 1;
            }
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            "--ack-hidden-workspace" => {
                options.user_acknowledged_hidden_workspace = true;
                index += 1;
            }
            "--ack-unenforced-policy" => {
                options.user_acknowledged_unenforced_policy = true;
                index += 1;
            }
            "--profile" => {
                profile_id = Some(value_after(args, index, "--profile")?.to_string());
                index += 2;
            }
            "--id" => {
                options.id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--purpose" => {
                options.purpose = Some(value_after(args, index, "--purpose")?.to_string());
                index += 2;
            }
            "--width" => {
                options.width = value_after(args, index, "--width")?
                    .parse()
                    .context("--width must be a positive integer")?;
                width_explicit = true;
                index += 2;
            }
            "--height" => {
                options.height = value_after(args, index, "--height")?
                    .parse()
                    .context("--height must be a positive integer")?;
                height_explicit = true;
                index += 2;
            }
            flag => bail!("unknown workspace start option '{flag}'"),
        }
    }
    if let Some(profile_id) = &profile_id {
        profile::apply_profile_to_start_options(
            profile_id,
            &mut options,
            width_explicit,
            height_explicit,
        )?;
    }
    Ok(ParsedStartOptions {
        options,
        profile_id,
        foreground,
        dry_run,
    })
}

fn parse_open_profile_options(
    args: &[String],
) -> Result<(
    ParsedStartOptions,
    String,
    profile::ProfileWorkspaceOpenOptions,
)> {
    let mut options = WorkspaceStartOptions::default();
    let mut profile_id = None;
    let mut width_explicit = false;
    let mut height_explicit = false;
    let mut open_options = profile::ProfileWorkspaceOpenOptions::default();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--ack-hidden-workspace" => {
                options.user_acknowledged_hidden_workspace = true;
                index += 1;
            }
            "--ack-unenforced-policy" => {
                options.user_acknowledged_unenforced_policy = true;
                open_options.setup.acknowledge_unenforced_policy = true;
                open_options.startup.acknowledge_unenforced_policy = true;
                index += 1;
            }
            "--dry-run" => {
                open_options.setup.dry_run = true;
                open_options.startup.dry_run = true;
                index += 1;
            }
            "--profile" => {
                profile_id = Some(value_after(args, index, "--profile")?.to_string());
                index += 2;
            }
            "--id" => {
                options.id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--purpose" => {
                options.purpose = Some(value_after(args, index, "--purpose")?.to_string());
                index += 2;
            }
            "--width" => {
                options.width = value_after(args, index, "--width")?
                    .parse()
                    .context("--width must be a positive integer")?;
                width_explicit = true;
                index += 2;
            }
            "--height" => {
                options.height = value_after(args, index, "--height")?
                    .parse()
                    .context("--height must be a positive integer")?;
                height_explicit = true;
                index += 2;
            }
            "--setup" => {
                open_options.run_setup = true;
                open_options.setup.wait = true;
                index += 1;
            }
            "--setup-timeout-ms" => {
                open_options.run_setup = true;
                open_options.setup.wait = true;
                open_options.setup.timeout_ms = Some(
                    value_after(args, index, "--setup-timeout-ms")?
                        .parse()
                        .context("--setup-timeout-ms must be a non-negative integer")?,
                );
                index += 2;
            }
            "--setup-kill-on-timeout" => {
                open_options.run_setup = true;
                open_options.setup.wait = true;
                open_options.setup.kill_on_timeout = true;
                index += 1;
            }
            "--startup-wait-window" => {
                open_options.startup.wait_window = true;
                index += 1;
            }
            "--startup-window-timeout-ms" => {
                open_options.startup.wait_window = true;
                open_options.startup.window_timeout_ms = Some(
                    value_after(args, index, "--startup-window-timeout-ms")?
                        .parse()
                        .context("--startup-window-timeout-ms must be a non-negative integer")?,
                );
                index += 2;
            }
            "--startup-screenshot-window" => {
                open_options.startup.wait_window = true;
                open_options.startup.screenshot_window = true;
                index += 1;
            }
            flag => bail!("unknown workspace open-profile option '{flag}'"),
        }
    }
    let profile_id = profile_id.context("workspace open-profile requires --profile PROFILE")?;
    profile::apply_profile_to_start_options(
        &profile_id,
        &mut options,
        width_explicit,
        height_explicit,
    )?;
    Ok((
        ParsedStartOptions {
            options,
            profile_id: Some(profile_id.clone()),
            foreground: false,
            dry_run: open_options.setup.dry_run || open_options.startup.dry_run,
        },
        profile_id,
        open_options,
    ))
}

fn parse_id_option(args: &[String]) -> Result<String> {
    let mut id = workspace::default_workspace_id();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            flag => bail!("unknown workspace option '{flag}'"),
        }
    }
    Ok(id)
}

fn parse_workspace_env_options(args: &[String]) -> Result<(String, bool)> {
    let mut id = workspace::default_workspace_id();
    let mut shell = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--shell" => {
                shell = true;
                index += 1;
            }
            flag => bail!("unknown workspace env option '{flag}'"),
        }
    }
    Ok((id, shell))
}

fn parse_workspace_artifacts_options(args: &[String]) -> Result<(String, bool)> {
    let mut id = workspace::default_workspace_id();
    let mut existing_only = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--existing" => {
                existing_only = true;
                index += 1;
            }
            flag => bail!("unknown workspace artifacts option '{flag}'"),
        }
    }
    Ok((id, existing_only))
}

struct ParsedAppsOptions {
    id: String,
    app_id: Option<String>,
    name_contains: Option<String>,
    command_contains: Option<String>,
    profile_id: Option<String>,
    running: Option<bool>,
}

struct ParsedBrowserTargetsOptions {
    id: String,
    app_id: Option<String>,
    user_data_dir: Option<PathBuf>,
    timeout_ms: Option<u64>,
}

struct ParsedBrowserSnapshotOptions {
    id: String,
    app_id: Option<String>,
    user_data_dir: Option<PathBuf>,
    target_id: Option<String>,
    title_contains: Option<String>,
    url_contains: Option<String>,
    max_text_chars: Option<usize>,
    timeout_ms: Option<u64>,
}

struct ParsedBrowserSearchResultsOptions {
    id: String,
    app_id: Option<String>,
    user_data_dir: Option<PathBuf>,
    target_id: Option<String>,
    title_contains: Option<String>,
    url_contains: Option<String>,
    max_results: Option<usize>,
    min_vram_gb: Option<u32>,
    timeout_ms: Option<u64>,
}

struct ParsedBrowserNavigateOptions {
    id: String,
    app_id: Option<String>,
    user_data_dir: Option<PathBuf>,
    target_id: Option<String>,
    title_contains: Option<String>,
    url_contains: Option<String>,
    url: String,
    wait_ms: Option<u64>,
    snapshot: bool,
    max_text_chars: Option<usize>,
    timeout_ms: Option<u64>,
}

fn parse_browser_targets_options(args: &[String]) -> Result<ParsedBrowserTargetsOptions> {
    let mut id = workspace::default_workspace_id();
    let mut app_id = None;
    let mut user_data_dir = None;
    let mut timeout_ms = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--app" => {
                app_id = Some(value_after(args, index, "--app")?.to_string());
                index += 2;
            }
            "--user-data-dir" => {
                user_data_dir = Some(PathBuf::from(value_after(args, index, "--user-data-dir")?));
                index += 2;
            }
            "--timeout-ms" => {
                timeout_ms = Some(
                    value_after(args, index, "--timeout-ms")?
                        .parse()
                        .context("--timeout-ms must be a non-negative integer")?,
                );
                index += 2;
            }
            flag => bail!("unknown workspace browser-targets option '{flag}'"),
        }
    }
    Ok(ParsedBrowserTargetsOptions {
        id,
        app_id,
        user_data_dir,
        timeout_ms,
    })
}

fn parse_browser_snapshot_options(args: &[String]) -> Result<ParsedBrowserSnapshotOptions> {
    let mut id = workspace::default_workspace_id();
    let mut app_id = None;
    let mut user_data_dir = None;
    let mut target_id = None;
    let mut title_contains = None;
    let mut url_contains = None;
    let mut max_text_chars = None;
    let mut timeout_ms = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--app" => {
                app_id = Some(value_after(args, index, "--app")?.to_string());
                index += 2;
            }
            "--user-data-dir" => {
                user_data_dir = Some(PathBuf::from(value_after(args, index, "--user-data-dir")?));
                index += 2;
            }
            "--target" => {
                target_id = Some(value_after(args, index, "--target")?.to_string());
                index += 2;
            }
            "--title" => {
                title_contains = Some(value_after(args, index, "--title")?.to_string());
                index += 2;
            }
            "--url-contains" => {
                url_contains = Some(value_after(args, index, "--url-contains")?.to_string());
                index += 2;
            }
            "--max-text-chars" => {
                max_text_chars = Some(
                    value_after(args, index, "--max-text-chars")?
                        .parse()
                        .context("--max-text-chars must be a non-negative integer")?,
                );
                index += 2;
            }
            "--timeout-ms" => {
                timeout_ms = Some(
                    value_after(args, index, "--timeout-ms")?
                        .parse()
                        .context("--timeout-ms must be a non-negative integer")?,
                );
                index += 2;
            }
            flag => bail!("unknown workspace browser-snapshot option '{flag}'"),
        }
    }
    Ok(ParsedBrowserSnapshotOptions {
        id,
        app_id,
        user_data_dir,
        target_id,
        title_contains,
        url_contains,
        max_text_chars,
        timeout_ms,
    })
}

fn parse_browser_search_results_options(
    args: &[String],
) -> Result<ParsedBrowserSearchResultsOptions> {
    let mut id = workspace::default_workspace_id();
    let mut app_id = None;
    let mut user_data_dir = None;
    let mut target_id = None;
    let mut title_contains = None;
    let mut url_contains = None;
    let mut max_results = None;
    let mut min_vram_gb = None;
    let mut timeout_ms = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--app" => {
                app_id = Some(value_after(args, index, "--app")?.to_string());
                index += 2;
            }
            "--user-data-dir" => {
                user_data_dir = Some(PathBuf::from(value_after(args, index, "--user-data-dir")?));
                index += 2;
            }
            "--target" => {
                target_id = Some(value_after(args, index, "--target")?.to_string());
                index += 2;
            }
            "--title" => {
                title_contains = Some(value_after(args, index, "--title")?.to_string());
                index += 2;
            }
            "--url-contains" => {
                url_contains = Some(value_after(args, index, "--url-contains")?.to_string());
                index += 2;
            }
            "--max-results" => {
                max_results = Some(
                    value_after(args, index, "--max-results")?
                        .parse()
                        .context("--max-results must be a positive integer")?,
                );
                index += 2;
            }
            "--min-vram-gb" => {
                min_vram_gb = Some(
                    value_after(args, index, "--min-vram-gb")?
                        .parse()
                        .context("--min-vram-gb must be a non-negative integer")?,
                );
                index += 2;
            }
            "--timeout-ms" => {
                timeout_ms = Some(
                    value_after(args, index, "--timeout-ms")?
                        .parse()
                        .context("--timeout-ms must be a non-negative integer")?,
                );
                index += 2;
            }
            flag => bail!("unknown workspace browser-search-results option '{flag}'"),
        }
    }
    Ok(ParsedBrowserSearchResultsOptions {
        id,
        app_id,
        user_data_dir,
        target_id,
        title_contains,
        url_contains,
        max_results,
        min_vram_gb,
        timeout_ms,
    })
}

fn parse_browser_navigate_options(args: &[String]) -> Result<ParsedBrowserNavigateOptions> {
    let mut id = workspace::default_workspace_id();
    let mut app_id = None;
    let mut user_data_dir = None;
    let mut target_id = None;
    let mut title_contains = None;
    let mut url_contains = None;
    let mut url = None;
    let mut wait_ms = None;
    let mut snapshot = true;
    let mut max_text_chars = None;
    let mut timeout_ms = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--app" => {
                app_id = Some(value_after(args, index, "--app")?.to_string());
                index += 2;
            }
            "--user-data-dir" => {
                user_data_dir = Some(PathBuf::from(value_after(args, index, "--user-data-dir")?));
                index += 2;
            }
            "--target" => {
                target_id = Some(value_after(args, index, "--target")?.to_string());
                index += 2;
            }
            "--title" => {
                title_contains = Some(value_after(args, index, "--title")?.to_string());
                index += 2;
            }
            "--url-contains" => {
                url_contains = Some(value_after(args, index, "--url-contains")?.to_string());
                index += 2;
            }
            "--wait-ms" => {
                wait_ms = Some(
                    value_after(args, index, "--wait-ms")?
                        .parse()
                        .context("--wait-ms must be a non-negative integer")?,
                );
                index += 2;
            }
            "--no-snapshot" => {
                snapshot = false;
                index += 1;
            }
            "--max-text-chars" => {
                max_text_chars = Some(
                    value_after(args, index, "--max-text-chars")?
                        .parse()
                        .context("--max-text-chars must be a non-negative integer")?,
                );
                index += 2;
            }
            "--timeout-ms" => {
                timeout_ms = Some(
                    value_after(args, index, "--timeout-ms")?
                        .parse()
                        .context("--timeout-ms must be a non-negative integer")?,
                );
                index += 2;
            }
            value if !value.starts_with('-') && url.is_none() => {
                url = Some(value.to_string());
                index += 1;
            }
            flag => bail!("unknown workspace browser-navigate option '{flag}'"),
        }
    }
    let url = url.context("workspace browser-navigate requires a URL")?;
    Ok(ParsedBrowserNavigateOptions {
        id,
        app_id,
        user_data_dir,
        target_id,
        title_contains,
        url_contains,
        url,
        wait_ms,
        snapshot,
        max_text_chars,
        timeout_ms,
    })
}

fn parse_apps_options(args: &[String]) -> Result<ParsedAppsOptions> {
    let mut id = workspace::default_workspace_id();
    let mut app_id = None;
    let mut name_contains = None;
    let mut command_contains = None;
    let mut profile_id = None;
    let mut running = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--app" => {
                app_id = Some(value_after(args, index, "--app")?.to_string());
                index += 2;
            }
            "--name" => {
                name_contains = Some(value_after(args, index, "--name")?.to_string());
                index += 2;
            }
            "--command" => {
                command_contains = Some(value_after(args, index, "--command")?.to_string());
                index += 2;
            }
            "--profile" => {
                profile_id = Some(value_after(args, index, "--profile")?.to_string());
                index += 2;
            }
            "--running" => {
                if running == Some(false) {
                    bail!("workspace apps accepts only one of --running or --stopped");
                }
                running = Some(true);
                index += 1;
            }
            "--stopped" => {
                if running == Some(true) {
                    bail!("workspace apps accepts only one of --running or --stopped");
                }
                running = Some(false);
                index += 1;
            }
            flag => bail!("unknown workspace apps option '{flag}'"),
        }
    }
    Ok(ParsedAppsOptions {
        id,
        app_id,
        name_contains,
        command_contains,
        profile_id,
        running,
    })
}

#[allow(clippy::type_complexity)]
fn parse_windows_options(
    args: &[String],
) -> Result<(
    String,
    bool,
    Option<String>,
    Option<String>,
    Option<u32>,
    Option<String>,
)> {
    let mut id = workspace::default_workspace_id();
    let mut include_hidden = false;
    let mut title_contains = None;
    let mut class_contains = None;
    let mut pid = None;
    let mut app_id = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--all" | "--include-hidden" => {
                include_hidden = true;
                index += 1;
            }
            "--title" => {
                title_contains = Some(value_after(args, index, "--title")?.to_string());
                index += 2;
            }
            "--class" => {
                class_contains = Some(value_after(args, index, "--class")?.to_string());
                index += 2;
            }
            "--pid" => {
                pid = Some(
                    value_after(args, index, "--pid")?
                        .parse()
                        .context("--pid must be a positive integer")?,
                );
                index += 2;
            }
            "--app" => {
                app_id = Some(value_after(args, index, "--app")?.to_string());
                index += 2;
            }
            flag => bail!("unknown workspace windows option '{flag}'"),
        }
    }
    Ok((
        id,
        include_hidden,
        title_contains,
        class_contains,
        pid,
        app_id,
    ))
}

fn parse_no_options(args: &[String], command: &str) -> Result<()> {
    if let Some(arg) = args.first() {
        bail!("{command} does not accept option '{arg}'");
    }
    Ok(())
}

fn parse_required_id_arg(args: &[String], missing_message: &str) -> Result<String> {
    if args.len() != 1 {
        bail!("{missing_message}");
    }
    Ok(args[0].clone())
}

fn parse_profile_put_options(args: &[String]) -> Result<(WorkspaceProfile, bool, bool)> {
    let (json_path, replace, dry_run) = parse_profile_json_file_options(args, "profile put")?;
    let profile = profile::read_profile_json_file(&json_path)?;
    Ok((profile, replace, dry_run))
}

fn parse_profile_import_options(args: &[String]) -> Result<(PathBuf, bool, bool)> {
    parse_profile_json_file_options(args, "profile import")
}

fn parse_profile_validate_options(args: &[String]) -> Result<PathBuf> {
    let mut json_path = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => {
                json_path = Some(PathBuf::from(value_after(args, index, "--json")?));
                index += 2;
            }
            flag => {
                bail!("unknown profile validate option '{flag}'. Expected: --json PATH")
            }
        }
    }
    json_path.context("profile validate requires --json PATH")
}

fn parse_permissions_validate_options(args: &[String]) -> Result<PathBuf> {
    let mut json_path = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => {
                json_path = Some(PathBuf::from(value_after(args, index, "--json")?));
                index += 2;
            }
            flag => {
                bail!("unknown permissions validate option '{flag}'. Expected: --json PATH")
            }
        }
    }
    json_path.context("permissions validate requires --json PATH")
}

#[allow(clippy::type_complexity)]
fn parse_permissions_template_options(
    args: &[String],
) -> Result<(String, Vec<String>, Vec<ProfileMount>, Vec<PathBuf>)> {
    let kind = args
        .first()
        .context("permissions template requires a kind: open, closed, or local")?
        .to_string();
    let mut allow_hosts = Vec::new();
    let mut mounts = Vec::new();
    let mut apps = Vec::new();
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--allow-host" => {
                allow_hosts.push(value_after(args, index, "--allow-host")?.to_string());
                index += 2;
            }
            "--mount" => {
                mounts.push(parse_permission_mount_spec(value_after(args, index, "--mount")?)?);
                index += 2;
            }
            "--app" => {
                apps.push(PathBuf::from(value_after(args, index, "--app")?));
                index += 2;
            }
            flag => bail!(
                "unknown permissions template option '{flag}'. Expected: [--allow-host HOST] [--mount HOST:WORKSPACE[:read_only|read_write]] [--app PROGRAM]"
            ),
        }
    }
    Ok((kind, allow_hosts, mounts, apps))
}

fn parse_permission_mount_spec(spec: &str) -> Result<ProfileMount> {
    let mut parts = spec.splitn(3, ':');
    let host_path = parts
        .next()
        .filter(|part| !part.is_empty())
        .context("--mount requires HOST:WORKSPACE[:read_only|read_write]")?;
    let workspace_path = parts
        .next()
        .filter(|part| !part.is_empty())
        .context("--mount requires HOST:WORKSPACE[:read_only|read_write]")?;
    let mode = match parts.next().unwrap_or("read_only") {
        "read_only" | "ro" => MountMode::ReadOnly,
        "read_write" | "rw" => MountMode::ReadWrite,
        other => bail!("unknown --mount mode '{other}'. Expected read_only or read_write"),
    };
    Ok(ProfileMount {
        host_path: PathBuf::from(host_path),
        workspace_path: PathBuf::from(workspace_path),
        mode,
    })
}

fn parse_profile_json_file_options(
    args: &[String],
    command: &str,
) -> Result<(PathBuf, bool, bool)> {
    let mut json_path = None;
    let mut replace = false;
    let mut dry_run = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => {
                json_path = Some(PathBuf::from(value_after(args, index, "--json")?));
                index += 2;
            }
            "--replace" => {
                replace = true;
                index += 1;
            }
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            flag => {
                bail!("unknown {command} option '{flag}'. Expected: --json PATH [--replace] [--dry-run]")
            }
        }
    }
    let json_path = json_path.with_context(|| format!("{command} requires --json PATH"))?;
    Ok((json_path, replace, dry_run))
}

#[allow(clippy::type_complexity)]
fn parse_profile_template_options(
    args: &[String],
) -> Result<(
    String,
    Option<String>,
    Option<PathBuf>,
    Option<PathBuf>,
    Option<PathBuf>,
)> {
    let kind = args
        .first()
        .context(
            "profile template requires a kind, for example project-dev, restricted-chrome, or browser-session",
        )?
        .to_string();
    let mut id = None;
    let mut host_path = None;
    let mut browser_path = None;
    let mut user_data_dir = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = Some(value_after(args, index, "--id")?.to_string());
                index += 2;
            }
            "--host-path" => {
                host_path = Some(PathBuf::from(value_after(args, index, "--host-path")?));
                index += 2;
            }
            "--browser-path" => {
                browser_path = Some(PathBuf::from(value_after(args, index, "--browser-path")?));
                index += 2;
            }
            "--user-data-dir" => {
                user_data_dir = Some(PathBuf::from(value_after(args, index, "--user-data-dir")?));
                index += 2;
            }
            flag => bail!("unknown profile template option '{flag}'"),
        }
    }
    Ok((kind, id, host_path, browser_path, user_data_dir))
}

fn parse_profile_export_options(args: &[String]) -> Result<(String, Option<PathBuf>, bool)> {
    let mut id = None;
    let mut output_path = None;
    let mut replace = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--output" => {
                output_path = Some(PathBuf::from(value_after(args, index, "--output")?));
                index += 2;
            }
            "--replace" => {
                replace = true;
                index += 1;
            }
            "--" => {
                if id.is_some() || index + 2 != args.len() {
                    bail!("profile export requires exactly one id");
                }
                id = Some(args[index + 1].clone());
                break;
            }
            value if value.starts_with("--") => {
                bail!("unknown profile export option '{value}'")
            }
            value => {
                if id.is_some() {
                    bail!("profile export accepts only one id");
                }
                id = Some(value.to_string());
                index += 1;
            }
        }
    }
    Ok((
        id.context("profile export requires an id")?,
        output_path,
        replace,
    ))
}

fn parse_profile_delete_options(args: &[String]) -> Result<(String, bool)> {
    let mut dry_run = false;
    let mut ids = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            "--" => {
                ids.extend(args[index + 1..].iter().cloned());
                break;
            }
            value if value.starts_with("--") => {
                bail!("unknown profile delete option '{value}'")
            }
            value => {
                ids.push(value.to_string());
                index += 1;
            }
        }
    }
    if ids.len() != 1 {
        bail!("profile delete requires an id");
    }
    Ok((ids.remove(0), dry_run))
}

fn parse_cleanup_options(args: &[String]) -> Result<(Option<String>, bool)> {
    let mut id = None;
    let mut dry_run = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = Some(value_after(args, index, "--id")?.to_string());
                index += 2;
            }
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            flag => bail!("unknown workspace cleanup option '{flag}'"),
        }
    }
    Ok((id, dry_run))
}

struct LaunchOptions {
    id: String,
    spec: LaunchSpec,
    wait_window: bool,
    window_timeout_ms: Option<u64>,
    screenshot_window: bool,
    dry_run: bool,
}

fn parse_launch_options(args: &[String]) -> Result<LaunchOptions> {
    let mut id = workspace::default_workspace_id();
    let mut name = None;
    let mut profile_id = None;
    let mut cwd = None;
    let mut cwd_explicit = false;
    let mut user_acknowledged_unenforced_policy = false;
    let mut env = Vec::new();
    let mut wait_window = false;
    let mut window_timeout_ms = None;
    let mut screenshot_window = false;
    let mut dry_run = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--profile" => {
                profile_id = Some(value_after(args, index, "--profile")?.to_string());
                index += 2;
            }
            "--name" => {
                name = Some(value_after(args, index, "--name")?.to_string());
                index += 2;
            }
            "--cwd" => {
                cwd = Some(PathBuf::from(value_after(args, index, "--cwd")?));
                cwd_explicit = true;
                index += 2;
            }
            "--env" => {
                env.push(parse_env_assignment(value_after(args, index, "--env")?)?);
                index += 2;
            }
            "--ack-unenforced-policy" => {
                user_acknowledged_unenforced_policy = true;
                index += 1;
            }
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            "--wait-window" => {
                wait_window = true;
                index += 1;
            }
            "--window-timeout-ms" => {
                wait_window = true;
                window_timeout_ms = Some(
                    value_after(args, index, "--window-timeout-ms")?
                        .parse()
                        .context("--window-timeout-ms must be a non-negative integer")?,
                );
                index += 2;
            }
            "--screenshot-window" => {
                wait_window = true;
                screenshot_window = true;
                index += 1;
            }
            "--" => {
                let command = args[index + 1..].to_vec();
                if command.is_empty() {
                    bail!("workspace launch requires a command after --");
                }
                let mut spec = LaunchSpec {
                    command,
                    name,
                    profile_id: None,
                    applied_policy: None,
                    user_acknowledged_unenforced_policy,
                    cwd,
                    env,
                };
                if let Some(profile_id) = &profile_id {
                    profile::apply_profile_to_launch_spec(profile_id, &mut spec, cwd_explicit)?;
                }
                return Ok(LaunchOptions {
                    id,
                    spec,
                    wait_window,
                    window_timeout_ms,
                    screenshot_window,
                    dry_run,
                });
            }
            _ => {
                let command = args[index..].to_vec();
                if command.is_empty() {
                    bail!("workspace launch requires a command");
                }
                let mut spec = LaunchSpec {
                    command,
                    name,
                    profile_id: None,
                    applied_policy: None,
                    user_acknowledged_unenforced_policy,
                    cwd,
                    env,
                };
                if let Some(profile_id) = &profile_id {
                    profile::apply_profile_to_launch_spec(profile_id, &mut spec, cwd_explicit)?;
                }
                return Ok(LaunchOptions {
                    id,
                    spec,
                    wait_window,
                    window_timeout_ms,
                    screenshot_window,
                    dry_run,
                });
            }
        }
    }
    bail!("workspace launch requires a command")
}

struct RunOptions {
    id: String,
    spec: LaunchSpec,
    timeout_ms: Option<u64>,
    tail_bytes: Option<u64>,
    kill_on_timeout: bool,
    dry_run: bool,
}

fn parse_run_options(args: &[String]) -> Result<RunOptions> {
    let mut id = workspace::default_workspace_id();
    let mut name = None;
    let mut profile_id = None;
    let mut cwd = None;
    let mut cwd_explicit = false;
    let mut user_acknowledged_unenforced_policy = false;
    let mut timeout_ms = None;
    let mut tail_bytes = None;
    let mut kill_on_timeout = false;
    let mut dry_run = false;
    let mut env = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--profile" => {
                profile_id = Some(value_after(args, index, "--profile")?.to_string());
                index += 2;
            }
            "--name" => {
                name = Some(value_after(args, index, "--name")?.to_string());
                index += 2;
            }
            "--cwd" => {
                cwd = Some(PathBuf::from(value_after(args, index, "--cwd")?));
                cwd_explicit = true;
                index += 2;
            }
            "--env" => {
                env.push(parse_env_assignment(value_after(args, index, "--env")?)?);
                index += 2;
            }
            "--ack-unenforced-policy" => {
                user_acknowledged_unenforced_policy = true;
                index += 1;
            }
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            "--timeout-ms" => {
                timeout_ms = Some(
                    value_after(args, index, "--timeout-ms")?
                        .parse()
                        .context("--timeout-ms must be a non-negative integer")?,
                );
                index += 2;
            }
            "--tail-bytes" => {
                tail_bytes = Some(
                    value_after(args, index, "--tail-bytes")?
                        .parse()
                        .context("--tail-bytes must be a non-negative integer")?,
                );
                index += 2;
            }
            "--kill-on-timeout" => {
                kill_on_timeout = true;
                index += 1;
            }
            "--" => {
                let command = args[index + 1..].to_vec();
                if command.is_empty() {
                    bail!("workspace run requires a command after --");
                }
                let mut spec = LaunchSpec {
                    command,
                    name,
                    profile_id: None,
                    applied_policy: None,
                    user_acknowledged_unenforced_policy,
                    cwd,
                    env,
                };
                if let Some(profile_id) = &profile_id {
                    profile::apply_profile_to_launch_spec(profile_id, &mut spec, cwd_explicit)?;
                }
                return Ok(RunOptions {
                    id,
                    spec,
                    timeout_ms,
                    tail_bytes,
                    kill_on_timeout,
                    dry_run,
                });
            }
            _ => {
                let command = args[index..].to_vec();
                if command.is_empty() {
                    bail!("workspace run requires a command");
                }
                let mut spec = LaunchSpec {
                    command,
                    name,
                    profile_id: None,
                    applied_policy: None,
                    user_acknowledged_unenforced_policy,
                    cwd,
                    env,
                };
                if let Some(profile_id) = &profile_id {
                    profile::apply_profile_to_launch_spec(profile_id, &mut spec, cwd_explicit)?;
                }
                return Ok(RunOptions {
                    id,
                    spec,
                    timeout_ms,
                    tail_bytes,
                    kill_on_timeout,
                    dry_run,
                });
            }
        }
    }
    bail!("workspace run requires a command")
}

fn parse_env_assignment(value: &str) -> Result<EnvVar> {
    let Some((name, value)) = value.split_once('=') else {
        bail!("--env requires NAME=VALUE");
    };
    if name.is_empty() {
        bail!("--env requires a non-empty variable name");
    }
    Ok(EnvVar {
        name: name.to_string(),
        value: value.to_string(),
    })
}

fn parse_screenshot_options(args: &[String]) -> Result<(String, Option<PathBuf>)> {
    let mut id = workspace::default_workspace_id();
    let mut output_path = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--output" => {
                output_path = Some(PathBuf::from(value_after(args, index, "--output")?));
                index += 2;
            }
            flag => bail!("unknown workspace screenshot option '{flag}'"),
        }
    }
    Ok((id, output_path))
}

type ObserveOptions = (
    String,
    bool,
    bool,
    Option<PathBuf>,
    bool,
    Option<usize>,
    Option<u64>,
);

fn parse_observe_options(args: &[String]) -> Result<ObserveOptions> {
    let mut id = workspace::default_workspace_id();
    let mut screenshot = false;
    let mut include_hidden = false;
    let mut output_path = None;
    let mut include_events = false;
    let mut events_tail = None;
    let mut events_since_sequence = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--screenshot" => {
                screenshot = true;
                index += 1;
            }
            "--all-windows" | "--include-hidden" => {
                include_hidden = true;
                index += 1;
            }
            "--output" => {
                output_path = Some(PathBuf::from(value_after(args, index, "--output")?));
                index += 2;
            }
            "--events" => {
                include_events = true;
                index += 1;
            }
            "--events-tail" => {
                include_events = true;
                events_tail = Some(
                    value_after(args, index, "--events-tail")?
                        .parse()
                        .context("--events-tail must be a non-negative integer")?,
                );
                index += 2;
            }
            "--events-since" => {
                include_events = true;
                events_since_sequence = Some(
                    value_after(args, index, "--events-since")?
                        .parse()
                        .context("--events-since must be a non-negative integer")?,
                );
                index += 2;
            }
            flag => bail!("unknown workspace observe option '{flag}'"),
        }
    }
    if output_path.is_some() && !screenshot {
        bail!("workspace observe --output requires --screenshot");
    }
    Ok((
        id,
        screenshot,
        include_hidden,
        output_path,
        include_events,
        events_tail,
        events_since_sequence,
    ))
}

type ScreenshotWindowOptions = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<u32>,
    Option<String>,
    Option<PathBuf>,
    Option<u64>,
);

fn parse_screenshot_window_options(args: &[String]) -> Result<ScreenshotWindowOptions> {
    let mut id = workspace::default_workspace_id();
    let mut window_id = None;
    let mut title_contains = None;
    let mut class_contains = None;
    let mut pid = None;
    let mut app_id = None;
    let mut output_path = None;
    let mut timeout_ms = None;
    let mut positional = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--window" => {
                window_id = Some(value_after(args, index, "--window")?.to_string());
                index += 2;
            }
            "--title" => {
                title_contains = Some(value_after(args, index, "--title")?.to_string());
                index += 2;
            }
            "--class" => {
                class_contains = Some(value_after(args, index, "--class")?.to_string());
                index += 2;
            }
            "--pid" => {
                pid = Some(
                    value_after(args, index, "--pid")?
                        .parse()
                        .context("--pid must be a positive integer")?,
                );
                index += 2;
            }
            "--app" => {
                app_id = Some(value_after(args, index, "--app")?.to_string());
                index += 2;
            }
            "--output" => {
                output_path = Some(PathBuf::from(value_after(args, index, "--output")?));
                index += 2;
            }
            "--timeout-ms" => {
                timeout_ms = Some(
                    value_after(args, index, "--timeout-ms")?
                        .parse()
                        .context("--timeout-ms must be a non-negative integer")?,
                );
                index += 2;
            }
            value if value.starts_with("--") => {
                bail!("unknown workspace screenshot-window option '{value}'")
            }
            value => {
                positional.push(value.to_string());
                index += 1;
            }
        }
    }

    if positional.len() > 1 {
        bail!("workspace screenshot-window accepts at most one positional window id");
    }
    if let Some(positional_window_id) = positional.into_iter().next() {
        if window_id.is_some() {
            bail!("workspace screenshot-window accepts only one window id");
        }
        window_id = Some(positional_window_id);
    }
    let has_match_filter =
        title_contains.is_some() || class_contains.is_some() || pid.is_some() || app_id.is_some();
    if window_id.is_some() && has_match_filter {
        bail!("workspace screenshot-window accepts either a window id or match filters, not both");
    }
    if window_id.is_none() && !has_match_filter {
        bail!(
            "workspace screenshot-window requires a window id or --title, --class, --pid, or --app"
        );
    }
    if window_id.is_some() && timeout_ms.is_some() {
        bail!("workspace screenshot-window accepts --timeout-ms only with match filters");
    }

    Ok((
        id,
        window_id,
        title_contains,
        class_contains,
        pid,
        app_id,
        output_path,
        timeout_ms,
    ))
}

#[allow(clippy::type_complexity)]
fn parse_wait_window_options(
    args: &[String],
) -> Result<(
    String,
    Option<String>,
    Option<String>,
    Option<u32>,
    Option<String>,
    Option<u64>,
)> {
    let mut id = workspace::default_workspace_id();
    let mut title_contains = None;
    let mut class_contains = None;
    let mut pid = None;
    let mut app_id = None;
    let mut timeout_ms = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--title" => {
                title_contains = Some(value_after(args, index, "--title")?.to_string());
                index += 2;
            }
            "--class" => {
                class_contains = Some(value_after(args, index, "--class")?.to_string());
                index += 2;
            }
            "--pid" => {
                pid = Some(
                    value_after(args, index, "--pid")?
                        .parse()
                        .context("--pid must be a positive integer")?,
                );
                index += 2;
            }
            "--app" => {
                app_id = Some(value_after(args, index, "--app")?.to_string());
                index += 2;
            }
            "--timeout-ms" => {
                timeout_ms = Some(
                    value_after(args, index, "--timeout-ms")?
                        .parse()
                        .context("--timeout-ms must be a non-negative integer")?,
                );
                index += 2;
            }
            flag => bail!("unknown workspace wait-window option '{flag}'"),
        }
    }
    Ok((id, title_contains, class_contains, pid, app_id, timeout_ms))
}

enum FocusWindowTarget {
    WindowId(String),
    Match {
        title_contains: Option<String>,
        class_contains: Option<String>,
        pid: Option<u32>,
        app_id: Option<String>,
        timeout_ms: Option<u64>,
    },
}

fn parse_focus_window_options(args: &[String]) -> Result<(String, FocusWindowTarget)> {
    let mut id = workspace::default_workspace_id();
    let mut window_id = None;
    let mut title_contains = None;
    let mut class_contains = None;
    let mut pid = None;
    let mut app_id = None;
    let mut timeout_ms = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--title" => {
                title_contains = Some(value_after(args, index, "--title")?.to_string());
                index += 2;
            }
            "--class" => {
                class_contains = Some(value_after(args, index, "--class")?.to_string());
                index += 2;
            }
            "--pid" => {
                pid = Some(
                    value_after(args, index, "--pid")?
                        .parse()
                        .context("--pid must be a positive integer")?,
                );
                index += 2;
            }
            "--app" => {
                app_id = Some(value_after(args, index, "--app")?.to_string());
                index += 2;
            }
            "--timeout-ms" => {
                timeout_ms = Some(
                    value_after(args, index, "--timeout-ms")?
                        .parse()
                        .context("--timeout-ms must be a non-negative integer")?,
                );
                index += 2;
            }
            value if value.starts_with("--") => {
                bail!("unknown workspace focus-window option '{value}'")
            }
            value => {
                if window_id.is_some() {
                    bail!("workspace focus-window accepts only one window id");
                }
                window_id = Some(value.to_string());
                index += 1;
            }
        }
    }

    let has_match_filter =
        title_contains.is_some() || class_contains.is_some() || pid.is_some() || app_id.is_some();
    if let Some(window_id) = window_id {
        if has_match_filter || timeout_ms.is_some() {
            bail!("workspace focus-window accepts either a window id or match options, not both");
        }
        return Ok((id, FocusWindowTarget::WindowId(window_id)));
    }
    if !has_match_filter {
        bail!("workspace focus-window requires a window id or --title, --class, --pid, or --app");
    }
    Ok((
        id,
        FocusWindowTarget::Match {
            title_contains,
            class_contains,
            pid,
            app_id,
            timeout_ms,
        },
    ))
}

enum CloseWindowTarget {
    WindowId(String),
    Match {
        title_contains: Option<String>,
        class_contains: Option<String>,
        pid: Option<u32>,
        app_id: Option<String>,
        timeout_ms: Option<u64>,
    },
}

fn parse_close_window_options(args: &[String]) -> Result<(String, CloseWindowTarget, bool)> {
    let mut id = workspace::default_workspace_id();
    let mut window_id = None;
    let mut title_contains = None;
    let mut class_contains = None;
    let mut pid = None;
    let mut app_id = None;
    let mut timeout_ms = None;
    let mut dry_run = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--title" => {
                title_contains = Some(value_after(args, index, "--title")?.to_string());
                index += 2;
            }
            "--class" => {
                class_contains = Some(value_after(args, index, "--class")?.to_string());
                index += 2;
            }
            "--pid" => {
                pid = Some(
                    value_after(args, index, "--pid")?
                        .parse()
                        .context("--pid must be a positive integer")?,
                );
                index += 2;
            }
            "--app" => {
                app_id = Some(value_after(args, index, "--app")?.to_string());
                index += 2;
            }
            "--timeout-ms" => {
                timeout_ms = Some(
                    value_after(args, index, "--timeout-ms")?
                        .parse()
                        .context("--timeout-ms must be a non-negative integer")?,
                );
                index += 2;
            }
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            value if value.starts_with("--") => {
                bail!("unknown workspace close-window option '{value}'")
            }
            value => {
                if window_id.is_some() {
                    bail!("workspace close-window accepts only one window id");
                }
                window_id = Some(value.to_string());
                index += 1;
            }
        }
    }

    let has_match_filter =
        title_contains.is_some() || class_contains.is_some() || pid.is_some() || app_id.is_some();
    if let Some(window_id) = window_id {
        if has_match_filter || timeout_ms.is_some() {
            bail!("workspace close-window accepts either a window id or match options, not both");
        }
        return Ok((id, CloseWindowTarget::WindowId(window_id), dry_run));
    }
    if !has_match_filter {
        bail!("workspace close-window requires a window id or --title, --class, --pid, or --app");
    }
    Ok((
        id,
        CloseWindowTarget::Match {
            title_contains,
            class_contains,
            pid,
            app_id,
            timeout_ms,
        },
        dry_run,
    ))
}

type MoveWindowOptions = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<u32>,
    Option<String>,
    i32,
    i32,
    Option<u64>,
);

fn parse_move_window_options(args: &[String]) -> Result<MoveWindowOptions> {
    let (id, title_contains, class_contains, pid, app_id, timeout_ms, values) =
        parse_window_target_values(args, "workspace move-window")?;
    let has_match_filter =
        title_contains.is_some() || class_contains.is_some() || pid.is_some() || app_id.is_some();
    let (window_id, x_value, y_value) = if has_match_filter {
        if values.len() != 2 {
            bail!("workspace move-window with match filters requires X and Y coordinates");
        }
        (None, &values[0], &values[1])
    } else {
        if values.len() != 3 {
            bail!("workspace move-window requires WINDOW_ID X Y or match filters with X Y");
        }
        if timeout_ms.is_some() {
            bail!("workspace move-window accepts --timeout-ms only with match filters");
        }
        (Some(values[0].clone()), &values[1], &values[2])
    };
    let x = x_value
        .parse()
        .context("move-window X must be an integer")?;
    let y = y_value
        .parse()
        .context("move-window Y must be an integer")?;
    Ok((
        id,
        window_id,
        title_contains,
        class_contains,
        pid,
        app_id,
        x,
        y,
        timeout_ms,
    ))
}

type ResizeWindowOptions = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<u32>,
    Option<String>,
    u32,
    u32,
    Option<u64>,
);

fn parse_resize_window_options(args: &[String]) -> Result<ResizeWindowOptions> {
    let (id, title_contains, class_contains, pid, app_id, timeout_ms, values) =
        parse_window_target_values(args, "workspace resize-window")?;
    let has_match_filter =
        title_contains.is_some() || class_contains.is_some() || pid.is_some() || app_id.is_some();
    let (window_id, width_value, height_value) = if has_match_filter {
        if values.len() != 2 {
            bail!("workspace resize-window with match filters requires WIDTH and HEIGHT");
        }
        (None, &values[0], &values[1])
    } else {
        if values.len() != 3 {
            bail!("workspace resize-window requires WINDOW_ID WIDTH HEIGHT or match filters with WIDTH HEIGHT");
        }
        if timeout_ms.is_some() {
            bail!("workspace resize-window accepts --timeout-ms only with match filters");
        }
        (Some(values[0].clone()), &values[1], &values[2])
    };
    let width = width_value
        .parse()
        .context("resize-window WIDTH must be a positive integer")?;
    let height = height_value
        .parse()
        .context("resize-window HEIGHT must be a positive integer")?;
    Ok((
        id,
        window_id,
        title_contains,
        class_contains,
        pid,
        app_id,
        width,
        height,
        timeout_ms,
    ))
}

type TargetedWindowActionOptions = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<u32>,
    Option<String>,
    Option<u64>,
);

fn parse_targeted_window_action_options(
    args: &[String],
    command_name: &str,
) -> Result<TargetedWindowActionOptions> {
    let (id, title_contains, class_contains, pid, app_id, timeout_ms, values) =
        parse_window_target_values(args, command_name)?;
    let has_match_filter =
        title_contains.is_some() || class_contains.is_some() || pid.is_some() || app_id.is_some();
    let window_id = if has_match_filter {
        if !values.is_empty() {
            bail!("{command_name} with match filters does not accept a window id");
        }
        None
    } else {
        if values.len() != 1 {
            bail!("{command_name} requires WINDOW_ID or match filters");
        }
        if timeout_ms.is_some() {
            bail!("{command_name} accepts --timeout-ms only with match filters");
        }
        Some(values[0].clone())
    };
    Ok((
        id,
        window_id,
        title_contains,
        class_contains,
        pid,
        app_id,
        timeout_ms,
    ))
}

#[allow(clippy::type_complexity)]
fn parse_click_options(args: &[String]) -> Result<(String, i32, i32, Option<u8>, Option<u8>)> {
    let mut id = workspace::default_workspace_id();
    let mut button = None;
    let mut count = None;
    let mut values = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--button" => {
                button = Some(
                    value_after(args, index, "--button")?
                        .parse()
                        .context("--button must be an integer between 1 and 5")?,
                );
                index += 2;
            }
            "--count" => {
                count = Some(
                    value_after(args, index, "--count")?
                        .parse()
                        .context("--count must be an integer between 1 and 20")?,
                );
                index += 2;
            }
            value if value.starts_with("--") => bail!("unknown workspace click option '{value}'"),
            value => {
                values.push(value.to_string());
                index += 1;
            }
        }
    }
    if values.len() != 2 {
        bail!("workspace click requires X and Y coordinates");
    }
    let x = values[0].parse().context("click X must be an integer")?;
    let y = values[1].parse().context("click Y must be an integer")?;
    Ok((id, x, y, button, count))
}

type ClickWindowOptions = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<u32>,
    Option<String>,
    i32,
    i32,
    Option<u8>,
    Option<u8>,
    Option<u64>,
);

fn parse_click_window_options(args: &[String]) -> Result<ClickWindowOptions> {
    let mut id = workspace::default_workspace_id();
    let mut title_contains = None;
    let mut class_contains = None;
    let mut pid = None;
    let mut app_id = None;
    let mut timeout_ms = None;
    let mut button = None;
    let mut count = None;
    let mut values = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--title" => {
                title_contains = Some(value_after(args, index, "--title")?.to_string());
                index += 2;
            }
            "--class" => {
                class_contains = Some(value_after(args, index, "--class")?.to_string());
                index += 2;
            }
            "--pid" => {
                pid = Some(
                    value_after(args, index, "--pid")?
                        .parse()
                        .context("--pid must be a positive integer")?,
                );
                index += 2;
            }
            "--app" => {
                app_id = Some(value_after(args, index, "--app")?.to_string());
                index += 2;
            }
            "--timeout-ms" => {
                timeout_ms = Some(
                    value_after(args, index, "--timeout-ms")?
                        .parse()
                        .context("--timeout-ms must be a non-negative integer")?,
                );
                index += 2;
            }
            "--button" => {
                button = Some(
                    value_after(args, index, "--button")?
                        .parse()
                        .context("--button must be an integer between 1 and 5")?,
                );
                index += 2;
            }
            "--count" => {
                count = Some(
                    value_after(args, index, "--count")?
                        .parse()
                        .context("--count must be an integer between 1 and 20")?,
                );
                index += 2;
            }
            value if value.starts_with("--") => {
                bail!("unknown workspace click-window option '{value}'")
            }
            value => {
                values.push(value.to_string());
                index += 1;
            }
        }
    }

    let has_match_filter =
        title_contains.is_some() || class_contains.is_some() || pid.is_some() || app_id.is_some();
    let (window_id, x_value, y_value) = if has_match_filter {
        if values.len() != 2 {
            bail!("workspace click-window with match filters requires X and Y coordinates");
        }
        (None, &values[0], &values[1])
    } else {
        if values.len() != 3 {
            bail!("workspace click-window requires WINDOW_ID X Y or match filters with X Y");
        }
        if timeout_ms.is_some() {
            bail!("workspace click-window accepts --timeout-ms only with match filters");
        }
        (Some(values[0].clone()), &values[1], &values[2])
    };
    let x = x_value
        .parse()
        .context("click-window X must be an integer")?;
    let y = y_value
        .parse()
        .context("click-window Y must be an integer")?;
    Ok((
        id,
        window_id,
        title_contains,
        class_contains,
        pid,
        app_id,
        x,
        y,
        button,
        count,
        timeout_ms,
    ))
}

fn parse_move_pointer_options(args: &[String]) -> Result<(String, i32, i32)> {
    let mut id = workspace::default_workspace_id();
    let mut values = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            value if value.starts_with("--") => {
                bail!("unknown workspace move-pointer option '{value}'")
            }
            value => {
                values.push(value.to_string());
                index += 1;
            }
        }
    }
    if values.len() != 2 {
        bail!("workspace move-pointer requires X and Y coordinates");
    }
    let x = values[0]
        .parse()
        .context("move-pointer X must be an integer")?;
    let y = values[1]
        .parse()
        .context("move-pointer Y must be an integer")?;
    Ok((id, x, y))
}

type MovePointerWindowOptions = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<u32>,
    Option<String>,
    i32,
    i32,
    Option<u64>,
);

fn parse_move_pointer_window_options(args: &[String]) -> Result<MovePointerWindowOptions> {
    let mut id = workspace::default_workspace_id();
    let mut title_contains = None;
    let mut class_contains = None;
    let mut pid = None;
    let mut app_id = None;
    let mut timeout_ms = None;
    let mut values = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--title" => {
                title_contains = Some(value_after(args, index, "--title")?.to_string());
                index += 2;
            }
            "--class" => {
                class_contains = Some(value_after(args, index, "--class")?.to_string());
                index += 2;
            }
            "--pid" => {
                pid = Some(
                    value_after(args, index, "--pid")?
                        .parse()
                        .context("--pid must be a positive integer")?,
                );
                index += 2;
            }
            "--app" => {
                app_id = Some(value_after(args, index, "--app")?.to_string());
                index += 2;
            }
            "--timeout-ms" => {
                timeout_ms = Some(
                    value_after(args, index, "--timeout-ms")?
                        .parse()
                        .context("--timeout-ms must be a non-negative integer")?,
                );
                index += 2;
            }
            value if value.starts_with("--") => {
                bail!("unknown workspace move-pointer-window option '{value}'")
            }
            value => {
                values.push(value.to_string());
                index += 1;
            }
        }
    }

    let has_match_filter =
        title_contains.is_some() || class_contains.is_some() || pid.is_some() || app_id.is_some();
    let (window_id, x_value, y_value) = if has_match_filter {
        if values.len() != 2 {
            bail!("workspace move-pointer-window with match filters requires X and Y coordinates");
        }
        (None, &values[0], &values[1])
    } else {
        if values.len() != 3 {
            bail!("workspace move-pointer-window requires WINDOW_ID X Y or match filters with X Y");
        }
        if timeout_ms.is_some() {
            bail!("workspace move-pointer-window accepts --timeout-ms only with match filters");
        }
        (Some(values[0].clone()), &values[1], &values[2])
    };
    let x = x_value
        .parse()
        .context("move-pointer-window X must be an integer")?;
    let y = y_value
        .parse()
        .context("move-pointer-window Y must be an integer")?;
    Ok((
        id,
        window_id,
        title_contains,
        class_contains,
        pid,
        app_id,
        x,
        y,
        timeout_ms,
    ))
}

fn parse_drag_options(args: &[String]) -> Result<(String, i32, i32, i32, i32, Option<u8>)> {
    let mut id = workspace::default_workspace_id();
    let mut button = None;
    let mut values = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--button" => {
                button = Some(
                    value_after(args, index, "--button")?
                        .parse()
                        .context("--button must be an integer between 1 and 5")?,
                );
                index += 2;
            }
            value if value.starts_with("--") => bail!("unknown workspace drag option '{value}'"),
            value => {
                values.push(value.to_string());
                index += 1;
            }
        }
    }
    if values.len() != 4 {
        bail!("workspace drag requires FROM_X FROM_Y TO_X TO_Y coordinates");
    }
    let from_x = values[0]
        .parse()
        .context("drag FROM_X must be an integer")?;
    let from_y = values[1]
        .parse()
        .context("drag FROM_Y must be an integer")?;
    let to_x = values[2].parse().context("drag TO_X must be an integer")?;
    let to_y = values[3].parse().context("drag TO_Y must be an integer")?;
    Ok((id, from_x, from_y, to_x, to_y, button))
}

type DragWindowOptions = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<u32>,
    Option<String>,
    i32,
    i32,
    i32,
    i32,
    Option<u8>,
    Option<u64>,
);

fn parse_drag_window_options(args: &[String]) -> Result<DragWindowOptions> {
    let mut id = workspace::default_workspace_id();
    let mut title_contains = None;
    let mut class_contains = None;
    let mut pid = None;
    let mut app_id = None;
    let mut timeout_ms = None;
    let mut button = None;
    let mut values = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--title" => {
                title_contains = Some(value_after(args, index, "--title")?.to_string());
                index += 2;
            }
            "--class" => {
                class_contains = Some(value_after(args, index, "--class")?.to_string());
                index += 2;
            }
            "--pid" => {
                pid = Some(
                    value_after(args, index, "--pid")?
                        .parse()
                        .context("--pid must be a positive integer")?,
                );
                index += 2;
            }
            "--app" => {
                app_id = Some(value_after(args, index, "--app")?.to_string());
                index += 2;
            }
            "--timeout-ms" => {
                timeout_ms = Some(
                    value_after(args, index, "--timeout-ms")?
                        .parse()
                        .context("--timeout-ms must be a non-negative integer")?,
                );
                index += 2;
            }
            "--button" => {
                button = Some(
                    value_after(args, index, "--button")?
                        .parse()
                        .context("--button must be an integer between 1 and 5")?,
                );
                index += 2;
            }
            value if value.starts_with("--") => {
                bail!("unknown workspace drag-window option '{value}'")
            }
            value => {
                values.push(value.to_string());
                index += 1;
            }
        }
    }

    let has_match_filter =
        title_contains.is_some() || class_contains.is_some() || pid.is_some() || app_id.is_some();
    let (window_id, coordinate_values) = if has_match_filter {
        if values.len() != 4 {
            bail!(
                "workspace drag-window with match filters requires FROM_X FROM_Y TO_X TO_Y coordinates"
            );
        }
        (None, values.as_slice())
    } else {
        if values.len() != 5 {
            bail!("workspace drag-window requires WINDOW_ID FROM_X FROM_Y TO_X TO_Y or match filters with FROM_X FROM_Y TO_X TO_Y");
        }
        if timeout_ms.is_some() {
            bail!("workspace drag-window accepts --timeout-ms only with match filters");
        }
        (Some(values[0].clone()), &values[1..])
    };
    let from_x = coordinate_values[0]
        .parse()
        .context("drag-window FROM_X must be an integer")?;
    let from_y = coordinate_values[1]
        .parse()
        .context("drag-window FROM_Y must be an integer")?;
    let to_x = coordinate_values[2]
        .parse()
        .context("drag-window TO_X must be an integer")?;
    let to_y = coordinate_values[3]
        .parse()
        .context("drag-window TO_Y must be an integer")?;
    Ok((
        id,
        window_id,
        title_contains,
        class_contains,
        pid,
        app_id,
        from_x,
        from_y,
        to_x,
        to_y,
        button,
        timeout_ms,
    ))
}

fn parse_scroll_options(
    args: &[String],
) -> Result<(String, i32, i32, workspace::ScrollDirection, Option<u8>)> {
    let mut id = workspace::default_workspace_id();
    let mut amount = None;
    let mut values = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--amount" => {
                amount = Some(
                    value_after(args, index, "--amount")?
                        .parse()
                        .context("--amount must be an integer between 1 and 100")?,
                );
                index += 2;
            }
            value if value.starts_with("--") => bail!("unknown workspace scroll option '{value}'"),
            value => {
                values.push(value.to_string());
                index += 1;
            }
        }
    }
    if values.len() != 3 {
        bail!("workspace scroll requires X Y DIRECTION");
    }
    let x = values[0].parse().context("scroll X must be an integer")?;
    let y = values[1].parse().context("scroll Y must be an integer")?;
    let direction = values[2]
        .parse()
        .context("scroll DIRECTION must be up, down, left, or right")?;
    Ok((id, x, y, direction, amount))
}

type ScrollWindowOptions = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<u32>,
    Option<String>,
    i32,
    i32,
    workspace::ScrollDirection,
    Option<u8>,
    Option<u64>,
);

fn parse_scroll_window_options(args: &[String]) -> Result<ScrollWindowOptions> {
    let mut id = workspace::default_workspace_id();
    let mut title_contains = None;
    let mut class_contains = None;
    let mut pid = None;
    let mut app_id = None;
    let mut timeout_ms = None;
    let mut amount = None;
    let mut values = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--title" => {
                title_contains = Some(value_after(args, index, "--title")?.to_string());
                index += 2;
            }
            "--class" => {
                class_contains = Some(value_after(args, index, "--class")?.to_string());
                index += 2;
            }
            "--pid" => {
                pid = Some(
                    value_after(args, index, "--pid")?
                        .parse()
                        .context("--pid must be a positive integer")?,
                );
                index += 2;
            }
            "--app" => {
                app_id = Some(value_after(args, index, "--app")?.to_string());
                index += 2;
            }
            "--timeout-ms" => {
                timeout_ms = Some(
                    value_after(args, index, "--timeout-ms")?
                        .parse()
                        .context("--timeout-ms must be a non-negative integer")?,
                );
                index += 2;
            }
            "--amount" => {
                amount = Some(
                    value_after(args, index, "--amount")?
                        .parse()
                        .context("--amount must be an integer between 1 and 100")?,
                );
                index += 2;
            }
            value if value.starts_with("--") => {
                bail!("unknown workspace scroll-window option '{value}'")
            }
            value => {
                values.push(value.to_string());
                index += 1;
            }
        }
    }

    let has_match_filter =
        title_contains.is_some() || class_contains.is_some() || pid.is_some() || app_id.is_some();
    let (window_id, x_value, y_value, direction_value) = if has_match_filter {
        if values.len() != 3 {
            bail!("workspace scroll-window with match filters requires X Y DIRECTION");
        }
        (None, &values[0], &values[1], &values[2])
    } else {
        if values.len() != 4 {
            bail!("workspace scroll-window requires WINDOW_ID X Y DIRECTION or match filters with X Y DIRECTION");
        }
        if timeout_ms.is_some() {
            bail!("workspace scroll-window accepts --timeout-ms only with match filters");
        }
        (Some(values[0].clone()), &values[1], &values[2], &values[3])
    };
    let x = x_value
        .parse()
        .context("scroll-window X must be an integer")?;
    let y = y_value
        .parse()
        .context("scroll-window Y must be an integer")?;
    let direction = direction_value
        .parse()
        .context("scroll-window DIRECTION must be up, down, left, or right")?;
    Ok((
        id,
        window_id,
        title_contains,
        class_contains,
        pid,
        app_id,
        x,
        y,
        direction,
        amount,
        timeout_ms,
    ))
}

type WindowTargetValues = (
    String,
    Option<String>,
    Option<String>,
    Option<u32>,
    Option<String>,
    Option<u64>,
    Vec<String>,
);

fn parse_window_target_values(args: &[String], command_name: &str) -> Result<WindowTargetValues> {
    let mut id = workspace::default_workspace_id();
    let mut title_contains = None;
    let mut class_contains = None;
    let mut pid = None;
    let mut app_id = None;
    let mut timeout_ms = None;
    let mut values = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--title" => {
                title_contains = Some(value_after(args, index, "--title")?.to_string());
                index += 2;
            }
            "--class" => {
                class_contains = Some(value_after(args, index, "--class")?.to_string());
                index += 2;
            }
            "--pid" => {
                pid = Some(
                    value_after(args, index, "--pid")?
                        .parse()
                        .context("--pid must be a positive integer")?,
                );
                index += 2;
            }
            "--app" => {
                app_id = Some(value_after(args, index, "--app")?.to_string());
                index += 2;
            }
            "--timeout-ms" => {
                timeout_ms = Some(
                    value_after(args, index, "--timeout-ms")?
                        .parse()
                        .context("--timeout-ms must be a non-negative integer")?,
                );
                index += 2;
            }
            value if value.starts_with("--") => bail!("unknown {command_name} option '{value}'"),
            value => {
                values.push(value.to_string());
                index += 1;
            }
        }
    }
    Ok((
        id,
        title_contains,
        class_contains,
        pid,
        app_id,
        timeout_ms,
        values,
    ))
}

type KeyWindowOptions = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<u32>,
    Option<String>,
    String,
    Option<u64>,
);

fn parse_key_window_options(args: &[String]) -> Result<KeyWindowOptions> {
    let (id, title_contains, class_contains, pid, app_id, timeout_ms, values) =
        parse_window_target_values(args, "workspace key-window")?;
    let has_match_filter =
        title_contains.is_some() || class_contains.is_some() || pid.is_some() || app_id.is_some();
    let (window_id, key) = if has_match_filter {
        if values.len() != 1 {
            bail!("workspace key-window with match filters requires a key");
        }
        (None, values[0].clone())
    } else {
        if values.len() != 2 {
            bail!("workspace key-window requires WINDOW_ID KEY or match filters with KEY");
        }
        if timeout_ms.is_some() {
            bail!("workspace key-window accepts --timeout-ms only with match filters");
        }
        (Some(values[0].clone()), values[1].clone())
    };
    Ok((
        id,
        window_id,
        title_contains,
        class_contains,
        pid,
        app_id,
        key,
        timeout_ms,
    ))
}

type TypeWindowOptions = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<u32>,
    Option<String>,
    String,
    Option<u64>,
);

fn parse_type_window_options(args: &[String]) -> Result<TypeWindowOptions> {
    let (id, title_contains, class_contains, pid, app_id, timeout_ms, values) =
        parse_window_target_values(args, "workspace type-window")?;
    let has_match_filter =
        title_contains.is_some() || class_contains.is_some() || pid.is_some() || app_id.is_some();
    let (window_id, text_values) = if has_match_filter {
        if values.is_empty() {
            bail!("workspace type-window with match filters requires text");
        }
        (None, values)
    } else {
        if values.len() < 2 {
            bail!("workspace type-window requires WINDOW_ID TEXT or match filters with TEXT");
        }
        if timeout_ms.is_some() {
            bail!("workspace type-window accepts --timeout-ms only with match filters");
        }
        (Some(values[0].clone()), values[1..].to_vec())
    };
    Ok((
        id,
        window_id,
        title_contains,
        class_contains,
        pid,
        app_id,
        text_values.join(" "),
        timeout_ms,
    ))
}

fn parse_one_arg_command(args: &[String], missing_message: &str) -> Result<(String, String)> {
    let (id, values) = parse_id_and_args(args)?;
    if values.len() != 1 {
        bail!("{missing_message}");
    }
    Ok((id, values[0].clone()))
}

fn parse_text_command(args: &[String]) -> Result<(String, String)> {
    let (id, values) = parse_id_and_args(args)?;
    if values.is_empty() {
        bail!("workspace type requires text");
    }
    Ok((id, values.join(" ")))
}

fn parse_clipboard_set_options(args: &[String]) -> Result<(String, String)> {
    let (id, values) = parse_id_and_args(args)?;
    if values.is_empty() {
        bail!("workspace clipboard-set requires text");
    }
    Ok((id, values.join(" ")))
}

fn parse_paste_options(args: &[String]) -> Result<(String, String, Option<String>)> {
    let mut id = workspace::default_workspace_id();
    let mut key = None;
    let mut values = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--key" => {
                key = Some(value_after(args, index, "--key")?.to_string());
                index += 2;
            }
            value if value.starts_with("--") => bail!("unknown workspace paste option '{value}'"),
            value => {
                values.push(value.to_string());
                index += 1;
            }
        }
    }
    if values.is_empty() {
        bail!("workspace paste requires text");
    }
    Ok((id, values.join(" "), key))
}

type PasteWindowOptions = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<u32>,
    Option<String>,
    String,
    Option<String>,
    Option<u64>,
);

fn parse_paste_window_options(args: &[String]) -> Result<PasteWindowOptions> {
    let mut id = workspace::default_workspace_id();
    let mut title_contains = None;
    let mut class_contains = None;
    let mut pid = None;
    let mut app_id = None;
    let mut timeout_ms = None;
    let mut key = None;
    let mut values = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--title" => {
                title_contains = Some(value_after(args, index, "--title")?.to_string());
                index += 2;
            }
            "--class" => {
                class_contains = Some(value_after(args, index, "--class")?.to_string());
                index += 2;
            }
            "--pid" => {
                pid = Some(
                    value_after(args, index, "--pid")?
                        .parse()
                        .context("--pid must be a positive integer")?,
                );
                index += 2;
            }
            "--app" => {
                app_id = Some(value_after(args, index, "--app")?.to_string());
                index += 2;
            }
            "--timeout-ms" => {
                timeout_ms = Some(
                    value_after(args, index, "--timeout-ms")?
                        .parse()
                        .context("--timeout-ms must be a non-negative integer")?,
                );
                index += 2;
            }
            "--key" => {
                key = Some(value_after(args, index, "--key")?.to_string());
                index += 2;
            }
            value if value.starts_with("--") => {
                bail!("unknown workspace paste-window option '{value}'")
            }
            value => {
                values.push(value.to_string());
                index += 1;
            }
        }
    }

    let has_match_filter =
        title_contains.is_some() || class_contains.is_some() || pid.is_some() || app_id.is_some();
    let (window_id, text_values) = if has_match_filter {
        if values.is_empty() {
            bail!("workspace paste-window with match filters requires text");
        }
        (None, values)
    } else {
        if values.len() < 2 {
            bail!("workspace paste-window requires WINDOW_ID TEXT or match filters with TEXT");
        }
        if timeout_ms.is_some() {
            bail!("workspace paste-window accepts --timeout-ms only with match filters");
        }
        (Some(values[0].clone()), values[1..].to_vec())
    };
    Ok((
        id,
        window_id,
        title_contains,
        class_contains,
        pid,
        app_id,
        text_values.join(" "),
        key,
        timeout_ms,
    ))
}

fn parse_logs_options(args: &[String]) -> Result<(String, String, String, Option<u64>)> {
    let mut id = workspace::default_workspace_id();
    let mut stream = "stdout".to_string();
    let mut tail_bytes = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--stream" => {
                stream = value_after(args, index, "--stream")?.to_string();
                index += 2;
            }
            "--tail-bytes" => {
                tail_bytes = Some(
                    value_after(args, index, "--tail-bytes")?
                        .parse()
                        .context("--tail-bytes must be a positive integer")?,
                );
                index += 2;
            }
            "--" => {
                let app_id = args
                    .get(index + 1)
                    .context("workspace logs requires an app id")?
                    .to_string();
                return Ok((id, app_id, stream, tail_bytes));
            }
            _ => {
                let app_id = args[index].clone();
                return Ok((id, app_id, stream, tail_bytes));
            }
        }
    }
    bail!("workspace logs requires an app id")
}

fn parse_wait_app_options(args: &[String]) -> Result<(String, String, Option<u64>, bool)> {
    let mut id = workspace::default_workspace_id();
    let mut timeout_ms = None;
    let mut kill_on_timeout = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--timeout-ms" => {
                timeout_ms = Some(
                    value_after(args, index, "--timeout-ms")?
                        .parse()
                        .context("--timeout-ms must be a non-negative integer")?,
                );
                index += 2;
            }
            "--kill-on-timeout" => {
                kill_on_timeout = true;
                index += 1;
            }
            "--" => {
                let app_id = args
                    .get(index + 1)
                    .context("workspace wait-app requires an app id")?
                    .to_string();
                return Ok((id, app_id, timeout_ms, kill_on_timeout));
            }
            _ => {
                let app_id = args[index].clone();
                return Ok((id, app_id, timeout_ms, kill_on_timeout));
            }
        }
    }
    bail!("workspace wait-app requires an app id")
}

fn parse_kill_app_options(args: &[String]) -> Result<(String, String, bool)> {
    let mut id = workspace::default_workspace_id();
    let mut dry_run = false;
    let mut values = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            "--" => {
                values.extend(args[index + 1..].iter().cloned());
                break;
            }
            value if value.starts_with("--") => {
                bail!("unknown workspace kill-app option '{value}'")
            }
            value => {
                values.push(value.to_string());
                index += 1;
            }
        }
    }
    if values.len() != 1 {
        bail!("workspace kill-app requires an app id or pid");
    }
    Ok((id, values.remove(0), dry_run))
}

fn parse_events_options(args: &[String]) -> Result<(String, Option<usize>, Option<u64>)> {
    let mut id = workspace::default_workspace_id();
    let mut tail = None;
    let mut since_sequence = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--tail" => {
                tail = Some(
                    value_after(args, index, "--tail")?
                        .parse()
                        .context("--tail must be a non-negative integer")?,
                );
                index += 2;
            }
            "--since" => {
                since_sequence = Some(
                    value_after(args, index, "--since")?
                        .parse()
                        .context("--since must be a non-negative integer")?,
                );
                index += 2;
            }
            flag => bail!("unknown workspace events option '{flag}'"),
        }
    }
    Ok((id, tail, since_sequence))
}

fn parse_stop_options(args: &[String]) -> Result<(String, Option<u64>, bool)> {
    let mut id = workspace::default_workspace_id();
    let mut timeout_ms = None;
    let mut dry_run = false;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--timeout-ms" => {
                timeout_ms = Some(
                    value_after(args, index, "--timeout-ms")?
                        .parse()
                        .context("--timeout-ms must be a non-negative integer")?,
                );
                index += 2;
            }
            "--dry-run" => {
                dry_run = true;
                index += 1;
            }
            flag => bail!("unknown workspace stop option '{flag}'"),
        }
    }
    Ok((id, timeout_ms, dry_run))
}

fn parse_workspace_setup_options(
    args: &[String],
) -> Result<(String, String, profile::ProfileSetupOptions)> {
    let mut id = workspace::default_workspace_id();
    let mut profile_id = None;
    let mut options = profile::ProfileSetupOptions::default();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--profile" => {
                profile_id = Some(value_after(args, index, "--profile")?.to_string());
                index += 2;
            }
            "--wait" => {
                options.wait = true;
                index += 1;
            }
            "--dry-run" => {
                options.dry_run = true;
                index += 1;
            }
            "--ack-unenforced-policy" => {
                options.acknowledge_unenforced_policy = true;
                index += 1;
            }
            "--timeout-ms" => {
                options.wait = true;
                options.timeout_ms = Some(
                    value_after(args, index, "--timeout-ms")?
                        .parse()
                        .context("--timeout-ms must be a non-negative integer")?,
                );
                index += 2;
            }
            "--kill-on-timeout" => {
                options.wait = true;
                options.kill_on_timeout = true;
                index += 1;
            }
            flag => bail!("unknown workspace setup option '{flag}'"),
        }
    }
    Ok((
        id,
        profile_id.context("workspace setup requires --profile PROFILE")?,
        options,
    ))
}

fn parse_profile_launch_options(
    args: &[String],
) -> Result<(String, String, profile::ProfileStartupOptions)> {
    let mut id = workspace::default_workspace_id();
    let mut profile_id = None;
    let mut options = profile::ProfileStartupOptions::default();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--profile" => {
                profile_id = Some(value_after(args, index, "--profile")?.to_string());
                index += 2;
            }
            "--ack-unenforced-policy" => {
                options.acknowledge_unenforced_policy = true;
                index += 1;
            }
            "--dry-run" => {
                options.dry_run = true;
                index += 1;
            }
            "--wait-window" => {
                options.wait_window = true;
                index += 1;
            }
            "--window-timeout-ms" => {
                options.wait_window = true;
                options.window_timeout_ms = Some(
                    value_after(args, index, "--window-timeout-ms")?
                        .parse()
                        .context("--window-timeout-ms must be a non-negative integer")?,
                );
                index += 2;
            }
            "--screenshot-window" => {
                options.wait_window = true;
                options.screenshot_window = true;
                index += 1;
            }
            flag => bail!("unknown workspace launch-profile-apps option '{flag}'"),
        }
    }
    Ok((
        id,
        profile_id.context("workspace launch-profile-apps requires --profile PROFILE")?,
        options,
    ))
}

fn parse_id_and_args(args: &[String]) -> Result<(String, Vec<String>)> {
    let mut id = workspace::default_workspace_id();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = value_after(args, index, "--id")?.to_string();
                index += 2;
            }
            "--" => return Ok((id, args[index + 1..].to_vec())),
            _ => return Ok((id, args[index..].to_vec())),
        }
    }
    Ok((id, Vec::new()))
}

fn parse_daemon_options(args: Vec<String>) -> Result<DaemonOptions> {
    let mut id = None;
    let mut session_id = None;
    let mut purpose = None;
    let mut profile_id = None;
    let mut profile_cwd = None;
    let mut profile_env = Vec::new();
    let mut display = None;
    let mut width = None;
    let mut height = None;
    let mut runtime_dir = None;
    let mut socket_path = None;
    let mut xauthority_path = None;
    let mut policy_path = None;
    let mut permissions_source = None;
    let mut user_acknowledged_hidden_workspace = false;
    let mut user_acknowledged_unenforced_policy = false;
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--id" => {
                id = Some(value_after(&args, index, "--id")?.to_string());
                index += 2;
            }
            "--session-id" => {
                session_id = Some(value_after(&args, index, "--session-id")?.to_string());
                index += 2;
            }
            "--profile" => {
                profile_id = Some(value_after(&args, index, "--profile")?.to_string());
                index += 2;
            }
            "--profile-cwd" => {
                profile_cwd = Some(PathBuf::from(value_after(&args, index, "--profile-cwd")?));
                index += 2;
            }
            "--profile-env" => {
                profile_env.push(parse_env_assignment(value_after(
                    &args,
                    index,
                    "--profile-env",
                )?)?);
                index += 2;
            }
            "--purpose" => {
                purpose = Some(value_after(&args, index, "--purpose")?.to_string());
                index += 2;
            }
            "--display" => {
                display = Some(value_after(&args, index, "--display")?.to_string());
                index += 2;
            }
            "--width" => {
                width = Some(
                    value_after(&args, index, "--width")?
                        .parse()
                        .context("--width must be a positive integer")?,
                );
                index += 2;
            }
            "--height" => {
                height = Some(
                    value_after(&args, index, "--height")?
                        .parse()
                        .context("--height must be a positive integer")?,
                );
                index += 2;
            }
            "--runtime-dir" => {
                runtime_dir = Some(PathBuf::from(value_after(&args, index, "--runtime-dir")?));
                index += 2;
            }
            "--socket" => {
                socket_path = Some(PathBuf::from(value_after(&args, index, "--socket")?));
                index += 2;
            }
            "--xauthority" => {
                xauthority_path = Some(PathBuf::from(value_after(&args, index, "--xauthority")?));
                index += 2;
            }
            "--policy" => {
                policy_path = Some(PathBuf::from(value_after(&args, index, "--policy")?));
                index += 2;
            }
            "--permissions" => {
                permissions_source =
                    Some(PathBuf::from(value_after(&args, index, "--permissions")?));
                index += 2;
            }
            "--ack-hidden-workspace" => {
                user_acknowledged_hidden_workspace = true;
                index += 1;
            }
            "--ack-unenforced-policy" => {
                user_acknowledged_unenforced_policy = true;
                index += 1;
            }
            flag => bail!("unknown daemon option '{flag}'"),
        }
    }
    let applied_policy = policy_path.as_ref().map(read_applied_policy).transpose()?;

    let id = id.context("daemon missing --id")?;
    let session_id = session_id.unwrap_or_else(|| workspace::new_session_id(&id));

    Ok(DaemonOptions {
        id,
        session_id,
        purpose,
        profile_id,
        applied_policy,
        profile_cwd,
        profile_env,
        user_acknowledged_hidden_workspace,
        user_acknowledged_unenforced_policy,
        display: display.context("daemon missing --display")?,
        width: width.context("daemon missing --width")?,
        height: height.context("daemon missing --height")?,
        runtime_dir: runtime_dir.context("daemon missing --runtime-dir")?,
        socket_path: socket_path.context("daemon missing --socket")?,
        xauthority_path: xauthority_path.context("daemon missing --xauthority")?,
        permissions_source,
    })
}

fn read_applied_policy(path: &PathBuf) -> Result<AppliedWorkspacePolicy> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read applied policy {}", path.display()))?;
    serde_json::from_str(&content)
        .with_context(|| format!("failed to parse applied policy {}", path.display()))
}

fn value_after<'a>(args: &'a [String], index: usize, flag: &str) -> Result<&'a str> {
    args.get(index + 1)
        .map(String::as_str)
        .ok_or_else(|| anyhow::anyhow!("{flag} requires a value"))
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).context("failed to serialize JSON")?
    );
    Ok(())
}

fn print_workspace_env_shell(response: &workspace::IpcResponse) -> Result<()> {
    let environment = response
        .environment
        .as_ref()
        .context("workspace env response did not include environment")?;
    for env_var in &environment.variables {
        println!("export {}={}", env_var.name, shell_quote(&env_var.value));
    }
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn print_help() {
    println!(
        r#"agent-workspace-linux

Usage:
  agent-workspace-linux [--permissions PATH] <command>
  agent-workspace-linux doctor
  agent-workspace-linux guardrails
  agent-workspace-linux mcp [--permissions PATH] [--headless]
  agent-workspace-linux viewer [--id ID] [--always-on-top] [--exit-when-workspace-gone]
  agent-workspace-linux permissions validate --json PATH
  agent-workspace-linux permissions template open|closed|local [--allow-host HOST] [--mount HOST:WORKSPACE[:read_only|read_write]] [--app PROGRAM]
  agent-workspace-linux profile path|list|get|check|validate|template|put|import|export|delete
  agent-workspace-linux profile validate --json PATH
  agent-workspace-linux profile put --json PATH [--replace] [--dry-run]
  agent-workspace-linux profile import --json PATH [--replace] [--dry-run]
  agent-workspace-linux profile export ID [--output PATH] [--replace]
  agent-workspace-linux profile delete [--dry-run] ID
  agent-workspace-linux profile template project-dev [--id ID] [--host-path PATH]
  agent-workspace-linux profile template restricted-chrome [--id ID] [--browser-path PATH]
  agent-workspace-linux profile template browser-session [--id ID] [--browser-path PATH] --user-data-dir PATH
  agent-workspace-linux workspace start [--dry-run] --ack-hidden-workspace [--ack-unenforced-policy] [--foreground] [--profile PROFILE] [--id ID] [--purpose TEXT] [--width PX] [--height PX]
  agent-workspace-linux workspace open-profile [--dry-run] [--ack-hidden-workspace] [--ack-unenforced-policy] --profile PROFILE [--setup] [--setup-timeout-ms N] [--setup-kill-on-timeout] [--startup-wait-window] [--startup-window-timeout-ms N] [--startup-screenshot-window] [--id ID] [--purpose TEXT] [--width PX] [--height PX]
  agent-workspace-linux workspace list
  agent-workspace-linux workspace cleanup [--id ID] [--dry-run]
  agent-workspace-linux workspace status [--id ID]
  agent-workspace-linux workspace manifest [--id ID]
  agent-workspace-linux workspace artifacts [--id ID] [--existing]
  agent-workspace-linux workspace ipc-info [--id ID]
  agent-workspace-linux workspace env [--id ID] [--shell]
  agent-workspace-linux workspace launch [--dry-run] [--id ID] [--name NAME] [--profile PROFILE] [--ack-unenforced-policy] [--cwd DIR] [--env NAME=VALUE] [--wait-window] [--window-timeout-ms N] [--screenshot-window] -- COMMAND [ARGS...]
  agent-workspace-linux workspace run [--dry-run] [--id ID] [--name NAME] [--profile PROFILE] [--ack-unenforced-policy] [--cwd DIR] [--env NAME=VALUE] [--timeout-ms N] [--tail-bytes N] [--kill-on-timeout] -- COMMAND [ARGS...]
  agent-workspace-linux workspace launch-profile-apps [--dry-run] [--id ID] --profile PROFILE [--ack-unenforced-policy] [--wait-window] [--window-timeout-ms N] [--screenshot-window]
  agent-workspace-linux workspace apps [--id ID] [--app APP_ID_OR_PID_OR_NAME] [--name TEXT] [--command TEXT] [--profile PROFILE] [--running|--stopped]
  agent-workspace-linux workspace browser-targets [--id ID] [--app APP_ID_OR_PID_OR_NAME] [--user-data-dir PATH] [--timeout-ms N]
  agent-workspace-linux workspace browser-snapshot [--id ID] [--app APP_ID_OR_PID_OR_NAME] [--user-data-dir PATH] [--target TARGET_ID] [--title TEXT] [--url-contains TEXT] [--max-text-chars N] [--timeout-ms N]
  agent-workspace-linux workspace browser-search-results [--id ID] [--app APP_ID_OR_PID_OR_NAME] [--user-data-dir PATH] [--target TARGET_ID] [--title TEXT] [--url-contains TEXT] [--max-results N] [--min-vram-gb N] [--timeout-ms N]
  agent-workspace-linux workspace browser-navigate [--id ID] [--app APP_ID_OR_PID_OR_NAME] [--user-data-dir PATH] [--target TARGET_ID] [--title TEXT] [--url-contains TEXT] [--wait-ms N] [--no-snapshot] [--max-text-chars N] [--timeout-ms N] URL
  agent-workspace-linux workspace windows [--id ID] [--all] [--title TEXT] [--class TEXT] [--pid PID] [--app APP_ID_OR_PID_OR_NAME]
  agent-workspace-linux workspace active-window [--id ID]
  agent-workspace-linux workspace pointer [--id ID]
  agent-workspace-linux workspace observe [--id ID] [--all-windows] [--screenshot] [--output PATH] [--events] [--events-tail N] [--events-since SEQUENCE]
  agent-workspace-linux workspace wait-window [--id ID] [--title TEXT] [--class TEXT] [--pid PID] [--app APP_ID_OR_PID_OR_NAME] [--timeout-ms N]
  agent-workspace-linux workspace screenshot [--id ID] [--output PATH]
  agent-workspace-linux workspace screenshot-window [--id ID] [--window WINDOW_ID] [--title TEXT] [--class TEXT] [--pid PID] [--app APP_ID_OR_PID_OR_NAME] [--output PATH] [--timeout-ms N]
  agent-workspace-linux workspace focus-window [--id ID] WINDOW_ID
  agent-workspace-linux workspace focus-window [--id ID] [--title TEXT] [--class TEXT] [--pid PID] [--app APP_ID_OR_PID_OR_NAME] [--timeout-ms N]
  agent-workspace-linux workspace close-window [--id ID] [--dry-run] WINDOW_ID
  agent-workspace-linux workspace close-window [--id ID] [--dry-run] [--title TEXT] [--class TEXT] [--pid PID] [--app APP_ID_OR_PID_OR_NAME] [--timeout-ms N]
  agent-workspace-linux workspace move-window [--id ID] WINDOW_ID X Y
  agent-workspace-linux workspace move-window [--id ID] [--title TEXT] [--class TEXT] [--pid PID] [--app APP_ID_OR_PID_OR_NAME] [--timeout-ms N] X Y
  agent-workspace-linux workspace resize-window [--id ID] WINDOW_ID WIDTH HEIGHT
  agent-workspace-linux workspace resize-window [--id ID] [--title TEXT] [--class TEXT] [--pid PID] [--app APP_ID_OR_PID_OR_NAME] [--timeout-ms N] WIDTH HEIGHT
  agent-workspace-linux workspace raise-window [--id ID] WINDOW_ID
  agent-workspace-linux workspace raise-window [--id ID] [--title TEXT] [--class TEXT] [--pid PID] [--app APP_ID_OR_PID_OR_NAME] [--timeout-ms N]
  agent-workspace-linux workspace minimize-window [--id ID] WINDOW_ID
  agent-workspace-linux workspace minimize-window [--id ID] [--title TEXT] [--class TEXT] [--pid PID] [--app APP_ID_OR_PID_OR_NAME] [--timeout-ms N]
  agent-workspace-linux workspace show-window [--id ID] WINDOW_ID
  agent-workspace-linux workspace show-window [--id ID] [--title TEXT] [--class TEXT] [--pid PID] [--app APP_ID_OR_PID_OR_NAME] [--timeout-ms N]
  agent-workspace-linux workspace click [--id ID] [--button N] [--count N] X Y
  agent-workspace-linux workspace click-window [--id ID] WINDOW_ID X Y
  agent-workspace-linux workspace click-window [--id ID] [--button N] [--count N] WINDOW_ID X Y
  agent-workspace-linux workspace click-window [--id ID] [--title TEXT] [--class TEXT] [--pid PID] [--app APP_ID_OR_PID_OR_NAME] [--button N] [--count N] [--timeout-ms N] X Y
  agent-workspace-linux workspace move-pointer [--id ID] X Y
  agent-workspace-linux workspace move-pointer-window [--id ID] WINDOW_ID X Y
  agent-workspace-linux workspace move-pointer-window [--id ID] [--title TEXT] [--class TEXT] [--pid PID] [--app APP_ID_OR_PID_OR_NAME] [--timeout-ms N] X Y
  agent-workspace-linux workspace drag [--id ID] [--button N] FROM_X FROM_Y TO_X TO_Y
  agent-workspace-linux workspace drag-window [--id ID] [--button N] WINDOW_ID FROM_X FROM_Y TO_X TO_Y
  agent-workspace-linux workspace drag-window [--id ID] [--title TEXT] [--class TEXT] [--pid PID] [--app APP_ID_OR_PID_OR_NAME] [--button N] [--timeout-ms N] FROM_X FROM_Y TO_X TO_Y
  agent-workspace-linux workspace scroll [--id ID] [--amount N] X Y up|down|left|right
  agent-workspace-linux workspace scroll-window [--id ID] [--amount N] WINDOW_ID X Y up|down|left|right
  agent-workspace-linux workspace scroll-window [--id ID] [--title TEXT] [--class TEXT] [--pid PID] [--app APP_ID_OR_PID_OR_NAME] [--amount N] [--timeout-ms N] X Y up|down|left|right
  agent-workspace-linux workspace key [--id ID] KEY
  agent-workspace-linux workspace key-window [--id ID] WINDOW_ID KEY
  agent-workspace-linux workspace key-window [--id ID] [--title TEXT] [--class TEXT] [--pid PID] [--app APP_ID_OR_PID_OR_NAME] [--timeout-ms N] KEY
  agent-workspace-linux workspace type [--id ID] TEXT
  agent-workspace-linux workspace type-window [--id ID] WINDOW_ID TEXT
  agent-workspace-linux workspace type-window [--id ID] [--title TEXT] [--class TEXT] [--pid PID] [--app APP_ID_OR_PID_OR_NAME] [--timeout-ms N] TEXT
  agent-workspace-linux workspace clipboard-set [--id ID] TEXT
  agent-workspace-linux workspace clipboard-get [--id ID]
  agent-workspace-linux workspace paste [--id ID] [--key KEY] TEXT
  agent-workspace-linux workspace paste-window [--id ID] [--key KEY] WINDOW_ID TEXT
  agent-workspace-linux workspace paste-window [--id ID] [--title TEXT] [--class TEXT] [--pid PID] [--app APP_ID_OR_PID_OR_NAME] [--key KEY] [--timeout-ms N] TEXT
  agent-workspace-linux workspace logs [--id ID] [--stream stdout|stderr] [--tail-bytes N] APP_ID_OR_PID_OR_NAME
  agent-workspace-linux workspace wait-app [--id ID] [--timeout-ms N] [--kill-on-timeout] APP_ID_OR_PID_OR_NAME
  agent-workspace-linux workspace events [--id ID] [--tail N] [--since SEQUENCE]
  agent-workspace-linux workspace setup [--dry-run] [--id ID] --profile PROFILE [--wait] [--timeout-ms N] [--kill-on-timeout] [--ack-unenforced-policy]
  agent-workspace-linux workspace kill-app [--id ID] [--dry-run] APP_ID_OR_PID_OR_NAME
  agent-workspace-linux workspace stop [--id ID] [--timeout-ms N] [--dry-run]"#
    );
}

fn print_mcp_help() {
    println!(
        r#"agent-workspace-linux mcp

Usage:
  agent-workspace-linux mcp [--permissions PATH] [--headless]

Options:
  --permissions PATH  Load a spawn-time MCP permission ceiling JSON file. If omitted, the MCP adds no ceiling and respects the host/client harness boundary. Empty fields in a file leave that dimension open; populated network, mounts, or app allowlist fields cap every MCP tool for this process.
  --headless          Disable host-visible GPUI viewer launches from this MCP process. Without this flag, workspace_open_viewer may open the live monitor when the agent/user asks for it.
"#
    );
}

fn print_viewer_help() {
    println!(
        r#"agent-workspace-linux viewer

Usage:
  agent-workspace-linux viewer [--id ID] [--always-on-top] [--input-forwarding] [--exit-when-workspace-gone]
  agent-workspace-linux viewer list
  agent-workspace-linux viewer close (--id ID | --all) [--dry-run]

Opens the small host-visible Agent Workspace GPUI monitor.
The viewer runs outside the MCP stdio server and surfaces the selected isolated
workspace without taking over the user's desktop.
By default it does not request always-on-top state; --always-on-top opts into
Wayland layer-shell or X11/Xwayland above hints for hosts that explicitly want
that behavior.
--exit-when-workspace-gone closes the viewer once the selected workspace runtime
is removed; workspace_open_viewer uses this so MCP-launched monitors do not
become orphan windows.
--input-forwarding allows the viewer's explicit in-window Input toggle to arm
manual mouse/keyboard/paste forwarding into the isolated workspace. It is off by
default and still requires a visible second-click confirmation in the viewer UI.
The list and close subcommands use the repo-owned viewer registry, so they can
inspect and close GPUI viewers even when the desktop compositor does not expose
them as ordinary controllable windows.
"#
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_open_mcp_permissions(state: &permissions::McpPermissionState) {
        assert!(!state.restricted);
        state
            .validate_launch_spec(&LaunchSpec {
                command: vec!["sh".to_string(), "-c".to_string(), "true".to_string()],
                name: None,
                profile_id: None,
                applied_policy: None,
                user_acknowledged_unenforced_policy: false,
                cwd: None,
                env: Vec::<EnvVar>::new(),
            })
            .expect("open MCP permissions should not restrict unprofiled launches");
    }

    #[test]
    fn mcp_without_permissions_uses_harness_owned_open_boundary() {
        let options = parse_mcp_options(&[], permissions::McpPermissionState::default())
            .expect("parse mcp options")
            .expect("mcp should start");

        assert!(!options.headless);
        assert!(!options.permissions.configured);
        assert_open_mcp_permissions(&options.permissions);
        assert!(
            options.permissions.message.contains("host/client session"),
            "default MCP message should point agents to the harness/session boundary"
        );
    }

    #[test]
    fn empty_mcp_permissions_file_is_configured_but_unrestricted() {
        let path = std::env::temp_dir().join(format!(
            "agent-workspace-empty-mcp-permissions-{}.json",
            std::process::id()
        ));
        fs::write(&path, "{}\n").expect("write empty permissions file");
        let args = vec![
            "--permissions".to_string(),
            path.display().to_string(),
            "--headless".to_string(),
        ];

        let options = parse_mcp_options(&args, permissions::McpPermissionState::default())
            .expect("parse mcp options")
            .expect("mcp should start");
        let _ = fs::remove_file(&path);

        assert!(options.headless);
        assert!(options.permissions.configured);
        assert_open_mcp_permissions(&options.permissions);
        assert!(
            options
                .permissions
                .message
                .contains("does not restrict network, mounts, or app launches"),
            "empty permissions file should be advisory/open, not a forced MCP sandbox"
        );
    }

    #[test]
    fn viewer_always_on_top_is_opt_in() {
        let default_options = parse_viewer_options(&[])
            .expect("parse viewer options")
            .expect("viewer should run");
        assert!(!default_options.always_on_top);
        assert!(!default_options.input_forwarding);
        assert!(!default_options.exit_when_workspace_gone);

        let args = vec!["--always-on-top".to_string()];
        let always_options = parse_viewer_options(&args)
            .expect("parse viewer options")
            .expect("viewer should run");
        assert!(always_options.always_on_top);
    }

    #[test]
    fn viewer_input_forwarding_is_opt_in() {
        let args = vec!["--input-forwarding".to_string()];
        let options = parse_viewer_options(&args)
            .expect("parse viewer options")
            .expect("viewer should run");
        assert!(options.input_forwarding);
    }

    #[test]
    fn viewer_exit_when_workspace_gone_is_opt_in() {
        let args = vec!["--exit-when-workspace-gone".to_string()];
        let options = parse_viewer_options(&args)
            .expect("parse viewer options")
            .expect("viewer should run");
        assert!(options.exit_when_workspace_gone);
    }
}
