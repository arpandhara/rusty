pub mod apply;
pub mod plan;
pub mod recover;
pub mod validate;

use anyhow::{bail, Context, Result};
use std::io::{self, IsTerminal, Write};

pub use plan::{ResetDestinations, ResetRequest};
pub use validate::{plan_has_destructive_warnings, validate_plan, validate_request};

use crate::state::operation::{cleanup_orphan_operation_dirs, recover_if_needed};
use crate::state::Repository;

/// A Git-compatible reset alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetAlias {
    Soft,
    Mixed,
    Hard,
}

impl ResetAlias {
    pub fn flag(self) -> &'static str {
        match self {
            ResetAlias::Soft => "--soft",
            ResetAlias::Mixed => "--mixed",
            ResetAlias::Hard => "--hard",
        }
    }

    pub fn git_equivalent(self) -> &'static str {
        match self {
            ResetAlias::Soft => "git reset --soft",
            ResetAlias::Mixed => "git reset --mixed",
            ResetAlias::Hard => "git reset --hard",
        }
    }

    pub fn destinations(self) -> ResetDestinations {
        match self {
            ResetAlias::Soft => ResetDestinations {
                move_ref: true,
                index: false,
                worktree: false,
            },
            ResetAlias::Mixed => ResetDestinations {
                move_ref: true,
                index: true,
                worktree: false,
            },
            ResetAlias::Hard => ResetDestinations {
                move_ref: true,
                index: true,
                worktree: true,
            },
        }
    }
}

/// Translate the mutually exclusive CLI destination flags into a destination
/// set, rejecting ambiguous combinations.
pub fn resolve_destinations(
    move_ref: bool,
    index: bool,
    worktree: bool,
    soft: bool,
    mixed: bool,
    hard: bool,
) -> Result<(ResetDestinations, Option<ResetAlias>)> {
    let alias_count = (soft as u8) + (mixed as u8) + (hard as u8);
    let explicit_count = (move_ref as u8) + (index as u8) + (worktree as u8);

    if alias_count > 1 {
        bail!("Cannot combine multiple compatibility aliases (--soft, --mixed, --hard)");
    }
    if alias_count > 0 && explicit_count > 0 {
        bail!(
            "Cannot combine compatibility aliases (--soft, --mixed, --hard) with explicit flags \
             (--move-ref, --index, --worktree)"
        );
    }

    let alias = if soft {
        Some(ResetAlias::Soft)
    } else if mixed {
        Some(ResetAlias::Mixed)
    } else if hard {
        Some(ResetAlias::Hard)
    } else {
        None
    };

    let destinations = alias.map(ResetAlias::destinations).unwrap_or(ResetDestinations {
        move_ref,
        index,
        worktree,
    });

    Ok((destinations, alias))
}

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

    // A full reset aborts an in-progress merge even when the index and worktree
    // already match the target tree. Clearing the merge state is itself a
    // mutation, so it must not be skipped by the no-op check.
    let aborts_merge = plan.is_full_reset() && crate::merge::is_merge_in_progress(&repo.rusty_dir);

    if !plan.has_mutations() && !aborts_merge {
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
    let mut warnings = Vec::new();

    if plan_has_destructive_warnings(plan) {
        warnings.push("locally modified tracked content will be overwritten:".to_string());
        for conflict in plan.conflicts.iter().filter(|conflict| !conflict.fatal) {
            if let Some(path) = &conflict.path {
                warnings.push(format!("  - {}: {}", path, conflict.message));
            } else {
                warnings.push(format!("  - {}", conflict.message));
            }
        }
    }

    confirm_action(yes, "Proceed with this reset?", &warnings)
}

/// Shared confirmation helper for reset and recovery commands.
pub(crate) fn confirm_action(yes: bool, prompt: &str, warnings: &[String]) -> Result<()> {
    if yes {
        return Ok(());
    }

    for warning in warnings {
        eprintln!("Warning: {}", warning);
    }

    if !io::stdin().is_terminal() {
        bail!(
            "Refusing to execute without --yes in a non-interactive environment. \
             Use '--dry-run' to preview, or pass '--yes' to confirm."
        );
    }

    print!("{} [y/N] ", prompt);
    io::stdout().flush().context("Failed to flush prompt")?;

    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("Failed to read confirmation")?;

    match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(()),
        _ => bail!("Cancelled; no files or repository state were changed."),
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

    #[test]
    fn soft_alias_maps_to_ref_only() {
        let (destinations, alias) =
            resolve_destinations(false, false, false, true, false, false).unwrap();
        assert_eq!(alias, Some(ResetAlias::Soft));
        assert!(destinations.move_ref);
        assert!(!destinations.index);
        assert!(!destinations.worktree);
        assert_eq!(alias.unwrap().git_equivalent(), "git reset --soft");
    }

    #[test]
    fn mixed_alias_maps_to_ref_and_index() {
        let (destinations, alias) =
            resolve_destinations(false, false, false, false, true, false).unwrap();
        assert_eq!(alias, Some(ResetAlias::Mixed));
        assert!(destinations.move_ref);
        assert!(destinations.index);
        assert!(!destinations.worktree);
    }

    #[test]
    fn hard_alias_maps_to_every_destination() {
        let (destinations, alias) =
            resolve_destinations(false, false, false, false, false, true).unwrap();
        assert_eq!(alias, Some(ResetAlias::Hard));
        assert!(destinations.move_ref);
        assert!(destinations.index);
        assert!(destinations.worktree);
    }

    #[test]
    fn explicit_flags_pass_through_without_alias() {
        let (destinations, alias) =
            resolve_destinations(false, true, true, false, false, false).unwrap();
        assert_eq!(alias, None);
        assert!(!destinations.move_ref);
        assert!(destinations.index);
        assert!(destinations.worktree);
    }

    #[test]
    fn multiple_aliases_are_rejected() {
        assert!(resolve_destinations(false, false, false, true, true, false).is_err());
        assert!(resolve_destinations(false, false, false, false, true, true).is_err());
    }

    #[test]
    fn alias_with_explicit_flag_is_rejected() {
        assert!(resolve_destinations(true, false, false, true, false, false).is_err());
        assert!(resolve_destinations(false, true, false, false, false, true).is_err());
    }

    #[test]
    fn non_interactive_shared_confirmation_is_refused() {
        let err = confirm_action(false, "Proceed?", &[]).unwrap_err();
        assert!(err.to_string().contains("non-interactive"));
        assert!(confirm_action(true, "Proceed?", &[]).is_ok());
    }
}
