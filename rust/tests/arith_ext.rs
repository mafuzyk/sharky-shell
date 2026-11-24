//! Extended arithmetic-expansion coverage matching bash semantics.
//!
//! Each case is expressed as a full script run through the `sharky` binary so
//! that both the expansion result and its effect on exit status are verified.

use std::process::{Command, Output};

const NO_CONFIG: &str = "/nonexistent/sharky-test";

fn sharky(script: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_sharky"))
        .args(["-c", script])
        .env("SHARKY_CONFIG", NO_CONFIG)
        .output()
        .expect("failed to run sharky")
}

fn value(expression: &str) -> String {
    let out = sharky(&format!("printf '%s' \"$(( {expression }))\""));
    assert!(
        out.status.success(),
        "expression {expression:?} failed: {out:?}"
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn expect_value(expression: &str, expected: &str) {
    assert_eq!(value(expression), expected, "expression {expression:?}");
}

fn expect_error(script: &str, token: &str) {
    let out = sharky(script);
    assert_eq!(out.status.code(), Some(1), "script {script:?}: {out:?}");
    assert!(out.stdout.is_empty(), "script {script:?}: {out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(token),
        "stderr for {script:?} does not mention {token:?}: {stderr}"
    );
}

#[test]
fn hexadecimal_literals() {
    for (expression, expected) in [
        ("0x10", "16"),
        ("0X1f", "31"),
        ("0xff + 1", "256"),
        ("0x7fffffffffffffff", "9223372036854775807"),
    ] {
        expect_value(expression, expected);
    }
}

#[test]
fn octal_literals() {
    for (expression, expected) in [("010", "8"), ("010 + 1", "9"), ("007", "7"), ("0", "0")] {
        expect_value(expression, expected);
    }
}

#[test]
fn invalid_octal_literal_is_an_error() {
    expect_error("printf '%s' \"$(( 08 ))\"", "08");
    expect_error("printf '%s' \"$(( 09 ))\"", "09");
}

#[test]
fn binary_literal_prefix_is_an_error() {
    expect_error("printf '%s' \"$(( 0b101 ))\"", "0b101");
}

#[test]
fn base_number_literals() {
    for (expression, expected) in [("16#ff", "255"), ("2#101", "5"), ("16#ff + 1", "256")] {
        expect_value(expression, expected);
    }
}

#[test]
fn invalid_base_number_literal_is_an_error() {
    expect_error("printf '%s' \"$(( 16#fg ))\"", "16#fg");
    expect_error("printf '%s' \"$(( 16# ))\"", "16#");
}

#[test]
fn shift_operators() {
    for (expression, expected) in [
        ("1 << 3", "8"),
        ("16 >> 2", "4"),
        ("1 + 2 << 3", "24"),
        ("1 << 2 + 1", "8"),
    ] {
        expect_value(expression, expected);
    }
}

#[test]
fn bitwise_operators() {
    for (expression, expected) in [
        ("5 & 3", "1"),
        ("5 | 3", "7"),
        ("5 ^ 3", "6"),
        ("~5", "-6"),
        ("~0 & 0xff", "255"),
        ("1 | 2 & 3", "3"),
        ("1 ^ 3 & 2", "3"),
        ("4 & 3 == 3", "0"),
    ] {
        expect_value(expression, expected);
    }
}

#[test]
fn logical_operators_are_not_confused_with_bitwise() {
    for (expression, expected) in [("1 && 0", "0"), ("1 || 0", "1"), ("2 && 3", "1")] {
        expect_value(expression, expected);
    }
}

#[test]
fn ternary_operator() {
    for (expression, expected) in [
        ("1 ? 2 : 3", "2"),
        ("0 ? 2 : 3", "3"),
        ("1 ? 2 : 3 ? 4 : 5", "2"),
        ("0 ? 2 : 3 ? 4 : 5", "4"),
    ] {
        expect_value(expression, expected);
    }
}

#[test]
fn ternary_is_lazy() {
    let out = sharky("x=9; printf '%s %s' \"$(( 0 ? x=1 : 2 ))\" \"$x\"");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "2 9");

    let out = sharky("x=9; printf '%s %s' \"$(( 1 ? x=1 : 2 ))\" \"$x\"");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1 1");

    expect_value("0 ? 1/0 : 5", "5");
    expect_value("1 ? 5 : 1/0", "5");
}

#[test]
fn ternary_interacts_with_assignment() {
    let out = sharky("x=0; printf '%s %s' \"$(( x = 1 ? 2 : 3 ))\" \"$x\"");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "2 2");

    let out = sharky("x=0; printf '%s %s' \"$(( x = 0 ? 2 : 3 ))\" \"$x\"");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "3 3");
}

#[test]
fn compound_bitwise_assignments() {
    for (script, expected) in [
        ("x=1; ((x <<= 3)); printf '%s' \"$x\"", "8"),
        ("x=8; ((x >>= 2)); printf '%s' \"$x\"", "2"),
        ("x=7; ((x &= 3)); printf '%s' \"$x\"", "3"),
        ("x=1; ((x |= 2)); printf '%s' \"$x\"", "3"),
        ("x=5; ((x ^= 3)); printf '%s' \"$x\"", "6"),
    ] {
        let out = sharky(script);
        assert_eq!(String::from_utf8_lossy(&out.stdout), expected, "{script}");
    }
}

#[test]
fn arithmetic_error_aborts_expansion_without_partial_output() {
    expect_error("printf '%s' before \"$(( 08 ))\" after", "08");
}

#[test]
fn regression_existing_arithmetic_features() {
    for (expression, expected) in [
        ("2 ** 8", "256"),
        ("2 ** 3 ** 2", "512"),
        ("10 % 3", "1"),
        ("10 / 3", "3"),
        ("3 < 4", "1"),
        ("3 > 4", "0"),
        ("3 <= 3", "1"),
        ("3 >= 4", "0"),
        ("3 == 3", "1"),
        ("3 != 3", "0"),
    ] {
        expect_value(expression, expected);
    }

    let out = sharky("i=1; printf '%s %s' \"$((i++))\" \"$i\"");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1 2");

    let out = sharky("i=1; printf '%s %s' \"$((++i))\" \"$i\"");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "2 2");
}
