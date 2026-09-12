//! Log rotation: size-based, keep-N-generations.
//!
//! Task 4 wires this into the supervisor's child stdio sink.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::path::{Path, PathBuf};

pub const MAX_BYTES: u64 = 10 * 1024 * 1024; // 10 MB
pub const KEEP_GENERATIONS: u32 = 3;

/// Opens (or rotates) the log at `path` and appends `bytes`.
///
/// Rotation: when the current file would exceed [`MAX_BYTES`], generations
/// shift (`log.1` ← `log`, `log.2` ← `log.1`, …) and a fresh `log` starts.
/// Files beyond [`KEEP_GENERATIONS`] are dropped.
pub fn append(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let current = path.metadata().map(|m| m.len()).unwrap_or(0);
    if current.saturating_add(bytes.len() as u64) > MAX_BYTES {
        rotate(path)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(bytes)
}

fn rotate(path: &Path) -> io::Result<()> {
    // Drop the oldest generation first, then shift each older generation up.
    let oldest = gen(path, KEEP_GENERATIONS);
    if oldest.exists() {
        fs::remove_file(&oldest)?;
    }
    for gen_num in (1..KEEP_GENERATIONS).rev() {
        let from = gen(path, gen_num);
        if from.exists() {
            fs::rename(&from, gen(path, gen_num + 1))?;
        }
    }
    if path.exists() {
        fs::rename(path, gen(path, 1))?;
    }
    Ok(())
}

fn gen(path: &Path, n: u32) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{file_name}.{n}"))
}

/// Convenience: append a line (adds the newline if missing).
pub fn append_line(path: &Path, line: &str) -> io::Result<()> {
    if line.ends_with('\n') {
        append(path, line.as_bytes())
    } else {
        let mut out = line.to_string();
        out.push('\n');
        append(path, out.as_bytes())
    }
}

/// Tails up to `max_bytes` from the end of `path` (`None` → whole file).
pub fn tail(path: &Path, max_bytes: Option<u64>) -> io::Result<String> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let take = max_bytes.unwrap_or(len).min(len);
    file.seek(io::SeekFrom::Start(len - take))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_writes_do_not_rotate() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("app.log");
        append_line(&log, "hello").unwrap();
        append_line(&log, "world").unwrap();
        assert_eq!(tail(&log, None).unwrap(), "hello\nworld\n");
        assert!(!gen(&log, 1).exists());
    }

    #[test]
    fn rotation_shifts_generations() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("switchboard.log");
        for _ in 0..240 {
            append_line(&log, &"x".repeat(120_000)).unwrap();
        }
        assert!(log.exists());
        assert!(gen(&log, 1).exists());
        assert!(gen(&log, 2).exists());
        assert!(!gen(&log, 3).exists());
        // the current file is small again
        assert!(log.metadata().unwrap().len() < MAX_BYTES);
    }

    #[test]
    fn tail_respects_max_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("t.log");
        append_line(&log, "abcdefghij").unwrap();
        // "abcdefghij\n" is 11 bytes; the last 3 are "ij\n".
        assert_eq!(tail(&log, Some(3)).unwrap(), "ij\n");
        assert_eq!(tail(&log, Some(1000)).unwrap(), "abcdefghij\n");
    }
}
