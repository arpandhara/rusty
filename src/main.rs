mod add;
mod auth;
mod branch;
mod checkout;
mod cli;
mod clone;
mod commit;
mod fetch;
mod fsck;
mod ignore;
mod index;
mod log;
mod merge;
mod objects;
mod path_check;
mod pull;
mod push;
mod remote;
mod repository;
mod reset;
mod rm;
mod state;
mod status;
mod tree;
mod tui;

use anyhow::Result;
use clap::Parser;
use cli::{Cli, Commands};
use std::env;
use std::path::PathBuf;

fn find_repo_path() -> Result<PathBuf> {
    let mut current_dir = env::current_dir()?;

    loop {
        let repo_path = current_dir.join(".rusty");

        if repo_path.is_dir() {
            return Ok(repo_path);
        }

        if !current_dir.pop() {
            break;
        }
    }

    anyhow::bail!("Not a rusty repository");
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let cmd = match cli.command {
        Some(cmd) => cmd,
        None => Commands::Tui,
    };

    match cmd {
        Commands::Tui => {
            tui::run_tui()?;
        }

        Commands::Init => {
            repository::init()?;
        }

        Commands::HashObject { file } => {
            let repo_path = find_repo_path()?;

            let hash = objects::hash_objects(&PathBuf::from(file), &repo_path)?;

            println!("Hash = {}", hash);
        }

        Commands::Add { file } => {
            let repo_path = find_repo_path()?;

            add::add_file(&PathBuf::from(file), &repo_path)?;
        }

        Commands::Status => {
            let repo_path = find_repo_path()?;

            status::check_status(&repo_path)?;
        }

        Commands::WriteTree => {
            let repo_path = find_repo_path()?;

            let hash = tree::write_tree(&repo_path)?;

            println!("Root tree: {}", hash);
        }

        Commands::Commit { message } => {
            let repo_path = find_repo_path()?;

            let hash = commit::create_commit(&repo_path, message)?;

            println!("Created commit: {}", hash);
        }

        Commands::Log => {
            let repo_path = find_repo_path()?;

            log::show_log(&repo_path)?;
        }

        Commands::Push => {
            let repo_path = find_repo_path()?;

            push::push(&repo_path)?;
        }

        Commands::Fetch => {
            let repo_path = find_repo_path()?;

            fetch::fetch(&repo_path)?;
        }

        Commands::Clone { url, directory } => {
            clone::clone(&url, directory)?;
        }

        Commands::Login { server } => {
            auth::login(server.as_deref())?;
        }

        Commands::Logout => {
            auth::logout()?;
        }

        Commands::Whoami => match auth::load_credentials()? {
            Some(creds) => {
                println!("Logged in as: {}", creds.email);

                if let Some(username) = creds.username {
                    println!("Username:     {}", username);
                }

                println!("Server:       {}", creds.server);
            }

            None => {
                println!("Not logged in. Run `rusty login` to authenticate.");
            }
        },

        Commands::Remote { command } => {
            let repo_path = find_repo_path()?;

            match command {
                cli::RemoteCommands::Add { name, url } => {
                    remote::add_remote(&repo_path, &name, &url)?;
                }
            }
        }
        Commands::Checkout { branch } => {
            let repo_path = find_repo_path()?;

            checkout::checkout(&repo_path, &branch)?;
        }
        Commands::Rm { cached, path } => {
            let repo_path = find_repo_path()?;

            if cached {
                rm::rm_cached(&path, &repo_path)?;
            } else {
                anyhow::bail!("only 'rm --cached' is currently supported");
            }
        }
        Commands::Merge { branch, abort } => {
            let repo_path = find_repo_path()?;
            if abort {
                merge::merge_abort(&repo_path)?;
            } else if let Some(branch_name) = branch {
                merge::merge(&repo_path, &branch_name)?;
            } else {
                anyhow::bail!("Usage: rusty merge <branch> | rusty merge --abort");
            }
        }
        Commands::Branch { name, remote, all } => {
            let repo_path = find_repo_path()?;

            if remote && all {
                anyhow::bail!("Use either --remote or --all, not both");
            }

            if name.is_some() && (remote || all) {
                anyhow::bail!("Branch creation cannot be combined with --remote or --all");
            }

            match name {
                Some(name) => {
                    branch::create_branch(&repo_path, &name)?;
                }

                None => {
                    branch::list_branches(&repo_path, remote, all)?;
                }
            }
        }
        Commands::Pull => {
            let repo_path = find_repo_path()?;

            pull::pull(&repo_path)?;
        }
        Commands::Fsck {
            dangling,
            unreachable,
            connectivity_only,
            strict,
            lost_found,
            json,
        } => {
            let repo_path = find_repo_path()?;
            let options = fsck::FsckOptions {
                dangling,
                unreachable,
                connectivity_only,
                strict,
                lost_found,
                json,
            };
            let report = fsck::run_fsck(&repo_path, &options)?;
            fsck::print_fsck_report(&report, &options);
            if !report.is_clean {
                std::process::exit(1);
            }
        }
        Commands::Reset {
            target,
            move_ref,
            index,
            worktree,
            soft,
            mixed,
            hard,
            dry_run,
            json,
            yes,
            paths,
        } => {
            let alias_count = (soft as u8) + (mixed as u8) + (hard as u8);
            let explicit_count = (move_ref as u8) + (index as u8) + (worktree as u8);

            if alias_count > 1 {
                anyhow::bail!("Cannot combine multiple compatibility aliases (--soft, --mixed, --hard)");
            }
            if alias_count > 0 && explicit_count > 0 {
                anyhow::bail!(
                    "Cannot combine compatibility aliases (--soft, --mixed, --hard) with explicit flags (--move-ref, --index, --worktree)"
                );
            }

            let (final_move_ref, final_index, final_worktree) = if soft {
                (true, false, false)
            } else if mixed {
                (true, true, false)
            } else if hard {
                (true, true, true)
            } else {
                (move_ref, index, worktree)
            };

            let repo = state::Repository::discover()?;
            let request = reset::ResetRequest {
                target,
                destinations: reset::ResetDestinations {
                    move_ref: final_move_ref,
                    index: final_index,
                    worktree: final_worktree,
                },
                paths,
                dry_run,
                json,
                yes,
            };

            reset::run_reset(&repo, &request)?;
        }
    }

    Ok(())
}
