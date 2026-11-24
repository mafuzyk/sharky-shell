use std::process::{Command, Output};

const NO_CONFIG: &str = "/nonexistent/sharky-test";

fn sharky(source: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["--no-config", "-c", source])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .output()
        .expect("failed to run sharky")
}

fn assert_output(source: &str, expected: &str) {
    let out = sharky(source);
    assert_eq!(
        out.status.code(),
        Some(0),
        "source: {source:?}; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        expected,
        "source: {source:?}"
    );
}

fn assert_rejected(source: &str) {
    let out = sharky(source);
    assert_eq!(
        out.status.code(),
        Some(2),
        "source: {source:?}; stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("unexpected LParen"),
        "source: {source:?}; stderr: {stderr}"
    );
}

#[test]
fn local_with_flag_accepts_indexed_array_literal() {
    assert_output(
        r#"f() { local -a a=(1 2); printf '%s' "${a[1]}"; }; f"#,
        "2",
    );
}

#[test]
fn local_without_flag_accepts_indexed_array_literal() {
    assert_output(
        r#"f() { local a=(x y); printf '%s' "${a[0]}${a[1]}"; }; f"#,
        "xy",
    );
}

#[test]
fn local_array_literal_appends_to_the_same_frame() {
    assert_output(
        r#"f() { local a+=(1); local a+=(2); printf '%s%s' "${a[0]}" "${a[1]}"; }; f"#,
        "12",
    );
}

#[test]
fn readonly_with_flag_accepts_indexed_array_literal() {
    assert_output(
        r#"readonly -a a=(p q); printf '%s' "${a[1]}""#,
        "q",
    );
}

#[test]
fn typeset_accepts_indexed_array_literal() {
    assert_output(r#"typeset a=(m n); printf '%s' "${a[0]}${a[1]}""#, "mn");
}

#[test]
fn declare_with_flag_still_accepts_indexed_array_literal() {
    assert_output(
        r#"declare -a a=(one two); printf '%s' "${a[1]}""#,
        "two",
    );
}

#[test]
fn declare_with_flag_still_accepts_associative_array_literal() {
    assert_output(
        r#"declare -A m=([k]=v); printf '%s' "${m[k]}""#,
        "v",
    );
}

#[test]
fn arbitrary_command_with_flag_rejects_array_literal() {
    assert_rejected("foo -a a=(1 2)");
}

#[test]
fn arbitrary_command_rejects_array_literal() {
    assert_rejected("foo a=(1 2)");
}

// A `[subscript]=value` element of an array literal selects the index bash
// uses; a plain element takes the next index after the previous one.

#[test]
fn array_literal_honors_explicit_indexes() {
    assert_output(
        r#"a=([3]=x [7]=y); printf 'k[%s]' "${!a[@]}"; printf ' n=%s' "${#a[@]}""#,
        "k[3]k[7] n=2",
    );
}

#[test]
fn array_literal_continues_after_an_explicit_index() {
    assert_output(
        r#"a=([3]=x y); printf 'k[%s]' "${!a[@]}"; printf ' v[%s]' "${a[@]}""#,
        "k[3]k[4] v[x] v[y]",
    );
}

#[test]
fn array_literal_plain_values_take_the_next_index() {
    assert_output(
        r#"a=(p [3]=x q); printf 'k[%s]' "${!a[@]}"; printf ' v[%s]' "${a[@]}""#,
        "k[0]k[3]k[4] v[p] v[x] v[q]",
    );
}

#[test]
fn array_literal_last_value_wins_for_a_repeated_index() {
    assert_output(
        r#"a=([3]=x [7]=y [3]=z); printf 'k[%s]' "${!a[@]}"; printf ' v=%s' "${a[3]}""#,
        "k[3]k[7] v=z",
    );
}

#[test]
fn array_literal_expands_an_arithmetic_subscript() {
    assert_output(
        r#"i=5; a=([3]=x [$i]=y); printf 'k[%s]' "${!a[@]}""#,
        "k[3]k[5]",
    );
}

#[test]
fn array_literal_keeps_an_empty_indexed_value() {
    assert_output(
        r#"a=([3]=); printf 'k[%s] v[%s]' "${!a[@]}" "${a[3]}""#,
        "k[3] v[]",
    );
}

#[test]
fn array_literal_append_honors_explicit_indexes() {
    assert_output(
        r#"a=([3]=x); a+=([5]=z); printf 'k[%s]' "${!a[@]}""#,
        "k[3]k[5]",
    );
}

#[test]
fn array_literal_append_continues_after_the_last_index() {
    assert_output(
        r#"a=([3]=x); a+=(w); printf 'k[%s]' "${!a[@]}""#,
        "k[3]k[4]",
    );
}

#[test]
fn array_literal_append_can_overwrite_an_existing_index() {
    assert_output(
        r#"a=(p q); a+=([1]=r); printf 'k[%s]' "${!a[@]}"; printf ' v[%s]' "${a[@]}""#,
        "k[0]k[1] v[p] v[r]",
    );
}

#[test]
fn local_array_literal_honors_explicit_indexes() {
    assert_output(
        r#"f() { local a=([3]=x [7]=y); printf 'k[%s]' "${!a[@]}"; }; f"#,
        "k[3]k[7]",
    );
}

#[test]
fn declare_array_literal_honors_explicit_indexes() {
    assert_output(
        r#"declare a=([3]=x [7]=y); printf 'k[%s]' "${!a[@]}""#,
        "k[3]k[7]",
    );
}

#[test]
fn readonly_array_literal_honors_explicit_indexes() {
    assert_output(
        r#"readonly a=([3]=x [7]=y); printf 'k[%s]' "${!a[@]}""#,
        "k[3]k[7]",
    );
}

#[test]
fn quoted_subscript_stays_a_literal_value() {
    assert_output(
        r#"a=("[3]=x"); printf 'k[%s] v[%s]' "${!a[@]}" "${a[0]}""#,
        "k[0] v[[3]=x]",
    );
}

#[test]
fn indexed_value_keeps_its_spaces() {
    assert_output(
        r#"a=([3]="x y"); printf 'v[%s]' "${a[3]}""#,
        "v[x y]",
    );
}
