mod bundle;
mod command;
mod lore;
mod p4;

use anyhow::{Context, Result};
use bundle::{Bundle, History};
use clap::{Args, Parser, Subcommand};
use lore::{Access, Lore};
use p4::P4;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    version,
    about = "Migrate versioned history between Epic Games Lore and Perforce Helix Core"
)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Args)]
struct SourceLore {
    /// Local Lore repository instance.
    #[arg(long)]
    repo: PathBuf,
    /// Read remote Lore history through the authenticated instance.
    #[arg(long)]
    remote: bool,
}
impl SourceLore {
    fn open(self) -> Result<Lore> {
        Ok(Lore {
            repo: std::path::absolute(self.repo)?,
            access: if self.remote {
                Access::Remote
            } else {
                Access::Offline
            },
        })
    }
}

#[derive(Subcommand)]
enum Action {
    /// Read submitted P4 changes, then replay them into a new offline Lore repository.
    P4ToLore {
        /// Create a new remote Lore repository, then push and verify its tip.
        #[arg(long)]
        lore_url: Option<String>,
        #[command(flatten)]
        p4: P4,
        #[arg(long)]
        repo: PathBuf,
        /// Retain a verified portable bundle at this new directory.
        #[arg(long)]
        bundle: Option<PathBuf>,
    },
    /// Read Lore branch history, then submit it into an empty P4 subtree.
    LoreToP4 {
        /// Restore original changelist owner and timestamp (requires P4 admin).
        #[arg(long)]
        restore_credit: bool,
        #[command(flatten)]
        lore: SourceLore,
        #[command(flatten)]
        p4: P4,
        /// Retain a verified portable bundle at this new directory.
        #[arg(long)]
        bundle: Option<PathBuf>,
    },
    /// Export only. Writes a new directory containing history.json and deduplicated blobs.
    ExportP4 {
        #[command(flatten)]
        p4: P4,
        #[arg(long)]
        bundle: PathBuf,
    },
    /// Export all visible Lore branches and both parents of each merge.
    ExportLore {
        #[command(flatten)]
        lore: SourceLore,
        #[arg(long)]
        bundle: PathBuf,
    },
    /// Verify the bundle and replay it into a new offline Lore repository.
    ImportLore {
        /// Create a new remote Lore repository, then push and verify its tip.
        #[arg(long)]
        lore_url: Option<String>,
        #[arg(long)]
        bundle: PathBuf,
        #[arg(long)]
        repo: PathBuf,
    },
    /// Verify the bundle and submit it into a P4 subtree using a dedicated temporary client.
    ImportP4 {
        /// Restore original changelist owner and timestamp (requires P4 admin).
        #[arg(long)]
        restore_credit: bool,
        #[command(flatten)]
        p4: P4,
        #[arg(long)]
        bundle: PathBuf,
        /// Verify the entire existing submitted prefix and continue from the next revision.
        #[arg(long)]
        resume: bool,
    },
    /// Check history ordering, path safety and every referenced file payload.
    Verify {
        #[arg(long)]
        bundle: PathBuf,
    },
}
fn export(path: &Path, f: impl FnOnce(&Bundle) -> Result<History>) -> Result<(Bundle, History)> {
    let b = Bundle::create(&std::path::absolute(path)?)?;
    let h = f(&b)?;
    b.finish(&h)?;
    eprintln!("Verified {} {} revisions", h.revisions.len(), h.system);
    Ok((b, h))
}
fn dest(path: PathBuf, connected: bool) -> Result<Lore> {
    Ok(Lore {
        repo: std::path::absolute(path)?,
        access: if connected {
            Access::Connected
        } else {
            Access::Offline
        },
    })
}
fn main() -> Result<()> {
    match Cli::parse().command {
        Action::P4ToLore {
            p4,
            repo,
            bundle,
            lore_url,
        } => {
            let tmp = tempfile::tempdir()?;
            let (b, h) = export(&bundle.unwrap_or_else(|| tmp.path().join("bundle")), |b| {
                p4.export(b)
            })?;
            dest(repo, lore_url.is_some())?.import(&b, &h, lore_url.as_deref())
        }
        Action::LoreToP4 {
            lore,
            p4,
            bundle,
            restore_credit,
        } => {
            let lore = lore.open()?;
            let tmp = tempfile::tempdir()?;
            let (b, h) = export(&bundle.unwrap_or_else(|| tmp.path().join("bundle")), |b| {
                lore.export(b)
            })?;
            p4.import(&b, &h, false, restore_credit)
        }
        Action::ExportP4 { p4, bundle } => {
            export(&bundle, |b| p4.export(b))?;
            Ok(())
        }
        Action::ExportLore { lore, bundle } => {
            let lore = lore.open()?;
            export(&bundle, |b| lore.export(b))?;
            Ok(())
        }
        Action::ImportLore {
            bundle,
            repo,
            lore_url,
        } => {
            let b = Bundle(bundle);
            let h = b.read()?;
            dest(repo, lore_url.is_some())?.import(&b, &h, lore_url.as_deref())
        }
        Action::ImportP4 {
            bundle,
            p4,
            resume,
            restore_credit,
        } => {
            let b = Bundle(bundle);
            let h = b.read()?;
            p4.import(&b, &h, resume, restore_credit)
        }
        Action::Verify { bundle } => {
            let h = Bundle(bundle).read().context("verifying bundle")?;
            println!("Verified {} {} revisions", h.revisions.len(), h.system);
            Ok(())
        }
    }
}
