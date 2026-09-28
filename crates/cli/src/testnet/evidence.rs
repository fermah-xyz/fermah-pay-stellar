//! Evidence records: one JSON file per network operation.

use std::path::Path;

use anyhow::Context;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub const EXPLORER: &str = "https://stellar.expert/explorer/testnet";

#[must_use]
pub fn tx_url(hash: &str) -> String {
    format!("{EXPLORER}/tx/{hash}")
}

/// Adds `recorded_at` and writes `record` to
/// `dir/<date>T<hhmmss>-<name>.json`. An existing record is never replaced:
/// repeating an operation, e.g. to show that a duplicate is refused, must
/// leave the first run's evidence intact.
pub fn write(dir: &Path, name: &str, record: serde_json::Value) -> anyhow::Result<()> {
    write_at(dir, name, record, OffsetDateTime::now_utc())
}

fn write_at(
    dir: &Path,
    name: &str,
    mut record: serde_json::Value,
    now: OffsetDateTime,
) -> anyhow::Result<()> {
    record["recorded_at"] = serde_json::Value::String(now.format(&Rfc3339)?);
    record["network"] = serde_json::Value::String("stellar:testnet".to_owned());
    std::fs::create_dir_all(dir)?;
    let stamp = format!("{}T{:02}{:02}{:02}", now.date(), now.hour(), now.minute(), now.second());
    let path = dir.join(format!("{stamp}-{name}.json"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    std::io::Write::write_all(&mut file, &serde_json::to_vec_pretty(&record)?)
        .with_context(|| format!("writing {}", path.display()))?;
    println!("{}", serde_json::to_string_pretty(&record)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_existing_record_is_never_replaced() {
        let dir = std::env::temp_dir().join(format!("evidence-test-{}", std::process::id()));
        let now = OffsetDateTime::now_utc();
        write_at(&dir, "same", serde_json::json!({ "run": 1 }), now).unwrap();
        let second = write_at(&dir, "same", serde_json::json!({ "run": 2 }), now);
        let kept: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
            .collect();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!((second.is_err(), kept.len(), kept[0].contains("\"run\": 1")), (true, 1, true));
    }
}
