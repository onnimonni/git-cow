use clap::{Args, Parser, Subcommand};
use git_cow::{join_paths, PopulateOptions, Report};
use std::path::PathBuf;
use std::process::ExitCode;

/// Copy-on-write git worktrees (APFS, btrfs, XFS, ZFS, ...)
#[derive(Parser)]
#[command(name = "git-cow", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fill a fresh `git worktree add --no-checkout` worktree by cloning an existing
    /// worktree; only files differing from its HEAD are written. The `git` wrapper
    /// runs this for every `git worktree add`.
    Populate(PopulateArgs),
}

#[derive(Args)]
struct PopulateArgs {
    /// Worktree to clone files from [default: main worktree]
    #[arg(long, value_name = "dir")]
    from: Option<PathBuf>,
    /// Don't carry ignored files (node_modules, _build, target, ...) over from the source.
    /// Virtualenvs, devenv state, tmp/ and log/ are never carried; add more with
    /// `git config --add cow.exclude <name|path|*.ext>`
    #[arg(long)]
    no_ignored: bool,
    /// Only print warnings
    #[arg(short, long)]
    quiet: bool,
    /// The fresh worktree
    worktree: PathBuf,
}

fn main() -> ExitCode {
    let Command::Populate(args) = Cli::parse().command;
    let opts = PopulateOptions {
        from: args.from,
        include_ignored: !args.no_ignored,
    };
    match git_cow::populate(&args.worktree, &opts) {
        Ok(report) => {
            print_report(&report, args.quiet);
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("git-cow: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn print_report(report: &Report, quiet: bool) {
    for warning in &report.warnings {
        eprintln!("git-cow: {warning}");
    }
    let lfs_problems = [
        (
            &report.lfs_missing,
            "objects not downloaded, run `git lfs pull`",
        ),
        (
            &report.lfs_corrupt,
            "local objects damaged, run `git lfs fetch` and `git lfs checkout`",
        ),
        (
            &report.lfs_unsupported,
            "files use ext-* extensions, run `git lfs checkout`",
        ),
    ];
    for (paths, problem) in lfs_problems {
        if !paths.is_empty() {
            eprintln!(
                "git-cow: {} git-lfs {problem} in {} ({})",
                paths.len(),
                report.path.display(),
                join_paths(paths)
            );
        }
    }
    if quiet {
        return;
    }
    let mut summary = format!("{}: ", report.filesystem);
    if let (Some(source), 1..) = (&report.source, report.cloned) {
        summary += &format!(
            "cloned from {}, reused stat data for {} files, ",
            source.display(),
            report.stat_reused
        );
    }
    summary += &format!("wrote {} files", report.rewritten);
    if !report.carried.is_empty() {
        summary += &format!("; carried {}", join_paths(&report.carried));
    }
    if !report.excluded.is_empty() {
        summary += &format!("; excluded {}", join_paths(&report.excluded));
    }
    if report.lfs_cloned + report.lfs_smudged > 0 {
        summary += &format!(
            "; git-lfs: {} kept, {} from local store",
            report.lfs_cloned, report.lfs_smudged
        );
    }
    eprintln!("git-cow: {summary}");
}
