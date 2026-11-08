//! The processes Attach to Process offers, from `/proc`, and what a refused attach is told. Reads files only;
//! the roots are parameters so tests use a fake tree.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;

pub const PROC: &str = "/proc";
pub const PASSWD: &str = "/etc/passwd";
pub const PTRACE_SCOPE: &str = "/proc/sys/kernel/yama/ptrace_scope";
/// Most bytes of a command line read.
pub const MAX_CMDLINE: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub pid: u32,
    pub ppid: u32,
    pub uid: u32,
    pub user: String,
    pub comm: String,
    pub cmdline: String,
}

/// `(comm, ppid)` of a `stat` file. The comm sits between the first `(` and the last `)` and may hold both and spaces.
fn parse_stat(text: &str) -> Option<(String, u32)> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    if close < open {
        return None;
    }
    let comm = text[open + 1..close].to_string();
    let mut rest = text[close + 1..].split_whitespace();
    let _state = rest.next()?;
    let ppid = rest.next()?.parse().ok()?;
    Some((comm, ppid))
}

/// The real uid of a `status` file.
fn parse_uid(status: &str) -> Option<u32> {
    let line = status.lines().find_map(|l| l.strip_prefix("Uid:"))?;
    line.split_whitespace().next()?.parse().ok()
}

fn parse_passwd(text: &str) -> HashMap<u32, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let mut f = line.split(':');
        let (Some(name), Some(_), Some(uid)) = (f.next(), f.next(), f.next()) else { continue };
        if let Ok(uid) = uid.parse::<u32>() {
            out.entry(uid).or_insert_with(|| name.to_string());
        }
    }
    out
}

/// The NUL-separated words of a `cmdline` file joined by one space; at most `MAX_CMDLINE` bytes are read.
fn read_cmdline(path: &Path) -> String {
    let mut buf = Vec::new();
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.take(MAX_CMDLINE as u64).read_to_end(&mut buf);
    }
    let mut words: Vec<&[u8]> = buf.split(|b| *b == 0).collect();
    while words.last().is_some_and(|w| w.is_empty()) {
        words.pop();
    }
    let joined: Vec<u8> = words.join(&b' ');
    String::from_utf8_lossy(&joined).into_owned()
}

/// Every process `own` and its descendants, gdb and kernel threads aside, highest pid first.
pub fn list(proc_root: &Path, passwd: &Path, own: u32) -> Vec<ProcInfo> {
    let names = std::fs::read_to_string(passwd).map(|t| parse_passwd(&t)).unwrap_or_default();
    let Ok(dir) = std::fs::read_dir(proc_root) else { return Vec::new() };
    let mut all = Vec::new();
    for entry in dir.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        let p = entry.path();
        let Some((comm, ppid)) = std::fs::read_to_string(p.join("stat")).ok().and_then(|t| parse_stat(&t)) else { continue };
        let Some(uid) = std::fs::read_to_string(p.join("status")).ok().and_then(|t| parse_uid(&t)) else { continue };
        let cmdline = read_cmdline(&p.join("cmdline"));
        let user = names.get(&uid).cloned().unwrap_or_else(|| uid.to_string());
        all.push(ProcInfo { pid, ppid, uid, user, comm, cmdline });
    }
    let parents: HashMap<u32, u32> = all.iter().map(|i| (i.pid, i.ppid)).collect();
    let ours = |pid: u32| {
        let mut seen = HashSet::new();
        let mut at = pid;
        loop {
            if at == own {
                return true;
            }
            if !seen.insert(at) {
                return false;
            }
            match parents.get(&at) {
                Some(&up) => at = up,
                None => return false,
            }
        }
    };
    let mut out: Vec<ProcInfo> = all.iter().filter(|i| i.comm != "gdb" && !i.cmdline.is_empty() && !ours(i.pid)).cloned().collect();
    out.sort_by_key(|i| std::cmp::Reverse(i.pid));
    out
}

/// The name of a process, `None` when it is gone.
pub fn comm(proc_root: &Path, pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(proc_root.join(pid.to_string()).join("comm")).ok()?;
    Some(text.trim_end_matches(['\n', '\r']).to_string())
}

/// The Yama `ptrace_scope`, `None` when the file is missing or not a number.
pub fn ptrace_scope(path: &Path) -> Option<u8> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// What a refused attach is told. gdb gives no reason over DAP (the error is only in its own log), so this
/// words the usual cause.
pub fn attach_failed_text(pid: Option<i64>, scope: Option<u8>) -> String {
    let Some(pid) = pid else { return "Could not attach: gdb gave no reason over DAP. Check launch.pid or launch.target.".into() };
    let head = format!("Could not attach to process {pid}: gdb gave no reason over DAP.");
    match scope {
        Some(1) => format!(
            "{head} ptrace_scope is 1 ({PTRACE_SCOPE}): only gdb's own descendants, or a process that called prctl(PR_SET_PTRACER, ...), can be attached."
        ),
        Some(2) => format!("{head} ptrace_scope is 2 ({PTRACE_SCOPE}): only a debugger with CAP_SYS_PTRACE can attach."),
        Some(3) => format!("{head} ptrace_scope is 3 ({PTRACE_SCOPE}): attaching is turned off until a reboot."),
        _ => format!("{head} Check that the process still runs, is yours and is not being debugged already."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct Tree(PathBuf);

    impl Tree {
        fn new(name: &str) -> Self {
            let d = std::env::temp_dir().join(format!("jetl-procs-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Tree(d)
        }

        fn add(&self, pid: &str, stat: &str, uid: u32, cmdline: &[u8]) {
            let d = self.0.join(pid);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("stat"), stat).unwrap();
            std::fs::write(d.join("status"), format!("Name:\tx\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\n")).unwrap();
            std::fs::write(d.join("cmdline"), cmdline).unwrap();
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn list_leaves_out_jetl_its_descendants_gdb_and_kernel_threads() {
        let t = Tree::new("list");
        t.add("1", "1 (init) S 0 1 1 0", 0, b"/sbin/init\0");
        t.add("100", "100 (jetl) S 1 1 1 0", 1000, b"jetl\0.\0");
        t.add("101", "101 (sh) S 100 1 1 0", 1000, b"sh\0");
        t.add("102", "102 (sleep) S 101 1 1 0", 1000, b"sleep\x00317\x00");
        t.add("200", "200 (gdb) S 1 1 1 0", 1000, b"gdb\0-q\0-i=dap\0");
        t.add("300", "300 (kworker) S 2 0 0 0", 0, b"");
        t.add("400", "400 (a) b) S 1 1 1 0", 1000, b"/tmp/x\0--flag\0\0");
        t.add("500", "500 (sleep) S 1 1 1 0", 1234, b"sleep\x00300\x00");
        std::fs::create_dir_all(t.0.join("self")).unwrap();
        let passwd = t.0.join("passwd");
        std::fs::write(&passwd, "root:x:0:0::/root:/bin/sh\nme:x:1000:1000::/home/me:/bin/zsh\n").unwrap();
        let got = list(&t.0, &passwd, 100);
        assert_eq!(got.iter().map(|i| i.pid).collect::<Vec<_>>(), [500, 400, 1]);
        let a = &got[1];
        assert_eq!((a.comm.as_str(), a.user.as_str(), a.cmdline.as_str(), a.ppid), ("a) b", "me", "/tmp/x --flag", 1));
        assert_eq!(got[0].user, "1234");
        assert_eq!(got[2].user, "root");
    }

    #[test]
    fn a_long_command_line_is_cut() {
        let t = Tree::new("long");
        t.add("5", "5 (a) S 1 1 1 0", 0, &[b'a'; 10_000]);
        let got = list(&t.0, &t.0.join("none"), 99);
        assert_eq!(got[0].cmdline.len(), MAX_CMDLINE);
    }

    #[test]
    fn ptrace_scope_reads_the_number() {
        let t = Tree::new("scope");
        for (name, text, want) in [("a", "1\n", Some(1)), ("b", "3", Some(3)), ("c", "x", None)] {
            std::fs::write(t.0.join(name), text).unwrap();
            assert_eq!(ptrace_scope(&t.0.join(name)), want, "{text:?}");
        }
        assert_eq!(ptrace_scope(&t.0.join("missing")), None);
    }

    #[test]
    fn attach_failed_text_names_ptrace_scope() {
        let p = "/proc/sys/kernel/yama/ptrace_scope";
        let head = "Could not attach to process 42: gdb gave no reason over DAP.";
        assert_eq!(
            attach_failed_text(Some(42), Some(1)),
            format!(
                "{head} ptrace_scope is 1 ({p}): only gdb's own descendants, or a process that called prctl(PR_SET_PTRACER, ...), can be attached."
            )
        );
        assert_eq!(
            attach_failed_text(Some(42), Some(2)),
            format!("{head} ptrace_scope is 2 ({p}): only a debugger with CAP_SYS_PTRACE can attach.")
        );
        assert_eq!(
            attach_failed_text(Some(42), Some(3)),
            format!("{head} ptrace_scope is 3 ({p}): attaching is turned off until a reboot.")
        );
        let other = format!("{head} Check that the process still runs, is yours and is not being debugged already.");
        assert_eq!(attach_failed_text(Some(42), Some(0)), other);
        assert_eq!(attach_failed_text(Some(42), None), other);
        assert_eq!(attach_failed_text(None, Some(1)), "Could not attach: gdb gave no reason over DAP. Check launch.pid or launch.target.");
    }

    #[test]
    fn comm_of_a_gone_process_is_none() {
        let t = Tree::new("comm");
        assert_eq!(comm(&t.0, 999), None);
        assert!(comm(Path::new(PROC), std::process::id()).is_some());
    }
}
