//! Headless acceptance: no UI backend, production paths or OS registration.
use crate::{
    db::{attachments, notes, Db},
    editor_probe::stats,
};
use anyhow::{ensure, Context, Result};
use serde_json::json;
use std::{
    fs::OpenOptions,
    io::Write,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const MARKER: &str = "sticky-headless-reliability-v1";
const CONTENT: &str = r#"{"blocks":[{"type":"text","runs":[{"t":"Durable café 😀","bold":true},{"type":"dateChip","attrs":{"chipId":"durable-chip","date":"2026-10-15","label":"Follow up"}}]}],"plain":"Durable café 😀 Follow up"}"#;

fn crash_fixture(root: &Path) -> Result<()> {
    ensure!(
        std::fs::read_to_string(root.join("headless-marker.txt"))? == MARKER,
        "Not a reliability sandbox"
    );
    let db = Db::open_in_dir(root)?;
    notes::create(&db.conn, "durable", CONTENT, "#FFF9B1")?;
    attachments::import_image(&db.conn, "durable", &root.join("small.png"))?;
    // Create the original-file-only crash state, without committed metadata.
    std::fs::write(
        root.join("attachments/11111111-1111-4111-8111-111111111111.original"),
        b"interrupted import",
    )?;
    let _tx = db.conn.unchecked_transaction()?;
    db.conn.execute(
        "UPDATE notes SET content='{\"blocks\":[],\"plain\":\"UNCOMMITTED\"}' WHERE id='durable'",
        [],
    )?;
    db.conn.execute("DELETE FROM reminders", [])?;
    db.conn.execute("DELETE FROM notes_fts", [])?;
    // No destructors, SQLite rollback/close, app Quit or save flush runs.
    std::process::exit(91);
}

fn crash_roundtrip(root: &Path) -> Result<serde_json::Value> {
    let status = std::process::Command::new(std::env::current_exe()?)
        .arg("--crash-fixture")
        .arg(root)
        .status()?;
    ensure!(
        status.code() == Some(91),
        "Crash fixture did not reach the intended exit point"
    );
    let db = Db::open_in_dir(root)?;
    let durable = notes::get(&db.conn, "durable")?.context("Committed note was lost")?;
    ensure!(
        notes::content_to_plain(&durable.content) == "Durable café 😀 Follow up",
        "Committed content was lost or uncommitted content leaked"
    );
    let structured: serde_json::Value = serde_json::from_str(&durable.content)?;
    ensure!(
        structured["blocks"][0]["runs"][0]["bold"] == true,
        "Committed formatting was lost"
    );
    ensure!(
        crate::native_projection::content_ids(&durable.content) == vec!["durable-chip".to_owned()],
        "Committed chip identity was lost"
    );
    let integrity: String = db
        .conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    ensure!(integrity == "ok", "Database integrity check failed");
    let fts: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM notes_fts WHERE notes_fts MATCH 'Durable'",
        [],
        |r| r.get(0),
    )?;
    let reminders: i64 = db.conn.query_row(
        "SELECT COUNT(*) FROM reminders WHERE note_id='durable' AND chip_id='durable-chip'",
        [],
        |r| r.get(0),
    )?;
    ensure!(
        fts == 1 && reminders == 1,
        "FTS or reminder transaction did not recover"
    );
    let items = attachments::list(&db.conn, "durable")?;
    ensure!(items.len() == 1, "Committed attachment metadata was lost");
    attachments::thumbnail(&db.conn, &items[0].id)?;
    attachments::cleanup_orphans(&db.conn)?;
    ensure!(
        !root
            .join("attachments/11111111-1111-4111-8111-111111111111.original")
            .exists(),
        "Interrupted import was not cleaned"
    );
    Ok(
        json!({"abrupt_exit_code":91,"integrity":integrity,"committed_note_preserved":true,"uncommitted_update_rolled_back":true,"fts_and_reminders_preserved":true,"attachment_preserved":true,"interrupted_import_cleaned":true}),
    )
}

fn measure<T>(action: impl FnOnce() -> Result<T>) -> Result<(T, serde_json::Value)> {
    let before = stats()?;
    let running = Arc::new(AtomicBool::new(true));
    let signal = running.clone();
    let sampler = std::thread::spawn(move || -> Result<(f64, f64)> {
        let mut working: f64 = 0.0;
        let mut private: f64 = 0.0;
        while signal.load(Ordering::Relaxed) {
            let current = stats()?;
            working = working.max(current.working_set_mib);
            private = private.max(current.private_commit_mib);
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok((working, private))
    });
    let started = Instant::now();
    let result = action();
    let elapsed = started.elapsed().as_secs_f64();
    running.store(false, Ordering::Relaxed);
    let (peak, private_peak) = sampler
        .join()
        .map_err(|_| anyhow::anyhow!("Sampler failed"))??;
    let after = stats()?;
    Ok((
        result?,
        json!({"elapsed_seconds":elapsed,"baseline_working_set_mib":before.working_set_mib,"baseline_private_commit_mib":before.private_commit_mib,"sampled_peak_working_set_mib":peak.max(after.working_set_mib),"sampled_peak_private_commit_mib":private_peak.max(after.private_commit_mib),"end_working_set_mib":after.working_set_mib,"cpu_seconds":after.cpu_seconds-before.cpu_seconds,"sampling_interval_ms":2}),
    ))
}

pub fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() == 3,
        "Usage: --reliability-probe <new-report.json>"
    );
    if args[1] == "--crash-fixture" {
        return crash_fixture(Path::new(&args[2]));
    }
    let mut report = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args[2])?;
    let root = Path::new(&args[2]).with_extension("sandbox");
    std::fs::create_dir(&root)?;
    std::fs::write(root.join("headless-marker.txt"), MARKER)?;
    image::RgbaImage::from_fn(320, 240, |x, y| {
        image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
    })
    .save(root.join("small.png"))?;
    let crash = crash_roundtrip(&root)?;
    let db = Db::open_in_dir(&root)?;
    let large = root.join("eight-megapixel.png");
    image::RgbaImage::from_fn(4000, 2000, |x, y| {
        image::Rgba([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8, 255])
    })
    .save(&large)?;
    let (large_item, import_measurement) =
        measure(|| attachments::import_image(&db.conn, "durable", &large))?;
    attachments::thumbnail(&db.conn, &large_item.id)?;
    let oversized = root.join("oversized.png");
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&oversized)?
        .set_len(20 * 1024 * 1024 + 1)?;
    ensure!(
        attachments::import_image(&db.conn, "durable", &oversized).is_err(),
        "Oversized file accepted"
    );
    image::RgbaImage::new(4001, 2000).save(root.join("too-many-pixels.png"))?;
    ensure!(
        attachments::import_image(&db.conn, "durable", &root.join("too-many-pixels.png")).is_err(),
        "Pixel limit failed"
    );
    let source = "Headless café 😀\n[ ] Follow up [[date:2026-10-15]]\n".repeat(64);
    let content = crate::content_for_editor_with_checklists_and_formats(
        &source,
        false,
        &crate::FormatRanges::default(),
    );
    for index in 0..20 {
        let id = format!("soak-{index}");
        notes::create(&db.conn, &id, &content, "#FFF9B1")?;
        for _ in 0..3 {
            attachments::import_image(&db.conn, &id, &root.join("small.png"))?;
        }
    }
    let ids =
        crate::native_projection::content_ids(&notes::get(&db.conn, "soak-0")?.unwrap().content);
    let mut samples = Vec::new();
    let mut edits = 0;
    let started = Instant::now();
    let baseline = stats()?;
    while started.elapsed() < Duration::from_secs(120) {
        if started.elapsed() < Duration::from_secs(60) {
            edits += 1;
            let text = format!("{source}{}", ".".repeat(edits));
            let snapshot = crate::content_for_editor_with_checklists_and_formats(
                &text,
                false,
                &crate::FormatRanges::default(),
            );
            // Carry editor-owned IDs exactly as the native snapshot path does.
            let snapshot = crate::native_projection::with_ids(&snapshot, &ids);
            notes::save_content_and_color(&db.conn, "soak-0", &snapshot, None)?;
            if edits % 10 == 0 {
                for index in 0..20 {
                    let images = attachments::list(&db.conn, &format!("soak-{index}"))?;
                    attachments::thumbnail(&db.conn, &images[(edits / 10) % 3].id)?;
                }
            }
        }
        if samples.len() * 15 <= started.elapsed().as_secs() as usize {
            let current = stats()?;
            println!("Headless soak: {} seconds", started.elapsed().as_secs());
            samples.push(json!({"elapsed_seconds":started.elapsed().as_secs_f64(),"working_set_mib":current.working_set_mib,"private_commit_mib":current.private_commit_mib,"cpu_seconds_since_start":current.cpu_seconds-baseline.cpu_seconds}));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let final_stats = stats()?;
    samples.push(json!({"elapsed_seconds":started.elapsed().as_secs_f64(),"working_set_mib":final_stats.working_set_mib,"private_commit_mib":final_stats.private_commit_mib,"cpu_seconds_since_start":final_stats.cpu_seconds-baseline.cpu_seconds}));
    let disk = Db::open_in_dir(&root)?;
    let saved = notes::get(&disk.conn, "soak-0")?.unwrap();
    ensure!(
        crate::native_projection::content_ids(&saved.content) == ids,
        "Chip identities changed"
    );
    ensure!(
        crate::editor_state_from_content_with_checklists(&saved.content).0
            == format!("{source}{}", ".".repeat(edits)),
        "Saved edits differ after reopen"
    );
    for index in 0..20 {
        ensure!(
            attachments::list(&disk.conn, &format!("soak-{index}"))?.len() == 3,
            "Attachments lost"
        );
    }
    let payload = json!({"release_build":!cfg!(debug_assertions),"headless":true,"production_data_accessed":false,"timestamp_utc":chrono::Utc::now().to_rfc3339(),"crash_roundtrip":crash,"large_image_import":import_measurement,"image_dimensions":"4000x2000 RGBA PNG","byte_and_pixel_limits_verified":true,"soak_seconds":started.elapsed().as_secs_f64(),"edits":edits,"note_count":20,"attachments_per_note":3,"samples":samples,"disk_content_and_chip_ids_verified":true,"limitations":"Headless persistence/thumbnail workload, not GUI/native typing or debounce; explicit process::exit skips all destructors, not power loss; import peaks sampled and exclude source generation; two-minute run is not an hours-long leak test"});
    report.write_all(serde_json::to_string_pretty(&payload)?.as_bytes())?;
    report.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crash_fixture_rejects_unmarked_paths_before_opening_a_database() {
        let root =
            std::env::temp_dir().join(format!("sticky-probe-guard-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        assert!(crash_fixture(&root).is_err());
        assert!(!root.join("sticky-notes.db").exists());
    }
    #[test]
    fn memory_sampler_stops_and_propagates_operation_errors() {
        let result = measure::<()>(|| anyhow::bail!("injected operation failure"));
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("injected operation failure"));
    }
}
