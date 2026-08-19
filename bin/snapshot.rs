//! Snapshot creation tool
//!
//! Creates a V8 snapshot for faster worker startup. No other backend has
//! snapshots, so this binary requires the v8 feature.

use openworkers_runner::runtime::snapshot::create_runtime_snapshot;

use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

const RUNTIME_SNAPSHOT_PATH: &str = env!("RUNTIME_SNAPSHOT_PATH");

fn main() -> std::io::Result<()> {
    let snapshot = match create_runtime_snapshot() {
        Ok(snapshot) => snapshot,
        Err(err) => {
            eprintln!("Failed to create snapshot: {:?}", err);
            std::process::exit(1);
        }
    };

    let mut file = File::create(PathBuf::from(RUNTIME_SNAPSHOT_PATH))?;
    file.write_all(&snapshot.output)?;

    println!("Snapshot created: {:?}", file);

    Ok(())
}
