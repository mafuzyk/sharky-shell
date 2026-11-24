use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use nix::libc;
use nix::pty::openpty;

const NO_CONFIG: &str = "/nonexistent/sharky-test";

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(dirs: &[&str]) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("sharky-v402-ux-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for d in dirs {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        Self { dir }
    }

    fn run(&self, script: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sharky"))
            .args(["--no-config", "-c", script])
            .current_dir(&self.dir)
            .env("SHARKY_CONFIG", NO_CONFIG)
            .output()
            .expect("run sharky")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn read_until(fd: RawFd, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut collected = String::new();
    let mut buffer = [0u8; 1024];

    while Instant::now() < deadline {
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: valid pollfd and stack buffer.
        let ready = unsafe { libc::poll(&mut pollfd, 1, 50) };
        if ready > 0 {
            let count = unsafe { libc::read(fd, buffer.as_mut_ptr() as *mut _, buffer.len()) };
            if count > 0 {
                collected.push_str(&String::from_utf8_lossy(&buffer[..count as usize]));
                if collected.contains(needle) {
                    return collected;
                }
            }
        }
    }
    collected
}

fn spawn_pty(cwd: &std::path::Path) -> (std::process::Child, std::os::fd::OwnedFd) {
    let pty = openpty(None, None).expect("openpty");
    let stdin = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stdout = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };
    let stderr = unsafe { Stdio::from_raw_fd(pty.slave.try_clone().unwrap().into_raw_fd()) };

    let child = Command::new(env!("CARGO_BIN_EXE_sharky"))
        .arg("--no-config")
        .stdin(stdin)
        .stdout(stdout)
        .stderr(stderr)
        .current_dir(cwd)
        .env("SHARKY_CONFIG", NO_CONFIG)
        .env("TERM", "xterm-256color")
        .env("HOME", "/tmp")
        .spawn()
        .expect("spawn sharky pty");

    (child, pty.master)
}

fn finish_pty(mut child: std::process::Child, writer: &mut std::fs::File) {
    let _ = writer.write_all(b"\x03");
    std::thread::sleep(Duration::from_millis(50));
    let _ = writer.write_all(b"exit 0\r");
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("pty child hung instead of exiting");
}

#[test]
fn chpwd_hook_and_functions_array_fire_and_preserve_status() {
    let sandbox = Sandbox::new(&["sub1", "sub2"]);
    let out = sandbox.run(
        "chpwd() { echo \"hook:$PWD\"; return 7; }\nextra_ch() { echo \"extra:$PWD\"; }\nchpwd_functions=(extra_ch)\n(exit 42); rc=$?\ncd sub1\necho \"after=$?\"\ncd ../sub2\n",
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("hook:"), "{stdout}");
    assert!(stdout.contains("sub1\nextra:"), "{stdout}");
    assert!(stdout.contains("after=0\n"), "{stdout}");

    let out2 = sandbox.run(
        "chpwd() { echo \"pwd=$PWD\"; return 9; }\nfalse\necho \"before_cd\" >/dev/null\ncd sub1\n",
    );
    assert_eq!(out2.status.code(), Some(0));
}

#[test]
fn precmd_and_preexec_fire_in_interactive_pty() {
    let sandbox = Sandbox::new(&[]);
    let (child, master) = spawn_pty(&sandbox.dir);
    let fd = master.as_raw_fd();
    let mut writer = unsafe { std::fs::File::from_raw_fd(master.try_clone().unwrap().into_raw_fd()) };

    let _ = read_until(fd, "❯", Duration::from_secs(5));
    writer
        .write_all(b"precmd() { echo \"PC_\"\"OK\"; }; preexec() { echo \"PE:$1\"; }\r")
        .unwrap();
    let out = read_until(fd, "PC_OK", Duration::from_secs(3));
    assert!(out.contains("PC_OK"), "{out:?}");
    let _ = read_until(fd, "❯", Duration::from_secs(3));

    writer.write_all(b"echo \"BODY_\"\"OK\"\r").unwrap();
    let out2 = read_until(fd, "BODY_OK", Duration::from_secs(3));
    assert!(out2.contains("PE:echo \"BODY_\"\"OK\""), "{out2:?}");

    finish_pty(child, &mut writer);
}

#[test]
fn complete_and_compgen_builtins_and_tab_menu_descriptions() {
    let sandbox = Sandbox::new(&[]);
    let out = sandbox.run(
        "compgen -W \"build test deploy\" d\ncomplete -W \"start::Begin stop::Halt\" myctl\ncomplete -p myctl\nmyfunc() { COMPREPLY=(one two); }\ncomplete -F myfunc fnctl\ncomplete -r myctl\ncomplete -p\n",
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("deploy\n"), "{text}");
    assert_eq!(text.lines().filter(|l| l.contains("myctl")).count(), 1, "{text}");
    assert!(text.contains("complete -W 'start::Begin stop::Halt' myctl\n"), "{text}");
    assert!(text.contains("complete -F myfunc fnctl\n"), "{text}");

    let (child, master) = spawn_pty(&sandbox.dir);
    let fd = master.as_raw_fd();
    let mut writer = unsafe { std::fs::File::from_raw_fd(master.try_clone().unwrap().into_raw_fd()) };
    let _ = read_until(fd, "❯", Duration::from_secs(5));
    writer
        .write_all(b"complete -W \"alpha::First beta::Second\" toolx\r")
        .unwrap();
    let _ = read_until(fd, "❯", Duration::from_secs(3));
    writer.write_all(b"toolx \t").unwrap();
    let menu = read_until(fd, "Second", Duration::from_secs(3));
    assert!(menu.contains("alpha") && menu.contains("First"), "{menu:?}");
    assert!(menu.contains("beta") && menu.contains("Second"), "{menu:?}");

    finish_pty(child, &mut writer);
}

#[test]
fn dir_history_prevd_nextd_and_alt_arrows_in_pty() {
    let sandbox = Sandbox::new(&["dirA", "dirB"]);
    let out = sandbox.run(
        "cd dirA\ncd ../dirB\nprevd\npwd\nprevd\npwd\nnextd\npwd\n",
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let lines: Vec<&str> = std::str::from_utf8(&out.stdout).unwrap().lines().collect();
    assert!(lines[0].ends_with("/dirA"), "{lines:?}");
    assert!(!lines[1].ends_with("/dirA") && !lines[1].ends_with("/dirB"), "{lines:?}");
    assert!(lines[2].ends_with("/dirA"), "{lines:?}");

    let (child, master) = spawn_pty(&sandbox.dir);
    let fd = master.as_raw_fd();
    let mut writer = unsafe { std::fs::File::from_raw_fd(master.try_clone().unwrap().into_raw_fd()) };
    let _ = read_until(fd, "❯", Duration::from_secs(5));
    writer.write_all(b"cd dirA\r").unwrap();
    let _ = read_until(fd, "❯", Duration::from_secs(3));
    // Alt+Left returns to parent dir and redraws prompt
    writer.write_all(b"\x1b[1;3D").unwrap();
    let _ = read_until(fd, "❯", Duration::from_secs(3));
    writer.write_all(b"printf '%s:%s\\n' BACK \"$PWD\"\r").unwrap();
    let mut back = read_until(fd, "BACK:", Duration::from_secs(3));
    back.push_str(&read_until(fd, "❯", Duration::from_secs(3)));
    assert!(back.contains("BACK:") && !back.contains("/dirA"), "{back:?}");
    // Alt+Right returns forward to dirA and redraws prompt
    writer.write_all(b"\x1b[1;3C").unwrap();
    let _ = read_until(fd, "❯", Duration::from_secs(3));
    writer.write_all(b"printf '%s:%s\\n' FWD \"$PWD\"\r").unwrap();
    let mut fwd = read_until(fd, "FWD:", Duration::from_secs(3));
    fwd.push_str(&read_until(fd, "❯", Duration::from_secs(3)));
    assert!(fwd.contains("/dirA"), "{fwd:?}");

    finish_pty(child, &mut writer);
}

#[test]
fn structured_pipe_builtins_from_to_json_where_select_table() {
    let sandbox = Sandbox::new(&[]);
    let out = sandbox.run(
        "printf '[{\"name\":\"ana\",\"age\":30,\"meta\":{\"role\":\"admin\"}},{\"name\":\"bob\",\"age\":17,\"meta\":{\"role\":\"guest\"}}]\\n' | from-json | where age '>' 20 | select name meta.role\n",
    );
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let s = String::from_utf8_lossy(&out.stdout);
    assert_eq!(s.trim(), r#"[{"name":"ana","meta.role":"admin"}]"#);

    let tbl = sandbox.run(
        "printf '{\"name\":\"ana\",\"score\":10}\\n{\"name\":\"bob\",\"score\":20}\\n' | from-json --jsonl | where name '=~' '^b' | table\n",
    );
    assert_eq!(tbl.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&tbl.stderr));
    let tout = String::from_utf8_lossy(&tbl.stdout);
    assert!(tout.contains("│ name │ score │") && tout.contains("│ bob"), "{tout}");

    let tj = sandbox.run("printf 'one\\ntwo\\n' | to-json\n");
    assert_eq!(tj.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&tj.stdout).trim(), r#"["one","two"]"#);
}
