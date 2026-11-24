use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

const NO_CONFIG: &str = "/nonexistent/sharky-test";

struct Sandbox {
    dir: PathBuf,
}

impl Sandbox {
    fn new(files: &[(&str, &str)]) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("sharky-bash-compat-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in files {
            let full = dir.join(name);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(full, body).unwrap();
        }
        Self { dir }
    }

    fn sharky(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_sharky"))
            .arg("--no-config")
            .args(args)
            .current_dir(&self.dir)
            .env("SHARKY_CONFIG", NO_CONFIG)
            .output()
            .expect("failed to run sharky")
    }

    fn run(&self, script: &str) -> Output {
        self.sharky(&["-c", script])
    }

    fn run_file(&self, name: &str, args: &[&str]) -> Output {
        let path = self.dir.join(name);
        let mut all = vec![path.to_str().expect("utf-8 path")];
        all.extend_from_slice(args);
        self.sharky(&all)
    }

    fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn status(out: &Output) -> i32 {
    out.status.code().unwrap_or(-1)
}

#[test]
fn bash_source_in_script_and_sourced_file_with_set_u() {
    let sandbox = Sandbox::new(&[
        (
            "lib/helper.sh",
            "echo \"lib0=${BASH_SOURCE[0]}\"\necho \"lib1=${BASH_SOURCE[1]}\"\necho \"fn_top=${FUNCNAME[0]:-none}\"\nlib_fn() {\n  echo \"in_fn=${BASH_SOURCE[0]}|${BASH_SOURCE[1]}|${FUNCNAME[0]}|${FUNCNAME[1]}\"\n}\n",
        ),
        (
            "main.sh",
            "#!/usr/bin/env bash\nset -euo pipefail\nHERE=\"$(cd -- \"$(dirname -- \"${BASH_SOURCE[0]}\")\" && pwd)\"\necho \"here=$HERE\"\necho \"main0=${BASH_SOURCE[0]}\"\nsource \"${HERE}/lib/helper.sh\"\nlib_fn\nouter_fn() {\n  source \"${HERE}/lib/helper.sh\"\n}\nouter_fn\necho \"after=${BASH_SOURCE[0]}\"\n",
        ),
    ]);
    let out = sandbox.run_file("main.sh", &[]);
    assert_eq!(status(&out), 0, "stderr: {}", stderr(&out));
    let expected_dir = sandbox.dir().canonicalize().unwrap();
    let main_path = sandbox.dir().join("main.sh");
    let lib_path = expected_dir.join("lib/helper.sh");
    let text = stdout(&out);
    assert!(text.contains(&format!("here={}", expected_dir.display())), "{text}");
    assert!(text.contains(&format!("main0={}", main_path.display())), "{text}");
    assert!(text.contains(&format!("lib0={}", lib_path.display())), "{text}");
    assert!(text.contains(&format!("lib1={}", main_path.display())), "{text}");
    assert!(text.contains("fn_top=none"), "{text}");
    assert!(text.contains("fn_top=source"), "{text}");
    assert!(
        text.contains(&format!(
            "in_fn={}|{}|lib_fn|main",
            lib_path.display(),
            main_path.display()
        )),
        "{text}"
    );
    assert!(text.contains(&format!("after={}", main_path.display())), "{text}");
}

#[test]
fn cd_supports_double_dash_flags_and_dash_prints_pwd() {
    let sandbox = Sandbox::new(&[]);
    let real_dir = sandbox.dir().join("real");
    let link_dir = sandbox.dir().join("link");
    std::fs::create_dir_all(&real_dir).unwrap();
    std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();

    let out = sandbox.run("cd -- real && cd -");
    assert_eq!(status(&out), 0, "stderr: {}", stderr(&out));
    let base = sandbox.dir().canonicalize().unwrap();
    assert_eq!(stdout(&out), format!("{}\n", base.display()));

    let out_p = sandbox.run("cd -P -- link && pwd");
    assert_eq!(status(&out_p), 0, "stderr: {}", stderr(&out_p));
    assert_eq!(stdout(&out_p), format!("{}\n", real_dir.canonicalize().unwrap().display()));

    let out_l = sandbox.run("cd -L -- link && pwd -L && pwd -P");
    assert_eq!(status(&out_l), 0, "stderr: {}", stderr(&out_l));
    assert_eq!(
        stdout(&out_l),
        format!(
            "{}\n{}\n",
            base.join("link").display(),
            real_dir.canonicalize().unwrap().display()
        )
    );

    let out_err = sandbox.run("cd a b");
    assert_eq!(status(&out_err), 1);
    assert!(stderr(&out_err).contains("too many arguments"), "{}", stderr(&out_err));

    let out_bad_flag = sandbox.run("cd -Z real");
    assert_eq!(status(&out_bad_flag), 2);
}

#[test]
fn pwd_supports_l_p_and_double_dash() {
    let sandbox = Sandbox::new(&[]);
    let real_dir = sandbox.dir().join("real");
    let link_dir = sandbox.dir().join("link");
    std::fs::create_dir_all(&real_dir).unwrap();
    std::os::unix::fs::symlink(&real_dir, &link_dir).unwrap();

    let out = sandbox.run("cd -L link && pwd -- && pwd -L && pwd -P");
    assert_eq!(status(&out), 0, "stderr: {}", stderr(&out));
    let base = sandbox.dir().canonicalize().unwrap();
    let real = real_dir.canonicalize().unwrap();
    assert_eq!(
        stdout(&out),
        format!("{}\n{}\n{}\n", base.join("link").display(), base.join("link").display(), real.display())
    );
}

#[test]
fn source_inherits_positional_args_and_supports_double_dash_and_path() {
    let sandbox = Sandbox::new(&[
        ("bin/inpath.sh", "echo \"path:$*\"\n"),
        ("local.sh", "echo \"local:$*\"\n"),
        ("main.sh", "source local.sh\nsource -- ./local.sh x y\necho \"restored:$*\"\nPATH=\"$PWD/bin:$PATH\"\n. inpath.sh\n"),
    ]);
    let out = sandbox.run_file("main.sh", &["one", "two"]);
    assert_eq!(status(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "local:one two\nlocal:x y\nrestored:one two\npath:one two\n"
    );
}

#[test]
fn builtin_bypasses_function_shadowing() {
    let sandbox = Sandbox::new(&[]);
    std::fs::create_dir_all(sandbox.dir().join("sub")).unwrap();
    let out = sandbox.run("cd() { echo shadow; }; cd sub; builtin cd -- sub && basename \"$PWD\"");
    assert_eq!(status(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "shadow\nsub\n");

    let not_bi = sandbox.run("builtin definitely_not_a_builtin");
    assert_eq!(status(&not_bi), 1);
    assert!(stderr(&not_bi).contains("not a shell builtin"), "{}", stderr(&not_bi));
}

#[test]
fn bash_rematch_captures_and_clears_on_mismatch() {
    let sandbox = Sandbox::new(&[]);
    let out = sandbox.run(
        "set -u\nif [[ \"foo-123\" =~ ([a-z]+)-([0-9]+) ]]; then\n  echo \"0=${BASH_REMATCH[0]}|1=${BASH_REMATCH[1]}|2=${BASH_REMATCH[2]}|n=${#BASH_REMATCH[@]}\"\nfi\nif [[ \"abc\" =~ [0-9]+ ]]; then\n  echo bad\nelse\n  echo \"after=${#BASH_REMATCH[@]}\"\nfi\n",
    );
    assert_eq!(status(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "0=foo-123|1=foo|2=123|n=3\nafter=0\n");

    let nocase = sandbox.run(
        "shopt -s nocasematch\n[[ \"FooBar\" =~ ^foo([a-z]+)$ ]] && echo \"matched:${BASH_REMATCH[1]}\"\n[[ \"FooBar\" == foo* ]] && echo glob_ok\n",
    );
    assert_eq!(status(&nocase), 0, "stderr: {}", stderr(&nocase));
    assert_eq!(stdout(&nocase), "matched:Bar\nglob_ok\n");
}

#[test]
fn dynamic_vars_random_seconds_bashpid_uid_euid() {
    let sandbox = Sandbox::new(&[]);
    let out = sandbox.run(
        "set -u\nRANDOM=42; r1=$RANDOM; r2=$RANDOM\nRANDOM=42; r3=$RANDOM\necho \"eq=$((r1 == r3)) range=$((r1 >= 0 && r1 < 32768 && r2 >= 0 && r2 < 32768))\"\nSECONDS=5; s=$SECONDS\necho \"sec=$((s >= 5 && s < 10))\"\nouter=\"$$:$BASHPID\"\ninner=$(echo \"$$:$BASHPID\")\n[[ \"$outer\" != \"$inner\" ]] && echo \"subshell_diff=1\"\necho \"uid=$UID euid=$EUID\"\n",
    );
    let text = stdout(&out);
    assert!(text.contains("eq=1 range=1\n"), "text: {text} err: {}", stderr(&out));
    assert!(text.contains("sec=1\n"), "{text}");
    assert!(text.contains("subshell_diff=1\n"), "{text}");
    assert!(text.contains("uid=") && text.contains("euid="), "{text}");

    let ro = sandbox.run("UID=999");
    assert_ne!(status(&ro), 0);
    assert!(stderr(&ro).contains("readonly"), "{}", stderr(&ro));
}

#[test]
fn shopt_nullglob_dotglob_failglob_nocasematch_q_p() {
    let sandbox = Sandbox::new(&[(".hidden", ""), ("visible.txt", "")]);
    let out = sandbox.run(
        "shopt -s nullglob\necho \"n=(\" *.nope \")\"\nshopt -q nullglob && echo q_on\nshopt -u nullglob\nshopt -q nullglob || echo q_off\nshopt -p nullglob\nshopt -s dotglob\necho \"d=(*)\" *\n",
    );
    assert_eq!(status(&out), 0, "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("n=( )\n"), "{text}");
    assert!(text.contains("q_on\nq_off\nshopt -u nullglob\n"), "{text}");
    assert!(text.contains(".hidden") && text.contains("visible.txt"), "{text}");

    let fg = sandbox.run("shopt -s failglob; echo *.missing");
    assert_ne!(status(&fg), 0);
    assert!(stderr(&fg).contains("no match"), "{}", stderr(&fg));
}

#[test]
fn cli_dash_c_positional_args_and_short_options() {
    let sandbox = Sandbox::new(&[("argscript.sh", "printf '%s|%s' \"$0\" \"$1\"\n")]);
    let out = sandbox.sharky(&["-c", "printf '%s|%s|%s' \"$0\" \"$1\" \"$2\"", "my0", "a", "b"]);
    assert_eq!(status(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "my0|a|b");

    let out_flags = sandbox.sharky(&["-euo", "pipefail", "-c", "false | true; echo never"]);
    assert_eq!(status(&out_flags), 1);
    assert_eq!(stdout(&out_flags), "");

    let out_dd = sandbox.sharky(&["--", sandbox.dir().join("argscript.sh").to_str().unwrap(), "p1"]);
    assert_eq!(status(&out_dd), 0, "stderr: {}", stderr(&out_dd));
    assert!(stdout(&out_dd).ends_with("|p1"), "{}", stdout(&out_dd));
}

#[test]
fn lineno_tracks_multiline_script_and_functions() {
    let sandbox = Sandbox::new(&[(
        "lines.sh",
        "echo \"L1=$LINENO\"\n# comment\necho \"L3=$LINENO\"\nf() {\n  echo \"fn=$LINENO\"\n  echo \"bl=${BASH_LINENO[0]}\"\n}\nf\n",
    )]);
    let out = sandbox.run_file("lines.sh", &[]);
    assert_eq!(status(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "L1=1\nL3=3\nfn=5\nbl=8\n");
}

#[test]
fn caller_prints_line_function_and_source() {
    let sandbox = Sandbox::new(&[(
        "call.sh",
        "inner() {\n  caller 0\n  caller 1\n}\nouter() {\n  inner\n}\nouter\ncaller 0 || echo \"top_rc=$?\"\n",
    )]);
    let out = sandbox.run_file("call.sh", &[]);
    assert_eq!(status(&out), 0, "stderr: {}", stderr(&out));
    let call_path = sandbox.dir().join("call.sh");
    let text = stdout(&out);
    assert!(
        text.contains(&format!("6 outer {}\n", call_path.display())),
        "{text}"
    );
    assert!(
        text.contains(&format!("8 main {}\n", call_path.display())),
        "{text}"
    );
    assert!(text.contains("top_rc=1\n"), "{text}");
}

#[test]
fn trap_err_and_return_fire_with_lineno_and_status() {
    let sandbox = Sandbox::new(&[]);
    let out = sandbox.run(
        "trap 'echo \"err:$?:$LINENO\"' ERR\nif false; then :; fi\nfalse || true\nfalse\necho after\n",
    );
    assert_eq!(status(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "err:1:4\nafter\n");

    let ret = sandbox.run(
        "set -T\ntrap 'echo \"ret:$?\"' RETURN\nf() { return 7; }\nf\necho done\n",
    );
    assert_eq!(status(&ret), 0, "stderr: {}", stderr(&ret));
    assert_eq!(stdout(&ret), "ret:7\ndone\n");
}

#[test]
fn syntax_error_reports_line_column_and_caret() {
    let sandbox = Sandbox::new(&[]);
    let out = sandbox.sharky(&["--check", "-c", "echo ok\n    done"]);
    assert_eq!(status(&out), 2);
    let err = stderr(&out);
    assert!(err.contains(":2:5:"), "missing line:col in {err:?}");
    assert!(err.contains("    done") && err.contains('^'), "missing caret in {err:?}");
}

#[test]
fn extglob_lexer_scans_nested_patterns_as_single_word() {
    let sandbox = Sandbox::new(&[
        ("a.txt", ""),
        ("b.md", ""),
        ("keep.rs", ""),
        ("foo", ""),
        ("bar", ""),
        ("abb", ""),
    ]);
    let out = sandbox.run(
        "shopt -s extglob\necho \"not=$(echo !(*.txt|*.md))\"\necho \"at=$(echo @(foo|bar))\"\necho \"plus=$(echo +(a|b))\"\n",
    );
    assert_eq!(status(&out), 0, "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("not=abb bar foo keep.rs\n"), "{text}");
    assert!(text.contains("at=bar foo\n"), "{text}");
    assert!(text.contains("plus=abb\n"), "{text}");
}

#[test]
fn extglob_works_in_case_double_bracket_and_param_sub() {
    let sandbox = Sandbox::new(&[]);
    let out = sandbox.run(
        "shopt -s extglob\ncase \"abbc\" in\n  @(x|+(a|b)c)) echo case_ok;;\nesac\n[[ \"foo123\" == @(foo|bar)+([0-9]) ]] && echo db_ok\ns=\"foo12bar34\"\necho \"sub=${s//@(foo|bar)/X}\"\n",
    );
    assert_eq!(status(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "case_ok\ndb_ok\nsub=X12X34\n");
}

#[test]
fn extglob_respects_shopt_toggle() {
    let sandbox = Sandbox::new(&[("foo", ""), ("bar", "")]);
    let out = sandbox.run(
        "shopt -u extglob\necho \"off=$(echo @(foo|bar))\"\nshopt -s nullglob\necho \"off_ng=$(echo @(foo|bar))\"\nshopt -u nullglob\nshopt -s extglob\necho \"on=$(echo @(foo|bar))\"\n",
    );
    assert_eq!(status(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "off=@(foo|bar)\noff_ng=@(foo|bar)\non=bar foo\n");
}

#[test]
fn declare_nameref_read_write_local_and_array() {
    let sandbox = Sandbox::new(&[]);
    let out = sandbox.run(
        "x=hello\ndeclare -n ref=x\necho \"read=$ref\"\nref=world\necho \"x=$x\"\narr=(a b c)\ndeclare -n aref=arr\necho \"arr1=${aref[1]}\"\nmut() { local -n target=$1; target=updated; }\nmut x\necho \"after_fn=$x\"\nunset -n ref\necho \"still_x=$x\"\n",
    );
    assert_eq!(status(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "read=hello\nx=world\narr1=b\nafter_fn=updated\nstill_x=updated\n"
    );
}

#[test]
fn declare_nameref_detects_cycles() {
    let sandbox = Sandbox::new(&[]);
    let out = sandbox.run("declare -n a=b; declare -n b=a; echo \"v=${a:-}\"");
    assert_ne!(status(&out), 0);
    assert!(stderr(&out).contains("circular name reference"), "{}", stderr(&out));
}

#[test]
fn declare_integer_lower_upper_attributes() {
    let sandbox = Sandbox::new(&[]);
    let out = sandbox.run(
        "declare -i n=2+3\nn+=4\necho \"n=$n\"\ndeclare -l lo=HeLLo\necho \"lo=$lo\"\ndeclare -u up=HeLLo\necho \"up=$up\"\ndeclare -p n lo up\n",
    );
    assert_eq!(status(&out), 0, "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("n=9\n"), "{text}");
    assert!(text.contains("lo=hello\n"), "{text}");
    assert!(text.contains("up=HELLO\n"), "{text}");
    assert!(text.contains("declare -i n=\"9\"\n"), "{text}");
    assert!(text.contains("declare -l lo=\"hello\"\n"), "{text}");
    assert!(text.contains("declare -u up=\"HELLO\"\n"), "{text}");
}

#[test]
fn param_transform_operators() {
    let sandbox = Sandbox::new(&[]);
    let out = sandbox.run(
        "v=\"a 'b'\"\necho \"Q=${v@Q}\"\nw=\"helloWorld\"\necho \"U=${w@U}|u=${w@u}|L=${w@L}\"\ndeclare -i num=7\necho \"a=${num@a}|A=${num@A}\"\n",
    );
    assert_eq!(status(&out), 0, "stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("Q='a '\\''b'\\'''\n"), "{text}");
    assert!(text.contains("U=HELLOWORLD|u=HelloWorld|L=helloworld\n"), "{text}");
    assert!(text.contains("a=i|A=declare -i num='7'\n"), "{text}");
}

#[test]
fn coproc_default_roundtrip() {
    let sandbox = Sandbox::new(&[]);
    let out = sandbox.run(
        "coproc { read -r msg; echo \"reply:$msg\"; }\necho \"hello\" >&${COPROC[1]}\nread -r out <&${COPROC[0]}\neval \"exec ${COPROC[1]}>&-\"\nwait \"$COPROC_PID\"\necho \"out=$out\"\n",
    );
    assert_eq!(status(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "out=reply:hello\n");
}

#[test]
fn coproc_named_roundtrip() {
    let sandbox = Sandbox::new(&[]);
    let out = sandbox.run(
        "coproc MYPROC { while IFS= read -r line; do echo \"got:$line\"; done; }\nprintf \"one\\ntwo\\n\" >&${MYPROC[1]}\neval \"exec ${MYPROC[1]}>&-\"\nIFS= read -r a <&${MYPROC[0]}\nIFS= read -r b <&${MYPROC[0]}\nwait \"$MYPROC_PID\"\necho \"$a|$b\"\n",
    );
    assert_eq!(status(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "got:one|got:two\n");
}

#[test]
fn coproc_simple_command() {
    let sandbox = Sandbox::new(&[]);
    let out = sandbox.run(
        "coproc tr a-z A-Z\ntest -n \"$COPROC_PID\" && echo \"pid_set\"\necho \"sharky\" >&${COPROC[1]}\neval \"exec ${COPROC[1]}>&-\"\nread -r upper <&${COPROC[0]}\nwait \"$COPROC_PID\"\necho \"upper=$upper\"\n",
    );
    assert_eq!(status(&out), 0, "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "pid_set\nupper=SHARKY\n");
}
