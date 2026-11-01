use std::process::Command;

const NO_CONFIG: &str = "/nonexistent/sharky-test";

const MALFORMED: &[&str] = &[
    "case a in a) :;;& esac",
    "case a in a) :;& esac",
    "case a in esac",
    "case a in a) ;; esac",
    "for ; do :; done",
    "for ((i=0;i<2;i++)); do :; done",
    "for ((i=0)); do :; done",
    "x=abc; printf '%s' \"${x:1:9223372036854775807}\"",
    "printf $(( ) 1 ))",
    "x=abc; printf '%s' \"${x:1/0}\"",
    "for in a; do :; done",
    "while; do :; done",
    "if then fi",
    "function ;",
    "function f",
    "f()",
    "{",
    "}",
    "(",
    ")",
    "$(",
    "$((",
    "${",
    "[[",
    "[[ ]]",
    "((",
    "(( ))",
    "cat <<",
    "cat <<EOF",
    "EOF",
    ">",
    "2>&1",
    "|",
    "&&",
    ";;",
    "!",
    "[[ -eq 1 ]]",
    "[[ 1 -eq ]]",
    "esac",
    "fi",
    "done",
    "a=(",
    "a[",
    "&",
];

#[test]
fn malformed_input_never_panics() {
    for source in MALFORMED {
        let out = Command::new(env!("CARGO_BIN_EXE_sharky"))
            .args(["--no-config", "-c", source])
            .env("SHARKY_CONFIG", NO_CONFIG)
            .output()
            .expect("failed to run sharky");

        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("panicked"),
            "{source:?} panicked: {stderr}"
        );

        let code = out.status.code();
        assert!(
            code.is_some(),
            "{source:?} was killed by a signal: {:?}",
            out.status
        );
        assert!(
            !matches!(code, Some(101) | Some(134)),
            "{source:?} aborted with {code:?}: {stderr}"
        );
    }
}
