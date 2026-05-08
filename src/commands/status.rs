#![allow(
    clippy::print_stdout,
    reason = "CLI subcommand whose primary purpose is to print a status report to stdout"
)]

use crate::cli;
use crate::config;
use crate::service::status::{render_oneline, service_state};
use crate::state;
use crate::state::{AssetRecord, StateDb};

use super::{print_truncation_tail, LISTING_CAP};

/// Run the status command.
pub(crate) async fn run_status(
    args: cli::StatusArgs,
    globals: &config::GlobalArgs,
    toml: Option<&config::TomlConfig>,
) -> anyhow::Result<()> {
    print_service_section().await;

    let db_path = super::super::get_db_path(globals, toml)?;

    if !db_path.exists() {
        println!("No state database found at {}", db_path.display());
        println!("Run a sync first to create the database.");
        return Ok(());
    }

    let db = state::SqliteStateDb::open(&db_path).await?;
    let summary = db.get_summary().await?;

    println!("State Database: {}", db_path.display());
    println!();
    println!("Assets:");
    println!("  Total:      {}", summary.total_assets);
    println!("  Downloaded: {}", summary.downloaded);
    println!("  Pending:    {}", summary.pending);
    println!("  Failed:     {}", summary.failed);
    println!();

    if let Some(started) = &summary.last_sync_started {
        println!(
            "Last sync started:   {}",
            started.format("%Y-%m-%d %H:%M:%S UTC")
        );
    }
    if let Some(completed) = &summary.last_sync_completed {
        println!(
            "Last sync completed: {}",
            completed.format("%Y-%m-%d %H:%M:%S UTC")
        );
    }

    if args.failed && summary.failed > 0 {
        println!();
        println!("Failed assets:");
        let printed = paginate_print(&db, Section::Failed).await?;
        print_truncation_tail(summary_count_to_usize(summary.failed), printed);
    }

    if args.pending && summary.pending > 0 {
        println!();
        println!("Pending assets:");
        let printed = paginate_print(&db, Section::Pending).await?;
        print_truncation_tail(summary_count_to_usize(summary.pending), printed);
    }

    if args.downloaded && summary.downloaded > 0 {
        println!();
        println!("Downloaded assets:");
        let printed = paginate_print(&db, Section::Downloaded).await?;
        print_truncation_tail(summary_count_to_usize(summary.downloaded), printed);
    }

    Ok(())
}

/// Prints the `Service:` line at the top of `kei status`. Errors from
/// the per-platform probe (no systemd, headless macOS, locked-down SCM)
/// are absorbed so a probe failure never poisons the rest of the status
/// command -- the state DB summary is the load-bearing output.
async fn print_service_section() {
    let state = match service_state().await {
        Ok(state) => state,
        Err(e) => {
            tracing::debug!(error = %e, "service_state probe failed; rendering placeholder");
            println!("Service: status unavailable");
            println!();
            return;
        }
    };
    println!("{}", render_oneline(&state));
    println!();
}

/// Which `kei status` listing is being paginated. Picks the state-DB page
/// fetcher and the per-row print fn.
#[derive(Clone, Copy)]
enum Section {
    Failed,
    Pending,
    Downloaded,
}

/// Stream a status listing through the state-DB pagination primitive,
/// printing up to [`LISTING_CAP`] rows. Returns the number of rows printed
/// so the caller can decide whether to emit the "... and N more" tail.
///
/// The page size is smaller than `LISTING_CAP` so pagination is exercised
/// before the cap kicks in; post-cap rows are skipped via an early break,
/// not by narrowing the SQL query.
async fn paginate_print(db: &state::SqliteStateDb, section: Section) -> anyhow::Result<usize> {
    let page_size: u32 = 100;
    let mut offset: u64 = 0;
    let mut printed: usize = 0;
    'outer: loop {
        let page = match section {
            Section::Failed => db.get_failed_page(offset, page_size).await?,
            Section::Pending => db.get_pending_page(offset, page_size).await?,
            Section::Downloaded => db.get_downloaded_page(offset, page_size).await?,
        };
        if page.is_empty() {
            break;
        }
        for asset in &page {
            if printed >= LISTING_CAP {
                break 'outer;
            }
            match section {
                Section::Failed => print_failed(asset),
                Section::Pending => print_pending(asset),
                Section::Downloaded => print_downloaded(asset),
            }
            printed += 1;
        }
        offset += page.len() as u64;
    }
    Ok(printed)
}

/// Convert a `summary.{failed,pending,downloaded}` count (u64 from SQLite
/// `COUNT(*)`) to `usize` for `print_truncation_tail`. Counts are well under
/// `u32::MAX` on any supported target, so the cast is lossless.
#[allow(
    clippy::cast_possible_truncation,
    reason = "asset counts from SQLite; cap-to-usize is safe on supported targets"
)]
fn summary_count_to_usize(count: u64) -> usize {
    count as usize
}

fn print_failed(asset: &AssetRecord) {
    let last_seen = asset.last_seen_at.format("%Y-%m-%d %H:%M:%S");
    println!(
        "  {} ({}) - {} (attempts: {}, last seen: {})",
        asset.filename,
        asset.id,
        asset.last_error.as_deref().unwrap_or("unknown error"),
        asset.download_attempts,
        last_seen
    );
}

fn print_pending(asset: &AssetRecord) {
    let last_seen = asset.last_seen_at.format("%Y-%m-%d %H:%M:%S");
    println!(
        "  {} ({}) - attempts: {}, last seen: {}",
        asset.filename, asset.id, asset.download_attempts, last_seen
    );
}

fn print_downloaded(asset: &AssetRecord) {
    // status='downloaded' rows are written with local_path by mark_downloaded,
    // so a missing path here means a state-DB invariant violation (manual
    // edit, partial migration, upsert after mark_downloaded without path).
    // Surface it clearly rather than hiding it.
    let local = asset.local_path.as_ref().map_or_else(
        || "<MISSING local_path>".to_string(),
        |p| p.display().to_string(),
    );
    println!("  {} ({}) -> {}", asset.filename, asset.id, local);
}
