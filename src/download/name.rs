//! Destination file naming: sanitization of untrusted release/inner file
//! names and collision-free allocation inside the download directory.

use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};

/// Make an untrusted name safe to use as a single file name: path
/// separators (and the Windows drive separator) become `_`, control
/// characters are dropped, leading dots are stripped so the result can
/// neither traverse directories nor hide itself. Falls back to
/// `"download"` when nothing survives.
pub fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' => '_',
            other => other,
        })
        .filter(|c| !c.is_control())
        .collect();
    let cleaned = cleaned.trim().trim_start_matches('.').trim();
    if cleaned.is_empty() {
        "download".to_string()
    } else {
        cleaned.to_string()
    }
}

/// `attempt` 1 keeps the name; higher attempts insert ` (n)` before the
/// extension: `movie.mkv` → `movie (2).mkv`.
fn numbered(name: &str, attempt: u32) -> String {
    if attempt <= 1 {
        return name.to_string();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({attempt}).{ext}"),
        _ => format!("{name} ({attempt})"),
    }
}

/// The in-flight temp file next to the final destination.
pub fn partial_path(final_path: &Path) -> PathBuf {
    let mut partial = final_path.as_os_str().to_owned();
    partial.push(".partial");
    PathBuf::from(partial)
}

/// Pick a collision-free destination for `name` inside `dir` and atomically
/// claim it by creating the `.partial` file (`create_new`). Returns the
/// final path and the opened partial file.
pub async fn allocate_destination(dir: &Path, name: &str) -> AppResult<(PathBuf, tokio::fs::File)> {
    let name = sanitize_file_name(name);
    tokio::fs::create_dir_all(dir).await.map_err(|e| {
        AppError::Internal(anyhow::anyhow!(
            "creating download dir {}: {e}",
            dir.display()
        ))
    })?;
    for attempt in 1..=1000u32 {
        let final_path = dir.join(numbered(&name, attempt));
        if tokio::fs::try_exists(&final_path).await.unwrap_or(false) {
            continue;
        }
        let partial = partial_path(&final_path);
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&partial)
            .await
        {
            Ok(file) => return Ok((final_path, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(AppError::Internal(anyhow::anyhow!(
                    "creating {}: {e}",
                    partial.display()
                )))
            }
        }
    }
    Err(AppError::Internal(anyhow::anyhow!(
        "could not find a free file name for '{name}' in {}",
        dir.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitization_neutralizes_traversal_and_weird_chars() {
        assert_eq!(sanitize_file_name("movie.mkv"), "movie.mkv");
        assert_eq!(sanitize_file_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(sanitize_file_name("..\\..\\boot.ini"), "_.._boot.ini");
        assert_eq!(sanitize_file_name("/etc/shadow"), "_etc_shadow");
        assert_eq!(sanitize_file_name(".hidden"), "hidden");
        assert_eq!(sanitize_file_name("..."), "download");
        assert_eq!(sanitize_file_name(""), "download");
        assert_eq!(sanitize_file_name("a\x00b\r\nc.mkv"), "abc.mkv");
        assert_eq!(sanitize_file_name("C:evil.exe"), "C_evil.exe");
        assert_eq!(
            sanitize_file_name("Movie (2026) [1080p] äöü.mkv"),
            "Movie (2026) [1080p] äöü.mkv",
            "harmless characters survive"
        );
    }

    #[test]
    fn numbering_inserts_before_the_extension() {
        assert_eq!(numbered("movie.mkv", 1), "movie.mkv");
        assert_eq!(numbered("movie.mkv", 2), "movie (2).mkv");
        assert_eq!(numbered("noext", 3), "noext (3)");
        assert_eq!(numbered(".partialish", 2), ".partialish (2)");
    }

    #[tokio::test]
    async fn allocation_skips_existing_finals_and_partials() {
        let dir = tempfile::tempdir().unwrap();
        // First claim gets the plain name.
        let (final1, _file1) = allocate_destination(dir.path(), "movie.mkv").await.unwrap();
        assert_eq!(final1, dir.path().join("movie.mkv"));
        assert!(partial_path(&final1).exists());

        // Second claim collides with the live partial → " (2)".
        let (final2, _file2) = allocate_destination(dir.path(), "movie.mkv").await.unwrap();
        assert_eq!(final2, dir.path().join("movie (2).mkv"));

        // A finished file also blocks its name.
        std::fs::write(dir.path().join("movie (3).mkv"), b"x").unwrap();
        let (final3, _file3) = allocate_destination(dir.path(), "movie.mkv").await.unwrap();
        assert_eq!(final3, dir.path().join("movie (4).mkv"));

        // Traversal attempts stay inside the directory.
        let (evil, _file4) = allocate_destination(dir.path(), "../../escape.bin")
            .await
            .unwrap();
        assert!(evil.starts_with(dir.path()));
        assert_eq!(evil.file_name().unwrap(), "_.._escape.bin");
    }
}
