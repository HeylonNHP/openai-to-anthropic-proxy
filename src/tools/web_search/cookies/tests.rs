use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

const TTL: Duration = Duration::from_secs(3600);
const NOW: u64 = 1_800_000_000;

fn url(host: &str) -> url::Url {
    url::Url::parse(&format!("https://{host}/html/")).expect("url")
}

/// A fresh empty directory, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "web-search-cookies-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        Self(dir)
    }

    fn file(&self) -> PathBuf {
        self.0.join(FILE_NAME)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn a_cookie_is_sent_back_to_its_site_only() {
    let jar = PersistentJar::open_at(None, TTL, NOW);
    jar.store_at(
        &["kl=wt-wt; Max-Age=3600; Secure"],
        &url("duckduckgo.test"),
        NOW,
    );
    assert_eq!(
        jar.header_at(&url("duckduckgo.test"), NOW).as_deref(),
        Some("kl=wt-wt")
    );
    assert_eq!(jar.header_at(&url("other.test"), NOW), None);
}

#[test]
fn an_expired_cookie_is_neither_sent_nor_saved() {
    let dir = TempDir::new();
    let jar = PersistentJar::open_at(Some(&dir.0), TTL, NOW);
    let site = url("duckduckgo.test");
    jar.store_at(&["keep=1; Max-Age=3600", "gone=1; Max-Age=1"], &site, NOW);
    assert!(
        jar.header_at(&site, NOW)
            .is_some_and(|h| h.contains("gone=1"))
    );

    std::thread::sleep(Duration::from_millis(1200));

    let sent = jar.header_at(&site, NOW).expect("one cookie is left");
    assert_eq!(sent, "keep=1", "the expired cookie is not sent");
    // Storing anything rewrites the file from the unexpired cookies only.
    jar.store_at(&["more=1; Max-Age=3600"], &site, NOW);
    let saved = std::fs::read_to_string(dir.file()).expect("saved");
    assert!(!saved.contains("gone"), "the expired cookie is not saved");
    assert!(saved.contains("keep") && saved.contains("more"));
}

#[test]
fn an_expired_cookie_in_the_file_is_not_loaded() {
    let dir = TempDir::new();
    let site = url("duckduckgo.test");
    let jar = PersistentJar::open_at(Some(&dir.0), TTL, NOW);
    jar.store_at(&["gone=1; Max-Age=1", "keep=1; Max-Age=3600"], &site, NOW);
    drop(jar);

    std::thread::sleep(Duration::from_millis(1200));

    let reopened = PersistentJar::open_at(Some(&dir.0), TTL, NOW);
    assert_eq!(reopened.header_at(&site, NOW).as_deref(), Some("keep=1"));
}

#[test]
fn the_session_survives_a_restart() {
    let dir = TempDir::new();
    let site = url("duckduckgo.test");
    let jar = PersistentJar::open_at(Some(&dir.0), TTL, NOW);
    // One persistent cookie and one session cookie (no expiry).
    jar.store_at(&["kl=wt-wt; Max-Age=3600", "s=abc"], &site, NOW);
    drop(jar);

    let reopened = PersistentJar::open_at(Some(&dir.0), TTL, NOW + 10);
    let sent = reopened.header_at(&site, NOW + 10).expect("cookies");
    assert!(
        sent.contains("kl=wt-wt") && sent.contains("s=abc"),
        "{sent}"
    );
}

#[test]
fn a_session_older_than_the_limit_is_discarded_on_load() {
    let dir = TempDir::new();
    let site = url("duckduckgo.test");
    let jar = PersistentJar::open_at(Some(&dir.0), TTL, NOW);
    jar.store_at(&["kl=wt-wt; Max-Age=86400"], &site, NOW);
    drop(jar);

    let at_limit = PersistentJar::open_at(Some(&dir.0), TTL, NOW + TTL.as_secs());
    assert!(
        at_limit.header_at(&site, NOW + TTL.as_secs()).is_some(),
        "exactly at the limit it is still valid"
    );
    let past = PersistentJar::open_at(Some(&dir.0), TTL, NOW + TTL.as_secs() + 1);
    assert_eq!(past.header_at(&site, NOW + TTL.as_secs() + 1), None);
}

#[test]
fn a_running_session_rotates_when_it_goes_stale() {
    let jar = PersistentJar::open_at(None, TTL, NOW);
    let site = url("duckduckgo.test");
    jar.store_at(&["s=abc"], &site, NOW);
    assert!(jar.header_at(&site, NOW + 100).is_some());

    let later = NOW + TTL.as_secs() + 1;
    assert_eq!(jar.header_at(&site, later), None, "the old session is gone");
    jar.store_at(&["s=new"], &site, later);
    assert_eq!(
        jar.header_at(&site, later + 1).as_deref(),
        Some("s=new"),
        "a new session starts from the next cookie"
    );
    // The new session runs its own full lifetime.
    assert!(jar.header_at(&site, later + TTL.as_secs()).is_some());
}

#[test]
fn a_session_from_the_future_is_not_trusted() {
    let dir = TempDir::new();
    let site = url("duckduckgo.test");
    let jar = PersistentJar::open_at(Some(&dir.0), TTL, NOW + 10_000);
    jar.store_at(&["kl=wt-wt; Max-Age=86400"], &site, NOW + 10_000);
    drop(jar);

    let reopened = PersistentJar::open_at(Some(&dir.0), TTL, NOW);
    assert_eq!(reopened.header_at(&site, NOW), None, "the clock moved back");
}

#[test]
fn a_corrupt_file_starts_an_empty_session_and_is_replaced() {
    let dir = TempDir::new();
    std::fs::create_dir_all(&dir.0).expect("dir");
    std::fs::write(dir.file(), "{ this is not json").expect("write");
    let site = url("duckduckgo.test");

    let jar = PersistentJar::open_at(Some(&dir.0), TTL, NOW);
    assert_eq!(jar.header_at(&site, NOW), None);
    jar.store_at(&["kl=wt-wt; Max-Age=3600"], &site, NOW);

    let reopened = PersistentJar::open_at(Some(&dir.0), TTL, NOW);
    assert_eq!(reopened.header_at(&site, NOW).as_deref(), Some("kl=wt-wt"));
}

#[test]
fn a_file_from_another_version_is_ignored() {
    let dir = TempDir::new();
    std::fs::create_dir_all(&dir.0).expect("dir");
    std::fs::write(
        dir.file(),
        format!(r#"{{"version":99,"started_at":{NOW},"cookies":[]}}"#),
    )
    .expect("write");
    let jar = PersistentJar::open_at(Some(&dir.0), TTL, NOW);
    assert_eq!(jar.header_at(&url("duckduckgo.test"), NOW), None);
}

#[test]
fn nothing_is_written_until_there_is_something_to_remember() {
    let dir = TempDir::new();
    let jar = PersistentJar::open_at(Some(&dir.0), TTL, NOW);
    // A response with no cookies, and one whose cookie is already expired.
    jar.store_at(&[], &url("duckduckgo.test"), NOW);
    jar.store_at(&["a=b; Max-Age=0"], &url("duckduckgo.test"), NOW);
    assert!(!dir.0.exists(), "no directory, no file");
}

#[test]
fn an_unwritable_state_dir_does_not_lose_the_session() {
    let dir = TempDir::new();
    // The "directory" is a file, so it can never be created.
    std::fs::write(&dir.0, "x").expect("write");
    let jar = PersistentJar::open_at(Some(&dir.0), TTL, NOW);
    let site = url("duckduckgo.test");
    jar.store_at(&["kl=wt-wt; Max-Age=3600"], &site, NOW);
    assert_eq!(jar.header_at(&site, NOW).as_deref(), Some("kl=wt-wt"));
    let _ = std::fs::remove_file(&dir.0);
}

#[test]
fn a_save_leaves_no_temporary_file() {
    let dir = TempDir::new();
    let jar = PersistentJar::open_at(Some(&dir.0), TTL, NOW);
    jar.store_at(&["kl=wt-wt; Max-Age=3600"], &url("duckduckgo.test"), NOW);
    let names: Vec<_> = std::fs::read_dir(&dir.0)
        .expect("dir")
        .map(|e| e.expect("entry").file_name())
        .collect();
    assert_eq!(names, [std::ffi::OsString::from(FILE_NAME)]);
}

#[cfg(unix)]
#[test]
fn the_session_file_and_directory_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new();
    let jar = PersistentJar::open_at(Some(&dir.0), TTL, NOW);
    jar.store_at(&["kl=wt-wt; Max-Age=3600"], &url("duckduckgo.test"), NOW);
    let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode(&dir.file()), 0o600);
    assert_eq!(mode(&dir.0), 0o700);
}
