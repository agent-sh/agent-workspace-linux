use crate::agent::{AgentModeSummary, AgentTargetHandles};
use crate::browser;
use crate::control::{self, McpControlMode};
use crate::guardrails;
use crate::permissions::McpPermissionState;
use crate::profile::{self, WorkspaceProfile};
use crate::viewer;
use crate::workspace::{
    self, EnvVar, IpcResponse, LaunchSpec, ScrollDirection, WorkspaceStartOptions, WorkspaceStatus,
    DEFAULT_WORKSPACE_ID,
};
use anyhow::{bail, Result};
use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    schemars::JsonSchema,
    tool, tool_handler, tool_router, ServerHandler, ServiceExt,
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::PathBuf};

#[derive(Clone, Default)]
pub struct AgentWorkspaceLinux {
    permissions: McpPermissionState,
    headless: bool,
}

fn live_control_reactivation_hint() -> &'static str {
    "Reactivation through mcp_control_update requires mode=active and confirmed_user_request=true after explicit user or controlling UI approval."
}

impl AgentWorkspaceLinux {
    pub fn new(permissions: McpPermissionState, headless: bool) -> Self {
        Self {
            permissions,
            headless,
        }
    }

    fn enforce_agent_mutation(&self, action: &str) -> Result<()> {
        let status = control::control_status()?;
        if status.state.mode.allows_agent_mutation() {
            return Ok(());
        }
        bail!(
            "MCP live control is {}; {action} is disabled. {}",
            status.state.mode.label(),
            live_control_reactivation_hint()
        )
    }

    fn enforce_agent_mutation_unless_dry_run(&self, dry_run: bool, action: &str) -> Result<()> {
        if dry_run {
            Ok(())
        } else {
            self.enforce_agent_mutation(action)
        }
    }

    fn result_response(&self, result: Result<IpcResponse>) -> IpcResponse {
        self.decorate_ipc_response(result_response(result))
    }

    fn decorate_ipc_response(&self, mut response: IpcResponse) -> IpcResponse {
        if response.agent_mode.is_none() {
            response.agent_mode = Some(build_agent_mode_summary(self.headless));
        }
        if response.target_handles.is_none() {
            let handles = target_handles_from_ipc_response(&response);
            if !handles.is_empty() {
                response.target_handles = Some(handles);
            }
        }
        if !response.ok && response.recovery_hints.is_empty() {
            response.recovery_hints = recovery_hints_for_message(&response.message);
        }
        response
    }

    fn decorate_profile_put_result(
        &self,
        mut response: profile::ProfilePutResult,
    ) -> profile::ProfilePutResult {
        response.agent_mode = Some(build_agent_mode_summary(self.headless));
        if !response.ok && response.recovery_hints.is_empty() {
            response.recovery_hints = recovery_hints_for_message(&response.message);
        }
        response
    }

    fn decorate_profile_delete_result(
        &self,
        mut response: profile::ProfileDeleteResult,
    ) -> profile::ProfileDeleteResult {
        response.agent_mode = Some(build_agent_mode_summary(self.headless));
        response
    }

    fn decorate_browser_targets(
        &self,
        mut response: browser::WorkspaceBrowserTargets,
    ) -> browser::WorkspaceBrowserTargets {
        response.agent_mode = Some(build_agent_mode_summary(self.headless));
        response
    }

    fn decorate_browser_open(
        &self,
        mut response: browser::WorkspaceBrowserOpen,
    ) -> browser::WorkspaceBrowserOpen {
        response.agent_mode = Some(build_agent_mode_summary(self.headless));
        response
    }

    fn decorate_browser_snapshot(
        &self,
        mut response: browser::WorkspaceBrowserSnapshot,
    ) -> browser::WorkspaceBrowserSnapshot {
        response.agent_mode = Some(build_agent_mode_summary(self.headless));
        response
    }

    fn decorate_browser_search_results(
        &self,
        mut response: browser::WorkspaceBrowserSearchResults,
    ) -> browser::WorkspaceBrowserSearchResults {
        response.agent_mode = Some(build_agent_mode_summary(self.headless));
        response
    }

    fn decorate_browser_navigate(
        &self,
        mut response: browser::WorkspaceBrowserNavigate,
    ) -> browser::WorkspaceBrowserNavigate {
        response.agent_mode = Some(build_agent_mode_summary(self.headless));
        response
    }

    fn decorate_browser_click(
        &self,
        mut response: browser::WorkspaceBrowserClick,
    ) -> browser::WorkspaceBrowserClick {
        response.agent_mode = Some(build_agent_mode_summary(self.headless));
        response
    }

    fn auto_open_workspace_viewer_for_response(
        &self,
        workspace_id: Option<&str>,
        requested: bool,
        always_on_top: bool,
    ) -> WorkspaceViewerAutoOpen {
        if !requested {
            return WorkspaceViewerAutoOpen {
                requested,
                attempted: false,
                ok: true,
                message: "viewer auto-open disabled by open_viewer=false".to_string(),
                launch: None,
            };
        }
        let Some(workspace_id) = workspace_id else {
            return WorkspaceViewerAutoOpen {
                requested,
                attempted: false,
                ok: false,
                message: "viewer auto-open skipped because the workspace did not start".to_string(),
                launch: None,
            };
        };
        if self.headless {
            return WorkspaceViewerAutoOpen {
                requested,
                attempted: false,
                ok: false,
                message: "viewer auto-open disabled because this MCP process was explicitly started with --headless".to_string(),
                launch: None,
            };
        }
        let doctor = workspace::doctor_report();
        if !doctor.ready_for_host_viewer {
            return WorkspaceViewerAutoOpen {
                requested,
                attempted: false,
                ok: false,
                message: format!(
                    "viewer auto-open skipped because workspace_doctor.ready_for_host_viewer=false: {}",
                    doctor.viewer_blockers.join("; ")
                ),
                launch: None,
            };
        }

        match viewer::open_viewer(
            Some(workspace_id.to_string()),
            &self.permissions,
            always_on_top,
            false,
        ) {
            Ok(launch) => WorkspaceViewerAutoOpen {
                requested,
                attempted: true,
                ok: true,
                message: if launch.reused {
                    "workspace viewer already open".to_string()
                } else {
                    "workspace viewer opened automatically".to_string()
                },
                launch: Some(launch),
            },
            Err(error) => WorkspaceViewerAutoOpen {
                requested,
                attempted: true,
                ok: false,
                message: format!("viewer auto-open failed: {error}"),
                launch: None,
            },
        }
    }

    fn open_profile_workspace_with_default_viewer(
        &self,
        options: WorkspaceStartOptions,
        profile_id: &str,
        open_options: profile::ProfileWorkspaceOpenOptions,
        open_viewer: bool,
        viewer_always_on_top: bool,
    ) -> Result<(profile::ProfileWorkspaceOpen, WorkspaceViewerAutoOpen)> {
        let workspace_id = options.id.clone();
        let start = workspace::start_workspace(options)?;
        let viewer_auto_open = self.auto_open_workspace_viewer_for_response(
            start.status.as_ref().map(|status| status.id.as_str()),
            open_viewer,
            viewer_always_on_top,
        );
        let (setup, startup) = if start.ok {
            let setup = if open_options.run_setup {
                Some(profile::launch_profile_setup(
                    &workspace_id,
                    profile_id,
                    open_options.setup,
                )?)
            } else {
                None
            };
            let setup_succeeded = setup
                .as_ref()
                .and_then(|setup| setup.succeeded)
                .unwrap_or(true);
            let startup = if setup_succeeded {
                Some(profile::launch_profile_startup_apps(
                    &workspace_id,
                    profile_id,
                    open_options.startup,
                )?)
            } else {
                None
            };
            (setup, startup)
        } else {
            (None, None)
        };
        let setup_succeeded = setup.as_ref().and_then(|setup| setup.succeeded);
        let startup_launched = startup
            .as_ref()
            .is_some_and(|startup| startup.launched.iter().all(|response| response.ok));
        let ready = start.ok && setup_succeeded.unwrap_or(true) && startup_launched;
        Ok((
            profile::ProfileWorkspaceOpen {
                workspace_id,
                profile_id: profile_id.to_string(),
                ready,
                setup_succeeded,
                startup_launched,
                start,
                setup,
                startup,
            },
            viewer_auto_open,
        ))
    }
}

#[tool_router]
impl AgentWorkspaceLinux {
    #[tool(
        name = "workspace_guardrails",
        description = "Return machine-readable guardrails for isolated workspace actions, including concise agent_rules with allowed, blocked, requires_ack, and exact_parameter fields plus detailed acknowledgement, dry-run, override, policy, and timeout rules.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workspace_guardrails(&self) -> Json<guardrails::GuardrailSummary> {
        Json(guardrails::guardrail_summary())
    }

    #[tool(
        name = "mcp_permissions",
        description = "Return the spawn-time MCP permission state for this server process. If configured=true, populated network, mount, or app dimensions are immutable ceilings that clients may only narrow. If configured=false, this MCP imposes no ceiling and only reports the host/client harness boundary.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn mcp_permissions(&self) -> Json<McpPermissionState> {
        Json(self.permissions.clone())
    }

    #[tool(
        name = "mcp_action_catalog",
        description = "Return a machine-readable catalog of MCP tools, their action type, idempotency/destructive/open-world hints, and how the live active/read_only/paused control mode treats them. Use this when no permission ceiling is configured or when deciding what a user likely needs to approve.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn mcp_action_catalog(&self) -> Json<McpActionCatalog> {
        Json(mcp_action_catalog())
    }

    #[tool(
        name = "mcp_session_brief",
        description = "Return a read-only agent UX brief for this MCP session: permission ceiling, live control mode, runtime readiness, known workspace/profile counts, and suggested next MCP actions with approval and headless/open-world hints.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn mcp_session_brief(&self) -> Json<McpSessionBrief> {
        Json(build_mcp_session_brief(&self.permissions, self.headless))
    }

    #[tool(
        name = "mcp_agent_context",
        description = "Return one compact read-only agent context snapshot: mode, permissions, selected workspace, active app/window handles, browser target handles, viewer handles, and exact next recovery tools. This is the low-noise orientation call before acting.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn mcp_agent_context(
        &self,
        Parameters(params): Parameters<McpAgentContextParams>,
    ) -> Json<McpAgentContext> {
        Json(build_mcp_agent_context(
            params,
            &self.permissions,
            self.headless,
        ))
    }

    #[tool(
        name = "mcp_task_plan",
        description = "Return a read-only intent-aware MCP plan for common user tasks such as app QA, browser/shopping workflows, observation, or cleanup. The plan suggests safe preview tools, viewer-first visibility when available, profile templates, approval points, structured task_context input state, dogfood evidence requirements, and live-control constraints without executing anything.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn mcp_task_plan(
        &self,
        Parameters(params): Parameters<McpTaskPlanParams>,
    ) -> Json<McpTaskPlan> {
        Json(build_mcp_task_plan(
            params,
            &self.permissions,
            self.headless,
        ))
    }

    #[tool(
        name = "mcp_control_state",
        description = "Return the live MCP control state shared by this server and the GPUI viewer. mode=active permits normal actions, mode=read_only blocks mutating agent actions while allowing inspection, and mode=paused blocks mutating agent actions until reactivated.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn mcp_control_state(&self) -> Json<McpControlResponse> {
        Json(match control::control_status() {
            Ok(status) => McpControlResponse {
                ok: true,
                message: "MCP control state returned".to_string(),
                status: Some(status),
            },
            Err(error) => McpControlResponse {
                ok: false,
                message: error.to_string(),
                status: None,
            },
        })
    }

    #[tool(
        name = "mcp_control_update",
        description = "Set the live MCP control mode to active, read_only, or paused. read_only and paused block mutating agent actions at the MCP boundary but still allow read-only inspection and safety stop operations. If the current mode is read_only or paused, switching back to active requires confirmed_user_request=true after an explicit user or controlling UI request.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn mcp_control_update(
        &self,
        Parameters(params): Parameters<McpControlUpdateParams>,
    ) -> Json<McpControlResponse> {
        Json(match update_mcp_control_from_tool(params) {
            Ok(status) => McpControlResponse {
                ok: true,
                message: format!("MCP control mode set to {}", status.state.mode.label()),
                status: Some(status),
            },
            Err(error) => McpControlResponse {
                ok: false,
                message: error.to_string(),
                status: None,
            },
        })
    }

    #[tool(
        name = "workspace_doctor",
        description = "Report readiness for isolated Linux agent workspaces, including optional policy backend candidates such as bubblewrap, firejail, unshare, and slirp4netns.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workspace_doctor(&self) -> Json<workspace::DoctorReport> {
        Json(workspace::doctor_report())
    }

    #[tool(
        name = "profile_path",
        description = "Return the local JSON file path used to persist agent workspace profiles.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn profile_path(&self) -> Json<profile::ProfilePath> {
        Json(profile::profile_path())
    }

    #[tool(
        name = "profile_list",
        description = "List saved agent workspace profiles.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn profile_list(&self) -> Json<profile::ProfileList> {
        Json(
            profile::list_profiles().unwrap_or_else(|error| profile::ProfileList {
                path: std::path::PathBuf::new(),
                profiles: vec![WorkspaceProfile {
                    id: "error".to_string(),
                    description: Some(error.to_string()),
                    width: None,
                    height: None,
                    cwd: None,
                    env: Vec::new(),
                    mounts: Vec::new(),
                    network: Default::default(),
                    require_enforced_policy: false,
                    setup_commands: Vec::new(),
                    startup_apps: Vec::new(),
                }],
            }),
        )
    }

    #[tool(
        name = "profile_get",
        description = "Get one saved agent workspace profile by id.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn profile_get(
        &self,
        Parameters(params): Parameters<ProfileIdParams>,
    ) -> Json<ProfileGetResult> {
        Json(match profile::get_profile(&params.id) {
            Ok(profile) => ProfileGetResult {
                ok: true,
                message: "profile returned".to_string(),
                profile: Some(profile),
            },
            Err(error) => ProfileGetResult {
                ok: false,
                message: error.to_string(),
                profile: None,
            },
        })
    }

    #[tool(
        name = "profile_check",
        description = "Preflight a saved profile against the current machine. Returns the applied policy, per-capability state/backend/planned_backend/backend_requirements/limitations, warnings, and acknowledgement requirements before starting a workspace.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn profile_check(
        &self,
        Parameters(params): Parameters<ProfileIdParams>,
    ) -> Json<ProfileCheckResult> {
        Json(
            match profile::get_profile(&params.id).and_then(|profile| {
                self.permissions.validate_profile(&profile)?;
                profile::check_profile(&params.id)
            }) {
                Ok(check) => ProfileCheckResult {
                    ok: true,
                    message: "profile check returned".to_string(),
                    check: Some(check),
                },
                Err(error) => ProfileCheckResult {
                    ok: false,
                    message: error.to_string(),
                    check: None,
                },
            },
        )
    }

    #[tool(
        name = "profile_validate",
        description = "Validate an agent workspace profile JSON file without saving it. Returns the parsed profile plus the same applied policy, warning, and acknowledgement preflight as profile_check.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn profile_validate(
        &self,
        Parameters(params): Parameters<ProfileValidateParams>,
    ) -> Json<ProfileValidateResponse> {
        Json(
            match profile::validate_profile_json_file(params.json_path) {
                Ok(validation) => match self.permissions.validate_profile(&validation.profile) {
                    Ok(()) => ProfileValidateResponse {
                        ok: true,
                        message: "profile JSON is valid".to_string(),
                        validation: Some(validation),
                    },
                    Err(error) => ProfileValidateResponse {
                        ok: false,
                        message: error.to_string(),
                        validation: None,
                    },
                },
                Err(error) => ProfileValidateResponse {
                    ok: false,
                    message: error.to_string(),
                    validation: None,
                },
            },
        )
    }

    #[tool(
        name = "profile_template",
        description = "Generate a starter profile without saving it. The project-dev template mounts a project directory read-write at /workspace/project and, when detected, mounts Cargo bin/rustup toolchains read-only without mounting Cargo credentials. The restricted-chrome template starts Chrome with network.mode=disabled, require_enforced_policy=true, an isolated user-data dir, and an explicit --no-sandbox startup command for bubblewrap network namespaces. The browser-session template requires user_data_dir, mounts that browser data directory read-write at /workspace/browser-user-data, inherits host network for authenticated web tasks, and visibly uses --no-sandbox because Chrome can abort inside the enforced mount namespace.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn profile_template(
        &self,
        Parameters(params): Parameters<ProfileTemplateParams>,
    ) -> Json<ProfileGetResult> {
        Json(
            match profile::template_profile(
                &params.kind,
                params.id,
                params.host_path,
                params.browser_path,
                params.user_data_dir,
            ) {
                Ok(profile) => match self.permissions.validate_profile(&profile) {
                    Ok(()) => ProfileGetResult {
                        ok: true,
                        message: "profile template returned".to_string(),
                        profile: Some(profile),
                    },
                    Err(error) => ProfileGetResult {
                        ok: false,
                        message: error.to_string(),
                        profile: None,
                    },
                },
                Err(error) => ProfileGetResult {
                    ok: false,
                    message: error.to_string(),
                    profile: None,
                },
            },
        )
    }

    #[tool(
        name = "profile_put",
        description = "Create an agent workspace profile. Existing profile ids are rejected unless replace=true is set explicitly. Set dry_run=true to preview whether the profile would be created, replaced, or rejected without writing. Mounts, network, require_enforced_policy, and setup commands are persisted as declared intent and surfaced in workspace status; display size, cwd, and env are currently applied by the X11 runtime.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn profile_put(
        &self,
        Parameters(params): Parameters<ProfilePutParams>,
    ) -> Json<profile::ProfilePutResult> {
        let requested_profile = params.profile.clone();
        Json(
            self.decorate_profile_put_result(
                self.enforce_agent_mutation_unless_dry_run(params.dry_run, "profile_put")
                    .and_then(|_| {
                        self.permissions
                            .validate_profile(&params.profile)
                            .and_then(|_| {
                                profile::put_profile(params.profile, params.replace, params.dry_run)
                            })
                    })
                    .unwrap_or_else(|error| {
                        profile::ProfilePutResult::error(
                            requested_profile,
                            params.replace,
                            params.dry_run,
                            error.to_string(),
                        )
                    }),
            ),
        )
    }

    #[tool(
        name = "profile_import",
        description = "Import an agent workspace profile from a local JSON file path. Existing profile ids are rejected unless replace=true is set explicitly. Set dry_run=true to validate and preview whether the imported profile would be created, replaced, or rejected without writing.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn profile_import(
        &self,
        Parameters(params): Parameters<ProfileImportParams>,
    ) -> Json<profile::ProfilePutResult> {
        Json(
            self.decorate_profile_put_result(
                match self
                    .enforce_agent_mutation_unless_dry_run(params.dry_run, "profile_import")
                    .and_then(|_| profile::read_profile_json_file(&params.json_path))
                {
                    Ok(requested_profile) => self
                        .permissions
                        .validate_profile(&requested_profile)
                        .and_then(|_| {
                            profile::put_profile(
                                requested_profile.clone(),
                                params.replace,
                                params.dry_run,
                            )
                        })
                        .unwrap_or_else(|error| {
                            profile::ProfilePutResult::error(
                                requested_profile,
                                params.replace,
                                params.dry_run,
                                error.to_string(),
                            )
                        }),
                    Err(error) => profile::ProfilePutResult::import_error(
                        params.replace,
                        params.dry_run,
                        error.to_string(),
                    ),
                },
            ),
        )
    }

    #[tool(
        name = "profile_export",
        description = "Return one saved profile by id and optionally write it as pretty JSON to output_path. If output_path is an existing directory, the file is written as <profile-id>.json inside it. Missing parent directories are created. Existing output files are rejected unless replace=true is set explicitly.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn profile_export(
        &self,
        Parameters(params): Parameters<ProfileExportParams>,
    ) -> Json<ProfileExportResponse> {
        Json(
            match self
                .enforce_agent_mutation_unless_dry_run(
                    params.output_path.is_none(),
                    "profile_export",
                )
                .and_then(|_| {
                    profile::export_profile(&params.id, params.output_path, params.replace)
                }) {
                Ok(export) => ProfileExportResponse {
                    ok: true,
                    message: if export.wrote {
                        "profile exported".to_string()
                    } else {
                        "profile returned".to_string()
                    },
                    agent_mode: Some(build_agent_mode_summary(self.headless)),
                    recovery_hints: Vec::new(),
                    export: Some(export),
                },
                Err(error) => {
                    let message = error.to_string();
                    ProfileExportResponse {
                        ok: false,
                        agent_mode: Some(build_agent_mode_summary(self.headless)),
                        recovery_hints: recovery_hints_for_message(&message),
                        message,
                        export: None,
                    }
                }
            },
        )
    }

    #[tool(
        name = "profile_delete",
        description = "Delete one saved agent workspace profile by id. Set dry_run=true to return the profile that would be deleted without removing it.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn profile_delete(
        &self,
        Parameters(params): Parameters<ProfileDeleteParams>,
    ) -> Json<profile::ProfileDeleteResult> {
        Json(
            self.decorate_profile_delete_result(
                self.enforce_agent_mutation_unless_dry_run(params.dry_run, "profile_delete")
                    .and_then(|_| profile::delete_profile(&params.id, params.dry_run))
                    .unwrap_or(profile::ProfileDeleteResult {
                        id: params.id,
                        deleted: false,
                        would_delete: false,
                        dry_run: params.dry_run,
                        profile: None,
                        agent_mode: None,
                    }),
            ),
        )
    }

    #[tool(
        name = "workspace_start",
        description = "Start an isolated X11 agent workspace with its own display and control IPC socket. By default, when this MCP process is not --headless and workspace_doctor.ready_for_host_viewer=true, the tool also opens the small host-visible GPUI viewer immediately; set open_viewer=false to explicitly opt out. Set dry_run=true for a pre-daemon approval preview: it checks acknowledgement, runtime, and policy requirements without creating a runtime directory, daemon, display, viewer, or apps. Dry-run responses include an approval bundle for UI confirmation. Set acknowledge_hidden_workspace=true to confirm the user knows this creates a separate agent-controlled environment. Optional purpose records a human-readable reason in status and the start event. If the selected profile requests currently unenforced mount or network restrictions, also set acknowledge_unenforced_policy=true. Mount, disabled-network, and local_only network profiles are enforced with bubblewrap when available; local_only uses a loopback-only namespace and does not bridge host localhost services. The current product network modes are closed/disabled, local/local_only, and open/inherit_host; allowlist network profiles are advanced/legacy declared intent only. Profiles with require_enforced_policy=true reject unenforced policy instead of accepting acknowledgement.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    fn workspace_start(
        &self,
        Parameters(params): Parameters<WorkspaceStartParams>,
    ) -> Json<WorkspaceStartResult> {
        let dry_run = params.dry_run;
        let profile_id = params.profile.clone();
        let open_viewer = params.open_viewer.unwrap_or(true);
        let viewer_always_on_top = params.viewer_always_on_top;
        let result = params.into_options().and_then(|mut options| {
            self.enforce_agent_mutation_unless_dry_run(dry_run, "workspace_start")?;
            if let Some(profile_id) = &profile_id {
                let saved_profile = profile::get_profile(profile_id)?;
                self.permissions.validate_profile(&saved_profile)?;
            }
            self.permissions.validate_start_options(&options)?;
            options.permissions_source = self.permissions.source.clone();
            if dry_run {
                workspace::preview_workspace_start(options)
            } else {
                workspace::start_workspace(options)
            }
        });
        let mut response = self.result_response(result);
        let viewer_auto_open = (!dry_run).then(|| {
            self.auto_open_workspace_viewer_for_response(
                response.status.as_ref().map(|status| status.id.as_str()),
                open_viewer,
                viewer_always_on_top,
            )
        });
        if let Some(auto_open) = &viewer_auto_open {
            merge_viewer_auto_open_handles(&mut response.target_handles, auto_open);
            append_viewer_auto_open_hint(&mut response.recovery_hints, auto_open);
        }
        Json(WorkspaceStartResult {
            response,
            viewer_auto_open,
        })
    }

    #[tool(
        name = "workspace_open_profile",
        description = "Start a profile-backed isolated workspace, optionally record a human-readable purpose, optionally wait for profile setup, optionally kill timed-out setup commands, and launch that profile's startup apps in one operation after setup succeeds. By default, when this MCP process is not --headless and workspace_doctor.ready_for_host_viewer=true, the small host-visible GPUI viewer opens immediately after the workspace starts and before setup/startup apps run; set open_viewer=false to explicitly opt out. Set dry_run=true for a pre-daemon approval preview: it returns the start preview plus setup/startup declarations without creating a runtime directory, daemon, display, viewer, or apps. The preview approval bundle merges all required acknowledgements. Setup/startup entries in this pre-daemon preview are declarations; daemon-attached launch previews are available only after a workspace is running. Set startup_wait_window=true to wait for each startup app's first visible window, or startup_screenshot_window=true to also capture each first startup window.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    fn workspace_open_profile(
        &self,
        Parameters(params): Parameters<WorkspaceOpenProfileParams>,
    ) -> Json<ProfileWorkspaceOpenResult> {
        Json(
            match params
                .to_start_options()
                .and_then(|(mut options, profile_id)| {
                    self.enforce_agent_mutation_unless_dry_run(
                        params.dry_run,
                        "workspace_open_profile",
                    )?;
                    let saved_profile = profile::get_profile(&profile_id)?;
                    self.permissions.validate_profile(&saved_profile)?;
                    self.permissions.validate_start_options(&options)?;
                    options.permissions_source = self.permissions.source.clone();
                    if params.dry_run {
                        return profile::preview_open_profile_workspace(
                            options,
                            &profile_id,
                            params.to_open_options(),
                        )
                        .map(|preview| (None, None, Some(preview)));
                    }
                    self.open_profile_workspace_with_default_viewer(
                        options,
                        &profile_id,
                        params.to_open_options(),
                        params.open_viewer.unwrap_or(true),
                        params.viewer_always_on_top,
                    )
                    .map(|(open, viewer_auto_open)| (Some(open), Some(viewer_auto_open), None))
                }) {
                Ok((open, viewer_auto_open, preview)) => {
                    let mut target_handles =
                        profile_open_target_handles(open.as_ref(), preview.as_ref());
                    if let Some(auto_open) = &viewer_auto_open {
                        merge_viewer_auto_open_handles(&mut target_handles, auto_open);
                    }
                    let mut recovery_hints = Vec::new();
                    if let Some(auto_open) = &viewer_auto_open {
                        append_viewer_auto_open_hint(&mut recovery_hints, auto_open);
                    }
                    ProfileWorkspaceOpenResult {
                        ok: true,
                        message: if preview.is_some() {
                            "profile workspace open dry run returned".to_string()
                        } else {
                            "profile workspace opened".to_string()
                        },
                        target_handles,
                        agent_mode: Some(build_agent_mode_summary(self.headless)),
                        recovery_hints,
                        open,
                        viewer_auto_open,
                        preview,
                    }
                }
                Err(error) => {
                    let message = error.to_string();
                    ProfileWorkspaceOpenResult {
                        ok: false,
                        target_handles: None,
                        agent_mode: Some(build_agent_mode_summary(self.headless)),
                        recovery_hints: recovery_hints_for_message(&message),
                        message,
                        open: None,
                        viewer_auto_open: None,
                        preview: None,
                    }
                }
            },
        )
    }

    #[tool(
        name = "workspace_status",
        description = "Return live status for an isolated agent workspace, including its per-run session_id, applied profile policy snapshot, and enforcement report when a profile started the workspace.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workspace_status(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.decorate_ipc_response(match workspace::status_workspace(&id) {
                Ok(status) => IpcResponse {
                    ok: true,
                    message: "workspace status returned".to_string(),
                    apps: Some(status.apps.clone()),
                    status: Some(status),
                    start_preview: None,
                    launch_preview: None,
                    ipc: None,
                    environment: None,
                    windows: None,
                    active_window: None,
                    pointer: None,
                    screenshot: None,
                    app_log: None,
                    clipboard: None,
                    events: None,
                    browser_targets: None,
                    browser_snapshot: None,
                    browser_search_results: None,
                    browser_navigate: None,
                    agent_mode: None,
                    target_handles: None,
                    recovery_hints: Vec::new(),
                },
                Err(error) => error_response(error.to_string(), None),
            }),
        )
    }

    #[tool(
        name = "workspace_manifest",
        description = "Read the saved workspace manifest from disk for live or stopped workspace inspection without contacting the workspace daemon. The manifest includes the per-run session_id. This is read-only saved state, not live status.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workspace_manifest(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
    ) -> Json<workspace::WorkspaceManifestRead> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(workspace::read_manifest(&id))
    }

    #[tool(
        name = "workspace_artifacts",
        description = "Return a read-only inventory of files left in a workspace runtime directory, including manifest, event log, daemon logs, app logs, and screenshots when present. Set existing_only=true to return only paths that currently exist.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workspace_artifacts(
        &self,
        Parameters(params): Parameters<WorkspaceArtifactsParams>,
    ) -> Json<workspace::WorkspaceArtifacts> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(workspace::artifacts(&id, params.existing_only))
    }

    #[tool(
        name = "workspace_ipc_info",
        description = "Return daemon IPC protocol metadata for an isolated agent workspace, including protocol version, session_id, transport, framing, encoding, and socket path.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workspace_ipc_info(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(self.result_response(workspace::ipc_info(&id)))
    }

    #[tool(
        name = "workspace_env",
        description = "Return the workspace-local environment needed to attach external tools to an isolated agent workspace, including DISPLAY, XAUTHORITY, AGENT_WORKSPACE_SESSION_ID, runtime directory, and control socket path.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workspace_env(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(self.result_response(workspace::environment(&id)))
    }

    #[tool(
        name = "workspace_list",
        description = "List known isolated agent workspace runtime directories and whether each workspace is currently running.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workspace_list(&self) -> Json<workspace::WorkspaceList> {
        Json(
            workspace::list_workspaces().unwrap_or_else(|error| workspace::WorkspaceList {
                runtime_base_dir: std::path::PathBuf::new(),
                workspaces: vec![workspace::WorkspaceListEntry {
                    id: "error".to_string(),
                    runtime_dir: std::path::PathBuf::new(),
                    socket_path: std::path::PathBuf::new(),
                    running: false,
                    manifest: None,
                    manifest_error: None,
                    status: None,
                    error: Some(error.to_string()),
                }],
            }),
        )
    }

    #[tool(
        name = "workspace_open_viewer",
        description = "Ensure the small host-visible Agent Workspace GPUI viewer is open for a workspace id when this MCP process is not headless and workspace_doctor reports ready_for_host_viewer=true. If a compatible registered viewer for that workspace is already alive, this reuses it instead of opening a second window; input_forwarding=true may open a separate input-capable viewer when only a read-only monitor exists. By default it does not request always-on-top state; set always_on_top=true only when the user or host explicitly wants overlay/above behavior. Set input_forwarding=true only when the user explicitly wants a viewer that can arm manual mouse/keyboard/paste forwarding; forwarding still starts disabled and requires an in-viewer confirmation. This intentionally launches a separate target-bound viewer child process outside the MCP stdio server so the user can inspect and control the hidden workspace from a gentle local monitor window; the MCP-opened viewer exits once its selected workspace runtime is removed. If the MCP was started with --headless or has no host display, this tool refuses instead of opening a host-visible window.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    fn workspace_open_viewer(
        &self,
        Parameters(params): Parameters<WorkspaceOpenViewerParams>,
    ) -> Json<WorkspaceOpenViewerResponse> {
        if self.headless {
            let message = "workspace_open_viewer is disabled because this MCP process was started with --headless".to_string();
            return Json(WorkspaceOpenViewerResponse {
                ok: false,
                recovery_hints: recovery_hints_for_message(&message),
                agent_mode: Some(build_agent_mode_summary(self.headless)),
                target_handles: None,
                message,
                launch: None,
            });
        }
        let doctor = workspace::doctor_report();
        if !doctor.ready_for_host_viewer {
            let message = format!(
                "workspace_open_viewer cannot open a host-visible viewer in this environment: {}",
                doctor.viewer_blockers.join("; ")
            );
            return Json(WorkspaceOpenViewerResponse {
                ok: false,
                recovery_hints: recovery_hints_for_message(&message),
                agent_mode: Some(build_agent_mode_summary(self.headless)),
                target_handles: None,
                message,
                launch: None,
            });
        }
        Json(
            match viewer::open_viewer(
                params.id,
                &self.permissions,
                params.always_on_top,
                params.input_forwarding,
            ) {
                Ok(launch) => WorkspaceOpenViewerResponse {
                    ok: true,
                    message: if launch.reused {
                        "workspace viewer already open".to_string()
                    } else {
                        "workspace viewer opened".to_string()
                    },
                    target_handles: Some(viewer_target_handles(&launch.id)),
                    agent_mode: Some(build_agent_mode_summary(self.headless)),
                    recovery_hints: Vec::new(),
                    launch: Some(launch),
                },
                Err(error) => {
                    let message = error.to_string();
                    WorkspaceOpenViewerResponse {
                        ok: false,
                        recovery_hints: recovery_hints_for_message(&message),
                        agent_mode: Some(build_agent_mode_summary(self.headless)),
                        target_handles: None,
                        message,
                        launch: None,
                    }
                }
            },
        )
    }

    #[tool(
        name = "workspace_list_viewers",
        description = "List GPUI viewer processes registered by this agent-workspace-linux runtime, including stale registry rows. This is repo-owned viewer state and does not rely on desktop-compositor window discovery.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workspace_list_viewers(&self) -> Json<viewer::ViewerList> {
        Json(
            viewer::list_viewers().unwrap_or_else(|_error| viewer::ViewerList {
                registry_dir: std::path::PathBuf::new(),
                viewers: vec![viewer::ViewerListEntry {
                    id: "error".to_string(),
                    viewer_id: "error".to_string(),
                    pid: 0,
                    backend: "error".to_string(),
                    always_on_top: false,
                    input_forwarding: false,
                    exit_when_workspace_gone: false,
                    executable: std::path::PathBuf::new(),
                    command: Vec::new(),
                    opened_at_unix: 0,
                    registry_path: std::path::PathBuf::new(),
                    alive: false,
                }],
            }),
        )
    }

    #[tool(
        name = "workspace_close_viewer",
        description = "Close registered GPUI viewer processes by workspace id, or all registered viewers with all=true. Only pids whose command line still matches the viewer registry entry are signaled. Set dry_run=true to preview without signaling or removing stale registry files.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    fn workspace_close_viewer(
        &self,
        Parameters(params): Parameters<WorkspaceCloseViewerParams>,
    ) -> Json<WorkspaceCloseViewerResponse> {
        match viewer::close_viewers(params.id, params.all, params.dry_run) {
            Ok(close) => Json(WorkspaceCloseViewerResponse {
                ok: true,
                message: if close.dry_run {
                    "viewer close preview complete".to_string()
                } else {
                    "viewer close complete".to_string()
                },
                target_handles: Some(viewer_close_target_handles(&close)),
                agent_mode: Some(build_agent_mode_summary(self.headless)),
                recovery_hints: Vec::new(),
                close: Some(close),
            }),
            Err(error) => {
                let message = error.to_string();
                Json(WorkspaceCloseViewerResponse {
                    ok: false,
                    recovery_hints: recovery_hints_for_message(&message),
                    agent_mode: Some(build_agent_mode_summary(self.headless)),
                    target_handles: None,
                    message,
                    close: None,
                })
            }
        }
    }

    #[tool(
        name = "workspace_cleanup_stale",
        description = "Remove stale isolated workspace runtime directories and best-effort terminate manifest-recorded orphan app process groups, X server, window manager, and daemon PIDs when their identity can be verified. Running workspaces are skipped. Set dry_run=true to return removable candidates plus process_cleanup actions without deleting files or signaling processes.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workspace_cleanup_stale(
        &self,
        Parameters(params): Parameters<WorkspaceCleanupParams>,
    ) -> Json<workspace::WorkspaceCleanup> {
        Json(
            self.enforce_agent_mutation_unless_dry_run(params.dry_run, "workspace_cleanup_stale")
                .and_then(|_| workspace::cleanup_stale_workspaces(params.id, params.dry_run))
                .unwrap_or_else(|error| workspace::WorkspaceCleanup {
                    runtime_base_dir: std::path::PathBuf::new(),
                    dry_run: params.dry_run,
                    candidates: Vec::new(),
                    removed: Vec::new(),
                    skipped: vec![workspace::WorkspaceCleanupEntry {
                        id: "error".to_string(),
                        runtime_dir: std::path::PathBuf::new(),
                        reason: error.to_string(),
                        process_cleanup: Vec::new(),
                    }],
                }),
        )
    }

    #[tool(
        name = "workspace_launch_app",
        description = "Launch an optionally named app inside an isolated agent workspace. Set dry_run=true for a daemon-attached preview against an already running workspace daemon; it fails when the workspace is not running and never starts a daemon for you. Dry-run returns the command, cwd/env, profile policy, approval bundle, and mount/network isolation without spawning a process. The command runs with the workspace attachment environment, including DISPLAY, XAUTHORITY, AGENT_WORKSPACE_ID, AGENT_WORKSPACE_RUNTIME_DIR, and AGENT_WORKSPACE_SOCKET. Set wait_window=true to wait for the launched app's first visible window and return it in the same response. Set screenshot_window=true to also capture the first visible launched-app window; this implies waiting for a window. If a launch profile is provided, its cwd/env and mount/network policy apply to this app; set acknowledge_unenforced_policy=true if that launch profile requests policy that remains unenforced. Action responses return the directly affected app in the top-level apps field and keep nested status compact; use workspace_status or workspace_list_apps for the full app history. Prefer the returned apps[0].id as app_id for later window-targeted actions because GUI window titles often change after launch.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_launch_app(
        &self,
        Parameters(params): Parameters<WorkspaceLaunchParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .clone()
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        let wait_window = params.wait_window;
        let window_timeout_ms = params.window_timeout_ms;
        let screenshot_window = params.screenshot_window;
        let dry_run = params.dry_run;
        Json(
            self.result_response(params.into_launch_spec().and_then(|spec| {
                self.enforce_agent_mutation_unless_dry_run(dry_run, "workspace_launch_app")?;
                self.permissions.validate_launch_spec(&spec)?;
                if dry_run {
                    return workspace::preview_launch_app(
                        &id,
                        spec,
                        wait_window,
                        window_timeout_ms,
                        screenshot_window,
                    );
                }
                workspace::launch_app_with_options(
                    &id,
                    spec,
                    wait_window,
                    window_timeout_ms,
                    screenshot_window,
                )
            })),
        )
    }

    #[tool(
        name = "workspace_run_app",
        description = "Launch an optionally named app inside an isolated agent workspace, wait for it to exit or time out, optionally kill it on timeout, and return stdout/stderr logs in one response. Set dry_run=true for a daemon-attached preview against an already running workspace daemon; it fails when the workspace is not running and never starts a daemon for you. Dry-run returns launch/run options and an approval bundle without spawning a process. The command uses the same workspace attachment environment, optional cwd/env overrides, and optional launch profile policy as workspace_launch_app. Nested launch/wait/log responses keep status compact while the affected app and logs are returned in dedicated top-level fields.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_run_app(
        &self,
        Parameters(params): Parameters<WorkspaceRunParams>,
    ) -> Json<WorkspaceRunResult> {
        let id = params
            .id
            .clone()
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        let timeout_ms = params.timeout_ms;
        let tail_bytes = params.tail_bytes;
        let kill_on_timeout = params.kill_on_timeout;
        let dry_run = params.dry_run;
        Json(
            match params.into_launch_spec().and_then(|spec| {
                self.enforce_agent_mutation_unless_dry_run(dry_run, "workspace_run_app")?;
                self.permissions.validate_launch_spec(&spec)?;
                if dry_run {
                    return workspace::preview_run_app_with_spec(
                        &id,
                        spec,
                        timeout_ms,
                        tail_bytes,
                        kill_on_timeout,
                    )
                    .map(|preview| (None, Some(preview)));
                }
                workspace::run_app_with_spec(&id, spec, timeout_ms, tail_bytes, kill_on_timeout)
                    .map(|run| (Some(run), None))
            }) {
                Ok((run, preview)) => WorkspaceRunResult {
                    ok: true,
                    message: if let Some(run) = &run {
                        if run.succeeded {
                            "workspace app completed successfully".to_string()
                        } else if run.completed {
                            "workspace app completed with non-zero status".to_string()
                        } else if run.killed_on_timeout {
                            "workspace app timed out and was killed".to_string()
                        } else {
                            "workspace app did not complete before timeout".to_string()
                        }
                    } else {
                        "workspace run dry run returned".to_string()
                    },
                    target_handles: workspace_run_target_handles(run.as_ref(), preview.as_ref()),
                    agent_mode: Some(build_agent_mode_summary(self.headless)),
                    recovery_hints: Vec::new(),
                    run,
                    preview,
                },
                Err(error) => {
                    let message = error.to_string();
                    WorkspaceRunResult {
                        ok: false,
                        target_handles: None,
                        agent_mode: Some(build_agent_mode_summary(self.headless)),
                        recovery_hints: recovery_hints_for_message(&message),
                        message,
                        run: None,
                        preview: None,
                    }
                }
            },
        )
    }

    #[tool(
        name = "workspace_run_in_terminal",
        description = "Launch an xterm inside an isolated workspace backed by a per-workspace tmux socket. Returns a terminal_id, tmux pane target, pane tty, app_id, and window handles so TUI agents can use workspace_terminal_read and workspace_terminal_input instead of screenshots and coordinate guessing. The command is optional; when omitted tmux starts the default shell.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_run_in_terminal(
        &self,
        Parameters(params): Parameters<WorkspaceRunInTerminalParams>,
    ) -> Json<WorkspaceTerminalRunResult> {
        let id = params
            .id
            .clone()
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            match workspace::terminal_launch_plan(
                &id,
                params.terminal_id,
                params.title,
                params.terminal_program,
                params.command,
            )
            .and_then(|plan| {
                self.enforce_agent_mutation("workspace_run_in_terminal")?;
                self.permissions.validate_launch_spec(&plan.spec)?;
                workspace::run_in_terminal(
                    &id,
                    plan,
                    params.wait_window.unwrap_or(true),
                    params.window_timeout_ms,
                    params.timeout_ms,
                )
            }) {
                Ok((launch, terminal)) => {
                    let app = launch.apps.as_ref().and_then(|apps| apps.first()).cloned();
                    WorkspaceTerminalRunResult {
                        ok: true,
                        message: "workspace terminal launched with tmux text control".to_string(),
                        target_handles: Some(terminal_target_handles(
                            &terminal,
                            app.as_ref(),
                            &launch.windows,
                        )),
                        agent_mode: Some(build_agent_mode_summary(self.headless)),
                        recovery_hints: Vec::new(),
                        windows: launch.windows.unwrap_or_default(),
                        app,
                        terminal: Some(terminal),
                    }
                }
                Err(error) => {
                    let message = error.to_string();
                    WorkspaceTerminalRunResult {
                        ok: false,
                        message: message.clone(),
                        terminal: None,
                        app: None,
                        windows: Vec::new(),
                        target_handles: None,
                        agent_mode: Some(build_agent_mode_summary(self.headless)),
                        recovery_hints: recovery_hints_for_message(&message),
                    }
                }
            },
        )
    }

    #[tool(
        name = "workspace_terminal_read",
        description = "Read the current text grid from a workspace terminal launched by workspace_run_in_terminal. This uses tmux capture-pane through the terminal_id handle, so TUI state is exact text instead of a PNG screenshot. Set preserve_trailing_spaces=true when fixed-width board layout matters.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_terminal_read(
        &self,
        Parameters(params): Parameters<WorkspaceTerminalReadParams>,
    ) -> Json<WorkspaceTerminalReadResult> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            match workspace::read_terminal(&id, params.terminal_id, params.preserve_trailing_spaces)
            {
                Ok(screen) => WorkspaceTerminalReadResult {
                    ok: true,
                    message: "workspace terminal text returned".to_string(),
                    target_handles: Some(terminal_screen_target_handles(&screen)),
                    agent_mode: Some(build_agent_mode_summary(self.headless)),
                    recovery_hints: Vec::new(),
                    screen: Some(screen),
                },
                Err(error) => {
                    let message = error.to_string();
                    WorkspaceTerminalReadResult {
                        ok: false,
                        message: message.clone(),
                        screen: None,
                        target_handles: None,
                        agent_mode: Some(build_agent_mode_summary(self.headless)),
                        recovery_hints: recovery_hints_for_message(&message),
                    }
                }
            },
        )
    }

    #[tool(
        name = "workspace_terminal_input",
        description = "Send literal text and/or a batch of keys to a workspace terminal launched by workspace_run_in_terminal. This writes through the tmux pane target, bypassing window-manager focus. Keys use tmux send-keys grammar: Enter/Return, Escape/Esc, Tab, Space, Backspace/BSpace, Delete, Up/Down/Left/Right, ctrl+c or C-c, plus tmux names such as Home, End, PageUp, PageDown, and F1. Optional delay_ms inserts a bounded pause between keys.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_terminal_input(
        &self,
        Parameters(params): Parameters<WorkspaceTerminalInputParams>,
    ) -> Json<WorkspaceTerminalInputResult> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            match self
                .enforce_agent_mutation("workspace_terminal_input")
                .and_then(|_| {
                    workspace::terminal_input(
                        &id,
                        params.terminal_id,
                        params.keys,
                        params.text,
                        params.delay_ms,
                    )
                }) {
                Ok(input) => WorkspaceTerminalInputResult {
                    ok: true,
                    message: "workspace terminal input sent".to_string(),
                    target_handles: Some(terminal_input_target_handles(&input)),
                    agent_mode: Some(build_agent_mode_summary(self.headless)),
                    recovery_hints: Vec::new(),
                    input: Some(input),
                },
                Err(error) => {
                    let message = error.to_string();
                    WorkspaceTerminalInputResult {
                        ok: false,
                        message: message.clone(),
                        input: None,
                        target_handles: None,
                        agent_mode: Some(build_agent_mode_summary(self.headless)),
                        recovery_hints: recovery_hints_for_message(&message),
                    }
                }
            },
        )
    }

    #[tool(
        name = "workspace_list_apps",
        description = "List apps launched inside an isolated agent workspace, optionally filtering by app id/pid/name, app name substring, command substring, profile id, or running/stopped state. Falls back to the saved app snapshot when the workspace daemon has stopped.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workspace_list_apps(
        &self,
        Parameters(params): Parameters<WorkspaceListAppsParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(self.result_response(workspace::list_apps(
            &id,
            params.app_id,
            params.name_contains,
            params.command_contains,
            params.profile_id,
            params.running,
        )))
    }

    #[tool(
        name = "workspace_open_browser",
        description = "Launch Chrome/Chromium inside an already-running isolated workspace with the correct workspace-owned browser-control flags: --user-data-dir, --remote-debugging-address=127.0.0.1, and --remote-debugging-port=0. Defaults to a disposable per-workspace browser profile and about:blank, waits for the first window, then returns app_id and browser_target_id handles. Use this before workspace_browser_navigate/snapshot/click so agents do not need to shell out or hand-build Chrome flags.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    fn workspace_open_browser(
        &self,
        Parameters(params): Parameters<WorkspaceOpenBrowserParams>,
    ) -> Json<browser::WorkspaceBrowserOpen> {
        let id = params
            .id
            .clone()
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        let url = params
            .url
            .clone()
            .unwrap_or_else(|| "about:blank".to_string());
        Json(
            self.decorate_browser_open(
                (|| -> Result<browser::WorkspaceBrowserOpen> {
                    self.enforce_agent_mutation("workspace_open_browser")?;
                    let plan = browser::workspace_browser_open_plan(
                        &id,
                        params.browser_path,
                        params.user_data_dir,
                        params.url,
                    )?;
                    self.permissions.validate_launch_spec(&plan.spec)?;
                    browser::workspace_open_browser(
                        &id,
                        plan,
                        params.wait_window.unwrap_or(true),
                        params.window_timeout_ms,
                        params.timeout_ms,
                    )
                })()
                .unwrap_or_else(|error| browser::WorkspaceBrowserOpen::error(id, url, error)),
            ),
        )
    }

    #[tool(
        name = "workspace_browser_targets",
        description = "Read-only discovery for a Chrome/Chromium browser launched inside an isolated workspace. It derives the DevTools endpoint from the running workspace app's --user-data-dir and DevToolsActivePort file, maps /workspace browser-profile mounts back to their host copy when needed, and returns Chrome DevTools page targets. Use this for workspace-owned browser control instead of attaching to the user's host Chrome bridge.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_browser_targets(
        &self,
        Parameters(params): Parameters<WorkspaceBrowserTargetsParams>,
    ) -> Json<browser::WorkspaceBrowserTargets> {
        let id = params
            .id
            .clone()
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.decorate_browser_targets(
                browser::workspace_browser_targets(
                    &id,
                    params.app_id,
                    params.user_data_dir,
                    params.timeout_ms,
                )
                .unwrap_or_else(|error| browser::WorkspaceBrowserTargets::error(id, error)),
            ),
        )
    }

    #[tool(
        name = "workspace_browser_snapshot",
        description = "Read the current DOM text, title, URL, headings, and links from a Chrome/Chromium page target launched inside the isolated workspace. This uses the workspace-owned Chrome DevTools target discovered from the running workspace browser app, not the host Chrome bridge, and records a metadata-only browser_snapshot workspace event without raw DOM text, headings, or links.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_browser_snapshot(
        &self,
        Parameters(params): Parameters<WorkspaceBrowserSnapshotParams>,
    ) -> Json<browser::WorkspaceBrowserSnapshot> {
        let id = params
            .id
            .clone()
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.decorate_browser_snapshot(
                browser::workspace_browser_snapshot(
                    &id,
                    params.app_id,
                    params.user_data_dir,
                    params.target_id,
                    params.title_contains,
                    params.url_contains,
                    params.max_text_chars,
                    params.timeout_ms,
                )
                .unwrap_or_else(|error| browser::WorkspaceBrowserSnapshot::error(id, error)),
            ),
        )
    }

    #[tool(
        name = "workspace_browser_search_results",
        description = "Extract structured product/search result cards from a Chrome/Chromium page target launched inside the isolated workspace. This uses the workspace-owned Chrome DevTools target, returns titles, prices, visible VRAM/memory GB, ratings, availability, delivery snippets, and links when present, can filter by min_vram_gb for GPU shopping, and records a metadata-only browser_search_results workspace event without raw result text or links.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_browser_search_results(
        &self,
        Parameters(params): Parameters<WorkspaceBrowserSearchResultsParams>,
    ) -> Json<browser::WorkspaceBrowserSearchResults> {
        let id = params
            .id
            .clone()
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.decorate_browser_search_results(
                browser::workspace_browser_search_results(
                    &id,
                    params.app_id,
                    params.user_data_dir,
                    params.target_id,
                    params.title_contains,
                    params.url_contains,
                    params.max_results,
                    params.min_vram_gb,
                    params.timeout_ms,
                )
                .unwrap_or_else(|error| browser::WorkspaceBrowserSearchResults::error(id, error)),
            ),
        )
    }

    #[tool(
        name = "workspace_browser_navigate",
        description = "Navigate a Chrome/Chromium page target launched inside the isolated workspace to an http(s), data:, or about:blank URL using the workspace-owned Chrome DevTools connection. This is workspace browser control, not host Chrome control; it is blocked while live MCP control is read_only or paused and records a browser_navigate workspace event.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    fn workspace_browser_navigate(
        &self,
        Parameters(params): Parameters<WorkspaceBrowserNavigateParams>,
    ) -> Json<browser::WorkspaceBrowserNavigate> {
        let id = params
            .id
            .clone()
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        let url = params.url.clone();
        Json(
            self.decorate_browser_navigate(
                (|| -> Result<browser::WorkspaceBrowserNavigate> {
                    self.enforce_agent_mutation("workspace_browser_navigate")?;
                    browser::workspace_browser_navigate(
                        &id,
                        params.app_id,
                        params.user_data_dir,
                        params.target_id,
                        params.title_contains,
                        params.url_contains,
                        params.url,
                        params.wait_ms,
                        params.snapshot,
                        params.max_text_chars,
                        params.timeout_ms,
                    )
                })()
                .unwrap_or_else(|error| browser::WorkspaceBrowserNavigate::error(id, url, error)),
            ),
        )
    }

    #[tool(
        name = "workspace_browser_click",
        description = "Click inside a workspace-owned Chrome/Chromium page target through Chrome DevTools. Target by CSS selector, visible text, or viewport_x/viewport_y page coordinates; viewport coordinates are page viewport-relative, not window/screenshot coordinates, so agents do not subtract browser toolbar height. This mutates only the isolated workspace browser, records a metadata-only browser_click event, and can return a post-click page snapshot.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    fn workspace_browser_click(
        &self,
        Parameters(params): Parameters<WorkspaceBrowserClickParams>,
    ) -> Json<browser::WorkspaceBrowserClick> {
        let id = params
            .id
            .clone()
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.decorate_browser_click(
                (|| -> Result<browser::WorkspaceBrowserClick> {
                    self.enforce_agent_mutation("workspace_browser_click")?;
                    browser::workspace_browser_click(
                        &id,
                        params.app_id,
                        params.user_data_dir,
                        params.target_id,
                        params.title_contains,
                        params.url_contains,
                        params.selector,
                        params.text,
                        params.viewport_x,
                        params.viewport_y,
                        params.wait_ms,
                        params.snapshot,
                        params.max_text_chars,
                        params.timeout_ms,
                    )
                })()
                .unwrap_or_else(|error| browser::WorkspaceBrowserClick::error(id, error)),
            ),
        )
    }

    #[tool(
        name = "workspace_list_windows",
        description = "List windows inside an isolated agent workspace, optionally filtering by title/class/pid/app. By default this returns visible windows; set include_hidden=true to include minimized/hidden windows with visibility metadata.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workspace_list_windows(
        &self,
        Parameters(params): Parameters<WorkspaceListWindowsParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(self.result_response(workspace::list_windows(
            &id,
            params.include_hidden,
            params.title_contains,
            params.class_contains,
            params.pid,
            params.app_id,
        )))
    }

    #[tool(
        name = "workspace_active_window",
        description = "Report the currently active visible window inside an isolated agent workspace.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workspace_active_window(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(self.result_response(workspace::active_window(&id)))
    }

    #[tool(
        name = "workspace_pointer",
        description = "Report the current pointer coordinates inside an isolated agent workspace.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workspace_pointer(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(self.result_response(workspace::pointer(&id)))
    }

    #[tool(
        name = "workspace_observe",
        description = "Return status, windows, active window, pointer coordinates, optional root screenshot, and optional recent/incremental events for an isolated agent workspace. By default windows are visible-only; set include_hidden=true to include minimized/hidden windows. When screenshot=true and output_path is omitted, repeated live observation reuses observe-frame.png instead of accumulating timestamped screenshots.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_observe(
        &self,
        Parameters(params): Parameters<WorkspaceObserveParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(self.result_response(workspace::observe(
            &id,
            params.screenshot,
            params.include_hidden,
            params.output_path,
            params.events,
            params.events_tail,
            params.events_since_sequence,
        )))
    }

    #[tool(
        name = "workspace_wait_window",
        description = "Wait for a visible window inside an isolated agent workspace, optionally filtered by title substring, pid, or launched app id/pid.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_wait_window(
        &self,
        Parameters(params): Parameters<WorkspaceWaitWindowParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(self.result_response(workspace::wait_window(
            &id,
            params.title_contains,
            params.class_contains,
            params.pid,
            params.app_id,
            params.timeout_ms,
        )))
    }

    #[tool(
        name = "workspace_screenshot",
        description = "Capture a screenshot of the isolated agent workspace root display and return the PNG path.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_screenshot(
        &self,
        Parameters(params): Parameters<WorkspaceScreenshotParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(self.result_response(workspace::screenshot(&id, params.output_path)))
    }

    #[tool(
        name = "workspace_screenshot_window",
        description = "Capture a screenshot of a specific visible window inside an isolated agent workspace, targeted by X11 window id or by title/class/pid/app filters.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_screenshot_window(
        &self,
        Parameters(params): Parameters<WorkspaceScreenshotWindowParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(self.result_response(workspace::screenshot_window(
            &id,
            params.window_id,
            params.title_contains,
            params.class_contains,
            params.pid,
            params.app_id,
            params.output_path,
            params.timeout_ms,
        )))
    }

    #[tool(
        name = "workspace_focus_window",
        description = "Focus a visible window inside an isolated agent workspace by X11 window id. Response includes the focused window and active_window when focus can be resolved after the action.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workspace_focus_window(
        &self,
        Parameters(params): Parameters<WorkspaceWindowTargetParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation("workspace_focus_window")
                    .and_then(|_| workspace::focus_window(&id, params.window_id)),
            ),
        )
    }

    #[tool(
        name = "workspace_focus_matching_window",
        description = "Wait for and focus a visible window inside an isolated agent workspace, filtered by title substring, class substring, pid, or launched app id/pid. Response includes the matched window and active_window when focus can be resolved after the action.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workspace_focus_matching_window(
        &self,
        Parameters(params): Parameters<WorkspaceWaitWindowParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation("workspace_focus_matching_window")
                    .and_then(|_| {
                        workspace::focus_matching_window(
                            &id,
                            params.title_contains,
                            params.class_contains,
                            params.pid,
                            params.app_id,
                            params.timeout_ms,
                        )
                    }),
            ),
        )
    }

    #[tool(
        name = "workspace_close_window",
        description = "Request that a window inside an isolated agent workspace close by X11 window id. Response includes the window record that was targeted for close. Set dry_run=true to resolve and return the targeted window without closing it.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_close_window(
        &self,
        Parameters(params): Parameters<WorkspaceCloseWindowParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation_unless_dry_run(
                    params.dry_run,
                    "workspace_close_window",
                )
                .and_then(|_| workspace::close_window(&id, params.window_id, params.dry_run)),
            ),
        )
    }

    #[tool(
        name = "workspace_close_matching_window",
        description = "Wait for and request close of a visible window inside an isolated agent workspace, filtered by title substring, class substring, pid, or launched app id/pid. Set dry_run=true to resolve and return the matched window without closing it.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_close_matching_window(
        &self,
        Parameters(params): Parameters<WorkspaceCloseMatchingWindowParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation_unless_dry_run(
                    params.dry_run,
                    "workspace_close_matching_window",
                )
                .and_then(|_| {
                    workspace::close_matching_window(
                        &id,
                        params.title_contains,
                        params.class_contains,
                        params.pid,
                        params.app_id,
                        params.timeout_ms,
                        params.dry_run,
                    )
                }),
            ),
        )
    }

    #[tool(
        name = "workspace_move_window",
        description = "Move a visible window inside an isolated agent workspace by X11 id or by title/class/pid/app filters. Coordinates are workspace-local top-left pixels.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_move_window(
        &self,
        Parameters(params): Parameters<WorkspaceMoveWindowParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation("workspace_move_window")
                    .and_then(|_| {
                        workspace::move_window(
                            &id,
                            params.window_id,
                            params.title_contains,
                            params.class_contains,
                            params.pid,
                            params.app_id,
                            params.x,
                            params.y,
                            params.timeout_ms,
                        )
                    }),
            ),
        )
    }

    #[tool(
        name = "workspace_resize_window",
        description = "Resize a visible window inside an isolated agent workspace by X11 id or by title/class/pid/app filters. Size is in pixels.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_resize_window(
        &self,
        Parameters(params): Parameters<WorkspaceResizeWindowParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation("workspace_resize_window")
                    .and_then(|_| {
                        workspace::resize_window(
                            &id,
                            params.window_id,
                            params.title_contains,
                            params.class_contains,
                            params.pid,
                            params.app_id,
                            params.width,
                            params.height,
                            params.timeout_ms,
                        )
                    }),
            ),
        )
    }

    #[tool(
        name = "workspace_raise_window",
        description = "Raise a visible window inside an isolated agent workspace above other windows, targeted by X11 id or title/class/pid/app filters.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_raise_window(
        &self,
        Parameters(params): Parameters<WorkspaceTargetedWindowParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation("workspace_raise_window")
                    .and_then(|_| {
                        workspace::raise_window(
                            &id,
                            params.window_id,
                            params.title_contains,
                            params.class_contains,
                            params.pid,
                            params.app_id,
                            params.timeout_ms,
                        )
                    }),
            ),
        )
    }

    #[tool(
        name = "workspace_minimize_window",
        description = "Minimize a visible window inside an isolated agent workspace, targeted by X11 id or title/class/pid/app filters. Response includes the refreshed window record after minimization.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_minimize_window(
        &self,
        Parameters(params): Parameters<WorkspaceTargetedWindowParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation("workspace_minimize_window")
                    .and_then(|_| {
                        workspace::minimize_window(
                            &id,
                            params.window_id,
                            params.title_contains,
                            params.class_contains,
                            params.pid,
                            params.app_id,
                            params.timeout_ms,
                        )
                    }),
            ),
        )
    }

    #[tool(
        name = "workspace_show_window",
        description = "Show/map a minimized workspace window by X11 window id or title/class/pid/app filters. Match filters include hidden windows so minimized apps can be restored directly.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_show_window(
        &self,
        Parameters(params): Parameters<WorkspaceTargetedWindowParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation("workspace_show_window")
                    .and_then(|_| {
                        workspace::show_window(
                            &id,
                            params.window_id,
                            params.title_contains,
                            params.class_contains,
                            params.pid,
                            params.app_id,
                            params.timeout_ms,
                        )
                    }),
            ),
        )
    }

    #[tool(
        name = "workspace_click",
        description = "Click workspace-local coordinates inside an isolated agent workspace, optionally setting button and repeat count. Response includes pointer and active_window when focus can be resolved after the action.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_click(
        &self,
        Parameters(params): Parameters<WorkspaceClickParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation("workspace_click")
                    .and_then(|_| {
                        workspace::click(&id, params.x, params.y, params.button, params.count)
                    }),
            ),
        )
    }

    #[tool(
        name = "workspace_click_window",
        description = "Click a coordinate relative to a visible window inside an isolated agent workspace, targeted by X11 window id or by title/class/pid/app filters, optionally setting button and repeat count. Response includes pointer, target window, and active_window when focus can be resolved after the action.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_click_window(
        &self,
        Parameters(params): Parameters<WorkspaceClickWindowParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation("workspace_click_window")
                    .and_then(|_| {
                        workspace::click_window(
                            &id,
                            params.window_id,
                            params.title_contains,
                            params.class_contains,
                            params.pid,
                            params.app_id,
                            params.x,
                            params.y,
                            params.button,
                            params.count,
                            params.timeout_ms,
                        )
                    }),
            ),
        )
    }

    #[tool(
        name = "workspace_move_pointer",
        description = "Move the pointer to workspace-local coordinates inside an isolated agent workspace without clicking. Response includes pointer and active_window when focus can be resolved after the action.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workspace_move_pointer(
        &self,
        Parameters(params): Parameters<WorkspacePointerParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation("workspace_move_pointer")
                    .and_then(|_| workspace::move_pointer(&id, params.x, params.y)),
            ),
        )
    }

    #[tool(
        name = "workspace_move_pointer_window",
        description = "Move the pointer to coordinates relative to a visible window inside an isolated agent workspace, targeted by X11 window id or by title/class/pid/app filters, without clicking. Response includes pointer, target window, and active_window when focus can be resolved after the action.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workspace_move_pointer_window(
        &self,
        Parameters(params): Parameters<WorkspacePointerWindowParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation("workspace_move_pointer_window")
                    .and_then(|_| {
                        workspace::move_pointer_window(
                            &id,
                            params.window_id,
                            params.title_contains,
                            params.class_contains,
                            params.pid,
                            params.app_id,
                            params.x,
                            params.y,
                            params.timeout_ms,
                        )
                    }),
            ),
        )
    }

    #[tool(
        name = "workspace_drag",
        description = "Drag from one workspace-local coordinate to another inside an isolated agent workspace, optionally setting the mouse button. Response includes pointer and active_window when focus can be resolved after the action.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_drag(
        &self,
        Parameters(params): Parameters<WorkspaceDragParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(self.enforce_agent_mutation("workspace_drag").and_then(|_| {
                workspace::drag(
                    &id,
                    params.from_x,
                    params.from_y,
                    params.to_x,
                    params.to_y,
                    params.button,
                )
            })),
        )
    }

    #[tool(
        name = "workspace_drag_window",
        description = "Drag between coordinates relative to a visible window inside an isolated agent workspace, targeted by X11 window id or by title/class/pid/app filters, optionally setting the mouse button. Response includes pointer, target window, and active_window when focus can be resolved after the action.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_drag_window(
        &self,
        Parameters(params): Parameters<WorkspaceDragWindowParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation("workspace_drag_window")
                    .and_then(|_| {
                        workspace::drag_window(
                            &id,
                            params.window_id,
                            params.title_contains,
                            params.class_contains,
                            params.pid,
                            params.app_id,
                            params.from_x,
                            params.from_y,
                            params.to_x,
                            params.to_y,
                            params.button,
                            params.timeout_ms,
                        )
                    }),
            ),
        )
    }

    #[tool(
        name = "workspace_scroll",
        description = "Scroll at workspace-local coordinates inside an isolated agent workspace. Direction is up, down, left, or right; amount is wheel ticks. Response includes pointer and active_window when focus can be resolved after the action.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_scroll(
        &self,
        Parameters(params): Parameters<WorkspaceScrollParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation("workspace_scroll")
                    .and_then(|_| {
                        workspace::scroll(&id, params.x, params.y, params.direction, params.amount)
                    }),
            ),
        )
    }

    #[tool(
        name = "workspace_scroll_window",
        description = "Scroll at coordinates relative to a visible window inside an isolated agent workspace, targeted by X11 window id or by title/class/pid/app filters. Direction is up, down, left, or right; amount is wheel ticks. Response includes pointer, target window, and active_window when focus can be resolved after the action.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_scroll_window(
        &self,
        Parameters(params): Parameters<WorkspaceScrollWindowParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation("workspace_scroll_window")
                    .and_then(|_| {
                        workspace::scroll_window(
                            &id,
                            params.window_id,
                            params.title_contains,
                            params.class_contains,
                            params.pid,
                            params.app_id,
                            params.x,
                            params.y,
                            params.direction,
                            params.amount,
                            params.timeout_ms,
                        )
                    }),
            ),
        )
    }

    #[tool(
        name = "workspace_key",
        description = "Send a key chord to the isolated agent workspace with xdotool syntax, for example Return or ctrl+l. Response includes active_window when focus can be resolved after the action.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_key(
        &self,
        Parameters(params): Parameters<WorkspaceKeyParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation("workspace_key")
                    .and_then(|_| workspace::key(&id, params.key)),
            ),
        )
    }

    #[tool(
        name = "workspace_key_window",
        description = "Send a key chord to a specific visible window inside an isolated agent workspace, targeted by X11 window id or by title/class/pid/app filters. Prefer app_id from workspace_launch_app when controlling a launched app because GUI titles can change. Response includes the target window and active_window when focus can be resolved after the action.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_key_window(
        &self,
        Parameters(params): Parameters<WorkspaceKeyWindowParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation("workspace_key_window")
                    .and_then(|_| {
                        workspace::key_window(
                            &id,
                            params.window_id,
                            params.title_contains,
                            params.class_contains,
                            params.pid,
                            params.app_id,
                            params.key,
                            params.timeout_ms,
                        )
                    }),
            ),
        )
    }

    #[tool(
        name = "workspace_type_text",
        description = "Type literal text into the focused app inside an isolated agent workspace. Response includes active_window when focus can be resolved after the action.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_type_text(
        &self,
        Parameters(params): Parameters<WorkspaceTypeTextParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation("workspace_type_text")
                    .and_then(|_| workspace::type_text(&id, params.text)),
            ),
        )
    }

    #[tool(
        name = "workspace_type_window",
        description = "Type literal text into a specific visible window inside an isolated agent workspace, targeted by X11 window id or by title/class/pid/app filters. Prefer app_id from workspace_launch_app when controlling a launched app because GUI titles can change. Response includes the target window and active_window when focus can be resolved after the action.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_type_window(
        &self,
        Parameters(params): Parameters<WorkspaceTypeWindowParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation("workspace_type_window")
                    .and_then(|_| {
                        workspace::type_window(
                            &id,
                            params.window_id,
                            params.title_contains,
                            params.class_contains,
                            params.pid,
                            params.app_id,
                            params.text,
                            params.timeout_ms,
                        )
                    }),
            ),
        )
    }

    #[tool(
        name = "workspace_set_clipboard",
        description = "Set the clipboard selection inside an isolated agent workspace. Text is capped at 64 KiB. Event logs store only size metadata, not the raw clipboard text.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workspace_set_clipboard(
        &self,
        Parameters(params): Parameters<WorkspaceClipboardSetParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation("workspace_set_clipboard")
                    .and_then(|_| workspace::set_clipboard(&id, params.text)),
            ),
        )
    }

    #[tool(
        name = "workspace_get_clipboard",
        description = "Read the clipboard selection inside an isolated agent workspace.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_get_clipboard(
        &self,
        Parameters(params): Parameters<WorkspaceIdParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(self.result_response(workspace::get_clipboard(&id)))
    }

    #[tool(
        name = "workspace_paste_text",
        description = "Set the isolated workspace clipboard to text, then send a paste key chord to the focused app. Text is capped at 64 KiB. Defaults to ctrl+v. Response includes active_window when focus can be resolved after the action. Event logs store only size metadata, not the raw text.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_paste_text(
        &self,
        Parameters(params): Parameters<WorkspacePasteTextParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation("workspace_paste_text")
                    .and_then(|_| workspace::paste_text(&id, params.text, params.key)),
            ),
        )
    }

    #[tool(
        name = "workspace_paste_window",
        description = "Set the isolated workspace clipboard to text, focus a visible window by X11 id/title/class/pid/app filter, then send a paste key chord. Text is capped at 64 KiB. Prefer app_id from workspace_launch_app when controlling a launched app because GUI titles can change. Defaults to ctrl+v. Response includes the target window and active_window when focus can be resolved after the action. Event logs store only size metadata, not the raw text.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_paste_window(
        &self,
        Parameters(params): Parameters<WorkspacePasteWindowParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation("workspace_paste_window")
                    .and_then(|_| {
                        workspace::paste_window(
                            &id,
                            params.window_id,
                            params.title_contains,
                            params.class_contains,
                            params.pid,
                            params.app_id,
                            params.text,
                            params.key,
                            params.timeout_ms,
                        )
                    }),
            ),
        )
    }

    #[tool(
        name = "workspace_read_app_log",
        description = "Read stdout or stderr captured from an app launched inside an isolated agent workspace, falling back to saved log paths when the workspace daemon has stopped.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workspace_read_app_log(
        &self,
        Parameters(params): Parameters<WorkspaceReadAppLogParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(self.result_response(workspace::read_app_log(
            &id,
            params.app_id,
            params.stream.unwrap_or_else(|| "stdout".to_string()),
            params.tail_bytes,
        )))
    }

    #[tool(
        name = "workspace_wait_app",
        description = "Wait until an app launched inside an isolated agent workspace exits, or until timeout_ms elapses. Defaults to 30000ms. Set kill_on_timeout=true to terminate the app process group when the timeout elapses.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_wait_app(
        &self,
        Parameters(params): Parameters<WorkspaceWaitAppParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation_unless_dry_run(
                    !params.kill_on_timeout,
                    "workspace_wait_app kill_on_timeout",
                )
                .and_then(|_| {
                    workspace::wait_app(
                        &id,
                        params.app_id,
                        params.timeout_ms,
                        params.kill_on_timeout,
                    )
                }),
            ),
        )
    }

    #[tool(
        name = "workspace_events",
        description = "Read the workspace-local IPC event log, falling back to saved event history when the workspace daemon has stopped. Use since_sequence to poll events after a previously seen sequence number. Sensitive typed text, clipboard/paste text, browser DOM text, and browser result-card text are recorded only as metadata.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workspace_events(
        &self,
        Parameters(params): Parameters<WorkspaceEventsParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(self.result_response(workspace::read_events(
            &id,
            params.tail,
            params.since_sequence,
        )))
    }

    #[tool(
        name = "workspace_run_profile_setup",
        description = "Launch setup commands declared by a saved profile inside an already running isolated workspace. Set dry_run=true for daemon-attached setup previews; it fails when the workspace is not running and never starts a daemon for you. Dry-run returns one launch preview per setup command without spawning processes. Set wait=true or timeout_ms to supervise setup in sequence and report completion/success; set kill_on_timeout=true to terminate timed-out setup commands.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_run_profile_setup(
        &self,
        Parameters(params): Parameters<WorkspaceSetupParams>,
    ) -> Json<ProfileSetupResult> {
        let profile_id = params.profile.clone();
        Json(
            match profile::get_profile(&profile_id).and_then(|profile| {
                self.enforce_agent_mutation_unless_dry_run(
                    params.dry_run,
                    "workspace_run_profile_setup",
                )?;
                self.permissions.validate_profile(&profile)?;
                profile::launch_profile_setup(
                    &params
                        .id
                        .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string()),
                    &profile_id,
                    profile::ProfileSetupOptions {
                        dry_run: params.dry_run,
                        wait: params.wait || params.timeout_ms.is_some(),
                        timeout_ms: params.timeout_ms,
                        kill_on_timeout: params.kill_on_timeout,
                        acknowledge_unenforced_policy: params.acknowledge_unenforced_policy,
                    },
                )
            }) {
                Ok(run) => ProfileSetupResult {
                    ok: true,
                    message: if run.dry_run {
                        "profile setup dry run returned".to_string()
                    } else if run.succeeded == Some(true) {
                        "profile setup completed successfully".to_string()
                    } else if run.completed == Some(true) {
                        "profile setup completed with failures".to_string()
                    } else if run.wait {
                        "profile setup did not complete before timeout".to_string()
                    } else {
                        "profile setup launched".to_string()
                    },
                    agent_mode: Some(build_agent_mode_summary(self.headless)),
                    recovery_hints: Vec::new(),
                    run: Some(run),
                },
                Err(error) => {
                    let message = error.to_string();
                    ProfileSetupResult {
                        ok: false,
                        agent_mode: Some(build_agent_mode_summary(self.headless)),
                        recovery_hints: recovery_hints_for_message(&message),
                        message,
                        run: None,
                    }
                }
            },
        )
    }

    #[tool(
        name = "workspace_launch_profile_apps",
        description = "Launch startup apps declared by a saved profile inside an already running isolated workspace. Set dry_run=true for daemon-attached startup previews; it fails when the workspace is not running and never starts a daemon for you. Dry-run returns one launch preview per startup app without spawning processes. Set wait_window=true to wait for each startup app's first visible window, or screenshot_window=true to also capture each first startup window.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    fn workspace_launch_profile_apps(
        &self,
        Parameters(params): Parameters<WorkspaceProfileLaunchParams>,
    ) -> Json<ProfileStartupResult> {
        let profile_id = params.profile.clone();
        Json(
            match profile::get_profile(&profile_id).and_then(|profile| {
                self.enforce_agent_mutation_unless_dry_run(
                    params.dry_run,
                    "workspace_launch_profile_apps",
                )?;
                self.permissions.validate_profile(&profile)?;
                profile::launch_profile_startup_apps(
                    &params
                        .id
                        .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string()),
                    &profile_id,
                    profile::ProfileStartupOptions {
                        dry_run: params.dry_run,
                        acknowledge_unenforced_policy: params.acknowledge_unenforced_policy,
                        wait_window: params.wait_window,
                        window_timeout_ms: params.window_timeout_ms,
                        screenshot_window: params.screenshot_window,
                    },
                )
            }) {
                Ok(run) => ProfileStartupResult {
                    ok: true,
                    message: if run.dry_run {
                        "profile startup apps dry run returned".to_string()
                    } else {
                        "profile startup apps launched".to_string()
                    },
                    agent_mode: Some(build_agent_mode_summary(self.headless)),
                    recovery_hints: Vec::new(),
                    run: Some(run),
                },
                Err(error) => {
                    let message = error.to_string();
                    ProfileStartupResult {
                        ok: false,
                        agent_mode: Some(build_agent_mode_summary(self.headless)),
                        recovery_hints: recovery_hints_for_message(&message),
                        message,
                        run: None,
                    }
                }
            },
        )
    }

    #[tool(
        name = "workspace_kill_app",
        description = "Terminate an app launched inside an isolated agent workspace by app id or pid. Set dry_run=true to resolve and return the matched app without terminating it.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workspace_kill_app(
        &self,
        Parameters(params): Parameters<WorkspaceKillAppParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(
            self.result_response(
                self.enforce_agent_mutation_unless_dry_run(params.dry_run, "workspace_kill_app")
                    .and_then(|_| workspace::kill_app(&id, params.app_id, params.dry_run)),
            ),
        )
    }

    #[tool(
        name = "workspace_stop",
        description = "Stop an isolated agent workspace, terminate apps launched inside it, return the apps stopped by the shutdown, and wait for the daemon IPC socket to close. Defaults to a 30000ms wait; set timeout_ms to override. Set dry_run=true to return currently running apps without stopping the workspace.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    fn workspace_stop(
        &self,
        Parameters(params): Parameters<WorkspaceStopParams>,
    ) -> Json<IpcResponse> {
        let id = params
            .id
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        Json(self.result_response(workspace::stop_workspace(
            &id,
            params.timeout_ms,
            params.dry_run,
        )))
    }
}

#[tool_handler(
    name = "agent-workspace-linux",
    instructions = "\
Use mcp_agent_context for one low-noise snapshot with active/read_only/paused, headless/no-host-display, viewer, app_id/window_id/viewer_id/browser_target_id handles, and recovery hints. Use mcp_permissions first when the host may have spawned this MCP with a permissions ceiling, mcp_control_state to check whether live user control has put the server in active, read_only, or paused mode, mcp_action_catalog when deciding whether a tool is read-only, idempotent, destructive, host-visible/open-world, or blocked by live control, mcp_session_brief when you need a condensed session summary with suggested next actions, and mcp_task_plan when the user intent is app QA, browser/shopping, observation, or cleanup and you need a safe read-only plan before calling mutating tools. \
If configured=true, any populated permission dimensions are an immutable spawn-time ceiling for profile, start, launch, setup, and startup actions; clients may only narrow those dimensions. If configured=true but restricted=false, the MCP has an explicit empty/open ceiling, so enforcement stays open while the configured state remains visible. If configured=false, the MCP does not impose its own ceiling; respect the host/client harness boundary and use mcp_action_catalog plus each tool's annotations and description to classify the action type before acting. \
Use mcp_control_update only when the user or controlling UI asks to switch active/read_only/paused; when reactivating from read_only or paused to active, pass confirmed_user_request=true only after explicit user or controlling UI approval. read_only and paused block mutating agent actions while preserving inspection and safety stop. \
workspace_start and workspace_open_profile are host-visible/open-world by default because non-headless sessions with workspace_doctor.ready_for_host_viewer=true auto-open the GPUI monitor; pass open_viewer=false only when the user or embedding host explicitly wants no monitor. \
Use workspace_open_browser to launch workspace-owned Chrome/Chromium with --user-data-dir and loopback DevTools flags before browser navigation, snapshots, search extraction, or browser clicks. Use workspace_browser_click for CSS selector, visible text, or page viewport-coordinate clicks. Use workspace_run_in_terminal for TUI apps, then workspace_terminal_read for exact pane text and workspace_terminal_input for batched focus-independent keys/text. \
workspace_open_viewer is host-visible and open-world; it can launch the GPUI monitor unless this MCP process was started with --headless or workspace_doctor reports ready_for_host_viewer=false, in which case it must run without host-visible UI. Use workspace_list_viewers and workspace_close_viewer for repo-owned GPUI viewer lifecycle control when compositor/window automation cannot see or close the viewer; workspace_close_viewer only signals registered viewer pids whose command line still matches the registry entry, and dry_run=true previews the close. Use workspace_guardrails to inspect acknowledgement, dry-run, explicit override, timeout-termination, and workspace-scope rules for UI approval flows. Use workspace_doctor to check runtime readiness, viewer host-display readiness, and optional policy backend candidates. Use profile_list/profile_get/profile_check/profile_validate/profile_template/profile_put/profile_import/profile_export/profile_delete to manage saved environment profiles. Use profile_validate to preflight a local JSON profile file without saving it. Use profile_export to return a saved profile and optionally write it to output_path; set replace=true only when intentionally overwriting an existing file. Use profile_put with dry_run=true to preview whether a profile would be created, replaced, or rejected without writing. Use profile_import when the UI has a local JSON file path instead of an already parsed profile object. Use profile_delete with dry_run=true to return the saved profile without deleting it. profile_template can generate starter JSON such as project-dev, restricted-chrome, and browser-session before saving with profile_put; restricted-chrome and browser-session intentionally expose their --no-sandbox browser commands for bubblewrap namespace compatibility. browser-session requires user_data_dir and is intended only for explicitly user-approved browser data directories. profile_check preflights acknowledgement requirements and unenforced policy warnings before workspace_start. Preview scope matters: workspace_start dry_run=true and workspace_open_profile dry_run=true are pre-daemon approval previews that do not create runtime state; workspace_launch_app, workspace_run_app, workspace_run_profile_setup, and workspace_launch_profile_apps dry_run=true are daemon-attached previews and require an already running workspace. Dry-run preview responses include an approval bundle when acknowledgement UI data is available. workspace_start requires acknowledge_hidden_workspace=true before creating a new hidden agent-controlled environment. Pass purpose when a human-readable reason should be shown in workspace_status and the start event. If a profile requests policy that remains unenforced, workspace_start also requires acknowledge_unenforced_policy=true. Mount profiles, disabled-network profiles, and local_only network profiles are enforced with bubblewrap when bubblewrap is available; local_only uses a loopback-only sandbox namespace and does not bridge host localhost services. The current product network modes are closed/disabled, local/local_only, and open/inherit_host; allowlist network profiles are advanced/legacy declared intent only and are not enforced by the X11 runtime. workspace_status reports live daemon state, including the applied profile policy snapshot, discovered backend candidates from start time, and enforcement state. Use workspace_manifest to inspect saved manifest state from disk for live or stopped workspaces without contacting the workspace daemon. Use workspace_artifacts to inventory saved runtime files such as manifest, event log, daemon logs, app logs, and screenshots when present; set existing_only=true when only present paths are needed. Use workspace_ipc_info to verify daemon IPC protocol metadata, transport, framing, encoding, and socket path. Use workspace_env to get DISPLAY, XAUTHORITY, runtime directory, and control socket values for external tools that need to attach to the hidden workspace. Use workspace_list to discover known/running workspaces and workspace_cleanup_stale with dry_run=true to preview unreachable runtime directories and verified orphan process cleanup before deletion. Use workspace_list_apps to inspect launched apps, including named apps and running/stopped state; it can read the saved app snapshot after a workspace has stopped. Use workspace_browser_targets after launching Chrome/Chromium with --remote-debugging-port=0 to discover workspace-owned page targets, workspace_browser_snapshot to read page title/text/links, and workspace_browser_navigate to change the workspace browser page without using the host Chrome bridge or external browser automation. App action and log-read responses include the directly affected app in the top-level apps field when available. Use workspace_open_profile to start a profile-backed workspace, optionally wait for setup, and open startup apps after setup succeeds in one call. Use workspace_start before launching apps manually. workspace_launch_profile_apps opens startup apps declared by the selected profile. workspace_run_app is the preferred one-shot helper for QA commands that should return stdout/stderr; set kill_on_timeout=true to terminate timed-out commands. workspace_wait_app also accepts kill_on_timeout=true to terminate an already launched app when its wait timeout elapses. workspace_launch_app and workspace_run_app accept optional names, workspace_list_apps can filter by app name or running pid or app_id, and named apps can be referenced anywhere an app target is accepted, including logs, waits, kill dry-runs, kills, and window app_id filters. workspace_launch_app, workspace_run_profile_setup, workspace_focus_window, workspace_focus_matching_window, workspace_close_window, workspace_close_matching_window, workspace_move_window, workspace_resize_window, workspace_raise_window, workspace_minimize_window, workspace_show_window, workspace_click, workspace_click_window, workspace_move_pointer, workspace_move_pointer_window, workspace_drag, workspace_drag_window, workspace_scroll, workspace_scroll_window, workspace_key, workspace_key_window, workspace_type_text, workspace_type_window, workspace_set_clipboard, workspace_get_clipboard, workspace_paste_text, and workspace_paste_window run only inside the isolated agent workspace; they do not target the user's host desktop. Use workspace_wait_window, workspace_active_window, workspace_pointer, workspace_observe, workspace_focus_matching_window, workspace_move_window, workspace_resize_window, workspace_raise_window, workspace_minimize_window, workspace_show_window, workspace_click_window, workspace_move_pointer_window, workspace_drag_window, workspace_scroll_window, workspace_key_window, workspace_type_window, or workspace_paste_window after launching GUI apps. Prefer window-targeted tools when acting on a specific app window rather than the workspace root or current focus. Window match filters accept title_contains, class_contains, pid, or app_id; class_contains matches wm_class and wm_instance. Use workspace_move_window, workspace_resize_window, workspace_raise_window, workspace_minimize_window, and workspace_show_window to arrange app windows before screenshots or repeated QA interactions. workspace_show_window match filters include hidden windows, so it can restore minimized apps by title/class/pid/app without first listing its raw X11 id. Use workspace_list_windows with title_contains, class_contains, pid, or app_id filters to inspect specific current app windows. Use workspace_list_windows or workspace_observe with include_hidden=true when a minimized or hidden app needs to be found again; returned windows include wm_class, wm_instance, and app_id when X11/process metadata is available. Use workspace_paste_text or workspace_paste_window when inserting long text is more reliable than synthetic typing. Use workspace_run_profile_setup with wait=true when setup command completion matters; set kill_on_timeout=true to clean up timed-out setup commands. Use workspace_status or workspace_observe when a full live app snapshot is useful. Use workspace_observe, workspace_screenshot, workspace_screenshot_window, workspace_list_apps, workspace_browser_targets, workspace_browser_snapshot, workspace_list_windows, workspace_active_window, workspace_pointer, workspace_wait_app, workspace_read_app_log, workspace_get_clipboard, and workspace_events to inspect the workspace before acting. For incremental event polling, pass workspace_events since_sequence with the last seen event sequence. workspace_screenshot_window captures a specific app window by id/title/class/pid/app filters. workspace_read_app_log can read saved stdout/stderr after a workspace has stopped when its manifest remains on disk. workspace_events records IPC activity without storing raw typed text, raw clipboard-set text, or raw pasted text, and can read saved event history after a workspace has stopped. workspace_close_window, workspace_close_matching_window, workspace_kill_app, workspace_minimize_window, and workspace_show_window affect only workspace-local windows/apps. workspace_close_window and workspace_close_matching_window with dry_run=true resolve the targeted window without closing it. workspace_kill_app with dry_run=true resolves the matched app without terminating it. workspace_stop with dry_run=true previews currently running apps without stopping; without dry_run it terminates the workspace and apps launched inside it, then waits for the daemon IPC socket to close."
)]
impl ServerHandler for AgentWorkspaceLinux {}

pub async fn serve_mcp(permissions: McpPermissionState, headless: bool) -> Result<()> {
    AgentWorkspaceLinux::new(permissions, headless)
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct McpControlUpdateParams {
    mode: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    confirmed_user_request: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpControlResponse {
    ok: bool,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status: Option<control::McpControlStatus>,
}

fn update_mcp_control_from_tool(
    params: McpControlUpdateParams,
) -> Result<control::McpControlStatus> {
    let requested_mode = McpControlMode::parse(&params.mode)?;
    let current = control::control_status()?;
    validate_control_reactivation_confirmation(
        current.state.mode,
        requested_mode,
        params.confirmed_user_request,
    )?;
    control::set_control_mode(requested_mode, "mcp_control_update", params.reason)
}

fn validate_control_reactivation_confirmation(
    current_mode: McpControlMode,
    requested_mode: McpControlMode,
    confirmed_user_request: bool,
) -> Result<()> {
    if requested_mode.allows_agent_mutation()
        && !current_mode.allows_agent_mutation()
        && !confirmed_user_request
    {
        bail!(
            "MCP control is {}; reactivating agent mutation requires confirmed_user_request=true after an explicit user or controlling UI request",
            current_mode.label()
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpSessionBrief {
    version: u32,
    headless: bool,
    permissions: McpPermissionState,
    control: McpSessionControlBrief,
    doctor: McpSessionDoctorBrief,
    profiles: McpSessionProfilesBrief,
    workspaces: McpSessionWorkspacesBrief,
    recommendations: Vec<McpRecommendedAction>,
    approval_summary: McpSessionApprovalSummary,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
struct McpAgentContextParams {
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    browser_timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpAgentContext {
    version: u32,
    mode: AgentModeSummary,
    permissions: McpPermissionState,
    workspace: McpAgentWorkspaceContext,
    viewers: Vec<McpAgentViewerContext>,
    handles: AgentTargetHandles,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    next_tools: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recovery_hints: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpAgentWorkspaceContext {
    id: String,
    running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    purpose: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile_id: Option<String>,
    app_count: usize,
    running_app_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_app_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    active_window: Option<McpAgentWindowRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    windows: Vec<McpAgentWindowRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    browser: Option<McpAgentBrowserContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpAgentWindowRef {
    window_id: String,
    title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    app_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    visible: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpAgentBrowserContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    app_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    targets: Vec<McpAgentBrowserTargetRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpAgentBrowserTargetRef {
    browser_target_id: String,
    title: String,
    url: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpAgentViewerContext {
    viewer_id: String,
    pid: u32,
    backend: String,
    alive: bool,
    always_on_top: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct McpTaskPlanParams {
    intent: String,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    profile_id: Option<String>,
    #[serde(default)]
    project_path: Option<PathBuf>,
    #[serde(default)]
    browser_path: Option<PathBuf>,
    #[serde(default)]
    user_data_dir: Option<PathBuf>,
    #[serde(default)]
    target_url: Option<String>,
    #[serde(default)]
    shopping_list: Option<String>,
    #[serde(default)]
    budget: Option<String>,
    #[serde(default)]
    fulfillment: Option<String>,
    #[serde(default)]
    substitution_policy: Option<String>,
    #[serde(default)]
    cart_mutation_approved: bool,
    #[serde(default)]
    final_cart_reviewed: bool,
    #[serde(default)]
    real_world_action_approved: bool,
    #[serde(default)]
    open_viewer: Option<bool>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpTaskPlan {
    version: u32,
    requested_intent: String,
    normalized_intent: String,
    summary: String,
    recommended_profile_kind: String,
    headless: bool,
    host_viewer_ready: bool,
    viewer_available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    viewer_unavailable_reason: Option<String>,
    permissions: McpPermissionState,
    control: McpSessionControlBrief,
    task_context: McpTaskPlanTaskContext,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    assumptions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    needs_user_input: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    approval_checkpoints: Vec<McpTaskPlanApprovalCheckpoint>,
    approval_summary: McpTaskPlanApprovalSummary,
    steps: Vec<McpTaskPlanStep>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpTaskPlanTaskContext {
    task_kind: String,
    workspace_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    provided_inputs: Vec<McpTaskPlanTaskInput>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    missing_inputs: Vec<McpTaskPlanTaskInputNeed>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    safety_boundaries: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    action_boundaries: Vec<McpTaskPlanActionBoundary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    approval_kinds: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpTaskPlanTaskInput {
    name: String,
    value: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpTaskPlanTaskInputNeed {
    name: String,
    reason: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpTaskPlanActionBoundary {
    id: String,
    label: String,
    action_type: String,
    ready: bool,
    approval_required: bool,
    approved: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    approval_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    required_inputs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    missing_approvals: Vec<String>,
    reason: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpTaskPlanStep {
    id: String,
    order: u8,
    label: String,
    tool: String,
    arguments: serde_json::Value,
    ready_to_call: bool,
    reason: String,
    approval_hint: String,
    read_only: bool,
    mutating: bool,
    destructive: bool,
    open_world: bool,
    blocked_by_live_control: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    permission_blockers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    required_input: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpTaskPlanApprovalCheckpoint {
    id: String,
    step_id: String,
    order: u8,
    kind: String,
    label: String,
    tool: String,
    approval_required: bool,
    blocks_step: bool,
    approval_hint: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    required_input: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    permission_blockers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpTaskPlanApprovalSummary {
    blocking_count: usize,
    approval_required_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    approval_kinds: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    next_boundary: Option<McpTaskPlanApprovalBoundary>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpTaskPlanApprovalBoundary {
    kind: String,
    label: String,
    step_id: String,
    tool: String,
    blocks_step: bool,
    approval_required: bool,
    approval_hint: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    required_input: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    permission_blockers: Vec<String>,
}

impl From<&McpTaskPlanApprovalCheckpoint> for McpTaskPlanApprovalBoundary {
    fn from(checkpoint: &McpTaskPlanApprovalCheckpoint) -> Self {
        Self {
            kind: checkpoint.kind.clone(),
            label: checkpoint.label.clone(),
            step_id: checkpoint.step_id.clone(),
            tool: checkpoint.tool.clone(),
            blocks_step: checkpoint.blocks_step,
            approval_required: checkpoint.approval_required,
            approval_hint: checkpoint.approval_hint.clone(),
            required_input: checkpoint.required_input.clone(),
            permission_blockers: checkpoint.permission_blockers.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpSessionControlBrief {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
    allows_agent_mutation: bool,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_at_unix: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    updated_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpSessionDoctorBrief {
    ready_for_x11_workspace: bool,
    ready_for_host_viewer: bool,
    recommended_next_step: String,
    blockers: Vec<String>,
    viewer_blockers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpSessionProfilesBrief {
    count: usize,
    ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpSessionWorkspacesBrief {
    count: usize,
    running_count: usize,
    stopped_count: usize,
    running_ids: Vec<String>,
    stopped_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    activity: Vec<McpSessionWorkspaceActivityBrief>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    suggested_workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpSessionWorkspaceActivityBrief {
    id: String,
    running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    purpose: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display: Option<String>,
    app_count: usize,
    running_app_count: usize,
    #[serde(default)]
    last_event_sequence: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    inferred_intent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    intent_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    intent_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    apps: Vec<McpSessionAppActivityBrief>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpSessionAppActivityBrief {
    id: String,
    label: String,
    running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpRecommendedAction {
    id: String,
    priority: u8,
    intent: String,
    label: String,
    tool: String,
    arguments: serde_json::Value,
    action_type: String,
    idempotent: bool,
    reason: String,
    approval_hint: String,
    read_only: bool,
    open_world: bool,
    blocked_by_live_control: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    approval_checkpoints: Vec<McpRecommendedActionCheckpoint>,
    approval_summary: McpRecommendedActionApprovalSummary,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpRecommendedActionCheckpoint {
    kind: String,
    label: String,
    approval_required: bool,
    blocks_action: bool,
    approval_hint: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    required_input: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpRecommendedActionApprovalSummary {
    blocking_count: usize,
    approval_required_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    approval_kinds: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    next_boundary: Option<McpRecommendedActionApprovalBoundary>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpRecommendedActionApprovalBoundary {
    kind: String,
    label: String,
    blocks_action: bool,
    approval_required: bool,
    approval_hint: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    required_input: Vec<String>,
}

impl From<&McpRecommendedActionCheckpoint> for McpRecommendedActionApprovalBoundary {
    fn from(checkpoint: &McpRecommendedActionCheckpoint) -> Self {
        Self {
            kind: checkpoint.kind.clone(),
            label: checkpoint.label.clone(),
            blocks_action: checkpoint.blocks_action,
            approval_required: checkpoint.approval_required,
            approval_hint: checkpoint.approval_hint.clone(),
            required_input: checkpoint.required_input.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpSessionApprovalSummary {
    blocking_recommendation_count: usize,
    approval_required_recommendation_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    approval_kinds: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    next_boundary: Option<McpSessionApprovalBoundary>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpSessionApprovalBoundary {
    recommendation_id: String,
    recommendation_label: String,
    priority: u8,
    tool: String,
    kind: String,
    label: String,
    blocks_action: bool,
    approval_required: bool,
    approval_hint: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    required_input: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpActionCatalog {
    version: u32,
    notes: Vec<String>,
    tools: Vec<McpActionInfo>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpActionInfo {
    name: String,
    category: String,
    read_only: bool,
    mutating: bool,
    destructive: bool,
    idempotent: bool,
    open_world: bool,
    control_behavior: String,
    notes: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    parameter_notes: Vec<McpActionParameterNote>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct McpActionParameterNote {
    parameter: String,
    when: String,
    effect: String,
    live_control: String,
    approval_hint: String,
}

fn build_mcp_task_plan(
    params: McpTaskPlanParams,
    permissions: &McpPermissionState,
    headless: bool,
) -> McpTaskPlan {
    let profile_ids = match profile::list_profiles() {
        Ok(list) => list
            .profiles
            .into_iter()
            .map(|profile| profile.id)
            .collect::<Vec<_>>(),
        Err(_) => Vec::new(),
    };
    let host_viewer_ready = workspace::doctor_report().ready_for_host_viewer;
    build_mcp_task_plan_with_profile_ids(
        params,
        permissions,
        headless,
        host_viewer_ready,
        profile_ids,
    )
}

fn build_mcp_task_plan_with_profile_ids(
    params: McpTaskPlanParams,
    permissions: &McpPermissionState,
    headless: bool,
    host_viewer_ready: bool,
    profile_ids: Vec<String>,
) -> McpTaskPlan {
    let running_workspace_ids = match workspace::list_workspaces() {
        Ok(list) => list
            .workspaces
            .into_iter()
            .filter(|workspace| workspace.running)
            .map(|workspace| workspace.id)
            .collect(),
        Err(_) => Vec::new(),
    };
    build_mcp_task_plan_with_context(
        params,
        permissions,
        headless,
        host_viewer_ready,
        profile_ids,
        running_workspace_ids,
    )
}

fn build_mcp_task_plan_with_context(
    params: McpTaskPlanParams,
    permissions: &McpPermissionState,
    headless: bool,
    host_viewer_ready: bool,
    profile_ids: Vec<String>,
    running_workspace_ids: Vec<String>,
) -> McpTaskPlan {
    let requested_intent = params.intent.clone();
    let normalized_intent = normalize_task_intent(&params.intent).to_string();
    let control = read_mcp_control_brief();
    let viewer_available = !headless && host_viewer_ready;
    let viewer_unavailable_reason = if headless {
        Some("Host-visible viewer steps are omitted because this MCP process was started with --headless.".to_string())
    } else if !host_viewer_ready {
        Some("Host-visible viewer steps are omitted because workspace_doctor.ready_for_host_viewer=false; start the MCP from a desktop session with DISPLAY or WAYLAND_DISPLAY to enable the viewer.".to_string())
    } else {
        None
    };
    let workspace_id = params
        .workspace_id
        .clone()
        .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
    let workspace_running = running_workspace_ids.iter().any(|id| id == &workspace_id);
    let mut task_context = mcp_task_plan_task_context(
        &params,
        &normalized_intent,
        &workspace_id,
        workspace_running,
    );
    let mut assumptions = Vec::new();
    let mut needs_user_input = Vec::new();
    let mut steps = vec![mcp_task_step(
        "orient_session",
        1,
        "Read the current session brief",
        "mcp_session_brief",
        serde_json::json!({}),
        true,
        "Start from the live permission ceiling, control mode, runtime readiness, and known workspace/profile state.",
        "Read-only; no user approval required.",
        true,
        false,
        false,
        false,
        false,
    )];

    let recommended_profile_kind;
    let summary;

    match normalized_intent.as_str() {
        "app_qa" => {
            recommended_profile_kind = "project-dev".to_string();
            summary = "Plan for project or desktop app QA in an isolated workspace.".to_string();
            if workspace_running {
                assumptions.push(format!(
                    "Workspace {workspace_id:?} is already running; continue from the live workspace instead of starting another project profile unless the user asks for a fresh run."
                ));
                steps.push(mcp_task_step(
                    "observe_running_project_workspace",
                    2,
                    "Observe the running app QA workspace",
                    "workspace_observe",
                    serde_json::json!({
                        "id": workspace_id,
                        "screenshot": true,
                        "include_events": true,
                        "events_tail": 20
                    }),
                    true,
                    "A project/app workspace is already running; inspect screen, active window, apps, and recent events before driving the app.",
                    "Read-only observation; no user approval required.",
                    true,
                    false,
                    false,
                    false,
                    false,
                ));
                steps.push(
                    mcp_task_step(
                        "list_running_project_apps",
                        3,
                        "List running app QA processes",
                        "workspace_list_apps",
                        serde_json::json!({
                            "id": workspace_id,
                            "running": true
                        }),
                        true,
                        "Identify app ids and process labels so later window, log, or input actions can use stable app_id targets instead of mutable titles.",
                        "Read-only inspection; no user approval required.",
                        true,
                        false,
                        false,
                        false,
                        false,
                    )
                    .with_dependency("observe_running_project_workspace"),
                );
                steps.push(
                    mcp_task_step(
                        "read_recent_project_events",
                        4,
                        "Read recent app QA workspace events",
                        "workspace_events",
                        serde_json::json!({
                            "id": workspace_id,
                            "tail": 50
                        }),
                        true,
                        "Read recent workspace-local events so the agent can explain what has already happened without leaking raw input or browser text.",
                        "Read-only event inspection; no user approval required.",
                        true,
                        false,
                        false,
                        false,
                        false,
                    )
                    .with_dependency("observe_running_project_workspace"),
                );
                steps.push(
                    mcp_task_step(
                        "read_project_app_log_after_app_id",
                        5,
                        "Read the active app log after app id is known",
                        "workspace_read_app_log",
                        serde_json::json!({
                            "id": workspace_id,
                            "app_id": null,
                            "stream": "stdout",
                            "tail_bytes": 12000
                        }),
                        false,
                        "Once a relevant app_id is known, inspect captured stdout before driving the UI or reporting QA evidence.",
                        "Read-only log inspection; no user approval required after a target app_id is known.",
                        true,
                        false,
                        false,
                        false,
                        false,
                    )
                    .with_dependency("list_running_project_apps")
                    .with_required_input(
                        "Use a stable app_id from list_running_project_apps or observe_running_project_workspace.",
                    ),
                );
                steps.push(
                    mcp_task_step(
                        "capture_active_project_window",
                        6,
                        "Capture the active app window when targeted",
                        "workspace_screenshot_window",
                        serde_json::json!({
                            "id": workspace_id,
                            "app_id": null,
                            "window_id": null
                        }),
                        false,
                        "After observation identifies an active window or app_id, capture that window as focused QA evidence before interacting.",
                        "Read-only screenshot; no user approval required after a target is known.",
                        true,
                        false,
                        false,
                        false,
                        false,
                    )
                    .with_dependency("observe_running_project_workspace")
                    .with_required_input(
                        "Use active_window.id or a stable app_id from observe_running_project_workspace/list_running_project_apps.",
                    ),
                );
                if viewer_available && params.open_viewer != Some(false) {
                    steps.push(mcp_task_step(
                        "open_viewer_for_running_project",
                        2,
                        "Open the viewer for the running app QA workspace",
                        "workspace_open_viewer",
                        serde_json::json!({ "id": workspace_id }),
                        true,
                        "Viewer-first default: open or reuse the floating viewer immediately so the user can watch, pause, or switch the agent read-only while QA continues.",
                        "Open-world host-visible UI; skipped only when open_viewer=false or the MCP is headless/no-host-display.",
                        false,
                        true,
                        false,
                        true,
                        false,
                    ));
                }
            } else {
                let mut project_run_step_id: Option<&str> = None;
                let profile_id = preferred_profile_id(
                    params.profile_id.as_deref(),
                    &profile_ids,
                    &["project-dev", "project_dev", "qa"],
                );
                if let Some(profile_id) = profile_id {
                    let permission_blockers =
                        saved_profile_permission_blockers(&profile_id, permissions);
                    steps.push(
                    mcp_task_step(
                        "preview_project_profile",
                        2,
                        "Preview the project QA profile",
                        "workspace_open_profile",
                        serde_json::json!({
                            "id": workspace_id,
                            "profile": profile_id,
                            "dry_run": true,
                            "purpose": "Project QA",
                            "setup": true,
                            "setup_timeout_ms": 30000,
                            "setup_kill_on_timeout": true,
                            "startup_wait_window": true,
                            "startup_screenshot_window": true
                        }),
                        permission_blockers.is_empty(),
                        "A saved project-style profile exists; dry-run the full start/setup/startup plan before creating the hidden workspace.",
                        "Dry-run preview is allowed. The real open-profile call requires hidden-workspace acknowledgement and active live control.",
                        true,
                        false,
                        false,
                        false,
                        false,
                    )
                    .with_permission_blockers(permission_blockers.clone()),
                );
                    steps.push(
                    mcp_task_step(
                        "run_project_profile_after_approval",
                        3,
                        "Run the approved project QA profile",
                        "workspace_open_profile",
                        serde_json::json!({
                            "id": workspace_id,
                            "profile": profile_id,
                            "acknowledge_hidden_workspace": true,
                            "acknowledge_unenforced_policy": true,
                            "purpose": "Project QA",
                            "setup": true,
                            "setup_timeout_ms": 30000,
                            "setup_kill_on_timeout": true,
                            "startup_wait_window": true,
                            "startup_screenshot_window": true
                        }),
                        false,
                        "Call only after the user approves the previewed hidden workspace and policy bundle.",
                        "Mutating; requires explicit approval and active live control.",
                        false,
                        true,
                        false,
                        false,
                        !control.allows_agent_mutation,
                    )
                    .with_dependency("preview_project_profile")
                    .with_permission_blockers(permission_blockers)
                    .with_required_input(
                        "Use the exact approval bundle from preview_project_profile before calling the real workspace_open_profile step.",
                    ),
                );
                    project_run_step_id = Some("run_project_profile_after_approval");
                } else if let Some(project_path) = params.project_path.clone() {
                    let permission_blockers = profile_template_permission_blockers(
                        "project-dev",
                        Some(project_path.clone()),
                        None,
                        None,
                        permissions,
                    );
                    steps.push(
                        mcp_task_step(
                            "template_project_profile",
                            2,
                            "Generate a project QA profile template",
                            "profile_template",
                            serde_json::json!({
                                "kind": "project-dev",
                                "id": "project-dev",
                                "host_path": project_path
                            }),
                            permission_blockers.is_empty(),
                            "No saved project profile was found, but a project path was provided.",
                            "Read-only template generation; save it only after review.",
                            true,
                            false,
                            false,
                            false,
                            false,
                        )
                        .with_permission_blockers(permission_blockers.clone()),
                    );
                    steps.push(
                    mcp_task_step(
                        "dry_run_save_project_profile",
                        3,
                        "Dry-run saving the generated profile",
                        "profile_put",
                        serde_json::json!({
                            "dry_run": true,
                            "profile": null
                        }),
                        false,
                        "Dry-run the generated profile save so the UI can show whether it creates or replaces a profile.",
                        "Preview only; the real save is mutating and should follow user review.",
                        true,
                        false,
                        false,
                        false,
                        false,
                    )
                    .with_dependency("template_project_profile")
                    .with_required_input(
                        "Use the WorkspaceProfile object returned by template_project_profile as the profile argument.",
                    ),
                );
                    steps.push(
                    mcp_task_step(
                        "save_project_profile_after_review",
                        4,
                        "Save the reviewed project QA profile",
                        "profile_put",
                        serde_json::json!({
                            "profile": null
                        }),
                        false,
                        "Call only after the user reviews the project mount, setup/startup commands, network behavior, and dry-run save result.",
                        "Mutating profile write; requires explicit approval and active live control.",
                        false,
                        true,
                        false,
                        false,
                        !control.allows_agent_mutation,
                    )
                    .with_dependency("dry_run_save_project_profile")
                    .with_permission_blockers(permission_blockers.clone())
                    .with_required_input(
                        "Use the same WorkspaceProfile object from template_project_profile after the user approves the dry-run result.",
                    ),
                );
                    steps.push(
                    mcp_task_step(
                        "run_project_profile_after_save",
                        5,
                        "Run the saved project QA profile",
                        "workspace_open_profile",
                        serde_json::json!({
                            "id": workspace_id,
                            "profile": "project-dev",
                            "acknowledge_hidden_workspace": true,
                            "acknowledge_unenforced_policy": true,
                            "purpose": "Project QA",
                            "setup": true,
                            "setup_timeout_ms": 30000,
                            "setup_kill_on_timeout": true,
                            "startup_wait_window": true,
                            "startup_screenshot_window": true
                        }),
                        false,
                        "Call only after the generated project-dev profile has been saved and approved for this QA task.",
                        "Mutating; requires hidden-workspace approval and active live control.",
                        false,
                        true,
                        false,
                        false,
                        !control.allows_agent_mutation,
                    )
                    .with_dependency("save_project_profile_after_review")
                    .with_permission_blockers(permission_blockers)
                    .with_required_input(
                        "Run only after profile_put saves the project-dev profile successfully.",
                    ),
                );
                    project_run_step_id = Some("run_project_profile_after_save");
                } else {
                    needs_user_input.push(
                    "Provide a saved project profile id or a project_path to generate a project-dev profile."
                        .to_string(),
                );
                }
                if project_run_step_id.is_some() {
                    let mut observe_step = mcp_task_step(
                        "observe_project_workspace",
                        6,
                    "Observe the project QA workspace after it starts",
                    "workspace_observe",
                    serde_json::json!({
                        "id": workspace_id,
                        "screenshot": true,
                        "include_events": true,
                        "events_tail": 20
                    }),
                    false,
                    "Once the project workspace is running, inspect screen, active window, app state, and events before driving the app under test.",
                    "Read-only observation; no user approval required after the workspace exists.",
                    true,
                    false,
                    false,
                    false,
                    false,
                )
                .with_required_input("Call after the project QA workspace has started.");
                    if let Some(dep) = project_run_step_id {
                        observe_step = observe_step.with_dependency(dep);
                    }
                    steps.push(observe_step);
                    steps.push(
                        mcp_task_step(
                            "list_project_apps_after_start",
                            7,
                            "List project QA apps after start",
                            "workspace_list_apps",
                            serde_json::json!({
                                "id": workspace_id,
                                "running": true
                            }),
                            false,
                            "After the project workspace starts, identify app ids and process labels so later actions can use stable app_id targets.",
                            "Read-only inspection; no user approval required after the workspace exists.",
                            true,
                            false,
                            false,
                            false,
                            false,
                        )
                        .with_dependency("observe_project_workspace")
                        .with_required_input("Call after observe_project_workspace succeeds."),
                    );
                    steps.push(
                        mcp_task_step(
                            "read_project_events_after_start",
                            8,
                            "Read project QA events after start",
                            "workspace_events",
                            serde_json::json!({
                                "id": workspace_id,
                                "tail": 50
                            }),
                            false,
                            "After the project workspace starts, read recent workspace-local events before UI input or reporting.",
                            "Read-only event inspection; no user approval required after the workspace exists.",
                            true,
                            false,
                            false,
                            false,
                            false,
                        )
                        .with_dependency("observe_project_workspace")
                        .with_required_input("Call after observe_project_workspace succeeds."),
                    );
                    steps.push(
                        mcp_task_step(
                            "read_project_app_log_after_start",
                            9,
                            "Read project app log after app id is known",
                            "workspace_read_app_log",
                            serde_json::json!({
                                "id": workspace_id,
                                "app_id": null,
                                "stream": "stdout",
                                "tail_bytes": 12000
                            }),
                            false,
                            "Once a relevant app_id is known, inspect captured stdout before driving the UI or reporting QA evidence.",
                            "Read-only log inspection; no user approval required after a target app_id is known.",
                            true,
                            false,
                            false,
                            false,
                            false,
                        )
                        .with_dependency("list_project_apps_after_start")
                        .with_required_input(
                            "Use a stable app_id from list_project_apps_after_start or observe_project_workspace.",
                        ),
                    );
                    steps.push(
                        mcp_task_step(
                            "capture_project_window_after_start",
                            10,
                            "Capture the project app window after target is known",
                            "workspace_screenshot_window",
                            serde_json::json!({
                                "id": workspace_id,
                                "app_id": null,
                                "window_id": null
                            }),
                            false,
                            "After observation identifies an active window or app_id, capture that window as focused QA evidence before interacting.",
                            "Read-only screenshot; no user approval required after a target is known.",
                            true,
                            false,
                            false,
                            false,
                            false,
                        )
                        .with_dependency("observe_project_workspace")
                        .with_required_input(
                            "Use active_window.id or a stable app_id from observe_project_workspace/list_project_apps_after_start.",
                        ),
                    );
                }
                if viewer_available
                    && params.open_viewer != Some(false)
                    && project_run_step_id.is_some()
                {
                    let mut viewer_step = mcp_task_step(
                        "open_viewer_when_project_runs",
                        11,
                    "Open the viewer when the project QA workspace is running",
                    "workspace_open_viewer",
                    serde_json::json!({ "id": workspace_id }),
                    false,
                    "Viewer-first default after start: open or reuse the floating viewer as soon as the project workspace exists.",
                    "Open-world host-visible UI; skipped only when open_viewer=false or the MCP is headless/no-host-display.",
                    false,
                    true,
                    false,
                    true,
                    false,
                )
                .with_required_input(
                    "Call after a project QA workspace has been started and the user wants a host-visible floating viewer.",
                );
                    if let Some(dep) = project_run_step_id {
                        viewer_step = viewer_step.with_dependency(dep);
                    }
                    steps.push(viewer_step);
                }
            }
        }
        "browser_task" => {
            recommended_profile_kind = "browser-session".to_string();
            summary =
                "Plan for browser, shopping, grocery, or logged-in web work with explicit user-approved browser data."
                    .to_string();
            let mut browser_run_step_id: Option<&str> = None;
            let shopping_intent = task_intent_is_shopping_or_grocery(&requested_intent);
            let shopping_required_inputs = mcp_task_plan_shopping_required_input_reasons(&params);
            if shopping_intent {
                assumptions.push(
                    "Shopping or grocery workflows should avoid purchasing or account changes unless the user explicitly approves each real-world action."
                        .to_string(),
                );
                assumptions.push(
                    "Shopping inputs are task requirements, not MCP permission limits; clean/default MCP usage still follows the host/client session boundary."
                        .to_string(),
                );
                needs_user_input.extend(shopping_required_inputs.iter().cloned());
            } else {
                assumptions.push(
                    "Browser workflows should avoid account changes unless the user explicitly approves each real-world action."
                        .to_string(),
                );
            }
            if workspace_running {
                assumptions.push(format!(
                    "Workspace {workspace_id:?} is already running; continue from the live browser workspace instead of creating another browser session unless the user asks for a fresh session."
                ));
                steps.push(mcp_task_step(
                    "observe_running_browser_workspace",
                    2,
                    "Observe the running browser workspace",
                    "workspace_observe",
                    serde_json::json!({
                        "id": workspace_id,
                        "screenshot": true,
                        "include_events": true,
                        "events_tail": 20
                    }),
                    true,
                    "A browser-like workspace is already running; inspect screen, active tab/window, app state, and recent events before browser actions.",
                    "Read-only observation; no user approval required.",
                    true,
                    false,
                    false,
                    false,
                    false,
                ));
                steps.push(
                    mcp_task_step(
                        "list_running_browser_apps",
                        3,
                        "List running browser apps",
                        "workspace_list_apps",
                        serde_json::json!({
                            "id": workspace_id,
                            "running": true
                        }),
                        true,
                        "Identify browser app ids so later window targeting, logs, or screenshots can use stable app_id handles.",
                        "Read-only inspection; no user approval required.",
                        true,
                        false,
                        false,
                        false,
                        false,
                    )
                    .with_dependency("observe_running_browser_workspace"),
                );
                steps.push(
                    mcp_task_step(
                        "read_recent_browser_events",
                        4,
                        "Read recent browser workspace events",
                        "workspace_events",
                        serde_json::json!({
                            "id": workspace_id,
                            "tail": 50
                        }),
                        true,
                        "Read recent workspace-local events before browser interaction so the agent can understand current state from metadata-only events without raw text leakage.",
                        "Read-only event inspection; no user approval required.",
                        true,
                        false,
                        false,
                        false,
                        false,
                    )
                    .with_dependency("observe_running_browser_workspace"),
                );
                steps.push(
                    mcp_task_step(
                        "discover_running_browser_devtools_targets",
                        5,
                        "Discover workspace browser DevTools targets",
                        "workspace_browser_targets",
                        serde_json::json!({
                            "id": workspace_id,
                            "app_id": null,
                            "timeout_ms": 5000
                        }),
                        true,
                        "Use the workspace-owned Chrome DevTools endpoint derived from the running browser app, rather than controlling the user's host Chrome bridge.",
                        "Read-only browser target discovery; no user approval required.",
                        true,
                        false,
                        false,
                        false,
                        false,
                    )
                    .with_dependency("list_running_browser_apps")
                    .with_required_input(
                        "If more than one browser app is running, pass the browser app_id from list_running_browser_apps.",
                    ),
                );
                steps.push(
                    mcp_task_step(
                        "snapshot_running_browser_page",
                        6,
                        "Read running browser page text",
                        "workspace_browser_snapshot",
                        serde_json::json!({
                            "id": workspace_id,
                            "app_id": null,
                            "max_text_chars": 12000,
                            "timeout_ms": 5000
                        }),
                        false,
                        "Use the workspace-owned browser snapshot before manual screenshot reading so listing titles, prices, links, and current URL are machine-readable through the MCP.",
                        "Read-only browser page inspection; no user approval required.",
                        true,
                        false,
                        false,
                        false,
                        false,
                    )
                    .with_dependency("discover_running_browser_devtools_targets")
                    .with_required_input(
                        "Pass the browser app_id or target_id discovered by discover_running_browser_devtools_targets when there are multiple browser pages.",
                    ),
                );
                if shopping_intent {
                    steps.push(
                        mcp_task_step(
                            "extract_running_browser_search_results",
                            7,
                            "Extract structured shopping results",
                            "workspace_browser_search_results",
                            serde_json::json!({
                                "id": workspace_id,
                                "app_id": null,
                                "max_results": 20,
                                "min_vram_gb": null,
                                "timeout_ms": 5000
                            }),
                            false,
                            "For shopping or grocery result pages, use structured product cards with titles, prices, ratings, availability, delivery snippets, visible VRAM when present, and links before falling back to raw page text. For GPU shopping, set min_vram_gb from the user's threshold.",
                            "Read-only browser result extraction; no user approval required.",
                            true,
                            false,
                            false,
                            false,
                            false,
                        )
                        .with_dependency("snapshot_running_browser_page")
                        .with_required_input(
                            "Pass the browser app_id or target_id discovered by discover_running_browser_devtools_targets when there are multiple browser pages.",
                        ),
                    );
                }
                let boundary_step = mcp_task_step(
                    "confirm_real_world_browser_boundary",
                    7,
                    "Confirm browser real-world action boundary",
                    "mcp_action_catalog",
                    serde_json::json!({}),
                    true,
                    "Before injecting input or navigating a logged-in browser session, classify the next action and separate browsing from purchases, checkout, order submission, or account changes.",
                    "Read-only planning; purchases, checkout, order submission, or account changes require separate explicit approval.",
                    true,
                    false,
                    false,
                    false,
                    false,
                )
                .with_dependency("observe_running_browser_workspace")
                .with_required_input(
                    "Do not submit purchases, checkout, order submission, or account changes without a separate explicit approval for that real-world action.",
                )
                .with_required_inputs(&shopping_required_inputs);
                steps.push(boundary_step);
                let mut navigate_step = mcp_task_step(
                    "navigate_running_browser_to_target_url",
                    8,
                    "Navigate workspace browser to target URL",
                    "workspace_browser_navigate",
                    serde_json::json!({
                        "id": workspace_id,
                        "app_id": null,
                        "url": params.target_url.clone(),
                        "snapshot": true,
                        "max_text_chars": 12000,
                        "timeout_ms": 10000
                    }),
                    params.target_url.is_some(),
                    "Use the MCP-owned browser navigation path for normal browsing/search pages instead of synthetic keyboard address-bar loops or external browser automation.",
                    "Mutating browser control; requires active live control. Checkout, purchase, order, payment, and account changes still need separate explicit approval.",
                    false,
                    true,
                    false,
                    false,
                    !control.allows_agent_mutation,
                )
                .with_dependency("confirm_real_world_browser_boundary")
                .with_dependency("snapshot_running_browser_page");
                if params.target_url.is_none() {
                    navigate_step = navigate_step.with_required_input(
                        "Provide target_url before asking the MCP to navigate the workspace browser.",
                    );
                }
                steps.push(navigate_step);
                steps.push(
                    mcp_task_step(
                        "capture_browser_window_when_targeted",
                        9,
                        "Capture the browser window when targeted",
                        "workspace_screenshot_window",
                        serde_json::json!({
                            "id": workspace_id,
                            "app_id": null,
                            "window_id": null
                        }),
                        false,
                        "After observation identifies the active browser window or app_id, capture it as evidence before navigation or input.",
                        "Read-only screenshot; no user approval required after a target is known.",
                        true,
                        false,
                        false,
                        false,
                        false,
                    )
                    .with_dependency("observe_running_browser_workspace")
                    .with_required_input(
                        "Use active_window.id or a stable browser app_id from observe_running_browser_workspace/list_running_browser_apps.",
                    ),
                );
                if viewer_available && params.open_viewer != Some(false) {
                    steps.push(mcp_task_step(
                        "open_viewer_for_running_browser",
                        2,
                        "Open the viewer for the running browser workspace",
                        "workspace_open_viewer",
                        serde_json::json!({ "id": workspace_id }),
                        true,
                        "Viewer-first default: open or reuse the floating viewer immediately so the user can watch and pause the agent while it works in the workspace browser.",
                        "Open-world host-visible UI; skipped only when open_viewer=false or the MCP is headless/no-host-display.",
                        false,
                        true,
                        false,
                        true,
                        false,
                    ));
                }
            } else {
                let profile_id = preferred_profile_id(
                    params.profile_id.as_deref(),
                    &profile_ids,
                    &["browser-session", "browser_session", "browser", "shopping"],
                );
                if let Some(profile_id) = profile_id {
                    let permission_blockers =
                        saved_profile_permission_blockers(&profile_id, permissions);
                    steps.push(
                    mcp_task_step(
                        "preview_browser_session",
                        2,
                        "Preview the browser-session profile",
                        "workspace_open_profile",
                        serde_json::json!({
                            "id": workspace_id,
                            "profile": profile_id,
                            "dry_run": true,
                            "purpose": "Browser task",
                            "startup_wait_window": true,
                            "startup_screenshot_window": true,
                            "startup_window_timeout_ms": 20000
                        }),
                        permission_blockers.is_empty(),
                        "A saved browser-session profile exists; dry-run startup before opening a hidden browser workspace.",
                        "Dry-run preview is allowed. The real profile start requires hidden-workspace acknowledgement and active live control.",
                        true,
                        false,
                        false,
                        false,
                        false,
                    )
                    .with_permission_blockers(permission_blockers.clone()),
                );
                    steps.push(
                    mcp_task_step(
                        "run_browser_session_after_approval",
                        3,
                        "Run the approved browser-session profile",
                        "workspace_open_profile",
                        serde_json::json!({
                            "id": workspace_id,
                            "profile": profile_id,
                            "acknowledge_hidden_workspace": true,
                            "purpose": "Browser task",
                            "startup_wait_window": true,
                            "startup_screenshot_window": true,
                            "startup_window_timeout_ms": 20000
                        }),
                        false,
                        "Call only after the user approves the hidden browser workspace, its profile policy, and any account-data exposure.",
                        "Mutating; requires explicit approval and active live control. Purchases, account changes, or order submission need separate user approval.",
                        false,
                        true,
                        false,
                        false,
                        !control.allows_agent_mutation,
                    )
                    .with_dependency("preview_browser_session")
                    .with_permission_blockers(permission_blockers)
                    .with_required_input(
                        "Use the exact approval bundle from preview_browser_session before calling the real workspace_open_profile step.",
                    )
                    .with_required_inputs(&shopping_required_inputs)
                    .with_required_input(
                        "Do not submit purchases, checkout, or account changes without a separate explicit approval for that real-world action.",
                    ),
                );
                    browser_run_step_id = Some("run_browser_session_after_approval");
                } else {
                    if params.user_data_dir.is_none() {
                        needs_user_input.push(
                        "Choose an explicit user-approved browser user-data directory or a copied browser profile before using browser-session for shopping/logged-in work."
                            .to_string(),
                    );
                    }
                    let permission_blockers = if params.user_data_dir.is_some() {
                        profile_template_permission_blockers(
                            "browser-session",
                            None,
                            params.browser_path.clone(),
                            params.user_data_dir.clone(),
                            permissions,
                        )
                    } else {
                        Vec::new()
                    };
                    let mut template_step = mcp_task_step(
                    "template_browser_session",
                    2,
                    "Generate a browser-session profile template",
                    "profile_template",
                    serde_json::json!({
                        "kind": "browser-session",
                        "id": "browser-session",
                        "browser_path": params.browser_path,
                        "user_data_dir": params.user_data_dir
                    }),
                    params.user_data_dir.is_some() && permission_blockers.is_empty(),
                    "The browser-session template mounts explicitly approved browser data and inherits host networking for authenticated web tasks.",
                    "Read-only template generation. The user must approve the browser data directory and should close the host browser or use a copied profile.",
                    true,
                    false,
                    false,
                    false,
                    false,
                )
                .with_permission_blockers(permission_blockers.clone());
                    if !template_step.ready_to_call {
                        template_step = template_step.with_required_input(
                        "Set user_data_dir to an explicit user-approved browser profile directory or copied profile before calling profile_template.",
                    );
                    }
                    steps.push(template_step);
                    if params.user_data_dir.is_some() {
                        steps.push(
                        mcp_task_step(
                            "dry_run_save_browser_profile",
                            3,
                            "Dry-run saving the generated browser profile",
                            "profile_put",
                            serde_json::json!({
                                "dry_run": true,
                                "profile": null
                            }),
                            false,
                            "Dry-run the generated browser profile save so the UI can show create/replace behavior before writing profile state.",
                            "Preview only; the real save is mutating and should follow user review.",
                            true,
                            false,
                            false,
                            false,
                            false,
                        )
                        .with_dependency("template_browser_session")
                        .with_permission_blockers(permission_blockers.clone())
                        .with_required_input(
                            "Use the WorkspaceProfile object returned by template_browser_session as the profile argument.",
                        ),
                    );
                        steps.push(
                        mcp_task_step(
                            "save_browser_profile_after_review",
                            4,
                            "Save the reviewed browser-session profile",
                            "profile_put",
                            serde_json::json!({
                                "profile": null
                            }),
                            false,
                            "Call only after the user reviews the browser binary, user-data directory, network behavior, and dry-run save result.",
                            "Mutating profile write; requires explicit approval and active live control.",
                            false,
                            true,
                            false,
                            false,
                            !control.allows_agent_mutation,
                        )
                        .with_dependency("dry_run_save_browser_profile")
                        .with_permission_blockers(permission_blockers.clone())
                        .with_required_input(
                            "Use the same WorkspaceProfile object from template_browser_session after the user approves the dry-run result.",
                        ),
                    );
                        steps.push(
                        mcp_task_step(
                            "run_browser_session_after_save",
                            5,
                            "Run the saved browser-session profile",
                            "workspace_open_profile",
                            serde_json::json!({
                                "id": workspace_id,
                                "profile": "browser-session",
                                "acknowledge_hidden_workspace": true,
                                "purpose": "Browser task",
                                "startup_wait_window": true,
                                "startup_screenshot_window": true,
                                "startup_window_timeout_ms": 20000
                            }),
                            false,
                            "Call only after the generated browser-session profile has been saved and approved for this task.",
                            "Mutating; requires hidden-workspace approval and active live control. Purchases, account changes, or order submission need separate user approval.",
                            false,
                            true,
                            false,
                            false,
                            !control.allows_agent_mutation,
                        )
                        .with_dependency("save_browser_profile_after_review")
                        .with_permission_blockers(permission_blockers)
                        .with_required_input(
                            "Run only after profile_put saves the browser-session profile successfully.",
                        )
                        .with_required_inputs(&shopping_required_inputs)
                        .with_required_input(
                            "Do not submit purchases, checkout, or account changes without a separate explicit approval for that real-world action.",
                        ),
                    );
                        browser_run_step_id = Some("run_browser_session_after_save");
                    }
                }
                if browser_run_step_id.is_some() {
                    let mut observe_step = mcp_task_step(
                        "observe_browser_workspace",
                        6,
                    "Observe the browser workspace after it starts",
                    "workspace_observe",
                    serde_json::json!({
                        "id": workspace_id,
                        "screenshot": true,
                        "include_events": true,
                        "events_tail": 20
                    }),
                    false,
                    "Once the browser workspace is running, inspect screen, window, app, and event state before taking browser actions.",
                    "Read-only observation; no user approval required after the workspace exists.",
                    true,
                    false,
                    false,
                    false,
                    false,
                )
                .with_required_input("Call after the browser workspace has started.");
                    if let Some(dep) = browser_run_step_id {
                        observe_step = observe_step.with_dependency(dep);
                    }
                    steps.push(observe_step);
                    steps.push(
                        mcp_task_step(
                            "list_browser_apps_after_start",
                            7,
                            "List browser apps after start",
                            "workspace_list_apps",
                            serde_json::json!({
                                "id": workspace_id,
                                "running": true
                            }),
                            false,
                            "After the browser workspace starts, identify stable browser app ids for screenshots, logs, and later window targeting.",
                            "Read-only inspection; no user approval required after the workspace exists.",
                            true,
                            false,
                            false,
                            false,
                            false,
                        )
                        .with_dependency("observe_browser_workspace")
                        .with_required_input("Call after observe_browser_workspace succeeds."),
                    );
                    steps.push(
                        mcp_task_step(
                            "discover_browser_devtools_targets_after_start",
                            8,
                            "Discover browser DevTools targets after start",
                            "workspace_browser_targets",
                            serde_json::json!({
                                "id": workspace_id,
                                "app_id": null,
                                "timeout_ms": 5000
                            }),
                            false,
                            "After the browser starts, discover the workspace-owned Chrome DevTools endpoint and page targets before using browser control.",
                            "Read-only browser target discovery; no user approval required after the workspace exists.",
                            true,
                            false,
                            false,
                            false,
                            false,
                        )
                        .with_dependency("list_browser_apps_after_start")
                        .with_required_input(
                            "Call after list_browser_apps_after_start succeeds; if more than one browser app is running, pass its app_id.",
                        ),
                    );
                    steps.push(
                        mcp_task_step(
                            "snapshot_browser_page_after_start",
                            9,
                            "Read browser page text after start",
                            "workspace_browser_snapshot",
                            serde_json::json!({
                                "id": workspace_id,
                                "app_id": null,
                                "max_text_chars": 12000,
                                "timeout_ms": 5000
                            }),
                            false,
                            "Read the current browser page through the workspace-owned MCP path before relying on manual visual extraction.",
                            "Read-only browser page inspection; no user approval required after the workspace exists.",
                            true,
                            false,
                            false,
                            false,
                            false,
                        )
                        .with_dependency("discover_browser_devtools_targets_after_start")
                        .with_required_input(
                            "Pass the browser app_id or target_id discovered by discover_browser_devtools_targets_after_start when there are multiple browser pages.",
                        ),
                    );
                    if shopping_intent {
                        steps.push(
                            mcp_task_step(
                                "extract_browser_search_results_after_start",
                                10,
                                "Extract structured shopping results after start",
                                "workspace_browser_search_results",
                                serde_json::json!({
                                    "id": workspace_id,
                                    "app_id": null,
                                    "max_results": 20,
                                    "min_vram_gb": null,
                                    "timeout_ms": 5000
                                }),
                                false,
                                "After the browser starts, extract product/search result cards through the workspace-owned MCP path so shopping agents do not need external browser automation or raw text scraping. For GPU shopping, set min_vram_gb from the user's threshold.",
                                "Read-only browser result extraction; no user approval required after the workspace exists.",
                                true,
                                false,
                                false,
                                false,
                                false,
                            )
                            .with_dependency("snapshot_browser_page_after_start")
                            .with_required_input(
                                "Pass the browser app_id or target_id discovered by discover_browser_devtools_targets_after_start when there are multiple browser pages.",
                            ),
                        );
                    }
                    steps.push(
                        mcp_task_step(
                            "read_browser_events_after_start",
                            10,
                            "Read browser events after start",
                            "workspace_events",
                            serde_json::json!({
                                "id": workspace_id,
                                "tail": 50
                            }),
                            false,
                            "After the browser workspace starts, read recent workspace-local events before navigation, input, or real-world browser actions.",
                            "Read-only event inspection; no user approval required after the workspace exists.",
                            true,
                            false,
                            false,
                            false,
                            false,
                        )
                        .with_dependency("observe_browser_workspace")
                        .with_required_input("Call after observe_browser_workspace succeeds."),
                    );
                    steps.push(
                        mcp_task_step(
                            "confirm_real_world_boundary_after_start",
                            11,
                            "Confirm real-world boundary after browser start",
                            "mcp_action_catalog",
                            serde_json::json!({}),
                            false,
                            "After the browser workspace starts and before browser input, classify the next action and separate browsing from purchases, checkout, order submission, or account changes.",
                            "Read-only planning; purchases, checkout, order submission, or account changes require separate explicit approval.",
                            true,
                            false,
                            false,
                            false,
                            false,
                        )
                        .with_dependency("observe_browser_workspace")
                        .with_required_input("Call after observe_browser_workspace succeeds.")
                        .with_required_inputs(&shopping_required_inputs)
                        .with_required_input(
                            "Do not submit purchases, checkout, order submission, or account changes without a separate explicit approval for that real-world action.",
                        ),
                    );
                    let mut navigate_step = mcp_task_step(
                        "navigate_browser_to_target_url_after_start",
                        12,
                        "Navigate browser to target URL after start",
                        "workspace_browser_navigate",
                        serde_json::json!({
                            "id": workspace_id,
                            "app_id": null,
                            "url": params.target_url.clone(),
                            "snapshot": true,
                            "max_text_chars": 12000,
                            "timeout_ms": 10000
                        }),
                        params.target_url.is_some(),
                        "Use repo-owned browser navigation once the workspace browser exists, keeping browser dogfood inside the same MCP path as observation and events.",
                        "Mutating browser control; requires active live control. Checkout, purchase, order, payment, and account changes still need separate explicit approval.",
                        false,
                        true,
                        false,
                        false,
                        !control.allows_agent_mutation,
                    )
                    .with_dependency("confirm_real_world_boundary_after_start")
                    .with_dependency("snapshot_browser_page_after_start");
                    if params.target_url.is_none() {
                        navigate_step = navigate_step.with_required_input(
                            "Provide target_url before asking the MCP to navigate the workspace browser.",
                        );
                    }
                    steps.push(navigate_step);
                    steps.push(
                        mcp_task_step(
                            "capture_browser_window_after_start",
                            13,
                            "Capture browser window after target is known",
                            "workspace_screenshot_window",
                            serde_json::json!({
                                "id": workspace_id,
                                "app_id": null,
                                "window_id": null
                            }),
                            false,
                            "After observation identifies the active browser window or app_id, capture it as evidence before navigation or input.",
                            "Read-only screenshot; no user approval required after a target is known.",
                            true,
                            false,
                            false,
                            false,
                            false,
                        )
                        .with_dependency("observe_browser_workspace")
                        .with_required_input(
                            "Use active_window.id or a stable browser app_id from observe_browser_workspace/list_browser_apps_after_start.",
                        ),
                    );
                }
                if viewer_available
                    && params.open_viewer != Some(false)
                    && browser_run_step_id.is_some()
                {
                    let mut viewer_step = mcp_task_step(
                        "open_viewer_when_browser_runs",
                        14,
                    "Open the viewer when the browser workspace is running",
                    "workspace_open_viewer",
                    serde_json::json!({ "id": workspace_id }),
                    false,
                    "Viewer-first default after start: open or reuse the floating viewer as soon as the browser workspace exists.",
                    "Open-world host-visible UI; skipped only when open_viewer=false or the MCP is headless/no-host-display.",
                    false,
                    true,
                    false,
                    true,
                    false,
                )
                .with_required_input(
                    "Call after a browser workspace has been started and the user wants a host-visible floating viewer.",
                );
                    if let Some(dep) = browser_run_step_id {
                        viewer_step = viewer_step.with_dependency(dep);
                    }
                    steps.push(viewer_step);
                }
            }
        }
        "observe" => {
            recommended_profile_kind = "none".to_string();
            summary =
                "Plan for observing existing workspace activity without mutating it.".to_string();
            steps.push(mcp_task_step(
                "observe_workspace",
                2,
                "Observe workspace screen and events",
                "workspace_observe",
                serde_json::json!({
                    "id": workspace_id,
                    "screenshot": true,
                    "include_events": true,
                    "events_tail": 20
                }),
                true,
                "Observation is the safest first move when a workspace may already be active.",
                "Read-only; no user approval required.",
                true,
                false,
                false,
                false,
                false,
            ));
            if viewer_available && params.open_viewer != Some(false) {
                steps.push(mcp_task_step(
                    "open_viewer",
                    2,
                    "Open the floating viewer",
                    "workspace_open_viewer",
                    serde_json::json!({ "id": workspace_id }),
                    true,
                    "Viewer-first default: open or reuse the floating viewer immediately for live visibility and control.",
                    "Open-world host-visible UI; skipped only when open_viewer=false or the MCP is headless/no-host-display.",
                    false,
                    true,
                    false,
                    true,
                    false,
                ));
            }
        }
        "cleanup" => {
            recommended_profile_kind = "none".to_string();
            summary = "Plan for inspecting and cleaning stopped workspace artifacts.".to_string();
            steps.push(mcp_task_step(
                "preview_cleanup",
                2,
                "Preview stale workspace cleanup",
                "workspace_cleanup_stale",
                serde_json::json!({ "dry_run": true }),
                true,
                "Preview removable runtime directories and process cleanup actions before deleting files.",
                "Dry-run is read-only. Real cleanup is destructive and requires active live control.",
                true,
                false,
                false,
                false,
                false,
            ));
            steps.push(
                mcp_task_step(
                    "cleanup_after_approval",
                    3,
                    "Run approved stale workspace cleanup",
                    "workspace_cleanup_stale",
                    serde_json::json!({}),
                    false,
                    "Call only after the user reviews the dry-run cleanup list and approves deleting stale runtime files or terminating verified orphan processes.",
                    "Destructive; requires explicit approval and active live control.",
                    false,
                    true,
                    true,
                    false,
                    !control.allows_agent_mutation,
                )
                .with_dependency("preview_cleanup")
                .with_required_input(
                    "Use the exact dry-run result from preview_cleanup as the approval surface before deleting anything.",
                ),
            );
            steps.push(
                mcp_task_step(
                    "verify_cleanup",
                    4,
                    "Verify workspace cleanup result",
                    "workspace_list",
                    serde_json::json!({}),
                    false,
                    "After cleanup, list known workspaces to verify runtime state.",
                    "Read-only; no user approval required after cleanup runs.",
                    true,
                    false,
                    false,
                    false,
                    false,
                )
                .with_dependency("cleanup_after_approval")
                .with_required_input("Call after cleanup_after_approval finishes."),
            );
        }
        _ => {
            recommended_profile_kind = "unknown".to_string();
            summary =
                "No specialized task plan matched; use the session brief and action catalog first."
                    .to_string();
            needs_user_input.push(
                "Use intent app_qa, browser_task, shopping, grocery, observe, or cleanup for a specialized plan."
                    .to_string(),
            );
            steps.push(mcp_task_step(
                "classify_actions",
                2,
                "Classify available MCP actions",
                "mcp_action_catalog",
                serde_json::json!({}),
                true,
                "When the task intent is unclear, classify actions before calling mutating tools.",
                "Read-only; no user approval required.",
                true,
                false,
                false,
                false,
                false,
            ));
        }
    }

    if permissions.restricted {
        assumptions.push(
            "The MCP permission ceiling is restricted; generated profiles, starts, and launches must stay within it."
                .to_string(),
        );
    }
    if !control.allows_agent_mutation {
        assumptions.push(format!(
            "Live MCP control is not active; only read-only and dry-run preview steps should be called. {}",
            live_control_reactivation_hint()
        ));
    }
    if let Some(reason) = &viewer_unavailable_reason {
        assumptions.push(reason.clone());
    }
    let approval_checkpoints = mcp_task_plan_approval_checkpoints(&steps);
    let approval_kinds =
        mcp_task_plan_approval_kinds(&approval_checkpoints, &task_context.action_boundaries);
    let approval_summary = mcp_task_plan_approval_summary(&approval_checkpoints, &approval_kinds);
    task_context.approval_kinds = approval_kinds;

    McpTaskPlan {
        version: 1,
        requested_intent,
        normalized_intent,
        summary,
        recommended_profile_kind,
        headless,
        host_viewer_ready,
        viewer_available,
        viewer_unavailable_reason,
        permissions: permissions.clone(),
        control,
        task_context,
        assumptions,
        needs_user_input,
        approval_checkpoints,
        approval_summary,
        steps,
    }
}

fn read_mcp_control_brief() -> McpSessionControlBrief {
    match control::control_status() {
        Ok(status) => McpSessionControlBrief {
            mode: Some(status.state.mode.as_str().to_string()),
            allows_agent_mutation: status.state.mode.allows_agent_mutation(),
            message: format!("MCP live control is {}", status.state.mode.label()),
            updated_at_unix: (status.state.updated_at_unix != 0)
                .then_some(status.state.updated_at_unix),
            updated_by: status.state.updated_by,
            reason: status.state.reason,
        },
        Err(error) => McpSessionControlBrief {
            mode: None,
            allows_agent_mutation: false,
            message: format!("MCP live control failed closed: {error}"),
            updated_at_unix: None,
            updated_by: None,
            reason: None,
        },
    }
}

fn build_agent_mode_summary(headless: bool) -> AgentModeSummary {
    let doctor = workspace::doctor_report();
    let (control_mode, allows_agent_mutation) = match control::control_status() {
        Ok(status) => (
            Some(status.state.mode.as_str().to_string()),
            status.state.mode.allows_agent_mutation(),
        ),
        Err(_) => (None, false),
    };
    let viewer_available = !headless && doctor.ready_for_host_viewer;
    let viewer_unavailable_reason = if headless {
        Some("--headless disables workspace_open_viewer.".to_string())
    } else if !doctor.ready_for_host_viewer {
        Some("no host display is ready; workspace_doctor.ready_for_host_viewer=false".to_string())
    } else {
        None
    };
    AgentModeSummary {
        control_mode,
        allows_agent_mutation,
        headless,
        ready_for_x11_workspace: doctor.ready_for_x11_workspace,
        ready_for_host_viewer: doctor.ready_for_host_viewer,
        viewer_available,
        viewer_unavailable_reason,
        exact_reactivation_parameters: if allows_agent_mutation {
            Vec::new()
        } else {
            vec![
                "mode=active".to_string(),
                "confirmed_user_request=true".to_string(),
            ]
        },
    }
}

fn normalize_task_intent(intent: &str) -> &'static str {
    let intent = intent.trim().to_ascii_lowercase();
    if intent.contains("grocery")
        || intent.contains("groceries")
        || intent.contains("shopping")
        || intent.contains("shop")
        || intent.contains("buy")
        || intent.contains("purchase")
        || intent.contains("cart")
        || intent.contains("checkout")
        || intent.contains("order")
        || intent.contains("delivery")
        || intent.contains("browser")
        || intent.contains("web")
    {
        "browser_task"
    } else if intent.contains("qa")
        || intent.contains("project")
        || intent.contains("app")
        || intent.contains("desktop")
        || intent.contains("frontend")
        || intent.contains("ui")
        || intent.contains("test")
        || intent.contains("verify")
        || intent.contains("debug")
        || intent.contains("smoke")
        || intent.contains("local dev")
        || intent.contains("render")
    {
        "app_qa"
    } else if intent.contains("observe")
        || intent.contains("monitor")
        || intent.contains("watch")
        || intent.contains("status")
    {
        "observe"
    } else if intent.contains("clean")
        || intent.contains("cleanup")
        || intent.contains("delete stopped")
        || intent.contains("stale")
    {
        "cleanup"
    } else {
        "unknown"
    }
}

fn task_intent_is_shopping_or_grocery(intent: &str) -> bool {
    let intent = intent.trim().to_ascii_lowercase();
    intent.contains("grocery")
        || intent.contains("groceries")
        || intent.contains("shopping")
        || intent.contains("shop")
        || intent.contains("buy")
        || intent.contains("purchase")
        || intent.contains("cart")
        || intent.contains("checkout")
        || intent.contains("order")
        || intent.contains("delivery")
}

fn option_has_text(value: &Option<String>) -> bool {
    value
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
}

fn mcp_task_plan_task_context(
    params: &McpTaskPlanParams,
    normalized_intent: &str,
    workspace_id: &str,
    workspace_running: bool,
) -> McpTaskPlanTaskContext {
    let mut provided_inputs = Vec::new();
    if let Some(input) = task_context_optional_string("profile_id", params.profile_id.as_deref()) {
        provided_inputs.push(input);
    }
    if let Some(input) = task_context_optional_path("project_path", params.project_path.as_ref()) {
        provided_inputs.push(input);
    }
    if let Some(input) = task_context_optional_path("browser_path", params.browser_path.as_ref()) {
        provided_inputs.push(input);
    }
    if let Some(input) = task_context_optional_path("user_data_dir", params.user_data_dir.as_ref())
    {
        provided_inputs.push(input);
    }
    for (name, value) in [
        ("target_url", params.target_url.as_deref()),
        ("shopping_list", params.shopping_list.as_deref()),
        ("budget", params.budget.as_deref()),
        ("fulfillment", params.fulfillment.as_deref()),
        ("substitution_policy", params.substitution_policy.as_deref()),
    ] {
        if let Some(input) = task_context_optional_string(name, value) {
            provided_inputs.push(input);
        }
    }
    for (name, value) in [
        ("cart_mutation_approved", params.cart_mutation_approved),
        ("final_cart_reviewed", params.final_cart_reviewed),
        (
            "real_world_action_approved",
            params.real_world_action_approved,
        ),
    ] {
        if let Some(input) = task_context_optional_bool(name, value) {
            provided_inputs.push(input);
        }
    }

    let mut missing_inputs = Vec::new();
    match normalized_intent {
        "app_qa" if params.profile_id.is_none() && params.project_path.is_none() => {
            missing_inputs.push(McpTaskPlanTaskInputNeed {
                name: "project_source".to_string(),
                reason:
                    "Provide profile_id or project_path before generating a project QA workspace."
                        .to_string(),
            });
        }
        "browser_task" => {
            if params.profile_id.is_none() && params.user_data_dir.is_none() {
                missing_inputs.push(McpTaskPlanTaskInputNeed {
                    name: "browser_user_data".to_string(),
                    reason: "Provide profile_id or an explicit user-approved user_data_dir before starting a browser-session workspace."
                        .to_string(),
                });
            }
            missing_inputs.extend(mcp_task_plan_shopping_required_input_needs(params));
        }
        _ => {}
    }

    let mut safety_boundaries = Vec::new();
    if normalized_intent == "browser_task" {
        safety_boundaries.push(
            "Browser/account-data exposure requires explicit profile review before start."
                .to_string(),
        );
        safety_boundaries.push(
            "Use workspace-owned Chrome DevTools when the workspace browser exposes DevToolsActivePort; do not target the user's host Chrome bridge for hidden-workspace browser control."
                .to_string(),
        );
        safety_boundaries.push(
            "Cart mutation can be approved separately from checkout or other real-world actions."
                .to_string(),
        );
        safety_boundaries.push(
            "Purchases, checkout, order submission, and account changes require separate explicit approval."
                .to_string(),
        );
    }
    if normalized_intent == "cleanup" {
        safety_boundaries.push(
            "Cleanup deletes stopped workspace runtime files only after a dry-run preview."
                .to_string(),
        );
    }
    let action_boundaries =
        mcp_task_plan_action_boundaries(params, normalized_intent, workspace_running);

    McpTaskPlanTaskContext {
        task_kind: normalized_intent.to_string(),
        workspace_id: workspace_id.to_string(),
        provided_inputs,
        missing_inputs,
        safety_boundaries,
        action_boundaries,
        approval_kinds: Vec::new(),
    }
}

fn mcp_task_plan_action_boundaries(
    params: &McpTaskPlanParams,
    normalized_intent: &str,
    workspace_running: bool,
) -> Vec<McpTaskPlanActionBoundary> {
    if normalized_intent == "app_qa" {
        return mcp_task_plan_app_qa_action_boundaries(params, workspace_running);
    }
    if normalized_intent != "browser_task" {
        return Vec::new();
    }

    let shopping_intent = task_intent_is_shopping_or_grocery(&params.intent);
    let mut boundaries = vec![mcp_task_plan_action_boundary(
        "observe_browser_state",
        "Observe browser state",
        "read_only_observation",
        true,
        false,
        None,
        Vec::new(),
        "Screenshots, app/window listing, logs, and workspace events are safe first steps before browser input.",
    )];

    let navigation_inputs = if shopping_intent && !option_has_text(&params.target_url) {
        vec!["target_url".to_string()]
    } else {
        Vec::new()
    };
    boundaries.push(mcp_task_plan_action_boundary(
        "navigate_and_search",
        "Navigate and search",
        "browser_navigation",
        navigation_inputs.is_empty(),
        false,
        None,
        navigation_inputs,
        "Opening the target site, searching, filtering, and reading product pages can proceed after the site target is known.",
    ));

    if shopping_intent {
        let shopping_inputs = missing_named_shopping_inputs(params);
        let cart_missing_approvals = if params.cart_mutation_approved {
            Vec::new()
        } else {
            vec!["explicit_cart_mutation_approval".to_string()]
        };
        let mut checkout_inputs = Vec::new();
        let mut checkout_missing_approvals = Vec::new();
        if !params.final_cart_reviewed {
            checkout_inputs.push("final_cart_review".to_string());
            checkout_missing_approvals.push("final_cart_review".to_string());
        }
        if !params.real_world_action_approved {
            checkout_inputs.push("explicit_checkout_approval".to_string());
            checkout_missing_approvals.push("explicit_checkout_approval".to_string());
        }
        let checkout_approved = params.final_cart_reviewed && params.real_world_action_approved;
        boundaries.push(mcp_task_plan_action_boundary(
            "compare_items_and_prices",
            "Compare items and prices",
            "shopping_research",
            shopping_inputs.is_empty(),
            false,
            None,
            shopping_inputs.clone(),
            "Product comparison should use the user's list, budget, fulfillment, and substitution preferences before choosing candidates.",
        ));
        boundaries.push(
            mcp_task_plan_action_boundary(
                "draft_cart_changes",
                "Draft cart changes",
                "cart_mutation",
                shopping_inputs.is_empty(),
                true,
                Some("cart_mutation"),
                shopping_inputs,
                "Adding, removing, or changing cart items mutates store/account state; get approval for the cart-building scope before making those changes.",
            )
            .with_approval_state(params.cart_mutation_approved, cart_missing_approvals),
        );
        boundaries.push(
            mcp_task_plan_action_boundary(
                "checkout_order_or_account_change",
                "Checkout, order, or account change",
                "real_world_action",
                checkout_inputs.is_empty(),
                true,
                Some("real_world_action"),
                checkout_inputs,
                "Checkout, payment, order submission, address changes, subscription changes, and account edits require separate explicit approval for that specific action.",
            )
            .with_approval_state(checkout_approved, checkout_missing_approvals),
        );
    } else {
        let missing_approvals = if params.real_world_action_approved {
            Vec::new()
        } else {
            vec!["explicit_action_approval".to_string()]
        };
        boundaries.push(
            mcp_task_plan_action_boundary(
                "account_or_form_change",
                "Account or form change",
                "real_world_action",
                params.real_world_action_approved,
                true,
                Some("real_world_action"),
                missing_approvals.clone(),
                "Submitting forms, changing account state, or committing changes on a logged-in site requires separate explicit approval.",
            )
            .with_approval_state(params.real_world_action_approved, missing_approvals),
        );
    }

    boundaries
}

fn mcp_task_plan_app_qa_action_boundaries(
    params: &McpTaskPlanParams,
    workspace_running: bool,
) -> Vec<McpTaskPlanActionBoundary> {
    let has_project_source = params.profile_id.is_some() || params.project_path.is_some();
    let project_source_inputs = if workspace_running || has_project_source {
        Vec::new()
    } else {
        vec!["project_source".to_string()]
    };
    let workspace_inputs = if workspace_running {
        Vec::new()
    } else {
        vec!["running_workspace".to_string()]
    };
    let mut target_inputs = workspace_inputs.clone();
    target_inputs.push("stable_app_id_or_window".to_string());

    vec![
        mcp_task_plan_action_boundary(
            "observe_project_state",
            "Observe project state",
            "read_only_observation",
            true,
            false,
            None,
            Vec::new(),
            "Screenshots, app/window listing, logs, and workspace events are safe first steps before app input.",
        ),
        mcp_task_plan_action_boundary(
            "start_or_attach_project_workspace",
            "Start or attach project workspace",
            "hidden_workspace_start",
            workspace_running || has_project_source,
            !workspace_running,
            if workspace_running {
                None
            } else {
                Some("hidden_workspace")
            },
            project_source_inputs,
            "A fresh app-QA run needs a saved profile or project_path plus hidden-workspace approval; an already-running workspace can be observed without starting another one.",
        ),
        mcp_task_plan_action_boundary(
            "collect_qa_evidence",
            "Collect QA evidence",
            "read_only_evidence",
            workspace_running,
            false,
            None,
            workspace_inputs,
            "After the workspace is running, collect events, logs, and targeted screenshots before driving the app or reporting results.",
        ),
        mcp_task_plan_action_boundary(
            "drive_workspace_app",
            "Drive workspace app",
            "workspace_input",
            false,
            false,
            None,
            target_inputs,
            "Keyboard, pointer, clipboard, and window actions are scoped to the isolated workspace, but they should wait for observation and a stable app/window target.",
        ),
        mcp_task_plan_action_boundary(
            "write_mounted_project_files",
            "Write mounted project files",
            "project_file_mutation",
            false,
            true,
            Some("project_file_write"),
            vec!["explicit_code_change_request".to_string()],
            "Changing files in a mounted project is separate from observing or driving the app; do it only when the user asked for code/file changes.",
        ),
    ]
}

#[allow(clippy::too_many_arguments)]
fn mcp_task_plan_action_boundary(
    id: &str,
    label: &str,
    action_type: &str,
    ready: bool,
    approval_required: bool,
    approval_kind: Option<&str>,
    required_inputs: Vec<String>,
    reason: &str,
) -> McpTaskPlanActionBoundary {
    McpTaskPlanActionBoundary {
        id: id.to_string(),
        label: label.to_string(),
        action_type: action_type.to_string(),
        ready,
        approval_required,
        approved: !approval_required,
        approval_kind: approval_kind.map(str::to_string),
        required_inputs,
        missing_approvals: Vec::new(),
        reason: reason.to_string(),
    }
}

impl McpTaskPlanActionBoundary {
    fn with_approval_state(mut self, approved: bool, missing_approvals: Vec<String>) -> Self {
        self.approved = approved;
        self.missing_approvals = missing_approvals;
        self
    }
}

fn missing_named_shopping_inputs(params: &McpTaskPlanParams) -> Vec<String> {
    let mut inputs = Vec::new();
    if !option_has_text(&params.target_url) {
        inputs.push("target_url".to_string());
    }
    if !option_has_text(&params.shopping_list) {
        inputs.push("shopping_list".to_string());
    }
    if !option_has_text(&params.fulfillment) {
        inputs.push("fulfillment".to_string());
    }
    if !option_has_text(&params.substitution_policy) {
        inputs.push("substitution_policy".to_string());
    }
    if !option_has_text(&params.budget) {
        inputs.push("budget".to_string());
    }
    inputs
}

fn task_context_optional_string(name: &str, value: Option<&str>) -> Option<McpTaskPlanTaskInput> {
    let value = value?.trim();
    if value.is_empty() {
        None
    } else {
        Some(McpTaskPlanTaskInput {
            name: name.to_string(),
            value: value.to_string(),
        })
    }
}

fn task_context_optional_path(name: &str, value: Option<&PathBuf>) -> Option<McpTaskPlanTaskInput> {
    value.map(|value| McpTaskPlanTaskInput {
        name: name.to_string(),
        value: value.display().to_string(),
    })
}

fn task_context_optional_bool(name: &str, value: bool) -> Option<McpTaskPlanTaskInput> {
    value.then(|| McpTaskPlanTaskInput {
        name: name.to_string(),
        value: "true".to_string(),
    })
}

fn mcp_task_plan_shopping_required_input_needs(
    params: &McpTaskPlanParams,
) -> Vec<McpTaskPlanTaskInputNeed> {
    if !task_intent_is_shopping_or_grocery(&params.intent) {
        return Vec::new();
    }
    let mut inputs = Vec::new();
    if !option_has_text(&params.target_url) {
        inputs.push(McpTaskPlanTaskInputNeed {
            name: "target_url".to_string(),
            reason: "Provide target_url or the store/site where the shopping workflow should run."
                .to_string(),
        });
    }
    if !option_has_text(&params.shopping_list) {
        inputs.push(McpTaskPlanTaskInputNeed {
            name: "shopping_list".to_string(),
            reason:
                "Provide shopping_list with items, quantities, and important brand or size preferences."
                    .to_string(),
        });
    }
    if !option_has_text(&params.fulfillment) {
        inputs.push(McpTaskPlanTaskInputNeed {
            name: "fulfillment".to_string(),
            reason: "Specify fulfillment preference such as delivery, pickup, or browsing only, including timing or location constraints."
                .to_string(),
        });
    }
    if !option_has_text(&params.substitution_policy) {
        inputs.push(McpTaskPlanTaskInputNeed {
            name: "substitution_policy".to_string(),
            reason: "Specify substitution_policy, including must-have items, acceptable replacements, and items to skip if unavailable."
                .to_string(),
        });
    }
    if !option_has_text(&params.budget) {
        inputs.push(McpTaskPlanTaskInputNeed {
            name: "budget".to_string(),
            reason: "Provide budget or payment constraints before cart comparison or checkout; final purchase still requires separate explicit approval."
                .to_string(),
        });
    }
    inputs
}

fn mcp_task_plan_shopping_required_input_reasons(params: &McpTaskPlanParams) -> Vec<String> {
    mcp_task_plan_shopping_required_input_needs(params)
        .into_iter()
        .map(|need| need.reason)
        .collect()
}

fn preferred_profile_id(
    requested: Option<&str>,
    profile_ids: &[String],
    preferred_markers: &[&str],
) -> Option<String> {
    if let Some(requested) = requested {
        if profile_ids.iter().any(|id| id == requested) {
            return Some(requested.to_string());
        }
    }
    preferred_markers.iter().find_map(|marker| {
        profile_ids
            .iter()
            .find(|id| id.to_ascii_lowercase().contains(marker))
            .cloned()
    })
}

fn inferred_profile_task_intent(profile_id: &str) -> (&'static str, &'static str, &'static str) {
    let normalized = profile_id.to_ascii_lowercase();
    if ["browser", "shopping", "grocery", "chrome", "firefox"]
        .iter()
        .any(|marker| normalized.contains(marker))
    {
        (
            "grocery shopping",
            "Plan browser or shopping workflow",
            "A saved browser-style profile exists; use the planner to surface browser-data, account, checkout, and live-control approval boundaries before starting it.",
        )
    } else if ["project", "qa", "app", "desktop", "dev"]
        .iter()
        .any(|marker| normalized.contains(marker))
    {
        (
            "app QA",
            "Plan app QA workflow",
            "A saved project/app-style profile exists; use the planner to preview setup/startup and approval boundaries before starting it.",
        )
    } else {
        (
            "app QA",
            "Plan saved-profile workflow",
            "A saved profile exists; use the planner to derive the safest start sequence and approval boundaries before running it.",
        )
    }
}

fn profile_template_permission_blockers(
    kind: &str,
    host_path: Option<PathBuf>,
    browser_path: Option<PathBuf>,
    user_data_dir: Option<PathBuf>,
    permissions: &McpPermissionState,
) -> Vec<String> {
    match profile::template_profile(
        kind,
        Some(kind.to_string()),
        host_path,
        browser_path,
        user_data_dir,
    ) {
        Ok(profile) => match permissions.validate_profile(&profile) {
            Ok(()) => Vec::new(),
            Err(error) => vec![format!(
                "Generated {kind} profile exceeds the active MCP permission ceiling: {error}"
            )],
        },
        Err(error) => vec![format!("Generated {kind} profile is not usable: {error}")],
    }
}

fn saved_profile_permission_blockers(
    profile_id: &str,
    permissions: &McpPermissionState,
) -> Vec<String> {
    match profile::get_profile(profile_id) {
        Ok(profile) => match permissions.validate_profile(&profile) {
            Ok(()) => Vec::new(),
            Err(error) => vec![format!(
                "Saved profile {profile_id:?} exceeds the active MCP permission ceiling: {error}"
            )],
        },
        Err(error) => vec![format!(
            "Saved profile {profile_id:?} is not usable: {error}"
        )],
    }
}

#[allow(clippy::too_many_arguments)]
fn mcp_task_step(
    id: &str,
    order: u8,
    label: &str,
    tool: &str,
    arguments: serde_json::Value,
    ready_to_call: bool,
    reason: &str,
    approval_hint: &str,
    read_only: bool,
    mutating: bool,
    destructive: bool,
    open_world: bool,
    blocked_by_live_control: bool,
) -> McpTaskPlanStep {
    McpTaskPlanStep {
        id: id.to_string(),
        order,
        label: label.to_string(),
        tool: tool.to_string(),
        arguments,
        ready_to_call,
        reason: reason.to_string(),
        approval_hint: approval_hint.to_string(),
        read_only,
        mutating,
        destructive,
        open_world,
        blocked_by_live_control,
        permission_blockers: Vec::new(),
        depends_on: Vec::new(),
        required_input: Vec::new(),
    }
}

impl McpTaskPlanStep {
    fn with_permission_blockers(mut self, blockers: Vec<String>) -> Self {
        self.permission_blockers = blockers;
        self
    }

    fn with_dependency(mut self, step_id: &str) -> Self {
        self.depends_on.push(step_id.to_string());
        self
    }

    fn with_required_input(mut self, input: &str) -> Self {
        self.required_input.push(input.to_string());
        self
    }

    fn with_required_inputs(mut self, inputs: &[String]) -> Self {
        self.required_input.extend(inputs.iter().cloned());
        self
    }
}

fn mcp_task_plan_approval_checkpoints(
    steps: &[McpTaskPlanStep],
) -> Vec<McpTaskPlanApprovalCheckpoint> {
    let mut checkpoints = Vec::new();
    for step in steps {
        if !step.permission_blockers.is_empty() {
            checkpoints.push(mcp_approval_checkpoint(
                step,
                "permission_ceiling",
                "Resolve MCP permission ceiling",
                false,
                true,
                Vec::new(),
                step.permission_blockers.clone(),
            ));
        }
        if !step.required_input.is_empty() {
            checkpoints.push(mcp_approval_checkpoint(
                step,
                "required_input",
                "Collect required user input",
                true,
                !step.ready_to_call,
                step.required_input.clone(),
                Vec::new(),
            ));
        }
        if mcp_step_is_preview_surface(step) {
            checkpoints.push(mcp_approval_checkpoint(
                step,
                "preview_surface",
                "Use preview as approval surface",
                false,
                false,
                Vec::new(),
                Vec::new(),
            ));
        }
        if step.blocked_by_live_control {
            checkpoints.push(mcp_approval_checkpoint(
                step,
                "live_control",
                "Reactivate live MCP control",
                true,
                true,
                live_control_reactivation_required_inputs(),
                Vec::new(),
            ));
        }
        if step.mutating {
            let (kind, label) = if step.destructive {
                ("destructive_action", "Approve destructive action")
            } else if mcp_step_is_profile_write(step) {
                ("profile_write", "Approve profile write")
            } else if mcp_step_is_workspace_start(step) {
                ("hidden_workspace", "Approve hidden workspace")
            } else {
                ("mutating_action", "Approve mutating action")
            };
            checkpoints.push(mcp_approval_checkpoint(
                step,
                kind,
                label,
                true,
                !step.ready_to_call,
                Vec::new(),
                Vec::new(),
            ));
        }
        if step.open_world {
            checkpoints.push(mcp_approval_checkpoint(
                step,
                "host_visible_ui",
                "Approve host-visible UI",
                true,
                !step.ready_to_call,
                Vec::new(),
                Vec::new(),
            ));
        }
        if mcp_step_mentions_real_world_action(step) {
            checkpoints.push(mcp_approval_checkpoint(
                step,
                "real_world_action",
                "Require separate real-world approval",
                true,
                false,
                Vec::new(),
                Vec::new(),
            ));
        }
    }
    checkpoints
}

fn mcp_task_plan_approval_kinds(
    checkpoints: &[McpTaskPlanApprovalCheckpoint],
    action_boundaries: &[McpTaskPlanActionBoundary],
) -> Vec<String> {
    checkpoints
        .iter()
        .map(|checkpoint| checkpoint.kind.clone())
        .chain(
            action_boundaries
                .iter()
                .filter(|boundary| boundary.approval_required)
                .filter_map(|boundary| boundary.approval_kind.clone()),
        )
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn mcp_task_plan_approval_summary(
    checkpoints: &[McpTaskPlanApprovalCheckpoint],
    approval_kinds: &[String],
) -> McpTaskPlanApprovalSummary {
    let next_boundary = checkpoints
        .iter()
        .find(|checkpoint| checkpoint.blocks_step)
        .or_else(|| {
            checkpoints
                .iter()
                .find(|checkpoint| checkpoint.approval_required)
        })
        .map(McpTaskPlanApprovalBoundary::from);

    McpTaskPlanApprovalSummary {
        blocking_count: checkpoints
            .iter()
            .filter(|checkpoint| checkpoint.blocks_step)
            .count(),
        approval_required_count: checkpoints
            .iter()
            .filter(|checkpoint| checkpoint.approval_required)
            .count(),
        approval_kinds: approval_kinds.to_vec(),
        next_boundary,
    }
}

fn mcp_approval_checkpoint(
    step: &McpTaskPlanStep,
    kind: &str,
    label: &str,
    approval_required: bool,
    blocks_step: bool,
    required_input: Vec<String>,
    permission_blockers: Vec<String>,
) -> McpTaskPlanApprovalCheckpoint {
    McpTaskPlanApprovalCheckpoint {
        id: format!("{}:{kind}", step.id),
        step_id: step.id.clone(),
        order: step.order,
        kind: kind.to_string(),
        label: label.to_string(),
        tool: step.tool.clone(),
        approval_required,
        blocks_step,
        approval_hint: step.approval_hint.clone(),
        required_input,
        permission_blockers,
    }
}

fn mcp_step_is_preview_surface(step: &McpTaskPlanStep) -> bool {
    matches!(
        step.arguments
            .get("dry_run")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    )
}

fn mcp_step_is_profile_write(step: &McpTaskPlanStep) -> bool {
    matches!(
        step.tool.as_str(),
        "profile_put" | "profile_import" | "profile_delete"
    )
}

fn mcp_step_is_workspace_start(step: &McpTaskPlanStep) -> bool {
    matches!(
        step.tool.as_str(),
        "workspace_start" | "workspace_open_profile"
    )
}

fn mcp_step_mentions_real_world_action(step: &McpTaskPlanStep) -> bool {
    step.required_input
        .iter()
        .chain(std::iter::once(&step.approval_hint))
        .any(|text| {
            let text = text.to_ascii_lowercase();
            text.contains("purchase")
                || text.contains("checkout")
                || text.contains("order submission")
                || text.contains("account changes")
        })
}

fn build_mcp_session_brief(permissions: &McpPermissionState, headless: bool) -> McpSessionBrief {
    let mut warnings = Vec::new();
    let doctor = workspace::doctor_report();
    let control = read_mcp_control_brief();

    let profiles = match profile::list_profiles() {
        Ok(list) => McpSessionProfilesBrief {
            count: list.profiles.len(),
            ids: list
                .profiles
                .iter()
                .take(8)
                .map(|profile| profile.id.clone())
                .collect(),
            error: None,
        },
        Err(error) => {
            warnings.push(format!("profile_list failed: {error}"));
            McpSessionProfilesBrief {
                count: 0,
                ids: Vec::new(),
                error: Some(error.to_string()),
            }
        }
    };

    let workspaces = match workspace::list_workspaces() {
        Ok(list) => {
            let running_ids: Vec<String> = list
                .workspaces
                .iter()
                .filter(|workspace| workspace.running)
                .take(8)
                .map(|workspace| workspace.id.clone())
                .collect();
            let stopped_ids: Vec<String> = list
                .workspaces
                .iter()
                .filter(|workspace| !workspace.running)
                .take(8)
                .map(|workspace| workspace.id.clone())
                .collect();
            McpSessionWorkspacesBrief {
                count: list.workspaces.len(),
                running_count: list
                    .workspaces
                    .iter()
                    .filter(|workspace| workspace.running)
                    .count(),
                stopped_count: list
                    .workspaces
                    .iter()
                    .filter(|workspace| !workspace.running)
                    .count(),
                suggested_workspace_id: running_ids
                    .first()
                    .or_else(|| stopped_ids.first())
                    .cloned()
                    .or_else(|| Some(DEFAULT_WORKSPACE_ID.to_string())),
                running_ids,
                stopped_ids,
                activity: list
                    .workspaces
                    .iter()
                    .filter(|workspace| workspace.running || workspace.manifest.is_some())
                    .take(8)
                    .map(mcp_workspace_activity_brief)
                    .collect(),
                error: None,
            }
        }
        Err(error) => {
            warnings.push(format!("workspace_list failed: {error}"));
            McpSessionWorkspacesBrief {
                count: 0,
                running_count: 0,
                stopped_count: 0,
                running_ids: Vec::new(),
                stopped_ids: Vec::new(),
                activity: Vec::new(),
                suggested_workspace_id: Some(DEFAULT_WORKSPACE_ID.to_string()),
                error: Some(error.to_string()),
            }
        }
    };

    let mut recommendations = Vec::new();
    recommendations.push(mcp_recommendation(
        "capture_agent_context",
        100,
        "orient_agent",
        "Read compact agent context",
        "mcp_agent_context",
        serde_json::json!({}),
        "Get active/read_only/paused mode, headless/no-host-display status, viewer state, and stable app_id/window_id/viewer_id/browser_target_id handles in one low-noise snapshot.",
        "Read-only; no user approval required.",
        true,
        false,
        false,
    ));
    if permissions.restricted {
        recommendations.push(mcp_recommendation(
            "review_permission_ceiling",
            95,
            "understand_boundary",
            "Review MCP permission ceiling",
            "mcp_permissions",
            serde_json::json!({}),
            "This MCP process has an immutable spawn-time ceiling; stay inside it before planning starts, launches, or profile changes.",
            "Read-only; no user approval required.",
            true,
            false,
            false,
        ));
    } else {
        recommendations.push(mcp_recommendation(
            "classify_action_before_acting",
            90,
            "understand_boundary",
            "Use action catalog before mutating",
            "mcp_action_catalog",
            serde_json::json!({}),
            "No restrictive MCP ceiling is active, so the agent should classify read-only, mutating, destructive, and open-world actions before acting.",
            "Read-only; no user approval required.",
            true,
            false,
            false,
        ));
    }

    if !control.allows_agent_mutation {
        recommendations.push(mcp_recommendation(
            "respect_live_control",
            100,
            "pause_or_read_only",
            "Stay read-only until confirmed reactivation",
            "mcp_control_state",
            serde_json::json!({}),
            &format!(
                "Live MCP control is not active; mutating agent actions are blocked while observation and safety stop remain available. {}",
                live_control_reactivation_hint()
            ),
            "Read-only; reactivate through mcp_control_update only with confirmed_user_request=true after an explicit user or control-UI request.",
            true,
            false,
            false,
        ));
    }

    if !doctor.ready_for_x11_workspace {
        recommendations.push(mcp_recommendation(
            "inspect_runtime_blockers",
            100,
            "fix_runtime",
            "Inspect runtime blockers",
            "workspace_doctor",
            serde_json::json!({}),
            "The runtime is not ready to start an X11 workspace.",
            "Read-only; no user approval required.",
            true,
            false,
            false,
        ));
    } else if workspaces.running_count > 0 {
        let workspace_id = workspaces
            .running_ids
            .first()
            .cloned()
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        recommendations.push(mcp_recommendation(
            "observe_running_workspace",
            100,
            "observe_agent_work",
            "Observe the running workspace",
            "workspace_observe",
            serde_json::json!({
                "id": workspace_id,
                "screenshot": true,
                "include_events": true,
                "events_tail": 20
            }),
            "A workspace is already running; inspect the live screen, recent events, active window, and app state before injecting input.",
            "Read-only observation; no user approval required.",
            true,
            false,
            false,
        ));
        recommendations.push(mcp_recommendation(
            "plan_running_observation",
            96,
            "derive_user_intent",
            "Plan observation workflow",
            "mcp_task_plan",
            serde_json::json!({
                "intent": "observe",
                "workspace_id": workspace_id
            }),
            "If the next user intent is unclear, get a read-only observation plan before injecting input into the running workspace.",
            "Read-only planning; no user approval required.",
            true,
            false,
            false,
        ));
        if let Some(activity) = workspaces
            .activity
            .iter()
            .find(|activity| activity.id == workspace_id)
        {
            if let Some(inferred_intent) = activity.inferred_intent.as_deref() {
                let mut arguments = serde_json::json!({
                    "intent": inferred_intent,
                    "workspace_id": workspace_id
                });
                if let Some(profile_id) = &activity.profile_id {
                    arguments["profile_id"] = serde_json::json!(profile_id);
                }
                recommendations.push(mcp_recommendation(
                    "plan_running_workspace_task",
                    97,
                    "derive_user_intent",
                    activity
                        .intent_label
                        .as_deref()
                        .unwrap_or("Plan running workspace task"),
                    "mcp_task_plan",
                    arguments,
                    activity.intent_reason.as_deref().unwrap_or(
                        "The running workspace activity suggests a task-specific plan before further action.",
                    ),
                    "Read-only planning; use before injecting input or changing running apps.",
                    true,
                    false,
                    false,
                ));
            }
        }
        if !headless && doctor.ready_for_host_viewer {
            let workspace_id = workspaces
                .running_ids
                .first()
                .cloned()
                .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
            recommendations.push(mcp_recommendation(
                "open_live_viewer",
                80,
                "user_visibility",
                "Open the floating live viewer",
                "workspace_open_viewer",
                serde_json::json!({ "id": workspace_id }),
                "A host-visible GPUI viewer can let the user monitor and control the hidden workspace while continuing other work.",
                "Open-world host-visible UI; use only when the user or host wants the viewer.",
                false,
                true,
                false,
            ));
        }
        let workspace_id = workspaces
            .running_ids
            .first()
            .cloned()
            .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());
        recommendations.push(mcp_recommendation(
            "safety_stop_available",
            60,
            "stop_agent_work",
            "Safety stop remains available",
            "workspace_stop",
            serde_json::json!({ "id": workspace_id, "dry_run": true }),
            "Preview the apps that would be terminated before stopping; the real stop path remains available even in read-only or paused mode.",
            "Dry-run is read-only; real stop is destructive but allowed as a safety stop.",
            true,
            false,
            false,
        ));
    } else if doctor.ready_for_x11_workspace {
        if let Some(profile_id) = profiles.ids.first() {
            let (intent, intent_label, intent_reason) = inferred_profile_task_intent(profile_id);
            recommendations.push(mcp_recommendation(
                "plan_saved_profile_task",
                98,
                "derive_user_intent",
                intent_label,
                "mcp_task_plan",
                serde_json::json!({
                    "intent": intent,
                    "profile_id": profile_id
                }),
                intent_reason,
                "Read-only planning; use before previewing or running the saved profile.",
                true,
                false,
                false,
            ));
            recommendations.push(mcp_recommendation(
                "preview_profile_workspace",
                90,
                "start_user_task",
                "Preview starting a saved profile",
                "workspace_open_profile",
                serde_json::json!({
                    "profile": profile_id,
                    "dry_run": true,
                    "purpose": "Agent workspace task",
                    "setup": true,
                    "setup_timeout_ms": 30000,
                    "setup_kill_on_timeout": true,
                    "startup_wait_window": true
                }),
                "A saved profile exists; dry-run the full start/setup/startup plan so a host UI can show one approval surface before execution.",
                "Dry-run preview is allowed; real start requires active control plus hidden-workspace acknowledgement.",
                false,
                false,
                false,
            ));
        } else {
            recommendations.push(mcp_recommendation(
                "plan_app_qa_without_profile",
                95,
                "derive_user_intent",
                "Plan app QA workspace",
                "mcp_task_plan",
                serde_json::json!({
                    "intent": "app QA"
                }),
                "No saved profile exists; ask the planner for the project/profile inputs needed for app QA before starting a default workspace.",
                "Read-only planning; no user approval required.",
                true,
                false,
                false,
            ));
            recommendations.push(mcp_recommendation(
                "preview_default_workspace",
                85,
                "start_user_task",
                "Preview starting a default workspace",
                "workspace_start",
                serde_json::json!({
                    "id": DEFAULT_WORKSPACE_ID,
                    "dry_run": true,
                    "purpose": "Agent workspace task"
                }),
                "No saved profile was found; dry-run the default hidden workspace start before asking for acknowledgement.",
                "Dry-run preview is allowed; real start requires active control plus hidden-workspace acknowledgement.",
                false,
                false,
                false,
            ));
            recommendations.push(mcp_recommendation(
                "plan_browser_or_grocery_task",
                75,
                "derive_user_intent",
                "Plan browser or grocery workflow",
                "mcp_task_plan",
            serde_json::json!({
                "intent": "grocery shopping"
            }),
            "For browser, shopping, or grocery work, get the read-only plan that requests explicit browser profile input and separate checkout, purchase, or account-change approval before any logged-in web session starts.",
            "Read-only planning; no user approval required.",
            true,
            false,
                false,
            ));
        }
    }

    if workspaces.stopped_count > 0 {
        recommendations.push(mcp_recommendation(
            "plan_cleanup",
            55,
            "clean_stopped_workspaces",
            "Plan cleanup flow",
            "mcp_task_plan",
            serde_json::json!({
                "intent": "cleanup",
                "workspace_id": workspaces
                    .stopped_ids
                    .first()
                    .cloned()
                    .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string())
            }),
            "Stopped workspace artifacts exist; use the cleanup plan to pair dry-run preview, approval, destructive cleanup, and verification.",
            "Read-only planning; no user approval required.",
            true,
            false,
            false,
        ));
        recommendations.push(mcp_recommendation(
            "preview_stale_cleanup",
            50,
            "clean_stopped_workspaces",
            "Preview cleanup for stopped workspaces",
            "workspace_cleanup_stale",
            serde_json::json!({ "dry_run": true }),
            "Stopped workspace artifacts exist; preview cleanup before deleting runtime files.",
            "Dry-run is read-only; real cleanup is destructive and requires active live control.",
            true,
            false,
            false,
        ));
    }

    recommendations.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| left.id.cmp(&right.id))
    });
    let approval_summary = mcp_session_approval_summary(&recommendations);

    McpSessionBrief {
        version: 1,
        headless,
        permissions: permissions.clone(),
        control,
        doctor: McpSessionDoctorBrief {
            ready_for_x11_workspace: doctor.ready_for_x11_workspace,
            ready_for_host_viewer: doctor.ready_for_host_viewer,
            recommended_next_step: doctor.recommended_next_step,
            blockers: doctor.blockers,
            viewer_blockers: doctor.viewer_blockers,
        },
        profiles,
        workspaces,
        recommendations,
        approval_summary,
        warnings,
    }
}

fn build_mcp_agent_context(
    params: McpAgentContextParams,
    permissions: &McpPermissionState,
    headless: bool,
) -> McpAgentContext {
    let mut warnings = Vec::new();
    let mode = build_agent_mode_summary(headless);
    let workspace_list = match workspace::list_workspaces() {
        Ok(list) => Some(list),
        Err(error) => {
            warnings.push(format!("workspace_list failed: {error}"));
            None
        }
    };
    let workspace_id = params
        .workspace_id
        .clone()
        .or_else(|| {
            workspace_list.as_ref().and_then(|list| {
                list.workspaces
                    .iter()
                    .find(|entry| entry.running)
                    .or_else(|| list.workspaces.first())
                    .map(|entry| entry.id.clone())
            })
        })
        .unwrap_or_else(|| DEFAULT_WORKSPACE_ID.to_string());

    let status = match workspace::status_workspace(&workspace_id) {
        Ok(status) => Some(status),
        Err(error) => {
            if workspace_list
                .as_ref()
                .is_none_or(|list| list.workspaces.iter().any(|entry| entry.id == workspace_id))
            {
                warnings.push(format!(
                    "workspace_status failed for {workspace_id}: {error}"
                ));
            }
            workspace_list.as_ref().and_then(|list| {
                list.workspaces
                    .iter()
                    .find(|entry| entry.id == workspace_id)
                    .and_then(|entry| entry.status.clone())
            })
        }
    };
    let running = status.is_some()
        || workspace_list.as_ref().is_some_and(|list| {
            list.workspaces
                .iter()
                .any(|entry| entry.id == workspace_id && entry.running)
        });

    let mut handles = AgentTargetHandles {
        workspace_id: Some(workspace_id.clone()),
        ..Default::default()
    };
    let mut active_window_ref = None;
    let mut window_refs = Vec::new();
    if running {
        match workspace::active_window(&workspace_id) {
            Ok(response) => {
                if let Some(window) = response.active_window {
                    active_window_ref = Some(mcp_agent_window_ref(&window));
                    push_unique(&mut handles.window_ids, window.id.clone());
                    if let Some(app_id) = window.app_id {
                        push_unique(&mut handles.app_ids, app_id);
                    }
                }
            }
            Err(error) => warnings.push(format!(
                "workspace_active_window failed for {workspace_id}: {error}"
            )),
        }
        match workspace::list_windows(&workspace_id, true, None, None, None, None) {
            Ok(response) => {
                if let Some(windows) = response.windows {
                    for window in windows.into_iter().take(8) {
                        push_unique(&mut handles.window_ids, window.id.clone());
                        if let Some(app_id) = window.app_id.clone() {
                            push_unique(&mut handles.app_ids, app_id);
                        }
                        window_refs.push(mcp_agent_window_ref(&window));
                    }
                }
            }
            Err(error) => warnings.push(format!(
                "workspace_list_windows failed for {workspace_id}: {error}"
            )),
        }
    }

    let apps = status
        .as_ref()
        .map(|status| status.apps.clone())
        .unwrap_or_default();
    for app in &apps {
        push_unique(&mut handles.app_ids, app.id.clone());
    }
    let active_app_id = active_window_ref
        .as_ref()
        .and_then(|window| window.app_id.clone())
        .or_else(|| {
            apps.iter()
                .find(|app| app.running)
                .map(|app| app.id.clone())
        });

    let browser = if running {
        match browser::workspace_browser_targets(
            &workspace_id,
            None,
            None,
            Some(params.browser_timeout_ms.unwrap_or(500).min(5_000)),
        ) {
            Ok(targets) => {
                for target_id in &targets.browser_target_ids {
                    push_unique(&mut handles.browser_target_ids, target_id.clone());
                }
                Some(McpAgentBrowserContext {
                    app_id: targets.app_id.clone(),
                    targets: targets
                        .targets
                        .iter()
                        .take(8)
                        .map(|target| McpAgentBrowserTargetRef {
                            browser_target_id: target.id.clone(),
                            title: target.title.clone(),
                            url: target.url.clone(),
                        })
                        .collect(),
                    warnings: targets.warnings,
                    error: None,
                })
            }
            Err(error) => Some(McpAgentBrowserContext {
                app_id: active_app_id.clone(),
                targets: Vec::new(),
                warnings: Vec::new(),
                error: Some(error.to_string()),
            }),
        }
    } else {
        None
    };

    let viewers = match viewer::list_viewers() {
        Ok(list) => list
            .viewers
            .into_iter()
            .filter(|viewer| viewer.id == workspace_id || params.workspace_id.is_none())
            .take(8)
            .map(|viewer| {
                push_unique(&mut handles.viewer_ids, viewer.viewer_id.clone());
                McpAgentViewerContext {
                    viewer_id: viewer.viewer_id,
                    pid: viewer.pid,
                    backend: viewer.backend,
                    alive: viewer.alive,
                    always_on_top: viewer.always_on_top,
                }
            })
            .collect(),
        Err(error) => {
            warnings.push(format!("workspace_list_viewers failed: {error}"));
            Vec::new()
        }
    };

    let workspace = McpAgentWorkspaceContext {
        id: workspace_id.clone(),
        running,
        session_id: status.as_ref().map(|status| status.session_id.clone()),
        purpose: status.as_ref().and_then(|status| status.purpose.clone()),
        profile_id: status.as_ref().and_then(|status| status.profile_id.clone()),
        app_count: apps.len(),
        running_app_count: apps.iter().filter(|app| app.running).count(),
        active_app_id,
        active_window: active_window_ref,
        windows: window_refs,
        browser,
        error: (!running).then_some("workspace is not currently running".to_string()),
    };
    let next_tools = mcp_agent_context_next_tools(&mode, &workspace);
    let recovery_hints = mcp_agent_context_recovery_hints(&mode, &workspace);

    McpAgentContext {
        version: 1,
        mode,
        permissions: permissions.clone(),
        workspace,
        viewers,
        handles,
        next_tools,
        recovery_hints,
        warnings,
    }
}

fn mcp_agent_window_ref(window: &workspace::WorkspaceWindow) -> McpAgentWindowRef {
    McpAgentWindowRef {
        window_id: window.id.clone(),
        title: window.title.clone(),
        app_id: window.app_id.clone(),
        pid: window.pid,
        visible: window.visible,
    }
}

fn mcp_agent_context_next_tools(
    mode: &AgentModeSummary,
    workspace: &McpAgentWorkspaceContext,
) -> Vec<String> {
    let mut tools = vec![
        "mcp_agent_context".to_string(),
        "mcp_action_catalog".to_string(),
    ];
    if mode.viewer_available {
        tools.push("workspace_open_viewer".to_string());
    } else {
        tools.push("workspace_doctor".to_string());
    }
    if workspace.running {
        tools.extend(
            [
                "workspace_observe",
                "workspace_list_windows",
                "workspace_list_apps",
                "workspace_browser_targets",
            ]
            .into_iter()
            .map(str::to_string),
        );
    } else {
        tools.extend(
            [
                "workspace_list",
                "workspace_start",
                "workspace_open_profile",
            ]
            .into_iter()
            .map(str::to_string),
        );
    }
    if !mode.allows_agent_mutation {
        tools.push("mcp_control_update".to_string());
    }
    tools
}

fn mcp_agent_context_recovery_hints(
    mode: &AgentModeSummary,
    workspace: &McpAgentWorkspaceContext,
) -> Vec<String> {
    let mut hints = Vec::new();
    if !mode.allows_agent_mutation {
        hints.push(live_control_reactivation_hint().to_string());
    }
    if !mode.viewer_available {
        hints.push(
            "Call workspace_doctor before expecting workspace_open_viewer to work.".to_string(),
        );
    }
    if workspace.running {
        hints.push("Use returned app_id, window_id, viewer_id, and browser_target_id handles instead of rediscovering by title.".to_string());
    } else {
        hints.push("No running workspace is selected; use workspace_start or workspace_open_profile before window/app/browser tools.".to_string());
    }
    hints
}

fn mcp_workspace_activity_brief(
    entry: &workspace::WorkspaceListEntry,
) -> McpSessionWorkspaceActivityBrief {
    let status = entry.status.as_ref();
    let manifest = entry.manifest.as_ref();
    let apps: &[workspace::WorkspaceApp] = if let Some(status) = status {
        &status.apps
    } else if let Some(manifest) = manifest {
        &manifest.apps
    } else {
        &[]
    };
    let purpose = status
        .and_then(|status| status.purpose.clone())
        .or_else(|| manifest.and_then(|manifest| manifest.purpose.clone()));
    let profile_id = status
        .and_then(|status| status.profile_id.clone())
        .or_else(|| manifest.and_then(|manifest| manifest.profile_id.clone()));
    let intent = inferred_workspace_activity_intent(profile_id.as_deref(), apps);

    McpSessionWorkspaceActivityBrief {
        id: entry.id.clone(),
        running: entry.running,
        purpose,
        profile_id,
        display: status
            .map(|status| status.display.clone())
            .or_else(|| manifest.map(|manifest| manifest.display.clone())),
        app_count: apps.len(),
        running_app_count: apps.iter().filter(|app| app.running).count(),
        last_event_sequence: status
            .map(|status| status.last_event_sequence)
            .or_else(|| manifest.map(|manifest| manifest.last_event_sequence))
            .unwrap_or_default(),
        inferred_intent: intent.map(|(intent, _, _)| intent.to_string()),
        intent_label: intent.map(|(_, label, _)| label.to_string()),
        intent_reason: intent.map(|(_, _, reason)| reason.to_string()),
        apps: apps.iter().take(6).map(mcp_app_activity_brief).collect(),
        error: if entry.running || entry.manifest.is_none() {
            entry.error.clone().or_else(|| entry.manifest_error.clone())
        } else {
            entry.manifest_error.clone()
        },
    }
}

fn inferred_workspace_activity_intent(
    profile_id: Option<&str>,
    apps: &[workspace::WorkspaceApp],
) -> Option<(&'static str, &'static str, &'static str)> {
    let contains_marker = |markers: &[&str]| {
        profile_id
            .into_iter()
            .chain(apps.iter().filter_map(|app| app.name.as_deref()))
            .chain(
                apps.iter()
                    .flat_map(|app| app.command.iter().map(String::as_str)),
            )
            .any(|text| {
                let text = text.to_ascii_lowercase();
                markers.iter().any(|marker| text.contains(marker))
            })
    };

    if contains_marker(&["browser", "shopping", "grocery", "chrome", "firefox"]) {
        return Some((
            "grocery shopping",
            "Plan browser or shopping workflow",
            "Running workspace activity looks browser-like; use the planner to preserve browser-data, account, checkout, and real-world approval boundaries before browser actions.",
        ));
    }

    if let Some(profile_id) = profile_id {
        return Some(inferred_profile_task_intent(profile_id));
    }

    if contains_marker(&["project", "qa", "app", "desktop", "dev", "editor"]) || !apps.is_empty() {
        return Some((
            "app QA",
            "Plan running app QA workflow",
            "The running workspace already has app activity; use the planner to observe, test, and collect evidence before injecting input.",
        ));
    }

    None
}

fn mcp_app_activity_brief(app: &workspace::WorkspaceApp) -> McpSessionAppActivityBrief {
    McpSessionAppActivityBrief {
        id: app.id.clone(),
        label: app
            .name
            .clone()
            .or_else(|| app.command.first().cloned())
            .unwrap_or_else(|| app.id.clone()),
        running: app.running,
        pid: app.running.then_some(app.pid),
        profile_id: app.profile_id.clone(),
    }
}

#[allow(clippy::too_many_arguments)]
fn mcp_recommendation(
    id: &str,
    priority: u8,
    intent: &str,
    label: &str,
    tool: &str,
    arguments: serde_json::Value,
    reason: &str,
    approval_hint: &str,
    read_only: bool,
    open_world: bool,
    blocked_by_live_control: bool,
) -> McpRecommendedAction {
    let action_type = mcp_recommendation_action_type(tool, &arguments, read_only, open_world);
    let idempotent = mcp_recommendation_is_idempotent(tool, &arguments, read_only);
    let approval_checkpoints = mcp_recommendation_checkpoints(
        tool,
        &arguments,
        reason,
        approval_hint,
        open_world,
        blocked_by_live_control,
    );
    let approval_summary = mcp_recommendation_approval_summary(&approval_checkpoints);
    McpRecommendedAction {
        id: id.to_string(),
        priority,
        intent: intent.to_string(),
        label: label.to_string(),
        tool: tool.to_string(),
        arguments,
        action_type,
        idempotent,
        reason: reason.to_string(),
        approval_hint: approval_hint.to_string(),
        read_only,
        open_world,
        blocked_by_live_control,
        approval_checkpoints,
        approval_summary,
    }
}

fn mcp_recommendation_action_type(
    tool: &str,
    arguments: &serde_json::Value,
    read_only: bool,
    open_world: bool,
) -> String {
    let action_type = if open_world || tool == "workspace_open_viewer" {
        "host_visible_ui"
    } else if mcp_arguments_request_dry_run(arguments) {
        "preview"
    } else if matches!(tool, "workspace_stop" | "workspace_cleanup_stale") {
        "destructive"
    } else if read_only {
        "read_only"
    } else {
        "mutating"
    };
    action_type.to_string()
}

fn mcp_recommendation_is_idempotent(
    tool: &str,
    arguments: &serde_json::Value,
    read_only: bool,
) -> bool {
    read_only
        || mcp_arguments_request_dry_run(arguments)
        || matches!(
            tool,
            "mcp_permissions"
                | "mcp_action_catalog"
                | "mcp_session_brief"
                | "mcp_agent_context"
                | "mcp_task_plan"
                | "mcp_control_state"
                | "workspace_open_viewer"
                | "workspace_doctor"
                | "workspace_guardrails"
                | "workspace_observe"
        )
}

fn mcp_recommendation_checkpoints(
    tool: &str,
    arguments: &serde_json::Value,
    reason: &str,
    approval_hint: &str,
    open_world: bool,
    blocked_by_live_control: bool,
) -> Vec<McpRecommendedActionCheckpoint> {
    let mut checkpoints = Vec::new();
    if mcp_arguments_request_dry_run(arguments) {
        checkpoints.push(mcp_recommendation_checkpoint(
            "preview_surface",
            "Use preview as approval surface",
            false,
            false,
            approval_hint,
        ));
    }
    if blocked_by_live_control {
        checkpoints.push(mcp_recommendation_checkpoint(
            "live_control",
            "Reactivate live MCP control",
            true,
            true,
            approval_hint,
        ));
    }
    if open_world || tool == "workspace_open_viewer" {
        checkpoints.push(mcp_recommendation_checkpoint(
            "host_visible_ui",
            "Approve host-visible UI",
            true,
            true,
            approval_hint,
        ));
    }
    if matches!(tool, "workspace_stop" | "workspace_cleanup_stale")
        && mcp_arguments_request_dry_run(arguments)
    {
        checkpoints.push(mcp_recommendation_checkpoint(
            "destructive_follow_up",
            "Approve destructive follow-up",
            true,
            false,
            approval_hint,
        ));
    }
    if mcp_recommendation_mentions_real_world_action(reason, approval_hint) {
        checkpoints.push(mcp_recommendation_checkpoint(
            "real_world_action",
            "Require separate real-world approval",
            true,
            false,
            approval_hint,
        ));
    }
    checkpoints
}

fn mcp_recommendation_checkpoint(
    kind: &str,
    label: &str,
    approval_required: bool,
    blocks_action: bool,
    approval_hint: &str,
) -> McpRecommendedActionCheckpoint {
    let required_input = if kind == "live_control" {
        live_control_reactivation_required_inputs()
    } else {
        Vec::new()
    };
    McpRecommendedActionCheckpoint {
        kind: kind.to_string(),
        label: label.to_string(),
        approval_required,
        blocks_action,
        approval_hint: approval_hint.to_string(),
        required_input,
    }
}

fn mcp_recommendation_approval_summary(
    checkpoints: &[McpRecommendedActionCheckpoint],
) -> McpRecommendedActionApprovalSummary {
    let approval_kinds = checkpoints
        .iter()
        .map(|checkpoint| checkpoint.kind.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let next_boundary = checkpoints
        .iter()
        .find(|checkpoint| checkpoint.blocks_action)
        .or_else(|| {
            checkpoints
                .iter()
                .find(|checkpoint| checkpoint.approval_required)
        })
        .map(McpRecommendedActionApprovalBoundary::from);

    McpRecommendedActionApprovalSummary {
        blocking_count: checkpoints
            .iter()
            .filter(|checkpoint| checkpoint.blocks_action)
            .count(),
        approval_required_count: checkpoints
            .iter()
            .filter(|checkpoint| checkpoint.approval_required)
            .count(),
        approval_kinds,
        next_boundary,
    }
}

fn mcp_session_approval_summary(
    recommendations: &[McpRecommendedAction],
) -> McpSessionApprovalSummary {
    let approval_kinds = recommendations
        .iter()
        .flat_map(|recommendation| {
            recommendation
                .approval_summary
                .approval_kinds
                .iter()
                .cloned()
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let blocking_recommendation_count = recommendations
        .iter()
        .filter(|recommendation| recommendation.approval_summary.blocking_count > 0)
        .count();
    let approval_required_recommendation_count = recommendations
        .iter()
        .filter(|recommendation| recommendation.approval_summary.approval_required_count > 0)
        .count();
    let next_boundary = recommendations
        .iter()
        .filter_map(|recommendation| {
            recommendation
                .approval_summary
                .next_boundary
                .as_ref()
                .map(|boundary| (recommendation, boundary))
        })
        .find(|(_, boundary)| boundary.blocks_action)
        .or_else(|| {
            recommendations
                .iter()
                .filter_map(|recommendation| {
                    recommendation
                        .approval_summary
                        .next_boundary
                        .as_ref()
                        .map(|boundary| (recommendation, boundary))
                })
                .find(|(_, boundary)| boundary.approval_required)
        })
        .map(|(recommendation, boundary)| McpSessionApprovalBoundary {
            recommendation_id: recommendation.id.clone(),
            recommendation_label: recommendation.label.clone(),
            priority: recommendation.priority,
            tool: recommendation.tool.clone(),
            kind: boundary.kind.clone(),
            label: boundary.label.clone(),
            blocks_action: boundary.blocks_action,
            approval_required: boundary.approval_required,
            approval_hint: boundary.approval_hint.clone(),
            required_input: boundary.required_input.clone(),
        });

    McpSessionApprovalSummary {
        blocking_recommendation_count,
        approval_required_recommendation_count,
        approval_kinds,
        next_boundary,
    }
}

fn live_control_reactivation_required_inputs() -> Vec<String> {
    vec![
        live_control_reactivation_hint().to_string(),
        "Include a reason that records why live MCP control was reactivated.".to_string(),
    ]
}

fn mcp_arguments_request_dry_run(arguments: &serde_json::Value) -> bool {
    matches!(
        arguments
            .get("dry_run")
            .and_then(serde_json::Value::as_bool),
        Some(true)
    )
}

fn mcp_recommendation_mentions_real_world_action(reason: &str, approval_hint: &str) -> bool {
    [reason, approval_hint].iter().any(|text| {
        let text = text.to_ascii_lowercase();
        text.contains("purchase")
            || text.contains("checkout")
            || text.contains("order submission")
            || text.contains("account changes")
    })
}

fn mcp_action_catalog() -> McpActionCatalog {
    McpActionCatalog {
        version: 1,
        notes: vec![
            "read_only and paused block tools whose control_behavior is blocked_when_not_active."
                .to_string(),
            "Dry-run preview paths stay allowed for tools marked blocked_when_not_active_unless_dry_run."
                .to_string(),
            "workspace_stop remains allowed as a safety stop even when live control is read_only or paused."
                .to_string(),
            "workspace_open_viewer is host-visible/open-world, remains available while live control is read_only or paused so the user can observe or regain control, and is additionally gated by --headless plus workspace_doctor.ready_for_host_viewer."
                .to_string(),
            "When mcp_permissions.configured=false, this catalog is advisory action classification and does not create an MCP permission ceiling."
                .to_string(),
            "When mcp_permissions.configured=true, populated permission dimensions are immutable ceilings and clients may only narrow them."
                .to_string(),
            "parameter_notes identify arguments that change risk, approval, or live-control behavior for a tool."
                .to_string(),
        ],
        tools: vec![
            mcp_action(
                "workspace_guardrails",
                "control_plane",
                true,
                false,
                false,
                true,
                false,
                "always_allowed",
                "Static guardrail metadata for approval UX, including concise agent_rules.allowed/blocked/requires_ack/exact_parameter wording.",
            ),
            mcp_action(
                "mcp_permissions",
                "control_plane",
                true,
                false,
                false,
                true,
                false,
                "always_allowed",
                "Spawn-time permission state. configured=false means no MCP ceiling; configured=true exposes immutable ceiling dimensions that clients may only narrow.",
            ),
            mcp_action(
                "mcp_action_catalog",
                "control_plane",
                true,
                false,
                false,
                true,
                false,
                "always_allowed",
                "Machine-readable action taxonomy for agent planning and approvals.",
            ),
            mcp_action(
                "mcp_session_brief",
                "control_plane",
                true,
                false,
                false,
                false,
                false,
                "always_allowed",
                "Read-only session summary with suggested next actions and approval hints.",
            ),
            mcp_action(
                "mcp_agent_context",
                "control_plane",
                true,
                false,
                false,
                false,
                false,
                "always_allowed",
                "Compact agent context snapshot with active/read_only/paused, headless/no-host-display, app_id/window_id/viewer_id/browser_target_id handles, and recovery hints.",
            ),
            mcp_action(
                "mcp_task_plan",
                "control_plane",
                true,
                false,
                false,
                false,
                false,
                "always_allowed",
                "Read-only intent plan for app QA, browser/shopping, observation, and cleanup workflows.",
            ),
            mcp_action(
                "mcp_control_state",
                "control_plane",
                true,
                false,
                false,
                true,
                false,
                "always_allowed",
                "Reads the live active/read_only/paused state.",
            ),
            mcp_action(
                "mcp_control_update",
                "control_plane",
                false,
                true,
                false,
                true,
                false,
                "control_plane_allowed",
                "Changes live MCP control mode; use only when the user or UI asks.",
            )
            .with_parameter_note(
                "confirmed_user_request",
                "true when mode=active while current mode is read_only or paused",
                "Confirms the user or controlling UI explicitly requested reactivation.",
                "Required before this MCP tool can re-enable mutating actions after read_only or paused.",
                "Use only after explicit user approval; the GPUI viewer can switch modes through the local control plane.",
            ),
            mcp_action(
                "workspace_doctor",
                "observation",
                true,
                false,
                false,
                true,
                false,
                "observation_allowed",
                "Reads local runtime readiness and policy backend candidates.",
            ),
            mcp_action(
                "profile_path",
                "profile",
                true,
                false,
                false,
                true,
                false,
                "observation_allowed",
                "Returns the profile store path.",
            ),
            mcp_action(
                "profile_list",
                "profile",
                true,
                false,
                false,
                true,
                false,
                "observation_allowed",
                "Lists saved profiles.",
            ),
            mcp_action(
                "profile_get",
                "profile",
                true,
                false,
                false,
                true,
                false,
                "observation_allowed",
                "Reads one saved profile.",
            ),
            mcp_action(
                "profile_check",
                "profile",
                true,
                false,
                false,
                true,
                false,
                "observation_allowed",
                "Preflights a saved profile without changing it.",
            ),
            mcp_action(
                "profile_validate",
                "profile",
                true,
                false,
                false,
                true,
                false,
                "observation_allowed",
                "Parses and preflights a local JSON profile without saving it.",
            ),
            mcp_action(
                "profile_template",
                "profile",
                true,
                false,
                false,
                true,
                false,
                "observation_allowed",
                "Generates an unsaved profile template.",
            ),
            mcp_action(
                "profile_put",
                "profile",
                false,
                true,
                false,
                true,
                false,
                "blocked_when_not_active_unless_dry_run",
                "Creates or replaces a saved profile unless dry_run=true.",
            )
            .with_parameter_note(
                "dry_run",
                "true",
                "Preview create/replace behavior without writing profile state.",
                "Allowed while live control is read_only or paused.",
                "No approval required for preview; require approval before the real profile write.",
            )
            .with_parameter_note(
                "replace",
                "true",
                "May overwrite an existing saved profile.",
                "Blocked unless live control is active when dry_run is false.",
                "Show the dry-run result and existing profile id before replacing.",
            ),
            mcp_action(
                "profile_import",
                "profile",
                false,
                true,
                false,
                true,
                false,
                "blocked_when_not_active_unless_dry_run",
                "Imports and saves a profile from a local JSON file unless dry_run=true.",
            )
            .with_parameter_note(
                "dry_run",
                "true",
                "Validate and preview import behavior without writing profile state.",
                "Allowed while live control is read_only or paused.",
                "No approval required for preview; require approval before importing.",
            )
            .with_parameter_note(
                "replace",
                "true",
                "May overwrite an existing saved profile during import.",
                "Blocked unless live control is active when dry_run is false.",
                "Show the dry-run result and existing profile id before replacing.",
            ),
            mcp_action(
                "profile_export",
                "profile",
                false,
                true,
                false,
                true,
                false,
                "conditional_output_path",
                "Reads a profile; writes a host file only when output_path is set.",
            )
            .with_parameter_note(
                "output_path",
                "set",
                "Writes a pretty JSON profile file on the host filesystem.",
                "Blocked unless live control is active.",
                "Ask before writing outside the read-only profile-return path.",
            )
            .with_parameter_note(
                "replace",
                "true",
                "May overwrite an existing export file when output_path is set.",
                "Blocked unless live control is active because output_path writes are mutating.",
                "Show the destination path before replacing a file.",
            ),
            mcp_action(
                "profile_delete",
                "profile",
                false,
                true,
                true,
                true,
                false,
                "blocked_when_not_active_unless_dry_run",
                "Deletes a saved profile unless dry_run=true.",
            )
            .with_parameter_note(
                "dry_run",
                "true",
                "Resolve and return the profile that would be deleted without removing it.",
                "Allowed while live control is read_only or paused.",
                "Use as the approval surface before deletion.",
            ),
            mcp_action(
                "workspace_start",
                "workspace_lifecycle",
                false,
                true,
                false,
                true,
                true,
                "blocked_when_not_active_unless_dry_run",
                "Creates a hidden workspace unless dry_run=true. By default, non-headless MCP sessions with a ready host display also open the GPUI viewer unless open_viewer=false.",
            )
            .with_parameter_note(
                "dry_run",
                "true",
                "Preview acknowledgement, runtime, and policy requirements without creating a workspace or viewer.",
                "Allowed while live control is read_only or paused.",
                "Use as the approval surface before hidden-workspace creation.",
            )
            .with_parameter_note(
                "open_viewer",
                "false",
                "Explicitly suppresses the default host-visible viewer auto-open.",
                "Allowed; this narrows host-visible UI behavior and does not make the MCP headless.",
                "Use only when the user or embedding host explicitly does not want the viewer window.",
            )
            .with_parameter_note(
                "viewer_always_on_top",
                "true",
                "Requests overlay/above window-manager behavior for the auto-opened viewer.",
                "Allowed only when the default viewer can open; blocked by --headless or no host display.",
                "Use only after the user or host explicitly asks for an always-on-top monitor.",
            ),
            mcp_action(
                "workspace_open_profile",
                "workspace_lifecycle",
                false,
                true,
                false,
                false,
                true,
                "blocked_when_not_active_unless_dry_run",
                "Starts a profile-backed workspace and may run setup/startup apps unless dry_run=true. By default, non-headless MCP sessions with a ready host display open the GPUI viewer immediately after workspace start unless open_viewer=false.",
            )
            .with_parameter_note(
                "dry_run",
                "true",
                "Preview start/setup/startup requirements without creating a workspace, viewer, or launching apps.",
                "Allowed while live control is read_only or paused.",
                "Use as the combined approval surface before running the profile.",
            )
            .with_parameter_note(
                "open_viewer",
                "false",
                "Explicitly suppresses the default host-visible viewer auto-open.",
                "Allowed; this narrows host-visible UI behavior and does not make the MCP headless.",
                "Use only when the user or embedding host explicitly does not want the viewer window.",
            )
            .with_parameter_note(
                "viewer_always_on_top",
                "true",
                "Requests overlay/above window-manager behavior for the auto-opened viewer.",
                "Allowed only when the default viewer can open; blocked by --headless or no host display.",
                "Use only after the user or host explicitly asks for an always-on-top monitor.",
            )
            .with_parameter_note(
                "setup_kill_on_timeout",
                "true",
                "May terminate a timed-out setup command after the workspace starts.",
                "Blocked unless live control is active because the real profile run is mutating.",
                "Call out timeout cleanup behavior in the profile-run approval.",
            ),
            mcp_action(
                "workspace_status",
                "observation",
                true,
                false,
                false,
                false,
                false,
                "observation_allowed",
                "Reads live workspace daemon status.",
            ),
            mcp_action(
                "workspace_manifest",
                "observation",
                true,
                false,
                false,
                true,
                false,
                "observation_allowed",
                "Reads saved workspace manifest state.",
            ),
            mcp_action(
                "workspace_artifacts",
                "observation",
                true,
                false,
                false,
                true,
                false,
                "observation_allowed",
                "Inventories saved runtime files.",
            ),
            mcp_action(
                "workspace_ipc_info",
                "observation",
                true,
                false,
                false,
                true,
                false,
                "observation_allowed",
                "Reads daemon IPC metadata.",
            ),
            mcp_action(
                "workspace_env",
                "observation",
                true,
                false,
                false,
                true,
                false,
                "observation_allowed",
                "Reads attachment environment values.",
            ),
            mcp_action(
                "workspace_list",
                "observation",
                true,
                false,
                false,
                true,
                false,
                "observation_allowed",
                "Lists known workspaces.",
            ),
            mcp_action(
                "workspace_open_viewer",
                "viewer",
                false,
                true,
                false,
                true,
                true,
                "headless_or_host_display_gated_open_world",
                "Ensures a host-visible GPUI viewer is open unless the MCP process is --headless or workspace_doctor.ready_for_host_viewer=false. Repeated calls reuse a compatible live registered viewer for that workspace; input_forwarding=true may open a separate input-capable viewer when only a read-only monitor exists. It remains available while live control is read_only or paused so the user can observe or regain control. The default viewer does not request always-on-top state; always_on_top is opt-in. MCP-opened viewers are target-bound and exit after their selected workspace runtime is removed.",
            )
            .with_parameter_note(
                "always_on_top",
                "true",
                "Requests overlay/above window-manager behavior for the host-visible viewer.",
                "Allowed while live control is read_only or paused. Blocked when MCP is --headless or the host viewer is not ready; otherwise host-visible/open-world.",
                "Use only after the user or host explicitly asks for an always-on-top monitor.",
            )
            .with_parameter_note(
                "input_forwarding",
                "true",
                "Launches or reuses a viewer capable of explicit in-viewer manual mouse/keyboard/paste forwarding into the isolated workspace; forwarding still starts disabled.",
                "Allowed while live control is read_only or paused because this only opens host-visible UI; actual forwarded input remains blocked unless MCP control is active.",
                "Use only when the user explicitly wants manual/RW viewer control.",
            ),
            mcp_action(
                "workspace_list_viewers",
                "viewer",
                true,
                false,
                false,
                true,
                false,
                "observation_allowed",
                "Lists repo-registered GPUI viewer processes without relying on desktop-compositor window discovery.",
            ),
            mcp_action(
                "workspace_close_viewer",
                "viewer",
                false,
                false,
                true,
                true,
                true,
                "viewer_control_allowed",
                "Closes only registered GPUI viewer pids whose command line still matches the registry entry. This gives agents a repo-owned cleanup path when the desktop compositor does not expose GPUI windows as ordinary controllable targets.",
            )
            .with_parameter_note(
                "dry_run",
                "true",
                "Preview registered viewers that would be closed and stale registry rows that would be removed.",
                "Allowed while live control is read_only or paused.",
                "Use as the approval surface before closing a host-visible viewer.",
            )
            .with_parameter_note(
                "all",
                "true",
                "Targets every registered viewer instead of one workspace id.",
                "Allowed while live control is read_only or paused because it only affects registered viewer pids.",
                "Use when cleaning an orphan viewer whose workspace id is unknown after inspecting workspace_list_viewers.",
            ),
            mcp_action(
                "workspace_cleanup_stale",
                "workspace_lifecycle",
                false,
                true,
                true,
                true,
                false,
                "blocked_when_not_active_unless_dry_run",
                "Deletes stale runtime files and may terminate orphan processes unless dry_run=true.",
            )
            .with_parameter_note(
                "dry_run",
                "true",
                "Preview removable runtime directories and verified orphan process cleanup without deleting or signaling.",
                "Allowed while live control is read_only or paused.",
                "Use as the approval surface before cleanup.",
            ),
            mcp_action(
                "workspace_launch_app",
                "app",
                false,
                true,
                false,
                false,
                false,
                "blocked_when_not_active_unless_dry_run",
                "Starts an app inside the isolated workspace unless dry_run=true.",
            )
            .with_parameter_note(
                "dry_run",
                "true",
                "Preview command, cwd/env, profile policy, and approval bundle without spawning an app.",
                "Allowed while live control is read_only or paused.",
                "Use before real app launches when approval or policy is uncertain.",
            ),
            mcp_action(
                "workspace_run_app",
                "app",
                false,
                true,
                false,
                false,
                false,
                "blocked_when_not_active_unless_dry_run",
                "Runs an app inside the isolated workspace unless dry_run=true.",
            )
            .with_parameter_note(
                "dry_run",
                "true",
                "Preview launch/run options without spawning an app.",
                "Allowed while live control is read_only or paused.",
                "Use before executing commands with meaningful side effects.",
            )
            .with_parameter_note(
                "kill_on_timeout",
                "true",
                "May terminate the launched app process group if it times out.",
                "Blocked unless live control is active when dry_run is false.",
                "Tell the user the timeout cleanup behavior before running long commands.",
            ),
            mcp_action(
                "workspace_run_in_terminal",
                "terminal",
                false,
                true,
                false,
                false,
                false,
                "blocked_when_not_active",
                "Starts an xterm backed by a per-workspace tmux socket and returns terminal_id, pane tty, app_id, and window handles. Use this for TUI apps instead of hand-rolled xterm launch commands.",
            )
            .with_parameter_note(
                "command",
                "set",
                "Runs a command inside the tmux-backed terminal; when omitted, tmux starts the default shell.",
                "Blocked while live control is read_only or paused because this launches an app.",
                "Use for games, editors, REPLs, and other terminal UIs that need text readback.",
            )
            .with_parameter_note(
                "terminal_id",
                "set",
                "Creates a stable terminal handle used by workspace_terminal_read and workspace_terminal_input.",
                "Blocked while live control is read_only or paused because this launches an app.",
                "Pick a short semantic id when several terminals will be active.",
            ),
            mcp_action(
                "workspace_terminal_read",
                "terminal",
                true,
                false,
                false,
                false,
                false,
                "observation_allowed",
                "Reads exact terminal pane text through tmux capture-pane instead of a screenshot.",
            )
            .with_parameter_note(
                "preserve_trailing_spaces",
                "true",
                "Asks tmux to preserve trailing spaces when fixed-width cell layout matters.",
                "Allowed while live control is read_only or paused.",
                "Use for board/grid TUIs where empty cells matter.",
            ),
            mcp_action(
                "workspace_terminal_input",
                "terminal",
                false,
                true,
                false,
                false,
                false,
                "blocked_when_not_active",
                "Sends literal text and/or an array of keys to a tmux-backed terminal, bypassing window focus and batching multiple keypresses in one call.",
            )
            .with_parameter_note(
                "keys",
                "set",
                "Sends a sequence such as [\"Up\", \"Left\", \"Enter\"] using tmux send-keys grammar.",
                "Blocked while live control is read_only or paused because it mutates terminal state.",
                "Use arrays for TUI navigation instead of one MCP round trip per key.",
            )
            .with_parameter_note(
                "text",
                "set",
                "Sends literal text with tmux send-keys -l; raw text is omitted from workspace events.",
                "Blocked while live control is read_only or paused because it mutates terminal state.",
                "Use with keys=[\"Enter\"] to submit commands in one call.",
            ),
            mcp_action(
                "workspace_list_apps",
                "observation",
                true,
                false,
                false,
                true,
                false,
                "observation_allowed",
                "Lists launched app records.",
            ),
            mcp_action(
                "workspace_open_browser",
                "browser",
                false,
                true,
                false,
                false,
                true,
                "blocked_when_not_active",
                "Launches a workspace-owned Chrome/Chromium app with a disposable profile by default and the required loopback DevTools flags. Returns app_id and browser_target_id handles so agents can navigate, snapshot, search, or click without shelling out.",
            )
            .with_parameter_note(
                "browser_path",
                "set",
                "Pins the Chrome/Chromium executable when auto-discovery is not enough.",
                "Blocked while live control is read_only or paused because this launches an app.",
                "Use only an explicitly available browser binary; permission ceilings may allowlist app commands.",
            )
            .with_parameter_note(
                "user_data_dir",
                "set",
                "Uses a specific browser profile directory instead of the disposable workspace runtime profile.",
                "Blocked while live control is read_only or paused because this launches an app.",
                "Use a disposable copy for authenticated sessions unless the user explicitly approved the profile path.",
            )
            .with_parameter_note(
                "url",
                "set",
                "Opens the initial page and may contact external web services for http(s) URLs.",
                "Blocked while live control is read_only or paused.",
                "For shopping/grocery tasks, keep checkout, purchase, payment, and account changes behind separate real-world approval.",
            ),
            mcp_action(
                "workspace_browser_targets",
                "browser",
                true,
                false,
                false,
                false,
                false,
                "observation_allowed",
                "Discovers Chrome DevTools targets for a browser app launched inside the isolated workspace, deriving the endpoint from the workspace app user-data directory instead of the host Chrome bridge.",
            )
            .with_parameter_note(
                "app_id",
                "set",
                "Pins discovery to a specific workspace-launched browser app when multiple browser apps are running.",
                "Allowed while live control is read_only or paused.",
                "Use the app_id returned by workspace_launch_app or workspace_list_apps to avoid ambiguous browser targets.",
            )
            .with_parameter_note(
                "timeout_ms",
                "set",
                "Waits briefly for Chrome to write DevToolsActivePort and expose /json/list after startup.",
                "Allowed while live control is read_only or paused.",
                "Use after launching a browser with --remote-debugging-port=0.",
            ),
            mcp_action(
                "workspace_browser_snapshot",
                "browser",
                true,
                false,
                false,
                false,
                false,
                "observation_allowed",
                "Reads DOM text, title, URL, headings, and links from a workspace-owned Chrome/Chromium page target and records a metadata-only browser_snapshot workspace event.",
            )
            .with_parameter_note(
                "max_text_chars",
                "set",
                "Caps returned DOM text before it leaves the workspace browser control path.",
                "Allowed while live control is read_only or paused.",
                "Use a smaller cap when the user only needs a quick page readback.",
            )
            .with_parameter_note(
                "target_id/title_contains/url_contains",
                "set",
                "Disambiguates page targets when the workspace browser has multiple tabs.",
                "Allowed while live control is read_only or paused.",
                "Use instead of guessing from screenshots when several tabs are open.",
            ),
            mcp_action(
                "workspace_browser_search_results",
                "browser",
                true,
                false,
                false,
                false,
                false,
                "observation_allowed",
                "Extracts structured product/search result cards from a workspace-owned Chrome/Chromium page target and records a metadata-only browser_search_results workspace event.",
            )
            .with_parameter_note(
                "max_results",
                "set",
                "Caps the number of extracted product/search result cards returned from the page.",
                "Allowed while live control is read_only or paused.",
                "Use for shopping/grocery result pages before falling back to manual page text parsing.",
            )
            .with_parameter_note(
                "min_vram_gb",
                "set",
                "Filters extracted GPU-like product cards to results whose visible text contains at least this many GB of memory/VRAM.",
                "Allowed while live control is read_only or paused.",
                "Use for GPU shopping tasks such as finding end-user cards above a requested VRAM threshold.",
            )
            .with_parameter_note(
                "target_id/title_contains/url_contains",
                "set",
                "Disambiguates page targets when the workspace browser has multiple tabs.",
                "Allowed while live control is read_only or paused.",
                "Use the same target selected by workspace_browser_targets or workspace_browser_snapshot.",
            ),
            mcp_action(
                "workspace_browser_navigate",
                "browser",
                false,
                true,
                false,
                false,
                true,
                "blocked_when_not_active",
                "Navigates a workspace-owned Chrome/Chromium page target to an http(s), data:, or about:blank URL and records a browser_navigate workspace event. This stays inside the isolated workspace browser rather than the user's host Chrome bridge.",
            )
            .with_parameter_note(
                "url",
                "set",
                "Changes the current workspace browser page and may contact external web services for http(s) URLs.",
                "Blocked while live control is read_only or paused.",
                "For shopping/grocery tasks, keep checkout, purchase, payment, and account changes behind separate real-world approval.",
            )
            .with_parameter_note(
                "snapshot",
                "false",
                "Skips the post-navigation DOM readback.",
                "Blocked while live control is read_only or paused because navigation still mutates browser state.",
                "Leave enabled when collecting dogfood evidence so the result proves the visible page state.",
            ),
            mcp_action(
                "workspace_browser_click",
                "browser",
                false,
                true,
                false,
                false,
                true,
                "blocked_when_not_active",
                "Clicks a workspace-owned browser page through Chrome DevTools by selector, visible text, or page viewport coordinates. Viewport coordinates are page-relative, not Chrome-window-relative, so browser toolbar height is not part of the coordinate math.",
            )
            .with_parameter_note(
                "selector",
                "set",
                "Clicks the visible element matched by a CSS selector.",
                "Blocked while live control is read_only or paused because clicking mutates browser state.",
                "Prefer selector when the DOM surface is stable.",
            )
            .with_parameter_note(
                "text",
                "set",
                "Clicks the shortest visible clickable element whose text, value, aria-label, or title contains the requested text.",
                "Blocked while live control is read_only or paused because clicking mutates browser state.",
                "Prefer text for filter chips, buttons, and links when a screenshot would otherwise require pixel guessing.",
            )
            .with_parameter_note(
                "viewport_x/viewport_y",
                "set",
                "Clicks the element at page viewport coordinates from document.elementFromPoint.",
                "Blocked while live control is read_only or paused because clicking mutates browser state.",
                "Use as a visual fallback after a browser screenshot; do not subtract browser toolbar height.",
            ),
            mcp_action(
                "workspace_list_windows",
                "observation",
                true,
                false,
                false,
                false,
                false,
                "observation_allowed",
                "Lists workspace windows.",
            ),
            mcp_action(
                "workspace_active_window",
                "observation",
                true,
                false,
                false,
                false,
                false,
                "observation_allowed",
                "Reads the active workspace window.",
            ),
            mcp_action(
                "workspace_pointer",
                "observation",
                true,
                false,
                false,
                false,
                false,
                "observation_allowed",
                "Reads pointer position/state.",
            ),
            mcp_action(
                "workspace_observe",
                "observation",
                true,
                false,
                false,
                false,
                false,
                "observation_allowed",
                "Reads a compact workspace snapshot.",
            )
            .with_parameter_note(
                "output_path",
                "set while screenshot=true",
                "Writes the observation screenshot to the requested host path instead of the workspace screenshot directory.",
                "Allowed as observation, but still writes a host file.",
                "Prefer the default artifact path unless the user requested a destination.",
            ),
            mcp_action(
                "workspace_wait_window",
                "observation",
                true,
                false,
                false,
                false,
                false,
                "observation_allowed",
                "Waits for a matching window without changing it.",
            ),
            mcp_action(
                "workspace_screenshot",
                "observation",
                true,
                false,
                false,
                false,
                false,
                "observation_allowed",
                "Captures the workspace screen for inspection.",
            )
            .with_parameter_note(
                "output_path",
                "set",
                "Writes the screenshot to the requested host path instead of the workspace screenshot directory.",
                "Allowed as observation, but still writes a host file.",
                "Prefer the default artifact path unless the user requested a destination.",
            ),
            mcp_action(
                "workspace_screenshot_window",
                "observation",
                true,
                false,
                false,
                false,
                false,
                "observation_allowed",
                "Captures a matching window for inspection.",
            )
            .with_parameter_note(
                "output_path",
                "set",
                "Writes the window screenshot to the requested host path instead of the workspace screenshot directory.",
                "Allowed as observation, but still writes a host file.",
                "Prefer the default artifact path unless the user requested a destination.",
            ),
            mcp_action(
                "workspace_focus_window",
                "window",
                false,
                true,
                false,
                true,
                false,
                "blocked_when_not_active",
                "Changes focus inside the isolated workspace.",
            ),
            mcp_action(
                "workspace_focus_matching_window",
                "window",
                false,
                true,
                false,
                true,
                false,
                "blocked_when_not_active",
                "Finds and focuses a matching isolated workspace window.",
            ),
            mcp_action(
                "workspace_close_window",
                "window",
                false,
                true,
                true,
                false,
                false,
                "blocked_when_not_active_unless_dry_run",
                "Requests a workspace window close unless dry_run=true.",
            )
            .with_parameter_note(
                "dry_run",
                "true",
                "Resolve and return the target window without closing it.",
                "Allowed while live control is read_only or paused.",
                "Use before closing windows when the target is ambiguous.",
            ),
            mcp_action(
                "workspace_close_matching_window",
                "window",
                false,
                true,
                true,
                false,
                false,
                "blocked_when_not_active_unless_dry_run",
                "Finds and closes a matching workspace window unless dry_run=true.",
            )
            .with_parameter_note(
                "dry_run",
                "true",
                "Resolve and return the matching window without closing it.",
                "Allowed while live control is read_only or paused.",
                "Use before closing windows when filters may match the wrong app.",
            ),
            mcp_action(
                "workspace_move_window",
                "window",
                false,
                true,
                false,
                false,
                false,
                "blocked_when_not_active",
                "Moves an isolated workspace window.",
            ),
            mcp_action(
                "workspace_resize_window",
                "window",
                false,
                true,
                false,
                false,
                false,
                "blocked_when_not_active",
                "Resizes an isolated workspace window.",
            ),
            mcp_action(
                "workspace_raise_window",
                "window",
                false,
                true,
                false,
                false,
                false,
                "blocked_when_not_active",
                "Raises an isolated workspace window.",
            ),
            mcp_action(
                "workspace_minimize_window",
                "window",
                false,
                true,
                false,
                false,
                false,
                "blocked_when_not_active",
                "Minimizes an isolated workspace window.",
            ),
            mcp_action(
                "workspace_show_window",
                "window",
                false,
                true,
                false,
                false,
                false,
                "blocked_when_not_active",
                "Shows or maps an isolated workspace window.",
            ),
            mcp_action(
                "workspace_click",
                "input",
                false,
                true,
                false,
                false,
                false,
                "blocked_when_not_active",
                "Injects a click inside the isolated workspace.",
            ),
            mcp_action(
                "workspace_click_window",
                "input",
                false,
                true,
                false,
                false,
                false,
                "blocked_when_not_active",
                "Injects a window-relative click inside the isolated workspace.",
            ),
            mcp_action(
                "workspace_move_pointer",
                "input",
                false,
                true,
                false,
                true,
                false,
                "blocked_when_not_active",
                "Moves the isolated workspace pointer.",
            ),
            mcp_action(
                "workspace_move_pointer_window",
                "input",
                false,
                true,
                false,
                true,
                false,
                "blocked_when_not_active",
                "Moves the isolated workspace pointer relative to a window.",
            ),
            mcp_action(
                "workspace_drag",
                "input",
                false,
                true,
                false,
                false,
                false,
                "blocked_when_not_active",
                "Injects a drag inside the isolated workspace.",
            ),
            mcp_action(
                "workspace_drag_window",
                "input",
                false,
                true,
                false,
                false,
                false,
                "blocked_when_not_active",
                "Injects a window-relative drag inside the isolated workspace.",
            ),
            mcp_action(
                "workspace_scroll",
                "input",
                false,
                true,
                false,
                false,
                false,
                "blocked_when_not_active",
                "Injects a scroll inside the isolated workspace.",
            ),
            mcp_action(
                "workspace_scroll_window",
                "input",
                false,
                true,
                false,
                false,
                false,
                "blocked_when_not_active",
                "Injects a window-relative scroll inside the isolated workspace.",
            ),
            mcp_action(
                "workspace_key",
                "input",
                false,
                true,
                false,
                false,
                false,
                "blocked_when_not_active",
                "Sends a key chord inside the isolated workspace.",
            ),
            mcp_action(
                "workspace_key_window",
                "input",
                false,
                true,
                false,
                false,
                false,
                "blocked_when_not_active",
                "Sends a key chord to a matching isolated workspace window.",
            ),
            mcp_action(
                "workspace_type_text",
                "input",
                false,
                true,
                false,
                false,
                false,
                "blocked_when_not_active",
                "Types literal text inside the isolated workspace.",
            ),
            mcp_action(
                "workspace_type_window",
                "input",
                false,
                true,
                false,
                false,
                false,
                "blocked_when_not_active",
                "Types literal text into a matching isolated workspace window.",
            ),
            mcp_action(
                "workspace_set_clipboard",
                "clipboard",
                false,
                true,
                false,
                true,
                false,
                "blocked_when_not_active",
                "Sets the isolated workspace clipboard.",
            ),
            mcp_action(
                "workspace_get_clipboard",
                "clipboard",
                true,
                false,
                false,
                false,
                false,
                "observation_allowed",
                "Reads the isolated workspace clipboard.",
            ),
            mcp_action(
                "workspace_paste_text",
                "clipboard",
                false,
                true,
                false,
                false,
                false,
                "blocked_when_not_active",
                "Sets clipboard text and sends paste inside the isolated workspace.",
            ),
            mcp_action(
                "workspace_paste_window",
                "clipboard",
                false,
                true,
                false,
                false,
                false,
                "blocked_when_not_active",
                "Sets clipboard text, focuses a window, and sends paste.",
            ),
            mcp_action(
                "workspace_read_app_log",
                "logs",
                true,
                false,
                false,
                true,
                false,
                "observation_allowed",
                "Reads captured stdout/stderr logs.",
            ),
            mcp_action(
                "workspace_wait_app",
                "app",
                false,
                true,
                true,
                false,
                false,
                "conditional_kill_on_timeout",
                "Waiting is observational; kill_on_timeout=true is blocked unless control is active.",
            )
            .with_parameter_note(
                "kill_on_timeout",
                "true",
                "May terminate the app process group if the wait times out.",
                "Blocked unless live control is active.",
                "Ask before turning a wait into a possible termination.",
            ),
            mcp_action(
                "workspace_events",
                "logs",
                true,
                false,
                false,
                true,
                false,
                "observation_allowed",
                "Reads sanitized workspace event history.",
            ),
            mcp_action(
                "workspace_run_profile_setup",
                "app",
                false,
                true,
                false,
                false,
                false,
                "blocked_when_not_active_unless_dry_run",
                "Launches profile setup commands unless dry_run=true.",
            )
            .with_parameter_note(
                "dry_run",
                "true",
                "Preview setup commands without spawning them.",
                "Allowed while live control is read_only or paused.",
                "Use before running saved setup commands from an unfamiliar profile.",
            )
            .with_parameter_note(
                "kill_on_timeout",
                "true",
                "May terminate a timed-out setup command.",
                "Blocked unless live control is active when dry_run is false.",
                "Call out timeout cleanup behavior before running setup.",
            ),
            mcp_action(
                "workspace_launch_profile_apps",
                "app",
                false,
                true,
                false,
                false,
                false,
                "blocked_when_not_active_unless_dry_run",
                "Launches profile startup apps unless dry_run=true.",
            )
            .with_parameter_note(
                "dry_run",
                "true",
                "Preview startup app launches without spawning them.",
                "Allowed while live control is read_only or paused.",
                "Use before launching saved startup apps from an unfamiliar profile.",
            ),
            mcp_action(
                "workspace_kill_app",
                "app",
                false,
                true,
                true,
                true,
                false,
                "blocked_when_not_active_unless_dry_run",
                "Terminates a launched app unless dry_run=true.",
            )
            .with_parameter_note(
                "dry_run",
                "true",
                "Resolve and return the matched app without terminating it.",
                "Allowed while live control is read_only or paused.",
                "Use as the approval surface before killing an app.",
            ),
            mcp_action(
                "workspace_stop",
                "workspace_lifecycle",
                false,
                true,
                true,
                true,
                false,
                "safety_stop_allowed",
                "Stops a workspace and remains available as a safety stop.",
            )
            .with_parameter_note(
                "dry_run",
                "true",
                "Return currently running apps without stopping the workspace.",
                "Allowed while live control is read_only or paused.",
                "Use to show what the safety stop would terminate.",
            ),
        ],
    }
}

#[allow(clippy::too_many_arguments)]
fn mcp_action(
    name: &str,
    category: &str,
    read_only: bool,
    mutating: bool,
    destructive: bool,
    idempotent: bool,
    open_world: bool,
    control_behavior: &str,
    notes: &str,
) -> McpActionInfo {
    McpActionInfo {
        name: name.to_string(),
        category: category.to_string(),
        read_only,
        mutating,
        destructive,
        idempotent,
        open_world,
        control_behavior: control_behavior.to_string(),
        notes: notes.to_string(),
        parameter_notes: Vec::new(),
    }
}

impl McpActionInfo {
    fn with_parameter_note(
        mut self,
        parameter: &str,
        when: &str,
        effect: &str,
        live_control: &str,
        approval_hint: &str,
    ) -> Self {
        self.parameter_notes.push(McpActionParameterNote {
            parameter: parameter.to_string(),
            when: when.to_string(),
            effect: effect.to_string(),
            live_control: live_control.to_string(),
            approval_hint: approval_hint.to_string(),
        });
        self
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct ProfileIdParams {
    id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct ProfileValidateParams {
    json_path: PathBuf,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct ProfileDeleteParams {
    id: String,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct ProfilePutParams {
    profile: WorkspaceProfile,
    #[serde(default)]
    replace: bool,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct ProfileImportParams {
    json_path: PathBuf,
    #[serde(default)]
    replace: bool,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct ProfileExportParams {
    id: String,
    #[serde(default)]
    output_path: Option<PathBuf>,
    #[serde(default)]
    replace: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct ProfileExportResponse {
    ok: bool,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    export: Option<profile::ProfileExportResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_mode: Option<AgentModeSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recovery_hints: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct ProfileTemplateParams {
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    host_path: Option<PathBuf>,
    #[serde(default)]
    browser_path: Option<PathBuf>,
    #[serde(default)]
    user_data_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct ProfileGetResult {
    ok: bool,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile: Option<WorkspaceProfile>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct ProfileCheckResult {
    ok: bool,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    check: Option<profile::ProfileCheck>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct ProfileValidateResponse {
    ok: bool,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    validation: Option<profile::ProfileValidateResult>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct ProfileSetupResult {
    ok: bool,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run: Option<profile::ProfileSetupRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_mode: Option<AgentModeSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recovery_hints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct ProfileStartupResult {
    ok: bool,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run: Option<profile::ProfileStartupRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_mode: Option<AgentModeSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recovery_hints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct WorkspaceStartResult {
    #[serde(flatten)]
    response: IpcResponse,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    viewer_auto_open: Option<WorkspaceViewerAutoOpen>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct ProfileWorkspaceOpenResult {
    ok: bool,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    open: Option<profile::ProfileWorkspaceOpen>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    viewer_auto_open: Option<WorkspaceViewerAutoOpen>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preview: Option<profile::ProfileWorkspaceOpenPreview>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_mode: Option<AgentModeSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_handles: Option<AgentTargetHandles>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recovery_hints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct WorkspaceRunResult {
    ok: bool,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run: Option<workspace::WorkspaceRun>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    preview: Option<workspace::WorkspaceRunPreview>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_mode: Option<AgentModeSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_handles: Option<AgentTargetHandles>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recovery_hints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct WorkspaceTerminalRunResult {
    ok: bool,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    terminal: Option<workspace::WorkspaceTerminal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    app: Option<workspace::WorkspaceApp>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    windows: Vec<workspace::WorkspaceWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_mode: Option<AgentModeSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_handles: Option<AgentTargetHandles>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recovery_hints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct WorkspaceTerminalReadResult {
    ok: bool,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    screen: Option<workspace::WorkspaceTerminalScreen>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_mode: Option<AgentModeSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_handles: Option<AgentTargetHandles>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recovery_hints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct WorkspaceTerminalInputResult {
    ok: bool,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input: Option<workspace::WorkspaceTerminalInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_mode: Option<AgentModeSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_handles: Option<AgentTargetHandles>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recovery_hints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct WorkspaceViewerAutoOpen {
    requested: bool,
    attempted: bool,
    ok: bool,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    launch: Option<viewer::ViewerLaunch>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct WorkspaceOpenViewerResponse {
    ok: bool,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    launch: Option<viewer::ViewerLaunch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_mode: Option<AgentModeSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_handles: Option<AgentTargetHandles>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recovery_hints: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct WorkspaceCloseViewerResponse {
    ok: bool,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    close: Option<viewer::ViewerClose>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent_mode: Option<AgentModeSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_handles: Option<AgentTargetHandles>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recovery_hints: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceStartParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    purpose: Option<String>,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    acknowledge_hidden_workspace: bool,
    #[serde(default)]
    acknowledge_unenforced_policy: bool,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    open_viewer: Option<bool>,
    #[serde(default)]
    viewer_always_on_top: bool,
}

impl WorkspaceStartParams {
    fn into_options(self) -> Result<WorkspaceStartOptions> {
        let width_explicit = self.width.is_some();
        let height_explicit = self.height.is_some();
        let mut options = WorkspaceStartOptions::default();
        if let Some(profile_id) = self.profile {
            profile::apply_profile_to_start_options(
                &profile_id,
                &mut options,
                width_explicit,
                height_explicit,
            )?;
        }
        if let Some(id) = self.id {
            options.id = id;
        }
        options.purpose = self.purpose;
        if let Some(width) = self.width {
            options.width = width;
        }
        if let Some(height) = self.height {
            options.height = height;
        }
        options.user_acknowledged_hidden_workspace = self.acknowledge_hidden_workspace;
        options.user_acknowledged_unenforced_policy = self.acknowledge_unenforced_policy;
        Ok(options)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct WorkspaceOpenProfileParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    purpose: Option<String>,
    profile: String,
    #[serde(default)]
    acknowledge_hidden_workspace: bool,
    #[serde(default)]
    acknowledge_unenforced_policy: bool,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    run_setup: bool,
    #[serde(default)]
    setup_timeout_ms: Option<u64>,
    #[serde(default)]
    setup_kill_on_timeout: bool,
    #[serde(default)]
    startup_wait_window: bool,
    #[serde(default)]
    startup_window_timeout_ms: Option<u64>,
    #[serde(default)]
    startup_screenshot_window: bool,
    #[serde(default)]
    open_viewer: Option<bool>,
    #[serde(default)]
    viewer_always_on_top: bool,
}

impl WorkspaceOpenProfileParams {
    fn to_start_options(&self) -> Result<(WorkspaceStartOptions, String)> {
        let width_explicit = self.width.is_some();
        let height_explicit = self.height.is_some();
        let profile_id = self.profile.clone();
        let mut options = WorkspaceStartOptions::default();
        profile::apply_profile_to_start_options(
            &profile_id,
            &mut options,
            width_explicit,
            height_explicit,
        )?;
        if let Some(id) = self.id.clone() {
            options.id = id;
        }
        options.purpose = self.purpose.clone();
        if let Some(width) = self.width {
            options.width = width;
        }
        if let Some(height) = self.height {
            options.height = height;
        }
        options.user_acknowledged_hidden_workspace = self.acknowledge_hidden_workspace;
        options.user_acknowledged_unenforced_policy = self.acknowledge_unenforced_policy;
        Ok((options, profile_id))
    }

    fn to_open_options(&self) -> profile::ProfileWorkspaceOpenOptions {
        profile::ProfileWorkspaceOpenOptions {
            run_setup: self.run_setup
                || self.setup_timeout_ms.is_some()
                || self.setup_kill_on_timeout,
            setup: profile::ProfileSetupOptions {
                dry_run: self.dry_run,
                wait: self.run_setup
                    || self.setup_timeout_ms.is_some()
                    || self.setup_kill_on_timeout,
                timeout_ms: self.setup_timeout_ms,
                kill_on_timeout: self.setup_kill_on_timeout,
                acknowledge_unenforced_policy: self.acknowledge_unenforced_policy,
            },
            startup: profile::ProfileStartupOptions {
                dry_run: self.dry_run,
                acknowledge_unenforced_policy: self.acknowledge_unenforced_policy,
                wait_window: self.startup_wait_window
                    || self.startup_window_timeout_ms.is_some()
                    || self.startup_screenshot_window,
                window_timeout_ms: self.startup_window_timeout_ms,
                screenshot_window: self.startup_screenshot_window,
            },
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceIdParams {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceOpenViewerParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    always_on_top: bool,
    #[serde(default)]
    input_forwarding: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceCloseViewerParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    all: bool,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceArtifactsParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    existing_only: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceListAppsParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    name_contains: Option<String>,
    #[serde(default)]
    command_contains: Option<String>,
    #[serde(default)]
    profile_id: Option<String>,
    #[serde(default)]
    running: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceOpenBrowserParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    browser_path: Option<PathBuf>,
    #[serde(default)]
    user_data_dir: Option<PathBuf>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    wait_window: Option<bool>,
    #[serde(default)]
    window_timeout_ms: Option<u64>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceBrowserTargetsParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    user_data_dir: Option<PathBuf>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceBrowserSnapshotParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    user_data_dir: Option<PathBuf>,
    #[serde(default)]
    target_id: Option<String>,
    #[serde(default)]
    title_contains: Option<String>,
    #[serde(default)]
    url_contains: Option<String>,
    #[serde(default)]
    max_text_chars: Option<usize>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceBrowserSearchResultsParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    user_data_dir: Option<PathBuf>,
    #[serde(default)]
    target_id: Option<String>,
    #[serde(default)]
    title_contains: Option<String>,
    #[serde(default)]
    url_contains: Option<String>,
    #[serde(default)]
    max_results: Option<usize>,
    #[serde(default)]
    min_vram_gb: Option<u32>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct WorkspaceBrowserNavigateParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    user_data_dir: Option<PathBuf>,
    #[serde(default)]
    target_id: Option<String>,
    #[serde(default)]
    title_contains: Option<String>,
    #[serde(default)]
    url_contains: Option<String>,
    url: String,
    #[serde(default)]
    wait_ms: Option<u64>,
    #[serde(default = "default_browser_navigate_snapshot")]
    snapshot: bool,
    #[serde(default)]
    max_text_chars: Option<usize>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

fn default_browser_navigate_snapshot() -> bool {
    true
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceBrowserClickParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    user_data_dir: Option<PathBuf>,
    #[serde(default)]
    target_id: Option<String>,
    #[serde(default)]
    title_contains: Option<String>,
    #[serde(default)]
    url_contains: Option<String>,
    #[serde(default)]
    selector: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    viewport_x: Option<i32>,
    #[serde(default)]
    viewport_y: Option<i32>,
    #[serde(default)]
    wait_ms: Option<u64>,
    #[serde(default = "default_browser_navigate_snapshot")]
    snapshot: bool,
    #[serde(default)]
    max_text_chars: Option<usize>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceListWindowsParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    include_hidden: bool,
    #[serde(default)]
    title_contains: Option<String>,
    #[serde(default)]
    class_contains: Option<String>,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    app_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceCleanupParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceStopParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct WorkspaceLaunchParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    acknowledge_unenforced_policy: bool,
    #[serde(default)]
    dry_run: bool,
    command: Vec<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: Vec<EnvVar>,
    #[serde(default)]
    wait_window: bool,
    #[serde(default)]
    window_timeout_ms: Option<u64>,
    #[serde(default)]
    screenshot_window: bool,
}

impl WorkspaceLaunchParams {
    fn into_launch_spec(self) -> Result<LaunchSpec> {
        let cwd_explicit = self.cwd.is_some();
        let mut spec = LaunchSpec {
            command: self.command,
            name: self.name,
            profile_id: None,
            applied_policy: None,
            user_acknowledged_unenforced_policy: self.acknowledge_unenforced_policy,
            cwd: self.cwd,
            env: self.env,
        };
        if let Some(profile_id) = self.profile {
            profile::apply_profile_to_launch_spec(&profile_id, &mut spec, cwd_explicit)?;
        }
        Ok(spec)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct WorkspaceRunParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    acknowledge_unenforced_policy: bool,
    #[serde(default)]
    dry_run: bool,
    command: Vec<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    env: Vec<EnvVar>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    tail_bytes: Option<u64>,
    #[serde(default)]
    kill_on_timeout: bool,
}

impl WorkspaceRunParams {
    fn into_launch_spec(self) -> Result<LaunchSpec> {
        let cwd_explicit = self.cwd.is_some();
        let mut spec = LaunchSpec {
            command: self.command,
            name: self.name,
            profile_id: None,
            applied_policy: None,
            user_acknowledged_unenforced_policy: self.acknowledge_unenforced_policy,
            cwd: self.cwd,
            env: self.env,
        };
        if let Some(profile_id) = self.profile {
            profile::apply_profile_to_launch_spec(&profile_id, &mut spec, cwd_explicit)?;
        }
        Ok(spec)
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceRunInTerminalParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    terminal_id: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    terminal_program: Option<PathBuf>,
    #[serde(default)]
    command: Vec<String>,
    #[serde(default)]
    wait_window: Option<bool>,
    #[serde(default)]
    window_timeout_ms: Option<u64>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceTerminalReadParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    terminal_id: Option<String>,
    #[serde(default)]
    preserve_trailing_spaces: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceTerminalInputParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    terminal_id: Option<String>,
    #[serde(default)]
    keys: Vec<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    delay_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceScreenshotParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    output_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceObserveParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    screenshot: bool,
    #[serde(default)]
    include_hidden: bool,
    #[serde(default)]
    output_path: Option<PathBuf>,
    #[serde(default)]
    events: bool,
    #[serde(default)]
    events_tail: Option<usize>,
    #[serde(default)]
    events_since_sequence: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceScreenshotWindowParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    window_id: Option<String>,
    #[serde(default)]
    title_contains: Option<String>,
    #[serde(default)]
    class_contains: Option<String>,
    pid: Option<u32>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    output_path: Option<PathBuf>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct WorkspaceWindowTargetParams {
    #[serde(default)]
    id: Option<String>,
    window_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct WorkspaceCloseWindowParams {
    #[serde(default)]
    id: Option<String>,
    window_id: String,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceWaitWindowParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    title_contains: Option<String>,
    #[serde(default)]
    class_contains: Option<String>,
    pid: Option<u32>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceCloseMatchingWindowParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    title_contains: Option<String>,
    #[serde(default)]
    class_contains: Option<String>,
    pid: Option<u32>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    dry_run: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceMoveWindowParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    window_id: Option<String>,
    #[serde(default)]
    title_contains: Option<String>,
    #[serde(default)]
    class_contains: Option<String>,
    pid: Option<u32>,
    #[serde(default)]
    app_id: Option<String>,
    x: i32,
    y: i32,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceResizeWindowParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    window_id: Option<String>,
    #[serde(default)]
    title_contains: Option<String>,
    #[serde(default)]
    class_contains: Option<String>,
    pid: Option<u32>,
    #[serde(default)]
    app_id: Option<String>,
    width: u32,
    height: u32,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceTargetedWindowParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    window_id: Option<String>,
    #[serde(default)]
    title_contains: Option<String>,
    #[serde(default)]
    class_contains: Option<String>,
    pid: Option<u32>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct WorkspaceClickParams {
    #[serde(default)]
    id: Option<String>,
    x: i32,
    y: i32,
    #[serde(default)]
    button: Option<u8>,
    #[serde(default)]
    count: Option<u8>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceClickWindowParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    window_id: Option<String>,
    #[serde(default)]
    title_contains: Option<String>,
    #[serde(default)]
    class_contains: Option<String>,
    pid: Option<u32>,
    #[serde(default)]
    app_id: Option<String>,
    x: i32,
    y: i32,
    #[serde(default)]
    button: Option<u8>,
    #[serde(default)]
    count: Option<u8>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct WorkspacePointerParams {
    #[serde(default)]
    id: Option<String>,
    x: i32,
    y: i32,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspacePointerWindowParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    window_id: Option<String>,
    #[serde(default)]
    title_contains: Option<String>,
    #[serde(default)]
    class_contains: Option<String>,
    pid: Option<u32>,
    #[serde(default)]
    app_id: Option<String>,
    x: i32,
    y: i32,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct WorkspaceDragParams {
    #[serde(default)]
    id: Option<String>,
    from_x: i32,
    from_y: i32,
    to_x: i32,
    to_y: i32,
    #[serde(default)]
    button: Option<u8>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceDragWindowParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    window_id: Option<String>,
    #[serde(default)]
    title_contains: Option<String>,
    #[serde(default)]
    class_contains: Option<String>,
    pid: Option<u32>,
    #[serde(default)]
    app_id: Option<String>,
    from_x: i32,
    from_y: i32,
    to_x: i32,
    to_y: i32,
    #[serde(default)]
    button: Option<u8>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct WorkspaceScrollParams {
    #[serde(default)]
    id: Option<String>,
    x: i32,
    y: i32,
    direction: ScrollDirection,
    #[serde(default)]
    amount: Option<u8>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct WorkspaceScrollWindowParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    window_id: Option<String>,
    #[serde(default)]
    title_contains: Option<String>,
    #[serde(default)]
    class_contains: Option<String>,
    pid: Option<u32>,
    #[serde(default)]
    app_id: Option<String>,
    x: i32,
    y: i32,
    direction: ScrollDirection,
    #[serde(default)]
    amount: Option<u8>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct WorkspaceKeyParams {
    #[serde(default)]
    id: Option<String>,
    key: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceKeyWindowParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    window_id: Option<String>,
    #[serde(default)]
    title_contains: Option<String>,
    #[serde(default)]
    class_contains: Option<String>,
    pid: Option<u32>,
    #[serde(default)]
    app_id: Option<String>,
    key: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct WorkspaceTypeTextParams {
    #[serde(default)]
    id: Option<String>,
    text: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceTypeWindowParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    window_id: Option<String>,
    #[serde(default)]
    title_contains: Option<String>,
    #[serde(default)]
    class_contains: Option<String>,
    pid: Option<u32>,
    #[serde(default)]
    app_id: Option<String>,
    text: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct WorkspaceClipboardSetParams {
    #[serde(default)]
    id: Option<String>,
    text: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct WorkspacePasteTextParams {
    #[serde(default)]
    id: Option<String>,
    text: String,
    #[serde(default)]
    key: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspacePasteWindowParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    window_id: Option<String>,
    #[serde(default)]
    title_contains: Option<String>,
    #[serde(default)]
    class_contains: Option<String>,
    pid: Option<u32>,
    #[serde(default)]
    app_id: Option<String>,
    text: String,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct WorkspaceReadAppLogParams {
    #[serde(default)]
    id: Option<String>,
    app_id: String,
    #[serde(default)]
    stream: Option<String>,
    #[serde(default)]
    tail_bytes: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct WorkspaceWaitAppParams {
    #[serde(default)]
    id: Option<String>,
    app_id: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    kill_on_timeout: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WorkspaceEventsParams {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    tail: Option<usize>,
    #[serde(default)]
    since_sequence: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct WorkspaceSetupParams {
    #[serde(default)]
    id: Option<String>,
    profile: String,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    wait: bool,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    kill_on_timeout: bool,
    #[serde(default)]
    acknowledge_unenforced_policy: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct WorkspaceProfileLaunchParams {
    #[serde(default)]
    id: Option<String>,
    profile: String,
    #[serde(default)]
    acknowledge_unenforced_policy: bool,
    #[serde(default)]
    dry_run: bool,
    #[serde(default)]
    wait_window: bool,
    #[serde(default)]
    window_timeout_ms: Option<u64>,
    #[serde(default)]
    screenshot_window: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct WorkspaceKillAppParams {
    #[serde(default)]
    id: Option<String>,
    app_id: String,
    #[serde(default)]
    dry_run: bool,
}

fn result_response(result: Result<IpcResponse>) -> IpcResponse {
    match result {
        Ok(response) => response,
        Err(error) => error_response(error.to_string(), None),
    }
}

fn error_response(message: String, status: Option<WorkspaceStatus>) -> IpcResponse {
    IpcResponse {
        ok: false,
        message,
        status,
        start_preview: None,
        launch_preview: None,
        ipc: None,
        environment: None,
        apps: None,
        windows: None,
        active_window: None,
        pointer: None,
        screenshot: None,
        app_log: None,
        clipboard: None,
        events: None,
        browser_targets: None,
        browser_snapshot: None,
        browser_search_results: None,
        browser_navigate: None,
        agent_mode: None,
        target_handles: None,
        recovery_hints: Vec::new(),
    }
}

fn target_handles_from_ipc_response(response: &IpcResponse) -> AgentTargetHandles {
    let mut handles = AgentTargetHandles::default();
    if let Some(status) = response.status.as_ref() {
        handles.workspace_id = Some(status.id.clone());
        for app in &status.apps {
            push_unique(&mut handles.app_ids, app.id.clone());
        }
    }
    if handles.workspace_id.is_none() {
        if let Some(environment) = response.environment.as_ref() {
            handles.workspace_id = Some(environment.workspace_id.clone());
        } else if let Some(ipc) = response.ipc.as_ref() {
            handles.workspace_id = Some(ipc.workspace_id.clone());
        }
    }
    if let Some(apps) = response.apps.as_ref() {
        for app in apps {
            push_unique(&mut handles.app_ids, app.id.clone());
        }
    }
    if let Some(windows) = response.windows.as_ref() {
        for window in windows {
            push_unique(&mut handles.window_ids, window.id.clone());
            if let Some(app_id) = window.app_id.clone() {
                push_unique(&mut handles.app_ids, app_id);
            }
        }
    }
    if let Some(window) = response.active_window.as_ref() {
        push_unique(&mut handles.window_ids, window.id.clone());
        if let Some(app_id) = window.app_id.clone() {
            push_unique(&mut handles.app_ids, app_id);
        }
    }
    if let Some(pointer) = response.pointer.as_ref() {
        if let Some(window_id) = pointer.window_id.clone() {
            push_unique(&mut handles.window_ids, window_id);
        }
    }
    if let Some(app_log) = response.app_log.as_ref() {
        push_unique(&mut handles.app_ids, app_log.app_id.clone());
    }
    if let Some(browser_targets) = response.browser_targets.as_ref() {
        handles.workspace_id = Some(browser_targets.id.clone());
        if let Some(app_id) = browser_targets.app_id.clone() {
            push_unique(&mut handles.app_ids, app_id);
        }
        for target_id in &browser_targets.browser_target_ids {
            push_unique(&mut handles.browser_target_ids, target_id.clone());
        }
    }
    if let Some(snapshot) = response.browser_snapshot.as_ref() {
        handles.workspace_id = Some(snapshot.id.clone());
        if let Some(app_id) = snapshot.app_id.clone() {
            push_unique(&mut handles.app_ids, app_id);
        }
        if let Some(target_id) = snapshot.browser_target_id.clone() {
            push_unique(&mut handles.browser_target_ids, target_id);
        }
    }
    if let Some(results) = response.browser_search_results.as_ref() {
        handles.workspace_id = Some(results.id.clone());
        if let Some(app_id) = results.app_id.clone() {
            push_unique(&mut handles.app_ids, app_id);
        }
        if let Some(target_id) = results.browser_target_id.clone() {
            push_unique(&mut handles.browser_target_ids, target_id);
        }
    }
    if let Some(navigate) = response.browser_navigate.as_ref() {
        handles.workspace_id = Some(navigate.id.clone());
        if let Some(app_id) = navigate.app_id.clone() {
            push_unique(&mut handles.app_ids, app_id);
        }
        if let Some(target_id) = navigate.browser_target_id.clone() {
            push_unique(&mut handles.browser_target_ids, target_id);
        }
    }
    handles
}

fn profile_open_target_handles(
    open: Option<&profile::ProfileWorkspaceOpen>,
    preview: Option<&profile::ProfileWorkspaceOpenPreview>,
) -> Option<AgentTargetHandles> {
    let mut handles = AgentTargetHandles::default();
    if let Some(open) = open {
        handles.workspace_id = Some(open.workspace_id.clone());
        merge_target_handles(&mut handles, target_handles_from_ipc_response(&open.start));
        if let Some(setup) = open.setup.as_ref() {
            for launch in &setup.launched {
                merge_target_handles(&mut handles, target_handles_from_ipc_response(launch));
            }
        }
        if let Some(startup) = open.startup.as_ref() {
            for launch in &startup.launched {
                merge_target_handles(&mut handles, target_handles_from_ipc_response(launch));
            }
        }
    }
    if let Some(preview) = preview {
        handles.workspace_id = Some(preview.workspace_id.clone());
        merge_target_handles(
            &mut handles,
            target_handles_from_ipc_response(&preview.start),
        );
    }
    (!handles.is_empty()).then_some(handles)
}

fn workspace_run_target_handles(
    run: Option<&workspace::WorkspaceRun>,
    preview: Option<&workspace::WorkspaceRunPreview>,
) -> Option<AgentTargetHandles> {
    let mut handles = AgentTargetHandles::default();
    if let Some(run) = run {
        push_unique(&mut handles.app_ids, run.app_id.clone());
        merge_target_handles(&mut handles, target_handles_from_ipc_response(&run.launch));
        merge_target_handles(&mut handles, target_handles_from_ipc_response(&run.wait));
        if let Some(kill) = run.kill.as_ref() {
            merge_target_handles(&mut handles, target_handles_from_ipc_response(kill));
        }
    }
    if let Some(preview) = preview {
        handles.workspace_id = Some(preview.workspace_id.clone());
    }
    (!handles.is_empty()).then_some(handles)
}

fn terminal_target_handles(
    terminal: &workspace::WorkspaceTerminal,
    app: Option<&workspace::WorkspaceApp>,
    windows: &Option<Vec<workspace::WorkspaceWindow>>,
) -> AgentTargetHandles {
    let mut handles = AgentTargetHandles {
        workspace_id: Some(terminal.workspace_id.clone()),
        ..Default::default()
    };
    push_unique(&mut handles.terminal_ids, terminal.terminal_id.clone());
    if let Some(app) = app {
        push_unique(&mut handles.app_ids, app.id.clone());
    }
    if let Some(app_id) = terminal.app_id.clone() {
        push_unique(&mut handles.app_ids, app_id);
    }
    if let Some(windows) = windows {
        for window in windows {
            push_unique(&mut handles.window_ids, window.id.clone());
            if let Some(app_id) = window.app_id.clone() {
                push_unique(&mut handles.app_ids, app_id);
            }
        }
    }
    handles
}

fn terminal_screen_target_handles(
    screen: &workspace::WorkspaceTerminalScreen,
) -> AgentTargetHandles {
    let mut handles = AgentTargetHandles {
        workspace_id: Some(screen.terminal.workspace_id.clone()),
        ..Default::default()
    };
    push_unique(
        &mut handles.terminal_ids,
        screen.terminal.terminal_id.clone(),
    );
    if let Some(app_id) = screen.terminal.app_id.clone() {
        push_unique(&mut handles.app_ids, app_id);
    }
    handles
}

fn terminal_input_target_handles(input: &workspace::WorkspaceTerminalInput) -> AgentTargetHandles {
    let mut handles = AgentTargetHandles {
        workspace_id: Some(input.terminal.workspace_id.clone()),
        ..Default::default()
    };
    push_unique(
        &mut handles.terminal_ids,
        input.terminal.terminal_id.clone(),
    );
    if let Some(app_id) = input.terminal.app_id.clone() {
        push_unique(&mut handles.app_ids, app_id);
    }
    handles
}

fn viewer_target_handles(viewer_id: &str) -> AgentTargetHandles {
    let mut handles = AgentTargetHandles {
        workspace_id: Some(viewer_id.to_string()),
        ..Default::default()
    };
    handles.viewer_ids.push(viewer_id.to_string());
    handles
}

fn merge_viewer_auto_open_handles(
    target: &mut Option<AgentTargetHandles>,
    auto_open: &WorkspaceViewerAutoOpen,
) {
    let Some(launch) = auto_open.launch.as_ref() else {
        return;
    };
    let mut handles = target.take().unwrap_or_default();
    merge_target_handles(&mut handles, viewer_target_handles(&launch.id));
    *target = Some(handles);
}

fn append_viewer_auto_open_hint(
    recovery_hints: &mut Vec<String>,
    auto_open: &WorkspaceViewerAutoOpen,
) {
    if auto_open.ok || !auto_open.requested {
        return;
    }
    push_unique(recovery_hints, auto_open.message.clone());
    push_unique(
        recovery_hints,
        "Call workspace_doctor and workspace_list_viewers to inspect viewer readiness.".to_string(),
    );
}

fn viewer_close_target_handles(close: &viewer::ViewerClose) -> AgentTargetHandles {
    let mut handles = AgentTargetHandles::default();
    if let Some(target_id) = close.target_id.clone() {
        handles.workspace_id = Some(target_id.clone());
        push_unique(&mut handles.viewer_ids, target_id);
    }
    for entry in close
        .candidates
        .iter()
        .chain(close.closed.iter())
        .chain(close.skipped.iter())
    {
        push_unique(&mut handles.viewer_ids, entry.viewer_id.clone());
    }
    handles
}

fn merge_target_handles(target: &mut AgentTargetHandles, source: AgentTargetHandles) {
    if target.workspace_id.is_none() {
        target.workspace_id = source.workspace_id;
    }
    for value in source.app_ids {
        push_unique(&mut target.app_ids, value);
    }
    for value in source.window_ids {
        push_unique(&mut target.window_ids, value);
    }
    for value in source.viewer_ids {
        push_unique(&mut target.viewer_ids, value);
    }
    for value in source.browser_target_ids {
        push_unique(&mut target.browser_target_ids, value);
    }
    for value in source.terminal_ids {
        push_unique(&mut target.terminal_ids, value);
    }
}

fn recovery_hints_for_message(message: &str) -> Vec<String> {
    let lower = message.to_ascii_lowercase();
    let mut hints = Vec::new();
    if lower.contains("live control")
        || lower.contains("read_only")
        || lower.contains("paused")
        || lower.contains("reactivat")
    {
        hints.push(live_control_reactivation_hint().to_string());
        hints.push(
            "Call mcp_control_state to confirm active/read_only/paused before retrying."
                .to_string(),
        );
    }
    if lower.contains("headless") || lower.contains("host-visible") || lower.contains("viewer") {
        hints.push("Call workspace_doctor and workspace_list_viewers before retrying workspace_open_viewer.".to_string());
    }
    if lower.contains("window")
        || lower.contains("xdotool")
        || lower.contains("click")
        || lower.contains("key")
    {
        hints.push("Call workspace_list_windows with include_hidden=true and retry with window_id or app_id.".to_string());
        hints.push(
            "Call workspace_observe with include_hidden=true if the visible target is unclear."
                .to_string(),
        );
    }
    if lower.contains("app") || lower.contains("pid") || lower.contains("process") {
        hints.push(
            "Call workspace_list_apps with running=true and use the returned app_id.".to_string(),
        );
    }
    if lower.contains("browser")
        || lower.contains("devtools")
        || lower.contains("target")
        || lower.contains("remote-debugging")
    {
        hints.push("Call workspace_open_browser to launch workspace Chrome with --user-data-dir and loopback DevTools flags.".to_string());
        hints.push("Call workspace_browser_targets to select a browser_target_id, then retry with target_id.".to_string());
        hints.push("Call workspace_list_apps to choose the browser app_id if more than one browser is running.".to_string());
    }
    if lower.contains("terminal")
        || lower.contains("tmux")
        || lower.contains("pane")
        || lower.contains("tty")
    {
        hints.push("Call workspace_run_in_terminal to create a tmux-backed terminal_id, then use workspace_terminal_read and workspace_terminal_input.".to_string());
    }
    if lower.contains("workspace") || lower.contains("socket") || lower.contains("not running") {
        hints.push(
            "Call workspace_list and workspace_status to verify the selected workspace id."
                .to_string(),
        );
    }
    if hints.is_empty() {
        hints.push("Call mcp_agent_context for current mode, handles, viewer, browser, and next recovery tools.".to_string());
    }
    hints
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !value.is_empty() && !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::ServerHandler;
    use std::fs;

    #[test]
    fn mcp_server_version_matches_crate_version() {
        // The #[tool_handler] macro defaults the advertised server version to
        // CARGO_PKG_VERSION when no `version =` is set. This guards against a
        // reintroduced hardcoded literal drifting from Cargo.toml (it had been
        // stuck at "0.1.1" across two releases).
        let info = AgentWorkspaceLinux::default().get_info();
        assert_eq!(info.server_info.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(info.server_info.name, "agent-workspace-linux");
    }

    fn catalog_tool<'a>(catalog: &'a McpActionCatalog, name: &str) -> &'a McpActionInfo {
        catalog
            .tools
            .iter()
            .find(|tool| tool.name == name)
            .unwrap_or_else(|| panic!("missing catalog entry for {name}"))
    }

    fn has_parameter_note(tool: &McpActionInfo, parameter: &str, text: &str) -> bool {
        tool.parameter_notes.iter().any(|note| {
            note.parameter == parameter
                && format!(
                    "{} {} {} {}",
                    note.when, note.effect, note.live_control, note.approval_hint
                )
                .contains(text)
        })
    }

    fn sample_workspace_app(
        id: &str,
        name: Option<&str>,
        running: bool,
    ) -> workspace::WorkspaceApp {
        workspace::WorkspaceApp {
            id: id.to_string(),
            name: name.map(str::to_string),
            pid: 1234,
            process_group_id: Some(1234),
            profile_id: Some("project-dev".to_string()),
            mount_isolation: "bubblewrap_mount_namespace".to_string(),
            network_isolation: "host".to_string(),
            command: vec!["xterm".to_string(), "-title".to_string(), id.to_string()],
            cwd: None,
            env: Vec::new(),
            stdout_path: None,
            stderr_path: None,
            started_at_unix: 10,
            running,
            exit_status: running.then_some("running".to_string()),
            exit_code: None,
            exit_signal: None,
            stopped_at_unix: (!running).then_some(20),
            runtime_seconds: Some(10),
        }
    }

    fn sample_workspace_status(id: &str, apps: Vec<workspace::WorkspaceApp>) -> WorkspaceStatus {
        WorkspaceStatus {
            id: id.to_string(),
            session_id: "session-1".to_string(),
            purpose: Some("Project QA".to_string()),
            profile_id: Some("project-dev".to_string()),
            applied_policy: None,
            profile_cwd: None,
            profile_env: Vec::new(),
            user_acknowledged_hidden_workspace: true,
            user_acknowledged_unenforced_policy: false,
            ready: true,
            started_at_unix: 1,
            display: ":90".to_string(),
            width: 1280,
            height: 720,
            runtime_dir: PathBuf::from("/tmp/agent-workspace-linux/default"),
            socket_path: PathBuf::from("/tmp/agent-workspace-linux/default/control.sock"),
            xauthority_path: PathBuf::from("/tmp/agent-workspace-linux/default/Xauthority"),
            daemon_pid: Some(100),
            x_server_pid: 101,
            window_manager_pid: Some(102),
            last_event_sequence: 42,
            apps,
        }
    }

    fn sample_workspace_manifest(
        id: &str,
        apps: Vec<workspace::WorkspaceApp>,
    ) -> workspace::WorkspaceManifest {
        workspace::WorkspaceManifest {
            id: id.to_string(),
            session_id: "session-2".to_string(),
            purpose: Some("Stopped browser task".to_string()),
            profile_id: Some("browser-session".to_string()),
            applied_policy: None,
            profile_cwd: None,
            profile_env: Vec::new(),
            user_acknowledged_hidden_workspace: true,
            user_acknowledged_unenforced_policy: false,
            ready: false,
            started_at_unix: 1,
            stopped_at_unix: Some(60),
            runtime_seconds: Some(59),
            display: ":91".to_string(),
            width: 1280,
            height: 720,
            runtime_dir: PathBuf::from("/tmp/agent-workspace-linux/stopped"),
            socket_path: PathBuf::from("/tmp/agent-workspace-linux/stopped/control.sock"),
            xauthority_path: PathBuf::from("/tmp/agent-workspace-linux/stopped/Xauthority"),
            daemon_pid: None,
            x_server_pid: None,
            window_manager_pid: None,
            event_log_path: PathBuf::from("/tmp/agent-workspace-linux/stopped/events.jsonl"),
            daemon_stdout_path: PathBuf::from("/tmp/agent-workspace-linux/stopped/daemon.out"),
            daemon_stderr_path: PathBuf::from("/tmp/agent-workspace-linux/stopped/daemon.err"),
            last_event_sequence: 7,
            apps,
        }
    }

    #[test]
    fn ipc_target_handles_collect_stable_ids() {
        let mut response = error_response(
            "workspace observed".to_string(),
            Some(sample_workspace_status(
                "default",
                vec![sample_workspace_app("app-1", Some("browser"), true)],
            )),
        );
        response.ok = true;
        response.windows = Some(vec![workspace::WorkspaceWindow {
            id: "win-1".to_string(),
            title: "Browser".to_string(),
            wm_class: Some("Chromium".to_string()),
            wm_instance: None,
            pid: Some(1234),
            app_id: Some("app-1".to_string()),
            visible: true,
            geometry: workspace::WindowGeometry {
                x: 0,
                y: 0,
                width: 800,
                height: 600,
                screen: None,
            },
        }]);
        response.browser_targets = Some(browser::WorkspaceBrowserTargets {
            ok: true,
            message: "targets".to_string(),
            id: "default".to_string(),
            app_id: Some("app-1".to_string()),
            app_pid: Some(1234),
            workspace_user_data_dir: None,
            host_user_data_dir: None,
            devtools_active_port_path: None,
            devtools_endpoint: None,
            targets: vec![browser::BrowserTarget {
                id: "target-1".to_string(),
                target_type: "page".to_string(),
                title: "Amazon".to_string(),
                url: "https://www.amazon.com".to_string(),
                web_socket_debugger_url: None,
            }],
            browser_target_ids: vec!["target-1".to_string()],
            agent_mode: None,
            target_handles: None,
            recovery_hints: Vec::new(),
            warnings: Vec::new(),
        });

        let handles = target_handles_from_ipc_response(&response);

        assert_eq!(handles.workspace_id.as_deref(), Some("default"));
        assert_eq!(handles.app_ids, vec!["app-1"]);
        assert_eq!(handles.window_ids, vec!["win-1"]);
        assert_eq!(handles.browser_target_ids, vec!["target-1"]);
    }

    #[test]
    fn recovery_hints_point_agents_to_next_tools() {
        let hints = recovery_hints_for_message(
            "MCP live control is read_only; xdotool click failed because browser target is missing",
        );

        assert!(hints.iter().any(|hint| hint.contains("mcp_control_state")));
        assert!(hints
            .iter()
            .any(|hint| hint.contains("workspace_list_windows")));
        assert!(hints
            .iter()
            .any(|hint| hint.contains("workspace_browser_targets")));
    }

    fn recommendation_checkpoint<'a>(
        action: &'a McpRecommendedAction,
        kind: &str,
    ) -> Option<&'a McpRecommendedActionCheckpoint> {
        action
            .approval_checkpoints
            .iter()
            .find(|checkpoint| checkpoint.kind == kind)
    }

    #[test]
    fn recommendations_classify_preview_and_host_visible_actions() {
        let preview = mcp_recommendation(
            "preview_profile_workspace",
            90,
            "start_user_task",
            "Preview profile",
            "workspace_open_profile",
            serde_json::json!({ "profile": "project-dev", "dry_run": true }),
            "Preview the profile-backed workspace before starting it.",
            "Dry-run preview is allowed; real start needs approval.",
            false,
            false,
            false,
        );
        assert_eq!(preview.action_type, "preview");
        assert!(preview.idempotent);
        let preview_checkpoint = recommendation_checkpoint(&preview, "preview_surface")
            .expect("dry-run recommendation should expose preview checkpoint");
        assert!(!preview_checkpoint.approval_required);
        assert!(!preview_checkpoint.blocks_action);
        assert_eq!(preview.approval_summary.blocking_count, 0);
        assert_eq!(preview.approval_summary.approval_required_count, 0);
        assert!(preview.approval_summary.next_boundary.is_none());

        let viewer = mcp_recommendation(
            "open_live_viewer",
            80,
            "user_visibility",
            "Open viewer",
            "workspace_open_viewer",
            serde_json::json!({ "id": "default" }),
            "Let the user monitor the hidden workspace.",
            "Open-world host-visible UI; use only when the user or host wants the viewer.",
            false,
            true,
            false,
        );
        assert_eq!(viewer.action_type, "host_visible_ui");
        assert!(viewer.idempotent);
        let viewer_checkpoint = recommendation_checkpoint(&viewer, "host_visible_ui")
            .expect("viewer recommendation should expose host-visible checkpoint");
        assert!(viewer_checkpoint.approval_required);
        assert!(viewer_checkpoint.blocks_action);
        let viewer_boundary = viewer
            .approval_summary
            .next_boundary
            .as_ref()
            .expect("viewer recommendation should summarize its host-visible boundary");
        assert_eq!(viewer_boundary.kind, "host_visible_ui");
        assert!(viewer_boundary.blocks_action);
        assert_eq!(viewer.approval_summary.blocking_count, 1);
    }

    #[test]
    fn recommendations_mark_destructive_followups_and_real_world_approval() {
        let safety_stop = mcp_recommendation(
            "safety_stop_available",
            60,
            "stop_agent_work",
            "Safety stop",
            "workspace_stop",
            serde_json::json!({ "id": "default", "dry_run": true }),
            "Preview apps that would be terminated before stopping.",
            "Dry-run is read-only; real stop is destructive but allowed as a safety stop.",
            true,
            false,
            false,
        );
        assert_eq!(safety_stop.action_type, "preview");
        assert!(safety_stop.idempotent);
        assert!(recommendation_checkpoint(&safety_stop, "preview_surface").is_some());
        let destructive_follow_up =
            recommendation_checkpoint(&safety_stop, "destructive_follow_up")
                .expect("stop preview should expose destructive follow-up");
        assert!(destructive_follow_up.approval_required);
        assert!(!destructive_follow_up.blocks_action);

        let grocery_plan = mcp_recommendation(
            "plan_browser_or_grocery_task",
            75,
            "derive_user_intent",
            "Plan grocery",
            "mcp_task_plan",
            serde_json::json!({ "intent": "grocery shopping" }),
            "Plan account, checkout, purchases, and browser data boundaries before acting.",
            "Read-only planning; no user approval required.",
            true,
            false,
            false,
        );
        assert_eq!(grocery_plan.action_type, "read_only");
        let real_world = recommendation_checkpoint(&grocery_plan, "real_world_action")
            .expect("grocery planning recommendation should flag real-world approval later");
        assert!(real_world.approval_required);
        assert!(!real_world.blocks_action);
        let grocery_boundary = grocery_plan
            .approval_summary
            .next_boundary
            .as_ref()
            .expect("grocery recommendation should summarize later real-world approval");
        assert_eq!(grocery_boundary.kind, "real_world_action");
        assert!(grocery_boundary.approval_required);
        assert!(!grocery_boundary.blocks_action);
        assert!(grocery_plan
            .approval_summary
            .approval_kinds
            .contains(&"real_world_action".to_string()));
    }

    #[test]
    fn session_approval_summary_selects_next_prioritized_boundary() {
        let read_only_plan = mcp_recommendation(
            "plan_browser_or_grocery_task",
            80,
            "derive_user_intent",
            "Plan grocery",
            "mcp_task_plan",
            serde_json::json!({ "intent": "grocery shopping" }),
            "Plan checkout and account-change boundaries before acting.",
            "Read-only planning; no user approval required.",
            true,
            false,
            false,
        );
        let viewer = mcp_recommendation(
            "open_live_viewer",
            70,
            "user_visibility",
            "Open viewer",
            "workspace_open_viewer",
            serde_json::json!({ "id": "default" }),
            "Let the user monitor the hidden workspace.",
            "Open-world host-visible UI; use only when the user or host wants the viewer.",
            false,
            true,
            false,
        );
        let summary = mcp_session_approval_summary(&[read_only_plan, viewer]);
        assert_eq!(summary.blocking_recommendation_count, 1);
        assert_eq!(summary.approval_required_recommendation_count, 2);
        assert!(summary
            .approval_kinds
            .contains(&"real_world_action".to_string()));
        assert!(summary
            .approval_kinds
            .contains(&"host_visible_ui".to_string()));
        let next = summary
            .next_boundary
            .as_ref()
            .expect("session should expose the first blocking recommendation boundary");
        assert_eq!(next.recommendation_id, "open_live_viewer");
        assert_eq!(next.kind, "host_visible_ui");
        assert!(next.blocks_action);
    }

    #[test]
    fn session_workspace_activity_summarizes_live_and_stopped_apps() {
        let running_entry = workspace::WorkspaceListEntry {
            id: "default".to_string(),
            runtime_dir: PathBuf::from("/tmp/agent-workspace-linux/default"),
            socket_path: PathBuf::from("/tmp/agent-workspace-linux/default/control.sock"),
            running: true,
            manifest: None,
            manifest_error: None,
            status: Some(sample_workspace_status(
                "default",
                vec![
                    sample_workspace_app("app-1", Some("xterm-qa"), true),
                    sample_workspace_app("app-2", None, false),
                ],
            )),
            error: None,
        };
        let running = mcp_workspace_activity_brief(&running_entry);
        assert_eq!(running.id, "default");
        assert!(running.running);
        assert_eq!(running.purpose.as_deref(), Some("Project QA"));
        assert_eq!(running.profile_id.as_deref(), Some("project-dev"));
        assert_eq!(running.display.as_deref(), Some(":90"));
        assert_eq!(running.app_count, 2);
        assert_eq!(running.running_app_count, 1);
        assert_eq!(running.last_event_sequence, 42);
        assert_eq!(running.inferred_intent.as_deref(), Some("app QA"));
        assert_eq!(
            running.intent_label.as_deref(),
            Some("Plan app QA workflow")
        );
        assert_eq!(running.apps[0].label, "xterm-qa");
        assert_eq!(running.apps[0].pid, Some(1234));
        assert_eq!(running.apps[1].label, "xterm");
        assert_eq!(running.apps[1].pid, None);

        let stopped_entry = workspace::WorkspaceListEntry {
            id: "browser-stopped".to_string(),
            runtime_dir: PathBuf::from("/tmp/agent-workspace-linux/stopped"),
            socket_path: PathBuf::from("/tmp/agent-workspace-linux/stopped/control.sock"),
            running: false,
            manifest: Some(sample_workspace_manifest(
                "browser-stopped",
                vec![sample_workspace_app(
                    "app-3",
                    Some("browser-session"),
                    false,
                )],
            )),
            manifest_error: None,
            status: None,
            error: Some("daemon is not running".to_string()),
        };
        let stopped = mcp_workspace_activity_brief(&stopped_entry);
        assert!(!stopped.running);
        assert_eq!(stopped.purpose.as_deref(), Some("Stopped browser task"));
        assert_eq!(stopped.profile_id.as_deref(), Some("browser-session"));
        assert_eq!(stopped.display.as_deref(), Some(":91"));
        assert_eq!(stopped.app_count, 1);
        assert_eq!(stopped.running_app_count, 0);
        assert_eq!(stopped.last_event_sequence, 7);
        assert_eq!(stopped.inferred_intent.as_deref(), Some("grocery shopping"));
        assert_eq!(
            stopped.intent_label.as_deref(),
            Some("Plan browser or shopping workflow")
        );
        assert!(
            stopped.error.is_none(),
            "normal stopped manifest fallback should not expose daemon connect errors"
        );
    }

    #[test]
    fn action_catalog_dry_run_tools_document_preview_boundary() {
        let catalog = mcp_action_catalog();
        for tool in catalog
            .tools
            .iter()
            .filter(|tool| tool.control_behavior == "blocked_when_not_active_unless_dry_run")
        {
            assert!(
                has_parameter_note(
                    tool,
                    "dry_run",
                    "Allowed while live control is read_only or paused"
                ),
                "{} should explain dry_run preview behavior",
                tool.name
            );
        }
    }

    #[test]
    fn action_catalog_parameter_notes_stay_advisory_for_clean_usage() {
        let catalog = mcp_action_catalog();
        assert!(catalog
            .notes
            .iter()
            .any(|note| note.contains("configured=false") && note.contains("advisory")));
        assert!(catalog
            .notes
            .iter()
            .any(|note| note.contains("configured=true") && note.contains("only narrow")));

        let permissions = catalog_tool(&catalog, "mcp_permissions");
        assert!(permissions.read_only);
        assert!(permissions.parameter_notes.is_empty());
        assert!(permissions.notes.contains("configured=false"));
        assert!(permissions.notes.contains("no MCP ceiling"));
        assert!(permissions.notes.contains("configured=true"));
        assert!(permissions.notes.contains("only narrow"));

        let screenshot = catalog_tool(&catalog, "workspace_screenshot");
        assert!(screenshot.read_only);
        assert!(has_parameter_note(
            screenshot,
            "output_path",
            "Allowed as observation"
        ));

        let wait_app = catalog_tool(&catalog, "workspace_wait_app");
        assert_eq!(wait_app.control_behavior, "conditional_kill_on_timeout");
        assert!(has_parameter_note(
            wait_app,
            "kill_on_timeout",
            "Blocked unless live control is active"
        ));

        let open_viewer = catalog_tool(&catalog, "workspace_open_viewer");
        assert!(has_parameter_note(
            open_viewer,
            "input_forwarding",
            "manual mouse/keyboard/paste forwarding"
        ));

        let export = catalog_tool(&catalog, "profile_export");
        assert_eq!(export.control_behavior, "conditional_output_path");
        assert!(has_parameter_note(export, "output_path", "host filesystem"));

        let control_update = catalog_tool(&catalog, "mcp_control_update");
        assert!(has_parameter_note(
            control_update,
            "confirmed_user_request",
            "re-enable mutating actions"
        ));
    }

    #[test]
    fn workspace_open_viewer_params_parse_input_forwarding() {
        let default_params: WorkspaceOpenViewerParams =
            serde_json::from_value(serde_json::json!({ "id": "qa" })).expect("default params");
        assert!(!default_params.input_forwarding);

        let rw_params: WorkspaceOpenViewerParams = serde_json::from_value(serde_json::json!({
            "id": "qa",
            "input_forwarding": true
        }))
        .expect("rw params");
        assert!(rw_params.input_forwarding);

        let schema = serde_json::to_value(schemars::schema_for!(WorkspaceOpenViewerParams))
            .expect("schema value");
        assert!(schema.to_string().contains("input_forwarding"));
    }

    #[test]
    fn action_catalog_live_control_matches_tool_dispatch_guards() {
        let source = include_str!("server.rs");
        let catalog = mcp_action_catalog();
        let catalog_names: BTreeSet<_> = catalog
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        let dispatch_names = dispatch_tool_names(source);

        assert_eq!(
            dispatch_names, catalog_names,
            "MCP tool dispatch and mcp_action_catalog should stay in lockstep"
        );

        for tool in &catalog.tools {
            let body = dispatch_body(source, &tool.name)
                .unwrap_or_else(|| panic!("missing dispatch body for {}", tool.name));
            match tool.control_behavior.as_str() {
                "blocked_when_not_active" => assert!(
                    body.contains(&format!("enforce_agent_mutation(\"{}\")", tool.name)),
                    "{} is cataloged as blocked_when_not_active but lacks direct live-control enforcement",
                    tool.name
                ),
                "blocked_when_not_active_unless_dry_run" => assert!(
                    body.contains("enforce_agent_mutation_unless_dry_run")
                        && body.contains(&format!("\"{}\"", tool.name)),
                    "{} is cataloged as dry-run conditional but lacks live-control dry-run enforcement",
                    tool.name
                ),
                "conditional_output_path" => {
                    assert_eq!(tool.name, "profile_export");
                    assert!(
                        body.contains("enforce_agent_mutation_unless_dry_run")
                            && body.contains("output_path.is_none()")
                            && body.contains("\"profile_export\""),
                        "profile_export should block output_path writes unless live control is active"
                    );
                }
                "conditional_kill_on_timeout" => {
                    assert_eq!(tool.name, "workspace_wait_app");
                    assert!(
                        body.contains("enforce_agent_mutation_unless_dry_run")
                            && body.contains("!params.kill_on_timeout")
                            && body.contains("\"workspace_wait_app kill_on_timeout\""),
                        "workspace_wait_app should block kill_on_timeout unless live control is active"
                    );
                }
                "safety_stop_allowed" => {
                    assert_eq!(tool.name, "workspace_stop");
                    assert!(
                        !body.contains("enforce_agent_mutation"),
                        "workspace_stop should remain available as the safety stop"
                    );
                }
                "headless_or_host_display_gated_open_world" => {
                    assert_eq!(tool.name, "workspace_open_viewer");
                    assert!(body.contains("self.headless"));
                    assert!(body.contains("ready_for_host_viewer"));
                }
                "viewer_control_allowed" => {
                    assert_eq!(tool.name, "workspace_close_viewer");
                    assert!(
                        body.contains("viewer::close_viewers"),
                        "workspace_close_viewer should dispatch through the repo-owned viewer registry cleanup path"
                    );
                    assert!(
                        !body.contains("enforce_agent_mutation"),
                        "workspace_close_viewer should remain available for viewer cleanup and recovery"
                    );
                }
                "always_allowed" | "observation_allowed" | "control_plane_allowed" | "allowed" => {}
                other => panic!("unknown live-control behavior {other:?} for {}", tool.name),
            }
        }
    }

    #[test]
    fn permission_sensitive_tools_validate_mcp_ceiling_in_dispatch() {
        let source = include_str!("server.rs");
        let expected_validators: &[(&str, &[&str])] = &[
            ("profile_check", &["self.permissions.validate_profile"]),
            ("profile_validate", &["self.permissions.validate_profile"]),
            ("profile_template", &["self.permissions.validate_profile"]),
            ("profile_put", &[".permissions", ".validate_profile"]),
            ("profile_import", &[".permissions", ".validate_profile"]),
            (
                "workspace_start",
                &[
                    "self.permissions.validate_profile",
                    "self.permissions.validate_start_options",
                ],
            ),
            (
                "workspace_open_profile",
                &[
                    "self.permissions.validate_profile",
                    "self.permissions.validate_start_options",
                ],
            ),
            (
                "workspace_launch_app",
                &["self.permissions.validate_launch_spec"],
            ),
            (
                "workspace_run_app",
                &["self.permissions.validate_launch_spec"],
            ),
            (
                "workspace_run_profile_setup",
                &["self.permissions.validate_profile"],
            ),
            (
                "workspace_launch_profile_apps",
                &["self.permissions.validate_profile"],
            ),
            (
                "workspace_open_viewer",
                &["viewer::open_viewer", "&self.permissions"],
            ),
        ];

        for (tool, validators) in expected_validators {
            assert_dispatch_contains_all(source, tool, validators);
        }
    }

    fn assert_dispatch_contains_all(source: &str, tool: &str, snippets: &[&str]) {
        let body = dispatch_body(source, tool)
            .unwrap_or_else(|| panic!("missing dispatch body for {tool}"));
        for snippet in snippets {
            assert!(
                body.contains(snippet),
                "{tool} must include {snippet:?} so configured MCP permissions stay an immutable ceiling"
            );
        }
    }

    fn dispatch_tool_names(source: &str) -> BTreeSet<&str> {
        let mut names = BTreeSet::new();
        for line in source.lines() {
            let trimmed = line.trim();
            let Some(rest) = trimmed.strip_prefix("name = \"") else {
                continue;
            };
            let Some((name, _)) = rest.split_once('"') else {
                continue;
            };
            if name != "agent-workspace-linux" {
                names.insert(name);
            }
        }
        names
    }

    fn dispatch_body<'a>(source: &'a str, tool_name: &str) -> Option<&'a str> {
        let name_marker = format!("name = \"{tool_name}\"");
        let name_index = source.find(&name_marker)?;
        let after_name = &source[name_index..];
        let fn_index = after_name.find("\n    fn ")?;
        let body_start = name_index + fn_index;
        let after_body_start = &source[body_start + 1..];
        let next_tool = after_body_start.find("\n    #[tool(");
        let next_handler = after_body_start.find("\n#[tool_handler(");
        let body_end = [next_tool, next_handler]
            .into_iter()
            .flatten()
            .min()
            .map(|offset| body_start + 1 + offset)
            .unwrap_or(source.len());
        Some(&source[body_start..body_end])
    }

    #[test]
    fn control_update_requires_confirmation_to_reactivate_mutation() {
        validate_control_reactivation_confirmation(
            McpControlMode::Active,
            McpControlMode::ReadOnly,
            false,
        )
        .unwrap();
        validate_control_reactivation_confirmation(
            McpControlMode::ReadOnly,
            McpControlMode::Paused,
            false,
        )
        .unwrap();

        let error = validate_control_reactivation_confirmation(
            McpControlMode::Paused,
            McpControlMode::Active,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("confirmed_user_request=true"));

        validate_control_reactivation_confirmation(
            McpControlMode::ReadOnly,
            McpControlMode::Active,
            true,
        )
        .unwrap();

        let inputs = live_control_reactivation_required_inputs();
        assert!(inputs
            .iter()
            .any(|input| input.contains("confirmed_user_request=true")));
        assert!(inputs.iter().any(|input| input.contains("mode=active")));
    }

    #[test]
    fn saved_profile_intent_inference_covers_common_workflows() {
        assert_eq!(
            inferred_profile_task_intent("browser-session").0,
            "grocery shopping"
        );
        assert_eq!(inferred_profile_task_intent("project-dev").0, "app QA");
        assert_eq!(inferred_profile_task_intent("unknown-profile").0, "app QA");
    }

    #[test]
    fn natural_shopping_phrases_normalize_to_browser_task() {
        for intent in [
            "buy milk and eggs",
            "purchase school supplies",
            "add bananas to cart",
            "checkout the grocery order",
            "order groceries for delivery",
            "delivery from supermarket",
        ] {
            assert_eq!(
                normalize_task_intent(intent),
                "browser_task",
                "intent phrase should route to browser/shopping planner: {intent}"
            );
            assert!(
                task_intent_is_shopping_or_grocery(intent),
                "intent phrase should expose shopping safety boundaries: {intent}"
            );
        }
    }

    #[test]
    fn natural_app_qa_phrases_normalize_to_app_qa() {
        for intent in [
            "test the local UI",
            "verify the frontend",
            "debug the desktop window",
            "run smoke checks",
            "check render behavior",
        ] {
            assert_eq!(
                normalize_task_intent(intent),
                "app_qa",
                "intent phrase should route to app-QA planner: {intent}"
            );
        }
    }

    fn task_plan_without_saved_profiles(
        intent: &str,
        user_data_dir: Option<PathBuf>,
        headless: bool,
    ) -> McpTaskPlan {
        task_plan_with_context(intent, user_data_dir, headless, Vec::new(), Vec::new())
    }

    fn task_plan_with_context(
        intent: &str,
        user_data_dir: Option<PathBuf>,
        headless: bool,
        profile_ids: Vec<String>,
        running_workspace_ids: Vec<String>,
    ) -> McpTaskPlan {
        task_plan_from_params(
            McpTaskPlanParams {
                intent: intent.to_string(),
                workspace_id: None,
                profile_id: None,
                project_path: None,
                browser_path: None,
                user_data_dir,
                target_url: None,
                shopping_list: None,
                budget: None,
                fulfillment: None,
                substitution_policy: None,
                cart_mutation_approved: false,
                final_cart_reviewed: false,
                real_world_action_approved: false,
                open_viewer: None,
            },
            headless,
            profile_ids,
            running_workspace_ids,
        )
    }

    fn task_plan_from_params(
        params: McpTaskPlanParams,
        headless: bool,
        profile_ids: Vec<String>,
        running_workspace_ids: Vec<String>,
    ) -> McpTaskPlan {
        build_mcp_task_plan_with_context(
            params,
            &McpPermissionState::default(),
            headless,
            true,
            profile_ids,
            running_workspace_ids,
        )
    }

    fn task_step<'a>(plan: &'a McpTaskPlan, id: &str) -> Option<&'a McpTaskPlanStep> {
        plan.steps.iter().find(|step| step.id == id)
    }

    fn task_checkpoint<'a>(
        plan: &'a McpTaskPlan,
        step_id: &str,
        kind: &str,
    ) -> Option<&'a McpTaskPlanApprovalCheckpoint> {
        plan.approval_checkpoints
            .iter()
            .find(|checkpoint| checkpoint.step_id == step_id && checkpoint.kind == kind)
    }

    fn action_boundary<'a>(
        plan: &'a McpTaskPlan,
        id: &str,
    ) -> Option<&'a McpTaskPlanActionBoundary> {
        plan.task_context
            .action_boundaries
            .iter()
            .find(|boundary| boundary.id == id)
    }

    #[test]
    fn running_workspace_app_qa_plan_continues_existing_workspace() {
        let plan = task_plan_with_context(
            "app QA",
            None,
            false,
            vec!["project-dev".to_string()],
            vec![DEFAULT_WORKSPACE_ID.to_string()],
        );

        assert_eq!(plan.normalized_intent, "app_qa");
        assert!(
            plan.assumptions
                .iter()
                .any(|assumption| assumption.contains("already running")),
            "running workspace plan should explain that it continues existing state: {plan:?}"
        );
        assert!(
            !plan
                .steps
                .iter()
                .any(|step| step.tool == "workspace_open_profile"),
            "running workspace app QA plan should not try to start another profile: {plan:?}"
        );
        let observe = task_step(&plan, "observe_running_project_workspace")
            .expect("running app QA plan should observe first");
        assert!(observe.ready_to_call);
        assert!(observe.read_only);
        let list_apps = task_step(&plan, "list_running_project_apps")
            .expect("running app QA plan should list apps");
        assert!(list_apps.ready_to_call);
        assert!(list_apps
            .depends_on
            .contains(&"observe_running_project_workspace".to_string()));
        let events = task_step(&plan, "read_recent_project_events")
            .expect("running app QA plan should read recent events");
        assert!(events.ready_to_call);
        assert!(events.read_only);
        let app_log = task_step(&plan, "read_project_app_log_after_app_id")
            .expect("running app QA plan should collect app logs after app id is known");
        assert!(!app_log.ready_to_call);
        assert!(app_log
            .required_input
            .iter()
            .any(|input| input.contains("app_id")));
        let capture = task_step(&plan, "capture_active_project_window")
            .expect("running app QA plan should request a targeted evidence screenshot");
        assert!(!capture.ready_to_call);
        assert!(capture
            .required_input
            .iter()
            .any(|input| input.contains("active_window.id") && input.contains("app_id")));
        assert!(task_step(&plan, "open_viewer_for_running_project").is_some());
    }

    #[test]
    fn running_workspace_browser_plan_preserves_real_world_boundary() {
        let plan = task_plan_with_context(
            "grocery shopping",
            None,
            true,
            vec!["browser-session".to_string()],
            vec![DEFAULT_WORKSPACE_ID.to_string()],
        );

        assert_eq!(plan.normalized_intent, "browser_task");
        assert!(
            !plan
                .steps
                .iter()
                .any(|step| step.tool == "workspace_open_profile"),
            "running browser plan should not start another browser session: {plan:?}"
        );
        let observe = task_step(&plan, "observe_running_browser_workspace")
            .expect("running browser plan should observe first");
        assert!(observe.ready_to_call);
        assert!(observe.read_only);
        let events = task_step(&plan, "read_recent_browser_events")
            .expect("running browser plan should read recent events");
        assert!(events.ready_to_call);
        assert!(events.read_only);
        let targets = task_step(&plan, "discover_running_browser_devtools_targets")
            .expect("running browser plan should discover workspace Chrome DevTools targets");
        assert!(targets.ready_to_call);
        assert_eq!(targets.tool, "workspace_browser_targets");
        assert!(targets
            .reason
            .contains("workspace-owned Chrome DevTools endpoint"));
        let search_results = task_step(&plan, "extract_running_browser_search_results")
            .expect("shopping browser plan should extract structured results");
        assert_eq!(search_results.tool, "workspace_browser_search_results");
        assert!(!search_results.ready_to_call);
        assert!(search_results
            .depends_on
            .contains(&"snapshot_running_browser_page".to_string()));
        let capture = task_step(&plan, "capture_browser_window_when_targeted")
            .expect("running browser plan should capture browser window evidence");
        assert!(!capture.ready_to_call);
        assert!(capture
            .required_input
            .iter()
            .any(|input| input.contains("active_window.id") && input.contains("app_id")));
        let boundary = task_step(&plan, "confirm_real_world_browser_boundary")
            .expect("running browser plan should include real-world boundary confirmation");
        assert!(boundary.ready_to_call);
        assert!(boundary.read_only);
        assert!(boundary
            .required_input
            .iter()
            .any(|input| input.contains("checkout") && input.contains("account changes")));
        assert!(boundary
            .required_input
            .iter()
            .any(|input| input.contains("target_url")));
        assert!(boundary
            .required_input
            .iter()
            .any(|input| input.contains("shopping_list")));
        assert!(boundary
            .required_input
            .iter()
            .any(|input| input.contains("substitution_policy")));
        assert!(plan
            .needs_user_input
            .iter()
            .any(|input| input.contains("target_url")));
        assert!(plan
            .needs_user_input
            .iter()
            .any(|input| input.contains("shopping_list")));
        assert_eq!(plan.task_context.task_kind, "browser_task");
        assert_eq!(plan.task_context.workspace_id, DEFAULT_WORKSPACE_ID);
        assert!(plan
            .task_context
            .safety_boundaries
            .iter()
            .any(|boundary| boundary.contains("workspace-owned Chrome DevTools")));
        assert!(plan
            .task_context
            .missing_inputs
            .iter()
            .any(|input| input.name == "target_url"));
        assert!(plan
            .task_context
            .missing_inputs
            .iter()
            .any(|input| input.name == "shopping_list"));
        assert!(plan
            .task_context
            .approval_kinds
            .contains(&"real_world_action".to_string()));
        let real_world_checkpoint = task_checkpoint(
            &plan,
            "confirm_real_world_browser_boundary",
            "real_world_action",
        )
        .expect("browser boundary step should expose real-world checkpoint");
        assert!(real_world_checkpoint.approval_required);
        assert!(!real_world_checkpoint.blocks_step);
        assert!(
            task_step(&plan, "open_viewer_for_running_browser").is_none(),
            "headless running browser plan should not offer host-visible viewer: {plan:?}"
        );
    }

    #[test]
    fn grocery_plan_exposes_structured_action_boundaries() {
        let user_data_dir = std::env::temp_dir().join(format!(
            "agent-workspace-grocery-boundary-test-{}",
            std::process::id()
        ));
        fs::create_dir_all(&user_data_dir).expect("create user-data dir");

        let plan = task_plan_from_params(
            McpTaskPlanParams {
                intent: "grocery shopping".to_string(),
                workspace_id: None,
                profile_id: None,
                project_path: None,
                browser_path: None,
                user_data_dir: Some(user_data_dir.clone()),
                target_url: Some("https://example-grocery.test".to_string()),
                shopping_list: Some("milk 2L, apples 1kg".to_string()),
                budget: Some("under 120 ILS".to_string()),
                fulfillment: Some("delivery tomorrow morning".to_string()),
                substitution_policy: Some("ask before replacing must-have items".to_string()),
                cart_mutation_approved: false,
                final_cart_reviewed: false,
                real_world_action_approved: false,
                open_viewer: None,
            },
            false,
            Vec::new(),
            Vec::new(),
        );
        let _ = fs::remove_dir_all(&user_data_dir);

        for expected in [
            "user_data_dir",
            "target_url",
            "shopping_list",
            "budget",
            "fulfillment",
            "substitution_policy",
        ] {
            assert!(
                plan.task_context
                    .provided_inputs
                    .iter()
                    .any(|input| input.name == expected),
                "complete grocery plan should preserve provided {expected}: {plan:?}",
            );
        }
        assert!(
            plan.task_context.missing_inputs.is_empty(),
            "complete grocery plan should not ask again for supplied grocery details: {plan:?}"
        );
        for stale_need in [
            "target_url",
            "shopping_list",
            "budget",
            "fulfillment",
            "substitution_policy",
        ] {
            assert!(
                !plan
                    .needs_user_input
                    .iter()
                    .any(|need| need.contains(stale_need)),
                "complete grocery plan should not keep stale {stale_need} prompts: {plan:?}",
            );
        }

        let navigate = action_boundary(&plan, "navigate_and_search")
            .expect("grocery plan should classify navigation/search");
        assert!(navigate.ready);
        assert!(!navigate.approval_required);
        assert!(navigate.required_inputs.is_empty());

        let compare = action_boundary(&plan, "compare_items_and_prices")
            .expect("grocery plan should classify item comparison");
        assert!(compare.ready);
        assert!(!compare.approval_required);
        assert_eq!(compare.action_type, "shopping_research");

        let cart = action_boundary(&plan, "draft_cart_changes")
            .expect("grocery plan should classify cart mutation");
        assert!(cart.ready);
        assert!(cart.approval_required);
        assert!(!cart.approved);
        assert_eq!(cart.approval_kind.as_deref(), Some("cart_mutation"));
        assert!(cart
            .missing_approvals
            .contains(&"explicit_cart_mutation_approval".to_string()));

        let checkout = action_boundary(&plan, "checkout_order_or_account_change")
            .expect("grocery plan should classify checkout/account changes");
        assert!(!checkout.ready);
        assert!(checkout.approval_required);
        assert!(!checkout.approved);
        assert_eq!(checkout.approval_kind.as_deref(), Some("real_world_action"));
        assert!(checkout
            .required_inputs
            .contains(&"explicit_checkout_approval".to_string()));
        assert!(checkout
            .missing_approvals
            .contains(&"final_cart_review".to_string()));
        assert!(checkout
            .missing_approvals
            .contains(&"explicit_checkout_approval".to_string()));
        assert!(plan
            .task_context
            .approval_kinds
            .contains(&"cart_mutation".to_string()));
        assert!(plan
            .task_context
            .approval_kinds
            .contains(&"real_world_action".to_string()));

        let run_step = task_step(&plan, "run_browser_session_after_save")
            .expect("complete grocery plan should include the approved browser run step");
        for stale_need in [
            "target_url",
            "shopping_list",
            "budget",
            "fulfillment",
            "substitution_policy",
        ] {
            assert!(
                !run_step
                    .required_input
                    .iter()
                    .any(|input| input.contains(stale_need)),
                "complete grocery run step should not require already supplied {stale_need}: {plan:?}",
            );
        }
        assert!(run_step
            .required_input
            .iter()
            .any(|input| input.contains("checkout")));

        let boundary_step = task_step(&plan, "confirm_real_world_boundary_after_start")
            .expect("complete grocery plan should still reconfirm real-world boundaries");
        for stale_need in [
            "target_url",
            "shopping_list",
            "budget",
            "fulfillment",
            "substitution_policy",
        ] {
            assert!(
                !boundary_step
                    .required_input
                    .iter()
                    .any(|input| input.contains(stale_need)),
                "complete grocery boundary step should not require already supplied {stale_need}: {plan:?}",
            );
        }
        assert!(boundary_step
            .required_input
            .iter()
            .any(|input| input.contains("checkout") && input.contains("account changes")));

        let approved_plan = task_plan_from_params(
            McpTaskPlanParams {
                intent: "grocery shopping".to_string(),
                workspace_id: None,
                profile_id: None,
                project_path: None,
                browser_path: None,
                user_data_dir: Some(user_data_dir),
                target_url: Some("https://example-grocery.test".to_string()),
                shopping_list: Some("milk 2L, apples 1kg".to_string()),
                budget: Some("under 120 ILS".to_string()),
                fulfillment: Some("delivery tomorrow morning".to_string()),
                substitution_policy: Some("ask before replacing must-have items".to_string()),
                cart_mutation_approved: true,
                final_cart_reviewed: true,
                real_world_action_approved: true,
                open_viewer: None,
            },
            false,
            Vec::new(),
            Vec::new(),
        );
        for expected in [
            "cart_mutation_approved",
            "final_cart_reviewed",
            "real_world_action_approved",
        ] {
            assert!(
                approved_plan
                    .task_context
                    .provided_inputs
                    .iter()
                    .any(|input| input.name == expected && input.value == "true"),
                "approved grocery plan should preserve {expected}: {approved_plan:?}",
            );
        }
        let approved_cart = action_boundary(&approved_plan, "draft_cart_changes")
            .expect("approved grocery plan should classify cart mutation");
        assert!(approved_cart.ready);
        assert!(approved_cart.approved);
        assert!(approved_cart.missing_approvals.is_empty());
        let approved_checkout = action_boundary(&approved_plan, "checkout_order_or_account_change")
            .expect("approved grocery plan should classify checkout/account changes");
        assert!(approved_checkout.ready);
        assert!(approved_checkout.approved);
        assert!(approved_checkout.required_inputs.is_empty());
        assert!(approved_checkout.missing_approvals.is_empty());
    }

    #[test]
    fn app_qa_plan_exposes_structured_action_boundaries() {
        let plan = task_plan_from_params(
            McpTaskPlanParams {
                intent: "test the local UI".to_string(),
                workspace_id: None,
                profile_id: None,
                project_path: Some(PathBuf::from("/tmp/example-project")),
                browser_path: None,
                user_data_dir: None,
                target_url: None,
                shopping_list: None,
                budget: None,
                fulfillment: None,
                substitution_policy: None,
                cart_mutation_approved: false,
                final_cart_reviewed: false,
                real_world_action_approved: false,
                open_viewer: None,
            },
            false,
            Vec::new(),
            Vec::new(),
        );

        assert_eq!(plan.normalized_intent, "app_qa");
        assert_eq!(plan.task_context.task_kind, "app_qa");
        assert!(plan
            .task_context
            .provided_inputs
            .iter()
            .any(|input| input.name == "project_path"));
        assert!(plan
            .task_context
            .approval_kinds
            .contains(&"hidden_workspace".to_string()));
        assert!(plan
            .task_context
            .approval_kinds
            .contains(&"project_file_write".to_string()));
        assert!(plan
            .approval_summary
            .approval_kinds
            .contains(&"project_file_write".to_string()));
        let next_boundary = plan
            .approval_summary
            .next_boundary
            .as_ref()
            .expect("app-QA plan should expose the next user boundary");
        assert_eq!(next_boundary.kind, "required_input");
        assert_eq!(next_boundary.step_id, "dry_run_save_project_profile");
        assert!(next_boundary.blocks_step);
        assert!(plan.approval_summary.blocking_count > 0);

        let observe = action_boundary(&plan, "observe_project_state")
            .expect("app-QA plan should classify safe observation");
        assert_eq!(observe.action_type, "read_only_observation");
        assert!(observe.ready);
        assert!(!observe.approval_required);

        let start = action_boundary(&plan, "start_or_attach_project_workspace")
            .expect("app-QA plan should classify workspace start/attach");
        assert_eq!(start.action_type, "hidden_workspace_start");
        assert!(start.ready);
        assert!(start.approval_required);
        assert_eq!(start.approval_kind.as_deref(), Some("hidden_workspace"));
        assert!(start.required_inputs.is_empty());

        let evidence = action_boundary(&plan, "collect_qa_evidence")
            .expect("app-QA plan should classify evidence collection");
        assert_eq!(evidence.action_type, "read_only_evidence");
        assert!(!evidence.ready);
        assert!(evidence
            .required_inputs
            .contains(&"running_workspace".to_string()));

        let input = action_boundary(&plan, "drive_workspace_app")
            .expect("app-QA plan should classify workspace-local input");
        assert_eq!(input.action_type, "workspace_input");
        assert!(!input.approval_required);
        assert!(input
            .required_inputs
            .contains(&"stable_app_id_or_window".to_string()));

        let file_write = action_boundary(&plan, "write_mounted_project_files")
            .expect("app-QA plan should classify mounted project writes separately");
        assert_eq!(file_write.action_type, "project_file_mutation");
        assert!(file_write.approval_required);
        assert_eq!(
            file_write.approval_kind.as_deref(),
            Some("project_file_write")
        );
        assert!(file_write
            .required_inputs
            .contains(&"explicit_code_change_request".to_string()));
    }

    #[test]
    fn running_app_qa_boundaries_do_not_request_another_workspace_start() {
        let plan = task_plan_with_context(
            "verify the UI",
            None,
            false,
            Vec::new(),
            vec![DEFAULT_WORKSPACE_ID.to_string()],
        );

        assert_eq!(plan.normalized_intent, "app_qa");
        let start = action_boundary(&plan, "start_or_attach_project_workspace")
            .expect("running app-QA plan should still classify start/attach");
        assert!(start.ready);
        assert!(!start.approval_required);
        assert!(start.approval_kind.is_none());
        assert!(start.required_inputs.is_empty());

        let evidence = action_boundary(&plan, "collect_qa_evidence")
            .expect("running app-QA plan should mark evidence collection ready");
        assert!(evidence.ready);
        assert!(evidence.required_inputs.is_empty());

        let input = action_boundary(&plan, "drive_workspace_app")
            .expect("running app-QA plan should still require a target before input");
        assert!(!input.ready);
        assert_eq!(
            input.required_inputs,
            vec!["stable_app_id_or_window".to_string()]
        );
    }

    #[test]
    fn saved_profile_app_qa_plan_collects_evidence_after_start() {
        let plan = task_plan_with_context(
            "app QA",
            None,
            false,
            vec!["project-dev".to_string()],
            Vec::new(),
        );

        let observe = task_step(&plan, "observe_project_workspace")
            .expect("saved-profile app QA plan should observe after start");
        assert!(!observe.ready_to_call);
        assert!(observe
            .depends_on
            .contains(&"run_project_profile_after_approval".to_string()));
        let list_apps = task_step(&plan, "list_project_apps_after_start")
            .expect("saved-profile app QA plan should list apps after start");
        assert!(!list_apps.ready_to_call);
        assert!(list_apps
            .depends_on
            .contains(&"observe_project_workspace".to_string()));
        let events = task_step(&plan, "read_project_events_after_start")
            .expect("saved-profile app QA plan should read events after start");
        assert!(!events.ready_to_call);
        assert!(events.read_only);
        let app_log = task_step(&plan, "read_project_app_log_after_start")
            .expect("saved-profile app QA plan should read app logs after app id is known");
        assert!(!app_log.ready_to_call);
        assert!(app_log
            .required_input
            .iter()
            .any(|input| input.contains("app_id")));
        let screenshot = task_step(&plan, "capture_project_window_after_start")
            .expect("saved-profile app QA plan should capture targeted window evidence");
        assert!(!screenshot.ready_to_call);
        assert!(screenshot
            .required_input
            .iter()
            .any(|input| input.contains("active_window.id") && input.contains("app_id")));
    }

    #[test]
    fn browser_plan_waits_for_profile_input_before_viewer() {
        let plan = task_plan_without_saved_profiles("grocery shopping", None, false);

        assert_eq!(plan.normalized_intent, "browser_task");
        assert!(
            plan.needs_user_input
                .iter()
                .any(|need| need.contains("browser user-data directory")),
            "plan should ask for explicit browser profile input: {plan:?}"
        );
        assert!(
            task_step(&plan, "open_viewer_when_browser_runs").is_none(),
            "viewer should not be offered before there is a runnable browser workspace step: {plan:?}"
        );
        assert!(
            task_step(&plan, "observe_browser_workspace").is_none(),
            "post-start observation should wait for a runnable browser workspace step: {plan:?}"
        );
        let template_input = task_checkpoint(&plan, "template_browser_session", "required_input")
            .expect("missing browser data should be exposed as a structured checkpoint");
        assert!(template_input.approval_required);
        assert!(template_input.blocks_step);
        assert!(template_input
            .required_input
            .iter()
            .any(|input| input.contains("user_data_dir")));
        assert!(plan
            .needs_user_input
            .iter()
            .any(|need| need.contains("target_url")));
        assert!(plan
            .needs_user_input
            .iter()
            .any(|need| need.contains("shopping_list")));
        assert!(plan
            .needs_user_input
            .iter()
            .any(|need| need.contains("substitution_policy")));
        assert!(plan
            .needs_user_input
            .iter()
            .any(|need| need.contains("budget")));
        assert!(plan
            .task_context
            .missing_inputs
            .iter()
            .any(|input| input.name == "browser_user_data"));
        assert!(plan
            .task_context
            .missing_inputs
            .iter()
            .any(|input| input.name == "substitution_policy"));
        assert!(plan
            .task_context
            .approval_kinds
            .contains(&"required_input".to_string()));
        let summary_boundary = plan
            .approval_summary
            .next_boundary
            .as_ref()
            .expect("browser plan should expose a next approval/input boundary");
        assert_eq!(summary_boundary.kind, "required_input");
        assert_eq!(summary_boundary.step_id, "template_browser_session");
        assert!(summary_boundary.blocks_step);
        assert!(summary_boundary
            .required_input
            .iter()
            .any(|input| input.contains("user_data_dir")));
        assert!(plan
            .approval_summary
            .approval_kinds
            .contains(&"real_world_action".to_string()));
    }

    #[test]
    fn non_headless_plan_suppresses_viewer_when_host_display_is_unavailable() {
        let user_data_dir = std::env::temp_dir().join(format!(
            "agent-workspace-no-host-viewer-plan-test-{}",
            std::process::id()
        ));
        fs::create_dir_all(&user_data_dir).expect("create user-data dir");

        let plan = build_mcp_task_plan_with_context(
            McpTaskPlanParams {
                intent: "grocery shopping".to_string(),
                workspace_id: None,
                profile_id: None,
                project_path: None,
                browser_path: None,
                user_data_dir: Some(user_data_dir.clone()),
                target_url: Some("https://example.invalid/grocery".to_string()),
                shopping_list: Some("milk, eggs".to_string()),
                budget: Some("under 50 ILS".to_string()),
                fulfillment: Some("delivery".to_string()),
                substitution_policy: Some("ask before replacing".to_string()),
                cart_mutation_approved: false,
                final_cart_reviewed: false,
                real_world_action_approved: false,
                open_viewer: None,
            },
            &McpPermissionState::default(),
            false,
            false,
            Vec::new(),
            Vec::new(),
        );
        let _ = fs::remove_dir_all(&user_data_dir);

        assert!(
            !plan.headless,
            "plan should still report non-headless MCP mode"
        );
        assert!(!plan.host_viewer_ready);
        assert!(!plan.viewer_available);
        assert!(
            plan.viewer_unavailable_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("ready_for_host_viewer=false")),
            "plan should explain why the viewer is absent: {plan:?}"
        );
        assert!(
            !plan
                .steps
                .iter()
                .any(|step| step.tool == "workspace_open_viewer"),
            "non-headless plan should not suggest host-visible viewer without a host display: {plan:?}"
        );
        assert!(
            !plan
                .task_context
                .approval_kinds
                .contains(&"host_visible_ui".to_string()),
            "plan should not expose host-visible approval kind when the viewer cannot open: {plan:?}"
        );
    }

    #[test]
    fn browser_plan_offers_viewer_only_after_runnable_browser_step() {
        let user_data_dir = std::env::temp_dir().join(format!(
            "agent-workspace-browser-plan-test-{}",
            std::process::id()
        ));
        fs::create_dir_all(&user_data_dir).expect("create user-data dir");

        let plan = task_plan_without_saved_profiles(
            "grocery shopping",
            Some(user_data_dir.clone()),
            false,
        );
        let _ = fs::remove_dir_all(&user_data_dir);

        assert!(plan
            .task_context
            .provided_inputs
            .iter()
            .any(|input| input.name == "user_data_dir"));
        assert!(plan
            .task_context
            .missing_inputs
            .iter()
            .any(|input| input.name == "target_url"));
        assert!(plan
            .task_context
            .safety_boundaries
            .iter()
            .any(|boundary| boundary.contains("Purchases")));
        assert!(plan
            .task_context
            .approval_kinds
            .contains(&"profile_write".to_string()));
        assert!(plan
            .task_context
            .approval_kinds
            .contains(&"hidden_workspace".to_string()));
        assert!(plan
            .task_context
            .approval_kinds
            .contains(&"host_visible_ui".to_string()));
        assert!(plan.host_viewer_ready);
        assert!(plan.viewer_available);
        assert!(plan.viewer_unavailable_reason.is_none());

        let run_step = task_step(&plan, "run_browser_session_after_save")
            .expect("browser plan should include an approved run step");
        assert!(run_step
            .depends_on
            .contains(&"save_browser_profile_after_review".to_string()));
        assert!(run_step
            .required_input
            .iter()
            .any(|input| input.contains("shopping_list")));

        let observe_step = task_step(&plan, "observe_browser_workspace")
            .expect("browser plan should include post-start observation");
        assert!(observe_step.read_only);
        assert!(observe_step
            .depends_on
            .contains(&"run_browser_session_after_save".to_string()));
        let list_apps = task_step(&plan, "list_browser_apps_after_start")
            .expect("browser plan should list browser apps after start");
        assert!(!list_apps.ready_to_call);
        assert!(list_apps
            .depends_on
            .contains(&"observe_browser_workspace".to_string()));
        let events = task_step(&plan, "read_browser_events_after_start")
            .expect("browser plan should read browser events after start");
        assert!(!events.ready_to_call);
        assert!(events.read_only);
        let screenshot = task_step(&plan, "capture_browser_window_after_start")
            .expect("browser plan should capture browser evidence after target is known");
        assert!(!screenshot.ready_to_call);
        assert!(screenshot
            .required_input
            .iter()
            .any(|input| input.contains("active_window.id") && input.contains("app_id")));
        let search_results = task_step(&plan, "extract_browser_search_results_after_start")
            .expect("shopping browser plan should extract structured results after start");
        assert_eq!(search_results.tool, "workspace_browser_search_results");
        assert!(search_results
            .depends_on
            .contains(&"snapshot_browser_page_after_start".to_string()));
        let boundary = task_step(&plan, "confirm_real_world_boundary_after_start")
            .expect("browser plan should reconfirm real-world boundary after start");
        assert!(!boundary.ready_to_call);
        assert!(boundary
            .required_input
            .iter()
            .any(|input| input.contains("target_url")));
        assert!(boundary
            .required_input
            .iter()
            .any(|input| input.contains("fulfillment")));
        let boundary_checkpoint = task_checkpoint(
            &plan,
            "confirm_real_world_boundary_after_start",
            "real_world_action",
        )
        .expect("browser boundary after start should expose real-world checkpoint");
        assert!(boundary_checkpoint.approval_required);
        assert!(!boundary_checkpoint.blocks_step);

        let viewer_step = task_step(&plan, "open_viewer_when_browser_runs")
            .expect("non-headless runnable browser plan should include optional viewer");
        assert!(viewer_step.open_world);
        assert!(viewer_step
            .depends_on
            .contains(&"run_browser_session_after_save".to_string()));

        let preview_checkpoint =
            task_checkpoint(&plan, "dry_run_save_browser_profile", "preview_surface")
                .expect("profile save dry-run should be exposed as approval surface");
        assert!(!preview_checkpoint.approval_required);
        assert!(!preview_checkpoint.blocks_step);

        let profile_write_checkpoint =
            task_checkpoint(&plan, "save_browser_profile_after_review", "profile_write")
                .expect("profile save should have an approval checkpoint");
        assert!(profile_write_checkpoint.approval_required);
        assert!(profile_write_checkpoint.blocks_step);

        let workspace_checkpoint =
            task_checkpoint(&plan, "run_browser_session_after_save", "hidden_workspace")
                .expect("browser run should have hidden-workspace approval checkpoint");
        assert!(workspace_checkpoint.approval_required);
        assert!(workspace_checkpoint.blocks_step);

        let real_world_checkpoint =
            task_checkpoint(&plan, "run_browser_session_after_save", "real_world_action")
                .expect("shopping plan should expose real-world approval checkpoint");
        assert!(real_world_checkpoint.approval_required);
        assert!(!real_world_checkpoint.blocks_step);

        let viewer_checkpoint =
            task_checkpoint(&plan, "open_viewer_when_browser_runs", "host_visible_ui")
                .expect("viewer step should have host-visible UI checkpoint");
        assert!(viewer_checkpoint.approval_required);

        let headless_plan =
            task_plan_without_saved_profiles("grocery shopping", Some(user_data_dir), true);
        assert!(!headless_plan.viewer_available);
        assert!(headless_plan
            .viewer_unavailable_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("--headless")));
        assert!(
            task_step(&headless_plan, "open_viewer_when_browser_runs").is_none(),
            "headless plans should not offer a host-visible viewer: {headless_plan:?}"
        );
    }

    #[test]
    fn task_plan_respects_explicit_viewer_opt_out() {
        let plan = task_plan_from_params(
            McpTaskPlanParams {
                intent: "app QA".to_string(),
                workspace_id: None,
                profile_id: None,
                project_path: Some(PathBuf::from("/tmp/example-project")),
                browser_path: None,
                user_data_dir: None,
                target_url: None,
                shopping_list: None,
                budget: None,
                fulfillment: None,
                substitution_policy: None,
                cart_mutation_approved: false,
                final_cart_reviewed: false,
                real_world_action_approved: false,
                open_viewer: Some(false),
            },
            false,
            Vec::new(),
            vec![DEFAULT_WORKSPACE_ID.to_string()],
        );

        assert!(plan.viewer_available);
        assert!(
            !plan
                .steps
                .iter()
                .any(|step| step.tool == "workspace_open_viewer"),
            "open_viewer=false should suppress viewer-first plan steps: {plan:?}"
        );
    }
}
