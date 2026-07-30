use super::*;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum IntegrationPhase {
    #[default]
    Pending,
    Integrating,
    Resolving,
    MergedLocal,
    Pushed,
}

impl IntegrationPhase {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Integrating => "integrating",
            Self::Resolving => "resolving",
            Self::MergedLocal => "merged-locally",
            Self::Pushed => "pushed",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct IntegrationEvidence {
    pub(crate) phase: IntegrationPhase,
    pub(crate) bookmark: Option<String>,
    pub(crate) merge_change_id: Option<String>,
    pub(crate) git_commit: Option<String>,
    pub(crate) pushed: bool,
    /// The jj operation the merge ran as, so it can be undone exactly.
    pub(crate) operation_id: Option<String>,
    /// Whether the merge change is still an ancestor of trunk. None = not checked
    /// yet. `Some(false)` means the row SAYS merged but the work is no longer in
    /// main — history was rewritten under it, which used to be invisible.
    pub(crate) in_trunk: Option<bool>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum DeliveryStatus {
    #[default]
    NotRequested,
    Pending,
    Deployed,
    Verified,
    Blocked,
    Failed,
}

impl DeliveryStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NotRequested => "not-requested",
            Self::Pending => "pending",
            Self::Deployed => "deployed",
            Self::Verified => "verified",
            Self::Blocked => "blocked",
            Self::Failed => "failed",
        }
    }

    pub(crate) fn parse(value: &str) -> Self {
        match value {
            "pending" | "running" | "pushed" => Self::Pending,
            "deployed" => Self::Deployed,
            "verified" => Self::Verified,
            "blocked" => Self::Blocked,
            "failed" => Self::Failed,
            _ => Self::NotRequested,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct DeliveryEvidence {
    pub(crate) required: bool,
    pub(crate) kind: Option<String>,
    pub(crate) status: DeliveryStatus,
    pub(crate) target: Option<String>,
    pub(crate) url: Option<String>,
    pub(crate) app_path: Option<String>,
    pub(crate) provider: Option<String>,
    pub(crate) revision: Option<String>,
    pub(crate) deployment_id: Option<String>,
    pub(crate) version: Option<String>,
    pub(crate) verified_at: Option<String>,
    pub(crate) checks: Vec<String>,
}

impl DeliveryEvidence {
    pub(crate) fn for_task(task: &str) -> Self {
        if task_requests_delivery(task) {
            Self {
                required: true,
                status: DeliveryStatus::Pending,
                ..Self::default()
            }
        } else {
            Self::default()
        }
    }

    pub(crate) fn is_verified(&self) -> bool {
        self.required
            && matches!(
                self.status,
                DeliveryStatus::Deployed | DeliveryStatus::Verified
            )
            && self
                .kind
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            && self
                .target
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            && self
                .verified_at
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            && !self.checks.is_empty()
    }

    /// Parse the structured `delivery` object written by `rudder done` or stored
    /// in run.json. A URL/app path also serves as the human-visible target so an
    /// agent cannot accidentally provide evidence that the dashboard then hides.
    pub(crate) fn from_json(value: &serde_json::Value, default_required: bool) -> Option<Self> {
        let object = value.as_object()?;
        let text = |key: &str| {
            object
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        };
        let url = text("url");
        let app_path = text("appPath").or_else(|| text("app_path"));
        let target = text("target")
            .or_else(|| url.clone())
            .or_else(|| app_path.clone());
        let checks = object
            .get("checks")
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let required = object
            .get("required")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(default_required);
        let status = object
            .get("status")
            .and_then(serde_json::Value::as_str)
            .map(DeliveryStatus::parse)
            .unwrap_or(if required {
                DeliveryStatus::Pending
            } else {
                DeliveryStatus::NotRequested
            });
        Some(Self {
            required,
            kind: text("kind"),
            status,
            target,
            url,
            app_path,
            provider: text("provider"),
            revision: text("revision"),
            deployment_id: text("deploymentId").or_else(|| text("deployment_id")),
            version: text("version"),
            verified_at: text("verifiedAt").or_else(|| text("verified_at")),
            checks,
        })
    }

    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "required": self.required,
            "kind": self.kind,
            "status": self.status.as_str(),
            "target": self.target,
            "url": self.url,
            "appPath": self.app_path,
            "provider": self.provider,
            "revision": self.revision,
            "deploymentId": self.deployment_id,
            "version": self.version,
            "verifiedAt": self.verified_at,
            "checks": self.checks,
        })
    }
}

/// Conservative explicit-intent detector. Delivery is never inferred from an
/// ordinary implementation request, but common direct requests such as “deploy
/// it”, “ship a release”, and “install/launch the app” become proof-gated.
pub(crate) fn task_requests_delivery(task: &str) -> bool {
    let text = task.to_ascii_lowercase();
    [
        "deploy",
        "make it live",
        "ship it",
        "ship this",
        "publish",
        "release",
        "install the app",
        "install it",
        "launch the app",
        "push and deploy",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum FinalGateStatus {
    #[default]
    Idle,
    Running,
    Passed,
    Failed,
}

#[derive(Debug)]
pub(crate) struct FinalGateResult {
    pub(crate) passed: bool,
    pub(crate) summary: String,
}

#[derive(Clone, Debug)]
pub(crate) struct VerificationCommand {
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    pub(crate) label: String,
    pub(crate) require_empty_stdout: bool,
}

/// Whether a verification command counts as passing. Commands whose real signal
/// is "empty stdout == clean" (notably `jj resolve --list`) exit NON-ZERO in the
/// clean case — jj prints "No conflicts found at this revision" to stderr and
/// returns non-zero — so they MUST be judged on stdout, not the exit code.
/// Judging that command on its exit status made the final gate report
/// "jj conflicts failed: No conflicts found at this revision" exactly when the
/// tree was conflict-free, so the gate could never go green.
pub(crate) fn verification_passed(
    require_empty_stdout: bool,
    exit_success: bool,
    stdout: &str,
) -> bool {
    if require_empty_stdout {
        stdout.trim().is_empty()
    } else {
        exit_success
    }
}

pub(crate) fn verification_commands(cwd: &Path) -> Vec<VerificationCommand> {
    let mut commands = vec![
        VerificationCommand {
            program: "jj".to_string(),
            args: vec!["resolve".to_string(), "--list".to_string()],
            label: "jj conflicts".to_string(),
            require_empty_stdout: true,
        },
        VerificationCommand {
            program: "git".to_string(),
            args: vec!["diff".to_string(), "--check".to_string()],
            label: "git diff --check".to_string(),
            require_empty_stdout: false,
        },
    ];
    if let Ok(raw) = fs::read_to_string(cwd.join("package.json")) {
        if let Ok(package) = serde_json::from_str::<serde_json::Value>(&raw) {
            let scripts = package.get("scripts");
            if scripts.and_then(|value| value.get("check")).is_some() {
                commands.push(VerificationCommand {
                    program: "npm".to_string(),
                    args: vec!["run".to_string(), "check".to_string()],
                    label: "npm run check".to_string(),
                    require_empty_stdout: false,
                });
            }
            if scripts.and_then(|value| value.get("test")).is_some() {
                commands.push(VerificationCommand {
                    program: "npm".to_string(),
                    args: vec!["test".to_string()],
                    label: "npm test".to_string(),
                    require_empty_stdout: false,
                });
            }
        }
    }
    if cwd.join("Cargo.toml").is_file() {
        commands.push(VerificationCommand {
            program: "cargo".to_string(),
            args: vec!["test".to_string(), "--workspace".to_string()],
            label: "cargo test --workspace".to_string(),
            require_empty_stdout: false,
        });
    }
    commands
}

impl AgentRun {
    pub(crate) fn lifecycle_label(&self) -> &'static str {
        if self.status == AgentStatus::Done && self.delivery.is_verified() {
            "deployed"
        } else if self.status == AgentStatus::Done
            && self.delivery.required
            && matches!(
                self.delivery.status,
                DeliveryStatus::Blocked | DeliveryStatus::Failed
            )
        {
            "delivery blocked"
        } else if self.delivery.required && self.status == AgentStatus::Done {
            "delivery proof needed"
        } else if self.merge_resolver && self.status == AgentStatus::Running {
            "resolving"
        } else if self.merge_conflict {
            "integration-blocked"
        } else if self.integration.phase == IntegrationPhase::Integrating {
            "integrating"
        } else if self.status == AgentStatus::Merged && self.integration.pushed {
            "pushed"
        } else if self.status == AgentStatus::Merged {
            "merged-locally"
        } else {
            match self.status {
                AgentStatus::Running => "working",
                // Done == the review bucket (the board calls it "review", the
                // completion path calls it "a task entering review"). The old
                // "verifying" label read as active work when the agent had in
                // fact finished and was awaiting review/merge.
                AgentStatus::Done if self.is_main() || self.is_oneoff() => "completed",
                AgentStatus::Done => "review",
                AgentStatus::Failed => "failed",
                AgentStatus::Stopped => "cancelled",
                AgentStatus::Paused => "paused",
                AgentStatus::Orphaned => "orphaned",
                AgentStatus::Migrated => "cloud-owned",
                AgentStatus::Merged => unreachable!(),
            }
        }
    }
}

impl App {
    pub(crate) fn refresh_remote_integration_state(&mut self) {
        let mut changed = false;
        let mut watched = 0usize;
        for run in self
            .agents
            .iter_mut()
            .filter(|run| run.status == AgentStatus::Merged && !run.integration.pushed)
        {
            watched += 1;
            let (Some(bookmark), Some(commit)) = (
                run.integration.bookmark.as_deref(),
                run.integration.git_commit.as_deref(),
            ) else {
                continue;
            };
            let remote = format!("origin/{bookmark}");
            let pushed = Command::new("git")
                .args(["merge-base", "--is-ancestor", commit, &remote])
                .current_dir(&self.cwd)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if run.integration.pushed != pushed {
                run.integration.pushed = pushed;
                run.integration.phase = if pushed {
                    IntegrationPhase::Pushed
                } else {
                    IntegrationPhase::MergedLocal
                };
                let _ = save_native_run_record(&self.cwd, run);
                changed = true;
            }
        }
        // Re-arm the cadence. A flipped flag or a newly merged agent means the
        // answer is moving, so go back to checking often; otherwise widen the gap
        // so an idle dashboard is not spawning `git merge-base` around the clock.
        self.remote_state_interval = if changed || watched > self.remote_state_watched {
            REMOTE_STATE_BASE_INTERVAL
        } else {
            backed_off_interval(self.remote_state_interval, REMOTE_STATE_MAX_INTERVAL)
        };
        self.remote_state_watched = watched;

        if changed {
            self.dirty = true;
            let _ = self.write_rudder_context_timed(None);
            self.mirror_graph();
        }
    }

    pub(crate) fn maybe_start_final_gate(&mut self) {
        if self.final_gate_status != FinalGateStatus::Idle
            || self.awaiting_approval
            || !self.planned_nodes.is_empty()
            || self.plan_launched_node_ids.is_empty()
            || !self
                .plan_launched_node_ids
                .is_subset(&self.plan_merged_node_ids)
        {
            return;
        }
        let commands = verification_commands(&self.cwd);
        let cwd = self.cwd.clone();
        let tx = self.final_gate_tx.clone();
        self.final_gate_status = FinalGateStatus::Running;
        self.final_gate_summary = Some(format!("running {} repository checks", commands.len()));
        self.persist_plan_queue();
        self.notice = self.final_gate_summary.clone();
        self.dirty = true;
        thread::spawn(move || {
            let mut passed = Vec::new();
            for command in commands {
                let output = Command::new(&command.program)
                    .args(&command.args)
                    .current_dir(&cwd)
                    .output();
                match output {
                    Ok(output) => {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        if verification_passed(
                            command.require_empty_stdout,
                            output.status.success(),
                            &stdout,
                        ) {
                            passed.push(command.label);
                        } else {
                            // For an empty-stdout check the meaningful detail is the
                            // stdout (the conflict list); otherwise prefer stderr.
                            let detail_owned = if command.require_empty_stdout {
                                stdout.trim().to_string()
                            } else {
                                let stderr = String::from_utf8_lossy(&output.stderr);
                                if stderr.trim().is_empty() {
                                    stdout.trim().to_string()
                                } else {
                                    stderr.trim().to_string()
                                }
                            };
                            let detail = truncate_chars(&detail_owned, 1200);
                            let _ = tx.send(FinalGateResult {
                                passed: false,
                                summary: format!("{} failed: {detail}", command.label),
                            });
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = tx.send(FinalGateResult {
                            passed: false,
                            summary: format!("{} could not start: {error}", command.label),
                        });
                        return;
                    }
                }
            }
            let _ = tx.send(FinalGateResult {
                passed: true,
                // Be explicit about scope: these are LOCAL checks only. "passed"
                // must not read as "shipped" — the plan can be fully merged and
                // green here while still unpushed and undeployed (see Ship status).
                summary: format!(
                    "LOCAL checks passed ({}) — merged locally only, NOT pushed or deployed; see Ship status",
                    passed.join(", ")
                ),
            });
        });
    }

    pub(crate) fn poll_final_gate(&mut self) {
        let Ok(result) = self.final_gate_rx.try_recv() else {
            return;
        };
        if !self.planned_nodes.is_empty()
            || !self
                .plan_launched_node_ids
                .is_subset(&self.plan_merged_node_ids)
        {
            self.final_gate_status = FinalGateStatus::Idle;
            self.final_gate_summary = None;
            self.persist_plan_queue();
            return;
        }
        self.final_gate_status = if result.passed {
            FinalGateStatus::Passed
        } else {
            FinalGateStatus::Failed
        };
        self.final_gate_summary = Some(result.summary.clone());
        self.notice = Some(result.summary);
        self.persist_plan_queue();
        let _ = self.write_rudder_context_timed(None);
        self.dirty = true;
    }
}
