//! `rusty recover reset`: inspect or undo the ref movement of the most recent
//! completed reset.
//!
//! Recovery intentionally restores only the ref. The index and worktree
//! backups belong to a finished operation and have already been removed, so
//! pretending to restore them would be unsafe. The command refuses to run if
//! the ref has moved since the reset, because that would discard newer history.

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::reset::confirm_action;
use crate::reset::plan::ResetDestinations;
use crate::state::operation::{
    append_ledger, cleanup_orphan_operation_dirs, now_unix_ms, recover_if_needed, LedgerRecord,
    LEDGER_SCHEMA_VERSION,
};
use crate::state::refs::{
    current_ref_value, read_recovery_record, restore_recovery_record, ResetRecoveryRecord,
};
use crate::state::repository::Repository;

#[derive(Debug, Clone, Copy)]
pub struct RecoverOptions {
    pub dry_run: bool,
    pub json: bool,
    pub yes: bool,
}

#[derive(Debug, Serialize)]
struct RecoverPreview {
    operation_id: String,
    timestamp_unix_ms: u64,
    ref_name: Option<String>,
    ref_label: String,
    from_commit: Option<String>,
    to_commit: String,
    current_commit: Option<String>,
    already_restored: bool,
    action: String,
}

impl RecoverPreview {
    fn render_text(&self) -> String {
        let mut out = String::new();
        out.push_str("============ Rusty Reset Recovery ============\n");
        out.push_str(&format!("Operation ID : {}\n", self.operation_id));
        out.push_str(&format!("Ref          : {}\n", self.ref_label));
        out.push_str(&format!(
            "Reset From   : {}\n",
            self.from_commit.as_deref().unwrap_or("<unborn>")
        ));
        out.push_str(&format!("Reset To     : {}\n", self.to_commit));
        out.push_str(&format!(
            "Current      : {}\n",
            self.current_commit.as_deref().unwrap_or("<unborn>")
        ));
        out.push_str(&format!("Action       : {}\n", self.action));
        out.push_str("==============================================\n");
        out
    }
}

pub fn run_recover_reset(repo: &Repository, options: &RecoverOptions) -> Result<()> {
    if let Some(outcome) = recover_if_needed(repo)? {
        eprintln!("Recovery: {}.", outcome);
    }

    let _lock = repo.lock()?;
    cleanup_orphan_operation_dirs(repo, None)?;

    let record = read_recovery_record(repo)?.context(
        "No completed ref-moving reset was found. \
         Only resets that moved a branch ref or detached HEAD can be recovered.",
    )?;

    // The ref must still have the shape it had when the reset completed.
    let current_symbolic = repo.current_symbolic_ref()?;
    if current_symbolic != record.ref_name {
        bail!(
            "Ref layout changed since the last reset: expected {}, found {}. \
             Refusing to restore a ref that is no longer the one the reset moved.",
            describe_ref(record.ref_name.as_deref()),
            describe_ref(current_symbolic.as_deref())
        );
    }

    if record.ref_name.is_none() && record.from_commit.is_none() {
        bail!(
            "Cannot recover this reset: it started from an unborn detached HEAD, \
             which has no ref value to restore."
        );
    }

    let current_value = current_ref_value(repo, record.ref_name.as_deref())?;
    let already_restored = current_value == record.from_commit;

    if !already_restored && current_value.as_deref() != Some(record.to_commit.as_str()) {
        bail!(
            "Ref '{}' has moved since the last reset (currently {}, expected {}). \
             Refusing to overwrite newer history.",
            describe_ref(record.ref_name.as_deref()),
            current_value.as_deref().unwrap_or("<unborn>"),
            record.to_commit
        );
    }

    let preview = RecoverPreview {
        operation_id: record.operation_id.clone(),
        timestamp_unix_ms: record.timestamp_unix_ms,
        ref_name: record.ref_name.clone(),
        ref_label: describe_ref(record.ref_name.as_deref()).to_string(),
        from_commit: record.from_commit.clone(),
        to_commit: record.to_commit.clone(),
        current_commit: current_value,
        already_restored,
        action: if already_restored {
            "nothing to do (ref already at its pre-reset value)".to_string()
        } else {
            "restore ref to its pre-reset value (index and worktree are not changed)".to_string()
        },
    };

    if options.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&preview)
                .context("Failed to serialize recovery preview to JSON")?
        );
        return Ok(());
    }

    print!("{}", preview.render_text());

    if options.dry_run {
        return Ok(());
    }

    if already_restored {
        println!("Nothing to do: the ref already points at its pre-reset value.");
        return Ok(());
    }

    let warnings = vec![
        "recovery restores only the ref; the index and working tree keep their current contents."
            .to_string(),
    ];
    confirm_action(options.yes, "Restore the ref to its pre-reset value?", &warnings)?;

    restore_recovery_record(repo, &record).context("Failed to restore ref")?;
    append_recovery_ledger(repo, &record)?;

    println!(
        "Restored {} to {} (was {}).",
        describe_ref(record.ref_name.as_deref()),
        record.from_commit.as_deref().unwrap_or("<unborn>"),
        record.to_commit
    );
    println!("The index and working tree were not changed.");
    Ok(())
}

fn append_recovery_ledger(repo: &Repository, record: &ResetRecoveryRecord) -> Result<()> {
    let ledger = LedgerRecord {
        schema_version: LEDGER_SCHEMA_VERSION,
        operation_id: record.operation_id.clone(),
        timestamp_unix_ms: now_unix_ms(),
        command_args: std::env::args().skip(1).collect(),
        from_commit: Some(record.to_commit.clone()),
        to_commit: record
            .from_commit
            .clone()
            .unwrap_or_else(|| "<unborn>".to_string()),
        branch_ref: record.ref_name.clone(),
        destinations: ResetDestinations {
            move_ref: true,
            index: false,
            worktree: false,
        },
        changed_paths: Vec::new(),
        status: "recovery_restored".to_string(),
        recovered: true,
    };

    append_ledger(repo, &ledger)
}

fn describe_ref(ref_name: Option<&str>) -> &str {
    ref_name.unwrap_or("HEAD (detached)")
}
