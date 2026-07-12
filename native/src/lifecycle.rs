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
        if self.merge_resolver && self.status == AgentStatus::Running {
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
                AgentStatus::Done => "verifying",
                AgentStatus::Failed => "failed",
                AgentStatus::Stopped => "cancelled",
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
        for run in self
            .agents
            .iter_mut()
            .filter(|run| run.status == AgentStatus::Merged && !run.integration.pushed)
        {
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
                    Ok(output)
                        if output.status.success()
                            && (!command.require_empty_stdout
                                || String::from_utf8_lossy(&output.stdout).trim().is_empty()) =>
                    {
                        passed.push(command.label);
                    }
                    Ok(output) => {
                        let detail = String::from_utf8_lossy(if output.stderr.is_empty() {
                            &output.stdout
                        } else {
                            &output.stderr
                        });
                        let detail = truncate_chars(detail.trim(), 1200);
                        let _ = tx.send(FinalGateResult {
                            passed: false,
                            summary: format!("{} failed: {detail}", command.label),
                        });
                        return;
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
                summary: format!("all integrated · checks passed: {}", passed.join(", ")),
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
