//! Execute generated POSIX-shell commands only in test-owned temporary directories.
#![cfg(unix)]
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\ntest transport bytes";

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "asd-image-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn generate(&self, image: &Path, destination: &Path) -> Output {
        Command::new(env!("CARGO_BIN_EXE_asd"))
            .args([
                "image-paste",
                image.to_str().unwrap(),
                "--directory",
                destination.to_str().unwrap(),
            ])
            .env("ASD_SOCKET", self.0.join("no-daemon.sock"))
            .output()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn generated_image_command_round_trips_without_local_execution_or_overwrite() {
    let fx = Fixture::new();
    let src = fx.0.join("source'$(touch BAD)`test`.png");
    std::fs::write(&src, PNG).unwrap();
    let destination = fx.0.join("remote'\"$(touch BAD)`test`");
    std::fs::create_dir(&destination).unwrap();
    let generated = fx.generate(&src, &destination);
    assert!(generated.status.success(), "{generated:?}");
    let command = String::from_utf8(generated.stdout).unwrap();
    assert!(!command.ends_with('\n'));
    assert!(!command.contains(src.file_name().unwrap().to_str().unwrap()));
    assert_eq!(
        std::fs::read_dir(&destination).unwrap().count(),
        0,
        "generation must not execute"
    );
    let mut paths = Vec::new();
    for _ in 0..2 {
        let decoded = Command::new("sh")
            .args(["-c", &command])
            .current_dir(&fx.0)
            .output()
            .unwrap();
        assert!(decoded.status.success(), "{decoded:?}");
        let path = PathBuf::from(String::from_utf8(decoded.stdout).unwrap().trim());
        assert!(path.starts_with(&destination));
        assert_eq!(std::fs::read(&path).unwrap(), PNG);
        paths.push(path);
    }
    assert_ne!(paths[0], paths[1]);
    assert!(!fx.0.join("BAD").exists());
}

#[test]
fn corrupted_payload_fails_before_creating_destination_file() {
    let fx = Fixture::new();
    let src = fx.0.join("source.png");
    std::fs::write(&src, PNG).unwrap();
    let dst = fx.0.join("remote");
    std::fs::create_dir(&dst).unwrap();
    let generated = fx.generate(&src, &dst);
    assert!(generated.status.success(), "{generated:?}");
    let command = String::from_utf8(generated.stdout)
        .unwrap()
        .replace("iVBOR", "aVBOR");
    let result = Command::new("sh")
        .args(["-c", &command])
        .current_dir(&fx.0)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    assert!(String::from_utf8_lossy(&result.stderr).contains("corrupted"));
    assert_eq!(std::fs::read_dir(&dst).unwrap().count(), 0);
}

#[test]
fn empty_unknown_oversized_and_missing_sources_emit_no_command() {
    let fx = Fixture::new();
    let src = fx.0.join("source.png");
    for bytes in [
        Vec::new(),
        b"not an image".to_vec(),
        vec![0; 1024 * 1024 + 1],
    ] {
        std::fs::write(&src, bytes).unwrap();
        let out = fx.generate(&src, &fx.0);
        assert!(!out.status.success());
        assert!(out.stdout.is_empty());
    }
    let out = fx.generate(&fx.0.join("missing"), &fx.0);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
}
