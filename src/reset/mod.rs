pub mod apply;
pub mod plan;
pub mod validate;

use anyhow::{bail, Context, Result};
use std::io::{self, IsTerminal, Write};

pub use plan::{ResetDestinations, ResetRequest};
pub use validate::{plan_has_destructive_warnings, validate_plan, validate_request};

use crate::state::operation::{cleanup_orphan_operation_dirs, recover_if_needed};
use crate::state::Repository;

/// Execute the `rusty reset` command according to the request.
pub fn run_reset(repo: &Repository, request: &ResetRequest) -> Result<()> {
    // 1. Validate the argument combination before touching repository state.
    validate_request(request)?;

    // 2. Complete or undo any interrupted operation from a previous process.
    //    This happens even for `--dry-run`: planning against a half-applied
    //    index/worktree would be misleading.
    if let Some(outcome) = recover_if_needed(repo)? {
        eprintln!("Recovery: {}.", outcome);
    }

    // 3. Lock first, then plan, so the preview and the confirmed plan describe
    //    exactly the state the applier will see. For preview-only modes the
    //    lock is released when this function returns.
    let _lock = repo.lock()?;
    cleanup_orphan_operation_dirs(repo, None)?;

    // 4. Preview-only modes never mutate state.
    if request.dry_run || request.json {
        let plan = plan::create_plan(repo, request)?;
        if request.json {
            println!("{}", plan.render_json()?);
        } else {
            print!("{}", plan.render_text_preview());
        }
        return Ok(());
    }

    // 5. Execution: the confirmed plan is never recomputed by the applier.
    let plan = plan::create_plan(repo, request)?;
    print!("{}", plan.render_text_preview());

    validate_plan(&plan)?;

    if plan.destinations.move_ref {
        bail!(
            "Ref movement is not enabled yet (Phase 4). \
             Use '--dry-run' to inspect the planned ref update, or omit '--move-ref' \
             and reset only the index and/or worktree."
        );
    }

    if !plan.has_mutations() {
        println!("Nothing to do: the selected destinations already match the target.");
        return Ok(());
    }

    confirm_execution(&plan, request.yes)?;

    let command_args: Vec<String> = std::env::args().skip(1).collect();
    apply::apply_plan(repo, &plan, command_args)?;

    Ok(())
}

/// Require explicit consent for any mutating reset.
///
/// Policy:
/// - `--yes` bypasses the prompt.
/// - In a terminal, show warnings and ask for `y`/`yes`.
/// - In a non-interactive process (CI, pipes, tests), refuse without `--yes`
///   rather than blocking forever on stdin.
fn confirm_execution(plan: &plan::ResetPlan, yes: bool) -> Result<()> {
    if yes {
        return Ok(());
    }

    if plan_has_destructive_warnings(plan) {
        eprintln!("Warning: locally modified tracked content will be overwritten:");
        for conflict in plan.conflicts.iter().filter(|conflict| !conflict.fatal) {
            if let Some(path) = &conflict.path {
                eprintln!("  - {}: {}", path, conflict.message);
            } else {
                eprintln!("  - {}", conflict.message);
            }
        }
    }

    if !io::stdin().is_terminal() {
        bail!(
            "Refusing to execute reset without --yes in a non-interactive environment. \
             Use '--dry-run' to preview, or pass '--yes' to confirm."
        );
    }

    print!("Proceed with this reset? [y/N] ");
    io::stdout().flush().context("Failed to flush prompt")?;

    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("Failed to read confirmation")?;

    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(()),
        _ => bail!("Reset cancelled; no files or index entries were changed."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reset::plan::{ResetDestinations, SafetyConflict};
    use crate::reset::plan::IndexUpdatePlan;

    fn plan_with(conflicts: Vec<SafetyConflict>) -> plan::ResetPlan {
        plan::ResetPlan {
            operation_id: "confirm-test".to_string(),
            target_commit: "abc".to_string(),
            destinations: ResetDestinations {
                move_ref: false,
                index: true,
                worktree: false,
            },
            filtered_paths: None,
            ref_update: None,
            index_update: Some(IndexUpdatePlan {
                entries_to_add_or_update: vec![("a".to_string(), "b".to_string())],
                entries_to_remove: Vec::new(),
            }),
            worktree_changes: Vec::new(),
            conflicts,
        }
    }

    #[test]
    fn yes_bypasses_confirmation() {
        assert!(confirm_execution(&plan_with(Vec::new()), true).is_ok());
    }

    #[test]
    fn non_interactive_without_yes_is_refused() {
        // The test harness stdin is not a terminal.
        let err = confirm_execution(&plan_with(Vec::new()), false).unwrap_err();
        assert!(err.to_string().contains("non-interactive"));
    }

    #[test]
    fn destructive_warning_detection() {
        let plan = plan_with(vec![SafetyConflict {
            path: Some("a".to_string()),
            message: "local change".to_string(),
            fatal: false,
        }]);
        assert!(plan_has_destructive_warnings(&plan));
    }
}
