//! prog: a command's progress as a card on the desktop. Each job is a JSON file in $XDG_RUNTIME_DIR/progress
//! (the shell's Progress service watches it); `prog -- CMD` runs CMD in a pty and reads % and (n/m) from its output.
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const HELP: &str = "prog: show a job's progress as a floating card

  prog [-t TITLE] -- CMD ARGS…   run CMD (in a pty, its output still here) and show its progress
  prog start TITLE [STEP]        a card of your own; prints its id
  prog set ID PERCENT [STEP]     PERCENT 0-100, or - for unknown
  prog done ID [--fail]          the card ends (and goes after a moment)
  prog list                      the jobs now

Jobs are JSON files in $XDG_RUNTIME_DIR/progress; anything may write one.";

fn dir() -> PathBuf {
    let d = PathBuf::from(std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into())).join("progress");
    let _ = std::fs::create_dir_all(&d);
    d
}
fn now() -> f64 { SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64()) }

/// Written whole and renamed in, so the shell never reads half a file.
fn write(id: &str, v: &Value) {
    let p = dir().join(format!("{id}.json"));
    let tmp = dir().join(format!(".{id}.tmp"));
    if std::fs::write(&tmp, v.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &p);
    }
}
fn read(id: &str) -> Option<Value> { serde_json::from_str(&std::fs::read_to_string(dir().join(format!("{id}.json"))).ok()?).ok() }

fn new_id() -> String { format!("{}-{}", std::process::id(), (now() * 1000.0) as u64 % 100000) }

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        None | Some("-h") | Some("--help") => { println!("{HELP}"); 0 }
        Some("start") => {
            let id = new_id();
            write(&id, &job(&id, args.get(1).map_or("Working", |s| s), args.get(2).map_or("", |s| s), "", None));
            println!("{id}");
            0
        }
        Some("set") => update(&args, |v, a| {
            v["percent"] = json!(a.get(2).and_then(|s| s.parse::<f64>().ok()).map_or(-1.0, |p| p.clamp(0.0, 100.0)));
            if let Some(s) = a.get(3) { v["step"] = json!(s) }
        }),
        Some("done") => update(&args, |v, a| {
            let fail = a.iter().any(|s| s == "--fail");
            v["state"] = json!(if fail { "failed" } else { "done" });
            if !fail { v["percent"] = json!(100) }
        }),
        Some("list") => {
            for e in std::fs::read_dir(dir()).into_iter().flatten().flatten() {
                if let Ok(s) = std::fs::read_to_string(e.path()) { if e.path().extension().is_some_and(|x| x == "json") { println!("{s}") } }
            }
            0
        }
        _ => {
            let (title, cmd) = match args.iter().position(|a| a == "--") {
                Some(i) => {
                    let t = args[..i].windows(2).find(|w| w[0] == "-t").map(|w| w[1].clone());
                    (t, args[i + 1..].to_vec())
                }
                None => (None, args.clone()),
            };
            if cmd.is_empty() { eprintln!("{HELP}"); 2 } else { run(title, cmd) }
        }
    };
    std::process::exit(code);
}

fn job(id: &str, title: &str, step: &str, cmd: &str, pid: Option<u32>) -> Value {
    json!({ "id": id, "title": title, "step": step, "cmd": cmd, "percent": -1, "count": null, "state": "running",
            "started": now(), "updated": now(), "pid": pid.unwrap_or(std::process::id()), "code": null })
}

fn update(a: &[String], f: impl Fn(&mut Value, &[String])) -> i32 {
    let Some(id) = a.get(1) else { eprintln!("prog: which id?"); return 2 };
    let Some(mut v) = read(id) else { eprintln!("prog: no job {id}"); return 1 };
    f(&mut v, a);
    v["updated"] = json!(now());
    write(id, &v);
    0
}

/// The terminal's termios, put back when dropped.
struct Raw(Option<libc::termios>);
impl Raw {
    fn on() -> Raw {
        unsafe {
            if libc::isatty(0) == 0 { return Raw(None) }
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 { return Raw(None) }
            let old = t;
            libc::cfmakeraw(&mut t);
            libc::tcsetattr(0, libc::TCSANOW, &t);
            Raw(Some(old))
        }
    }
}
impl Drop for Raw {
    fn drop(&mut self) { if let Some(t) = self.0 { unsafe { libc::tcsetattr(0, libc::TCSANOW, &t); } } }
}

/// What the output says so far: a percentage, a (n/m) count and the line it came on.
#[derive(Default)]
struct Seen { percent: Option<f64>, count: Option<(u64, u64)>, step: String, line: String, esc: u8 }

impl Seen {
    fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            // ANSI escapes skipped: ESC [ … final, ESC ] … BEL
            match self.esc {
                1 => { self.esc = if b == b'[' { 2 } else if b == b']' { 3 } else { 0 }; continue }
                2 => { if (0x40..=0x7e).contains(&b) { self.esc = 0 } continue }
                3 => { if b == 7 { self.esc = 0 } else if b == 0x1b { self.esc = 1 } continue }
                _ => {}
            }
            match b {
                0x1b => self.esc = 1,
                b'\r' | b'\n' => self.line_end(),
                _ if b >= 0x20 || b == b'\t' => self.line.push(b as char),
                _ => {}
            }
            if self.line.len() > 400 { self.line_end() }
        }
        // a bar drawn without a line end yet counts too
        if !self.line.is_empty() { let l = self.line.clone(); self.parse(&l, false) }
    }
    fn line_end(&mut self) {
        let l = std::mem::take(&mut self.line);
        self.parse(&l, true);
    }
    fn parse(&mut self, l: &str, done: bool) {
        let t = l.trim();
        if t.is_empty() { return }
        if let Some(p) = last_percent(t) { self.percent = Some(p) }
        if let Some(c) = count(t) { self.count = Some(c) }
        if done || self.step.is_empty() {
            // the words before the bar: "downloading foo-1.2…", "(3/12) installing bar"
            let s: String = t.split(|c: char| c == '[' || c == '#').next().unwrap_or(t).trim().chars().take(80).collect();
            if s.chars().any(|c| c.is_alphabetic()) { self.step = s }
        }
    }
}

fn last_percent(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let mut out = None;
    for (i, &c) in b.iter().enumerate() {
        if c != b'%' { continue }
        let mut j = i;
        while j > 0 && (b[j - 1].is_ascii_digit() || b[j - 1] == b'.') { j -= 1 }
        if j < i { if let Ok(p) = s[j..i].parse::<f64>() { if p <= 100.0 { out = Some(p) } } }
    }
    out
}

fn count(s: &str) -> Option<(u64, u64)> {
    let o = s.find('(')?;
    let c = s[o..].find(')')? + o;
    let (a, b) = s[o + 1..c].split_once('/')?;
    let (a, b) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    (b > 0 && a <= b).then_some((a, b))
}

fn run(title: Option<String>, cmd: Vec<String>) -> i32 {
    let id = new_id();
    // the title: -t, else the command without sudo / env
    let shown = cmd.iter().skip_while(|a| *a == "sudo" || *a == "env" || a.starts_with('-')).take(2).cloned().collect::<Vec<_>>().join(" ");
    let title = title.unwrap_or_else(|| if shown.is_empty() { cmd.join(" ") } else { shown });
    // a pty with this terminal's size, so the command draws its bars as it would here
    let (mut master, mut slave) = (0, 0);
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    unsafe {
        if libc::ioctl(1, libc::TIOCGWINSZ, &mut ws) != 0 { ws.ws_col = 100; ws.ws_row = 30 }
        if libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null(), &ws) != 0 {
            eprintln!("prog: no pty");
            return 1;
        }
    }
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    let sfd = slave.as_raw_fd();
    let mut c = std::process::Command::new(&cmd[0]);
    c.args(&cmd[1..]);
    unsafe {
        c.pre_exec(move || {
            libc::setsid();
            libc::ioctl(sfd, libc::TIOCSCTTY as _, 0);
            for fd in 0..3 { libc::dup2(sfd, fd); }
            Ok(())
        });
    }
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => { eprintln!("prog: {}: {e}", cmd[0]); return 127 }
    };
    drop(slave);
    let mut v = job(&id, &title, "", &cmd.join(" "), Some(child.id()));
    write(&id, &v);
    let log_path = dir().join(format!("{id}.log"));
    let mut log = std::fs::File::create(&log_path).ok();
    let raw = Raw::on();

    // keys typed here go to the command (a sudo password, a [Y/n])
    let mfd = master.as_raw_fd();
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        let mut stdin = std::io::stdin();
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 { break }
            unsafe { libc::write(mfd, buf.as_ptr() as *const _, n); }
        }
    });

    let mut out = std::io::stdout();
    let mut from = unsafe { std::fs::File::from_raw_fd(libc::dup(mfd)) };
    let mut seen = Seen::default();
    let mut last = Instant::now() - Duration::from_secs(1);
    let mut buf = [0u8; 8192];
    loop {
        let n = match from.read(&mut buf) { Ok(0) | Err(_) => break, Ok(n) => n };
        let _ = out.write_all(&buf[..n]);
        let _ = out.flush();
        if let Some(l) = log.as_mut() { let _ = l.write_all(&buf[..n]); }
        seen.feed(&buf[..n]);
        // ten updates a second at most
        if last.elapsed() >= Duration::from_millis(100) {
            last = Instant::now();
            fill(&mut v, &seen);
            write(&id, &v);
        }
    }
    let status = child.wait().ok();
    drop(raw);
    let code = status.and_then(|s| s.code()).unwrap_or(1);
    fill(&mut v, &seen);
    v["state"] = json!(if code == 0 { "done" } else { "failed" });
    v["code"] = json!(code);
    if code == 0 { v["percent"] = json!(100) }
    write(&id, &v);
    code
}

fn fill(v: &mut Value, s: &Seen) {
    // a (n/m) count gives the overall part; the % is the item's own, used when there is no count
    let p = match (s.count, s.percent) {
        (Some((a, b)), Some(p)) if b > 1 => ((a.saturating_sub(1)) as f64 + p / 100.0) / b as f64 * 100.0,
        (Some((a, b)), None) => a as f64 / b as f64 * 100.0,
        (_, Some(p)) => p,
        _ => -1.0,
    };
    v["percent"] = json!((p * 10.0).round() / 10.0);
    v["count"] = s.count.map_or(Value::Null, |(a, b)| json!([a, b]));
    v["step"] = json!(s.step);
    v["updated"] = json!(now());
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_bars() {
        let mut s = Seen::default();
        s.feed(b"\x1b[1m(2/4) upgrading foo\x1b[0m      [######----]  60%\r");
        assert_eq!(s.count, Some((2, 4)));
        assert_eq!(s.percent, Some(60.0));
        assert_eq!(s.step, "(2/4) upgrading foo");
        let mut v = json!({});
        fill(&mut v, &s);
        assert_eq!(v["percent"], json!(40.0));
    }
}
