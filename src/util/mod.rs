pub mod clipboard;
pub mod coalesce;
pub mod format;
pub mod fs;
pub mod text;

/// Whether process `pid` is gone, polling `/proc` for up to 10 s (tests):
/// no longer listed, a zombie waiting for its new parent to reap it (`Z`),
/// or being reaped (`X`, shown briefly between the two). The first poll
/// that sees it gone answers; reading again could catch the `X` state.
#[cfg(all(test, target_os = "linux"))]
pub(crate) async fn process_gone(pid: u32) -> bool {
    let gone = || match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        // `pid (comm) S ...`: the state follows the last `)`, as comm may hold one.
        Ok(stat) => stat.rfind(')').and_then(|i| stat[i + 1..].split_whitespace().next()).is_some_and(|s| matches!(s, "Z" | "X" | "x")),
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if gone() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}
