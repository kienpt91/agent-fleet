//! Resident memory of this process, for `fleet status` and the memory soak test.

/// Resident set size in KiB. Linux reads `/proc/self/statm` (resident pages times the page size);
/// other platforms ask `ps`. `None` when it cannot be determined.
pub fn rss_kb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/self/statm").ok()?;
        rss_kb_from_statm(&text, 4096)
    }
    #[cfg(not(target_os = "linux"))]
    {
        use std::process::{Command, Stdio};
        let out = Command::new("/bin/ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .env_clear()
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        String::from_utf8(out.stdout).ok()?.trim().parse().ok()
    }
}

/// Parses `/proc/<pid>/statm` (`size resident shared ...`, in pages).
pub fn rss_kb_from_statm(text: &str, page_bytes: u64) -> Option<u64> {
    let resident: u64 = text.split_whitespace().nth(1)?.parse().ok()?;
    Some(resident.checked_mul(page_bytes)? / 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_statm() {
        assert_eq!(rss_kb_from_statm("100 25 10 1 0 5 0", 4096), Some(100));
        assert_eq!(rss_kb_from_statm("100", 4096), None);
        assert_eq!(rss_kb_from_statm("a b", 4096), None);
        assert_eq!(rss_kb_from_statm("1 18446744073709551615", 4096), None);
    }

    #[test]
    fn this_process_has_a_plausible_rss() {
        let kb = rss_kb().expect("rss is available");
        assert!(kb > 500 && kb < 64 * 1024 * 1024, "{kb}");
    }
}
