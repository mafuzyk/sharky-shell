//! Frozen corpus for the agent protocol foundation (`src/agent/json.rs`).
//!
//! Every case here is a contract, not an implementation detail: the request
//! parser, the base64 codec and the wire encoders must keep passing unchanged
//! once the session driver is built on top of them. Add cases; do not weaken
//! them.

#[path = "../src/agent/json.rs"]
#[allow(dead_code)] // this target exercises a subset of the module
mod json;

// The decoder is compiled against `crate::agent::json` and one supervisor
// constant. This shim supplies both so the strict protocol layer can be
// exercised without pulling the whole session runtime into the target.
mod agent {
    pub(crate) use crate::json as json;

    pub mod supervisor {
        /// Mirrors `agent::supervisor::CANCEL_GRACE_MS`; the session test
        /// `agent_session` is what pins the real value on the wire.
        pub(crate) const CANCEL_GRACE_MS: i64 = 2000;
    }
}

#[path = "../src/agent/protocol.rs"]
#[allow(dead_code)] // this target exercises a subset of the module
mod protocol;

use json::{
    Captured, Closed, ErrorCode, ErrorResponse, ExecRequest, ExecResult, JsonErrorKind,
    ParallelPolicy, Ready, Request, Response, StateRequest, StateSection, StreamData, Termination,
    Value, base64_decode, base64_encode, parse_line,
};

fn parse(text: &str) -> Value {
    parse_line(text.as_bytes()).expect("expected valid JSON")
}

fn err(text: &str) -> JsonErrorKind {
    parse_line(text.as_bytes())
        .expect_err("expected invalid JSON")
        .kind
}

fn object_keys(value: &Value) -> Vec<&str> {
    match value {
        Value::Object(entries) => entries.iter().map(|(key, _)| key.as_str()).collect(),
        other => panic!("not an object: {other:?}"),
    }
}

fn get<'a>(value: &'a Value, key: &str) -> &'a Value {
    value.get(key).unwrap_or_else(|| panic!("missing {key}"))
}

// ---------------------------------------------------------------- valid values

#[test]
fn parses_scalars() {
    assert_eq!(parse("null"), Value::Null);
    assert_eq!(parse("true"), Value::Bool(true));
    assert_eq!(parse("false"), Value::Bool(false));
    assert_eq!(parse("0"), Value::Int(0));
    assert_eq!(parse("-0"), Value::Int(0));
    assert_eq!(parse("42"), Value::Int(42));
    assert_eq!(parse("-17"), Value::Int(-17));
    assert_eq!(parse("\"\""), Value::String(String::new()));
    assert_eq!(parse("\"abc\""), Value::String("abc".to_string()));
}

#[test]
fn parses_empty_containers() {
    assert_eq!(parse("{}"), Value::Object(Vec::new()));
    assert_eq!(parse("[]"), Value::Array(Vec::new()));
}

#[test]
fn parses_containers_and_preserves_order() {
    let value = parse(r#"{"a":1,"b":[true,null,"x"],"c":{"d":-2}}"#);
    assert_eq!(object_keys(&value), vec!["a", "b", "c"]);
    assert_eq!(get(&value, "a"), &Value::Int(1));
    assert_eq!(
        get(&value, "b"),
        &Value::Array(vec![
            Value::Bool(true),
            Value::Null,
            Value::String("x".to_string())
        ])
    );
    assert_eq!(get(get(&value, "c"), "d"), &Value::Int(-2));
}

#[test]
fn parses_the_rfc_8259_document_example() {
    let text = r#"
{
  "Image": {
    "Width":  800,
    "Height": 600,
    "Title":  "View from 15th Floor",
    "Thumbnail": {
      "Url": "http://www.example.com/image/481989943",
      "Height": 125,
      "Width": "100"
    },
    "Animated": false,
    "IDs": [116, 943, 234, 38793]
  }
}
"#;
    let value = parse(text);
    let image = get(&value, "Image");
    assert_eq!(get(image, "Width"), &Value::Int(800));
    assert_eq!(get(image, "Title"), &Value::String("View from 15th Floor".to_string()));
    assert_eq!(get(get(image, "Thumbnail"), "Width"), &Value::String("100".to_string()));
    assert_eq!(get(image, "Animated"), &Value::Bool(false));
    assert_eq!(
        get(image, "IDs"),
        &Value::Array(vec![
            Value::Int(116),
            Value::Int(943),
            Value::Int(234),
            Value::Int(38793)
        ])
    );
}

#[test]
fn parses_every_string_escape() {
    assert_eq!(
        parse(r#""\"\\\/\b\f\n\r\t""#),
        Value::String("\"\\/\u{08}\u{0c}\n\r\t".to_string())
    );
    assert_eq!(parse(r#""\u0041""#), Value::String("A".to_string()));
    assert_eq!(parse(r#""\u00e9""#), Value::String("é".to_string()));
    // An escaped NUL is legal; only raw control characters are rejected.
    assert_eq!(parse(r#""\u0000""#), Value::String("\0".to_string()));
    // DEL (0x7f) is not a control character under RFC 8259's `unescaped` rule.
    assert_eq!(parse("\"\u{7f}\""), Value::String("\u{7f}".to_string()));
}

#[test]
fn parses_surrogate_pairs() {
    // U+1D11E MUSICAL SYMBOL G CLEF.
    assert_eq!(
        parse(r#""\uD834\uDD1E""#),
        Value::String("\u{1d11e}".to_string())
    );
    assert_eq!(parse(r#""\uD83D\uDE00""#), Value::String("\u{1f600}".to_string()));
}

#[test]
fn accepts_json_whitespace_only_between_tokens() {
    assert_eq!(parse(" \t\r\n 26 \t\r\n "), Value::Int(26));
    assert_eq!(parse("[ 1 , 2 ]"), Value::Array(vec![Value::Int(1), Value::Int(2)]));
    assert_eq!(
        parse("{\n\t\"a\"\r\n:\t1\n}"),
        Value::Object(vec![("a".to_string(), Value::Int(1))])
    );
}

#[test]
fn duplicate_keys_last_wins_and_position_is_first_occurrence() {
    let value = parse(r#"{"a":1,"b":2,"a":3}"#);
    assert_eq!(object_keys(&value), vec!["a", "b"]);
    assert_eq!(get(&value, "a"), &Value::Int(3));

    let value = parse(r#"{"a":1,"a":2,"a":3}"#);
    assert_eq!(value, Value::Object(vec![("a".to_string(), Value::Int(3))]));
}

#[test]
fn parses_ten_thousand_keys_with_duplicates_in_order() {
    const COUNT: usize = 10_000;
    let mut text = String::with_capacity(COUNT * 16 + 32);
    text.push('{');
    for i in 0..COUNT {
        if i > 0 {
            text.push(',');
        }
        text.push_str(&format!("\"key{i:05}\":{i}"));
    }
    // The duplicate keeps the first occurrence's position but the last value.
    text.push_str(",\"key00000\":-1");
    text.push('}');

    let value = parse(&text);
    let Value::Object(entries) = &value else {
        panic!("expected object");
    };
    assert_eq!(entries.len(), COUNT);
    for (position, (key, entry)) in entries.iter().enumerate() {
        assert_eq!(key, &format!("key{position:05}"));
        let expected = if position == 0 { -1 } else { position as i64 };
        assert_eq!(entry, &Value::Int(expected), "value for {key}");
    }
}

#[test]
fn parses_a_one_megabyte_object_line_quickly() {
    use std::time::{Duration, Instant};

    const COUNT: usize = 20_000;
    const PAYLOAD: &str = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
    let mut text = String::with_capacity(1024 * 1024 + 64);
    text.push('{');
    for i in 0..COUNT {
        if i > 0 {
            text.push(',');
        }
        text.push_str(&format!("\"key{i:06}\":\"{PAYLOAD}\""));
    }
    text.push('}');
    assert!(
        text.len() >= 1024 * 1024,
        "guard line should be about 1 MiB, got {} bytes",
        text.len()
    );

    let started = Instant::now();
    let value = parse(&text);
    let elapsed = started.elapsed();
    eprintln!("parsed {} bytes / {COUNT} keys in {elapsed:?}", text.len());
    assert!(
        elapsed < Duration::from_secs(1),
        "parsing a {}-byte line with {COUNT} keys took {elapsed:?}",
        text.len()
    );

    let Value::Object(entries) = value else {
        panic!("expected object");
    };
    assert_eq!(entries.len(), COUNT);
    assert_eq!(entries[0].0, "key000000");
    assert_eq!(entries[COUNT - 1].0, "key019999");
    assert_eq!(entries[0].1, Value::String(PAYLOAD.to_string()));
}

#[test]
fn numbers_are_i64_and_cover_the_full_range() {
    assert_eq!(parse("9223372036854775807"), Value::Int(i64::MAX));
    assert_eq!(parse("-9223372036854775808"), Value::Int(i64::MIN));
    assert_eq!(err("9223372036854775808"), JsonErrorKind::Number);
    assert_eq!(err("-9223372036854775809"), JsonErrorKind::Number);
}

// ------------------------------------------------------------------- depth cap

#[test]
fn depth_32_is_accepted() {
    let text = format!("{}{}", "[".repeat(32), "]".repeat(32));
    assert!(parse_line(text.as_bytes()).is_ok());
}

#[test]
fn depth_33_is_rejected() {
    let text = format!("{}{}", "[".repeat(33), "]".repeat(33));
    assert_eq!(err(&text), JsonErrorKind::Depth);
    let text = format!("{}1{}", "{\"a\":".repeat(33), "}".repeat(33));
    assert_eq!(err(&text), JsonErrorKind::Depth);
}

// -------------------------------------------------------------- number grammar

#[test]
fn rejects_malformed_numbers() {
    // Fractional and exponential forms are rejected outright: no protocol field
    // is a float, and the value type has no float variant.
    assert_eq!(err("01"), JsonErrorKind::Number);
    assert_eq!(err("-01"), JsonErrorKind::Number);
    assert_eq!(err("1."), JsonErrorKind::Number);
    assert_eq!(err("1.0"), JsonErrorKind::Number);
    assert_eq!(err("1e5"), JsonErrorKind::Number);
    assert_eq!(err("1E5"), JsonErrorKind::Number);
    assert_eq!(err("1e-5"), JsonErrorKind::Number);
    assert_eq!(err("+1"), JsonErrorKind::UnexpectedByte);
    assert_eq!(err("-"), JsonErrorKind::UnexpectedEnd);
    assert_eq!(err("--1"), JsonErrorKind::UnexpectedByte);
}

// -------------------------------------------------------------- string grammar

#[test]
fn rejects_raw_control_characters_in_strings() {
    assert_eq!(err("\"a\nb\""), JsonErrorKind::ControlChar);
    assert_eq!(err("\"a\u{0001}b\""), JsonErrorKind::ControlChar);
    assert_eq!(err("\"\u{001f}\""), JsonErrorKind::ControlChar);
}

#[test]
fn rejects_bad_escapes() {
    assert_eq!(err(r#""\x""#), JsonErrorKind::Escape);
    assert_eq!(err(r#""\u12""#), JsonErrorKind::Escape);
    assert_eq!(err(r#""\uZZZZ""#), JsonErrorKind::Escape);
    assert_eq!(err("\"\\"), JsonErrorKind::UnexpectedEnd);
}

#[test]
fn rejects_lone_and_mismatched_surrogates() {
    assert_eq!(err(r#""\uD834""#), JsonErrorKind::LoneSurrogate);
    assert_eq!(err(r#""\uDD1E""#), JsonErrorKind::LoneSurrogate);
    assert_eq!(err(r#""\uD834\u0041""#), JsonErrorKind::LoneSurrogate);
    assert_eq!(err(r#""\uD834x""#), JsonErrorKind::LoneSurrogate);
    assert_eq!(err(r#""\uD834\uD834""#), JsonErrorKind::LoneSurrogate);
}

#[test]
fn rejects_invalid_utf8_before_parsing() {
    assert_eq!(err_bytes(b"\"\xff\""), JsonErrorKind::Utf8);
    assert_eq!(err_bytes(b"\xc3\x28"), JsonErrorKind::Utf8);
}

fn err_bytes(bytes: &[u8]) -> JsonErrorKind {
    parse_line(bytes).expect_err("expected invalid JSON").kind
}

// ------------------------------------------------------------ structural errors

#[test]
fn rejects_empty_and_whitespace_only_input() {
    assert_eq!(err(""), JsonErrorKind::Empty);
    assert_eq!(err("   "), JsonErrorKind::Empty);
    assert_eq!(err("\t\r\n"), JsonErrorKind::Empty);
}

#[test]
fn rejects_trailing_bytes() {
    assert_eq!(err("{}x"), JsonErrorKind::TrailingBytes);
    assert_eq!(err("{} {}"), JsonErrorKind::TrailingBytes);
    assert_eq!(err("1 2"), JsonErrorKind::TrailingBytes);
    assert_eq!(err("[]]"), JsonErrorKind::TrailingBytes);
    assert_eq!(err("truefalse"), JsonErrorKind::TrailingBytes);
}

#[test]
fn rejects_truncated_values() {
    assert_eq!(err("\"abc"), JsonErrorKind::UnexpectedEnd);
    assert_eq!(err("[1"), JsonErrorKind::UnexpectedEnd);
    assert_eq!(err("[1,"), JsonErrorKind::UnexpectedEnd);
    assert_eq!(err("{\"a\":"), JsonErrorKind::UnexpectedEnd);
    assert_eq!(err("tru"), JsonErrorKind::UnexpectedEnd);
}

#[test]
fn rejects_broken_structure() {
    assert_eq!(err("[1,]"), JsonErrorKind::UnexpectedByte);
    assert_eq!(err("[,]"), JsonErrorKind::UnexpectedByte);
    assert_eq!(err("[1 2]"), JsonErrorKind::UnexpectedByte);
    assert_eq!(err("{\"a\":1,}"), JsonErrorKind::UnexpectedByte);
    assert_eq!(err("{\"a\" 1}"), JsonErrorKind::UnexpectedByte);
    assert_eq!(err("{\"a\":}"), JsonErrorKind::UnexpectedByte);
    assert_eq!(err("{a:1}"), JsonErrorKind::UnexpectedByte);
    assert_eq!(err("{'a':1}"), JsonErrorKind::UnexpectedByte);
    assert_eq!(err("{\"a\":1 \"b\":2}"), JsonErrorKind::UnexpectedByte);
}

#[test]
fn rejects_non_json_tokens() {
    assert_eq!(err("True"), JsonErrorKind::UnexpectedByte);
    assert_eq!(err("nul"), JsonErrorKind::UnexpectedEnd);
    assert_eq!(err("NaN"), JsonErrorKind::UnexpectedByte);
    assert_eq!(err("undefined"), JsonErrorKind::UnexpectedByte);
    assert_eq!(err("// comment"), JsonErrorKind::UnexpectedByte);
    assert_eq!(err("#!/bin/sh"), JsonErrorKind::UnexpectedByte);
}

#[test]
fn rejects_a_utf8_bom() {
    assert_eq!(err_bytes(b"\xef\xbb\xbf{}"), JsonErrorKind::UnexpectedByte);
}

// ------------------------------------------------------------- value encoding

#[test]
fn value_encoding_round_trips() {
    let value = parse(r#"{"a":[1,-2,true,false,null,"x\n\u0001"],"b":{}}"#);
    let encoded = value.encode();
    assert_eq!(parse(&encoded), value);
    assert_eq!(encoded, r#"{"a":[1,-2,true,false,null,"x\n\u0001"],"b":{}}"#);
}

#[test]
fn value_encoding_escapes_every_special_character() {
    let value = Value::String("\"\\\u{08}\u{0c}\n\r\t\u{1f}".to_string());
    assert_eq!(value.encode(), r#""\"\\\b\f\n\r\t\u001f""#);
}

// ---------------------------------------------------------------------- base64

#[test]
fn base64_matches_the_rfc_4648_vectors() {
    let vectors: [(&[u8], &str); 7] = [
        (b"", ""),
        (b"f", "Zg=="),
        (b"fo", "Zm8="),
        (b"foo", "Zm9v"),
        (b"foob", "Zm9vYg=="),
        (b"fooba", "Zm9vYmE="),
        (b"foobar", "Zm9vYmFy"),
    ];
    for (raw, encoded) in vectors {
        assert_eq!(base64_encode(raw), encoded);
        assert_eq!(base64_decode(encoded).unwrap(), raw);
    }
}

#[test]
fn base64_round_trips_every_byte() {
    let raw: Vec<u8> = (0..=255).collect();
    let encoded = base64_encode(&raw);
    assert_eq!(encoded.len() % 4, 0);
    assert_eq!(base64_decode(&encoded).unwrap(), raw);
}

#[test]
fn base64_accepts_non_canonical_trailing_bits() {
    // "Zh==" differs from "Zg==" only in bits that are discarded; the decoded
    // bytes are identical and stay accepted.
    assert_eq!(base64_decode("Zh==").unwrap(), b"f");
}

#[test]
fn base64_rejects_bad_input() {
    assert_eq!(base64_decode("Zg=").unwrap_err(), json::Base64Error::Length);
    assert_eq!(base64_decode("Zg").unwrap_err(), json::Base64Error::Length);
    assert_eq!(base64_decode("Z").unwrap_err(), json::Base64Error::Length);
    assert_eq!(base64_decode("Zg===").unwrap_err(), json::Base64Error::Length);
    assert_eq!(base64_decode("====").unwrap_err(), json::Base64Error::Padding);
    assert_eq!(base64_decode("Z===").unwrap_err(), json::Base64Error::Padding);
    assert_eq!(base64_decode("Zg=A").unwrap_err(), json::Base64Error::Padding);
    assert_eq!(base64_decode("Zm9!").unwrap_err(), json::Base64Error::Alphabet);
    assert_eq!(base64_decode("Zm9\n").unwrap_err(), json::Base64Error::Alphabet);
    assert_eq!(base64_decode("éab").unwrap_err(), json::Base64Error::Alphabet);
}

// ------------------------------------------------------------------ wire layer

#[test]
fn encodes_an_exec_request() {
    let request = Request::Exec(ExecRequest {
        stdin: Some(json::Stdin::Base64(b"hi\n".to_vec())),
        ..ExecRequest::new("r2", "cat")
    });
    let line = request.encode();
    let value = parse_line(line.as_bytes()).unwrap();
    assert_eq!(get(&value, "schema_version"), &Value::Int(1));
    assert_eq!(get(&value, "id"), &Value::String("r2".to_string()));
    assert_eq!(get(&value, "op"), &Value::String("exec".to_string()));
    assert_eq!(get(&value, "command"), &Value::String("cat".to_string()));
    assert_eq!(get(&value, "stdin"), &Value::String("aGkK".to_string()));
    assert_eq!(
        get(&value, "stdin_encoding"),
        &Value::String("base64".to_string())
    );
    assert_eq!(get(&value, "env_delta"), &Value::Bool(true));
}

#[test]
fn encodes_state_cancel_and_close_requests() {
    let line = Request::State(StateRequest {
        id: "r3".to_string(),
        include: vec![StateSection::Env, StateSection::Jobs],
    })
    .encode();
    let value = parse(&line);
    assert_eq!(get(&value, "op"), &Value::String("state".to_string()));
    assert_eq!(
        get(&value, "include"),
        &Value::Array(vec![
            Value::String("env".to_string()),
            Value::String("jobs".to_string())
        ])
    );

    let line = Request::Cancel(json::CancelRequest {
        id: "r4".to_string(),
        target: "r1".to_string(),
        task: Some("diff".to_string()),
        grace_ms: Some(200),
    })
    .encode();
    let value = parse(&line);
    assert_eq!(get(&value, "op"), &Value::String("cancel".to_string()));
    assert_eq!(get(&value, "target"), &Value::String("r1".to_string()));
    assert_eq!(get(&value, "task"), &Value::String("diff".to_string()));
    assert_eq!(get(&value, "grace_ms"), &Value::Int(200));

    let line = Request::Close(json::CloseRequest {
        id: "r5".to_string(),
        wait_ms: None,
    })
    .encode();
    let value = parse(&line);
    assert_eq!(get(&value, "op"), &Value::String("close".to_string()));
    assert!(value.get("wait_ms").is_none());
}

#[test]
fn encodes_ready_with_the_advertised_contract() {
    let ready = Ready::new("s-4242-8f1c", 4242, "3.7.0");
    let value = parse(&Response::Ready(ready).encode());
    assert_eq!(get(&value, "schema_version"), &Value::Int(1));
    assert_eq!(get(&value, "type"), &Value::String("ready".to_string()));
    assert_eq!(get(&value, "session"), &Value::String("s-4242-8f1c".to_string()));
    assert_eq!(get(&value, "pid"), &Value::Int(4242));
    let protocol = get(&value, "protocol");
    assert_eq!(get(protocol, "versions"), &Value::Array(vec![Value::Int(1)]));
    let ops = get(protocol, "ops").as_array().unwrap();
    assert_eq!(ops.len(), 8, "{ops:?}");
    assert!(ops.contains(&Value::String("workflow".to_string())));
    assert!(ops.contains(&Value::String("read_capture".to_string())));
    assert!(ops.contains(&Value::String("code".to_string())));
    let features = get(protocol, "features").as_array().unwrap();
    assert!(features.len() >= 22, "{features:?}");
    assert!(features.contains(&Value::String("workflow".to_string())));
    assert!(features.contains(&Value::String("output_projection".to_string())));
    assert!(features.contains(&Value::String("capture_store".to_string())));
    assert!(features.contains(&Value::String("code_mode".to_string())));
    for phase_g in [
        "pty",
        "streaming_output",
        "cross_session_locks",
        "opt_in_sandbox",
        "session_resume",
        "mcp",
    ] {
        assert!(
            features.contains(&Value::String(phase_g.to_string())),
            "missing feature {phase_g}: {features:?}"
        );
    }
    let limits = get(protocol, "limits");
    assert_eq!(get(limits, "max_line_bytes"), &Value::Int(1048576));
    assert_eq!(get(limits, "max_stdin_bytes"), &Value::Int(1048576));
    assert_eq!(get(limits, "max_capture_bytes"), &Value::Int(4194304));
    assert_eq!(get(limits, "max_id_bytes"), &Value::Int(256));
    // The parallel limits are what the decoder enforces, so a client can size
    // a request without guessing.
    assert_eq!(get(limits, "max_parallel_tasks"), &Value::Int(128));
    assert_eq!(get(limits, "max_parallel"), &Value::Int(32));
    assert_eq!(
        get(limits, "max_parallel_output_bytes"),
        &Value::Int(8388608)
    );
    assert_eq!(get(limits, "max_workflow_attempts"), &Value::Int(8));
    assert_eq!(get(limits, "max_workflow_backoff_ms"), &Value::Int(60_000));
    assert_eq!(get(limits, "max_capture_store_entries"), &Value::Int(32));
    assert_eq!(
        get(limits, "max_capture_store_bytes"),
        &Value::Int(16 * 1024 * 1024)
    );
    assert_eq!(get(limits, "max_code_steps"), &Value::Int(100_000));
    assert_eq!(get(limits, "max_code_calls"), &Value::Int(256));
    assert_eq!(get(protocol, "cancel_grace_ms"), &Value::Int(2000));
}

// ------------------------------------------------------ parallel wire contract

fn parallel(line: &str) -> Result<protocol::ParallelRequest, protocol::DecodeError> {
    protocol::decode_parallel(&parse(line), &json::Limits::default(), "p1".to_string())
}

fn parallel_rejected(line: &str) -> protocol::DecodeError {
    parallel(line).expect_err("expected a rejected parallel request")
}

fn task_line(tasks: &str) -> String {
    format!(r#"{{"schema_version":1,"id":"p1","op":"parallel","tasks":[{tasks}]}}"#)
}

#[test]
fn decodes_a_minimal_parallel_request() {
    let request = parallel(
        r#"{"schema_version":1,"id":"p1","op":"parallel","tasks":[{"id":"t1","command":"true"}]}"#,
    )
    .expect("valid parallel");
    assert_eq!(request.id, "p1");
    assert_eq!(request.policy, ParallelPolicy::WaitAll);
    assert_eq!(request.max_concurrency, 4);
    assert_eq!(request.timeout_ms, None);
    assert_eq!(request.tasks.len(), 1);
    let task = &request.tasks[0];
    assert_eq!(task.id, "t1");
    assert_eq!(task.command(), Some("true"));
    assert_eq!(task.stdin, None);
    assert_eq!(task.timeout_ms, None);
    assert_eq!(task.cwd, None);
    assert!(task.env.is_empty());
    assert!(!task.env_delta);
}

#[test]
fn decodes_every_parallel_field() {
    let request = parallel(
        r#"{"schema_version":1,"id":"p1","op":"parallel","policy":"fail-fast",
            "max_concurrency":3,"timeout_ms":120000,
            "tasks":[
              {"id":"status","command":"git status --short"},
              {"id":"diff","command":"git diff --stat","timeout_ms":30000},
              {"id":"find","command":"find src -name '*.rs'","cwd":"/repo",
               "stdin":"aGkK","stdin_encoding":"base64",
               "env":{"GIT_PAGER":"cat","DROP":null},"env_delta":true}
            ]}"#,
    )
    .expect("valid parallel");
    assert_eq!(request.id, "p1");
    assert_eq!(request.policy, ParallelPolicy::FailFast);
    assert_eq!(request.max_concurrency, 3);
    assert_eq!(request.timeout_ms, Some(120_000));
    assert_eq!(request.tasks.len(), 3);
    assert_eq!(request.tasks[0].id, "status");
    assert_eq!(request.tasks[1].timeout_ms, Some(30_000));
    let find = &request.tasks[2];
    assert_eq!(find.cwd.as_deref(), Some("/repo"));
    assert_eq!(find.stdin, Some(b"hi\n".to_vec()));
    assert_eq!(
        find.env,
        vec![
            ("GIT_PAGER".to_string(), Some("cat".to_string())),
            ("DROP".to_string(), None),
        ]
    );
    assert!(find.env_delta);
}

#[test]
fn parallel_shape_and_type_violations_are_invalid_request() {
    for line in [
        // `tasks` itself.
        r#"{"schema_version":1,"id":"p1","op":"parallel"}"#,
        r#"{"schema_version":1,"id":"p1","op":"parallel","tasks":[]}"#,
        r#"{"schema_version":1,"id":"p1","op":"parallel","tasks":{}}"#,
        r#"{"schema_version":1,"id":"p1","op":"parallel","tasks":[7]}"#,
        r#"{"schema_version":1,"id":"p1","op":"parallel","tasks":[[]]}"#,
        // The task's required command.
        r#"{"schema_version":1,"id":"p1","op":"parallel","tasks":[{"id":"t1"}]}"#,
        r#"{"schema_version":1,"id":"p1","op":"parallel","tasks":[{"id":"t1","command":7}]}"#,
        // `policy`.
        r#"{"schema_version":1,"id":"p1","op":"parallel","policy":"wait-all-first","tasks":[{"id":"t1","command":"true"}]}"#,
        r#"{"schema_version":1,"id":"p1","op":"parallel","policy":7,"tasks":[{"id":"t1","command":"true"}]}"#,
        // `max_concurrency`.
        r#"{"schema_version":1,"id":"p1","op":"parallel","max_concurrency":0,"tasks":[{"id":"t1","command":"true"}]}"#,
        r#"{"schema_version":1,"id":"p1","op":"parallel","max_concurrency":33,"tasks":[{"id":"t1","command":"true"}]}"#,
        r#"{"schema_version":1,"id":"p1","op":"parallel","max_concurrency":-1,"tasks":[{"id":"t1","command":"true"}]}"#,
        r#"{"schema_version":1,"id":"p1","op":"parallel","max_concurrency":"2","tasks":[{"id":"t1","command":"true"}]}"#,
        // Group and per-task `timeout_ms`.
        r#"{"schema_version":1,"id":"p1","op":"parallel","timeout_ms":-1,"tasks":[{"id":"t1","command":"true"}]}"#,
        r#"{"schema_version":1,"id":"p1","op":"parallel","timeout_ms":"10","tasks":[{"id":"t1","command":"true"}]}"#,
        r#"{"schema_version":1,"id":"p1","op":"parallel","tasks":[{"id":"t1","command":"true","timeout_ms":-5}]}"#,
        r#"{"schema_version":1,"id":"p1","op":"parallel","tasks":[{"id":"t1","command":"true","timeout_ms":"5"}]}"#,
        // `cwd`.
        r#"{"schema_version":1,"id":"p1","op":"parallel","tasks":[{"id":"t1","command":"true","cwd":7}]}"#,
        r#"{"schema_version":1,"id":"p1","op":"parallel","tasks":[{"id":"t1","command":"true","cwd":""}]}"#,
        // `env` and `env_delta`.
        r#"{"schema_version":1,"id":"p1","op":"parallel","tasks":[{"id":"t1","command":"true","env":[]}]}"#,
        r#"{"schema_version":1,"id":"p1","op":"parallel","tasks":[{"id":"t1","command":"true","env":{"A":7}}]}"#,
        r#"{"schema_version":1,"id":"p1","op":"parallel","tasks":[{"id":"t1","command":"true","env":{"":""}}]}"#,
        r#"{"schema_version":1,"id":"p1","op":"parallel","tasks":[{"id":"t1","command":"true","env":{"A=B":"x"}}]}"#,
        r#"{"schema_version":1,"id":"p1","op":"parallel","tasks":[{"id":"t1","command":"true","env_delta":"yes"}]}"#,
    ] {
        let error = parallel_rejected(line);
        assert_eq!(error.code, ErrorCode::InvalidRequest, "{line}");
        assert_eq!(error.id.as_deref(), Some("p1"), "{line}");
    }
}

#[test]
fn parallel_task_ids_are_validated_and_must_be_unique() {
    for line in [
        r#"{"schema_version":1,"id":"p1","op":"parallel","tasks":[{"command":"true"}]}"#,
        r#"{"schema_version":1,"id":"p1","op":"parallel","tasks":[{"id":"","command":"true"}]}"#,
        r#"{"schema_version":1,"id":"p1","op":"parallel","tasks":[{"id":7,"command":"true"}]}"#,
    ] {
        let error = parallel_rejected(line);
        assert_eq!(error.code, ErrorCode::InvalidId, "{line}");
        assert_eq!(error.id.as_deref(), Some("p1"), "{line}");
    }

    let long = "a".repeat(257);
    let error = parallel_rejected(&task_line(&format!(
        r#"{{"id":"{long}","command":"true"}}"#
    )));
    assert_eq!(error.code, ErrorCode::InvalidId);

    let error = parallel_rejected(&task_line(
        r#"{"id":"dup","command":"a"},{"id":"dup","command":"b"}"#,
    ));
    assert_eq!(error.code, ErrorCode::DuplicateTaskId);
    assert_eq!(error.id.as_deref(), Some("p1"));
    assert!(error.message.contains("dup"), "{}", error.message);
    assert!(!error.code.is_fatal());

    // Uniqueness is per request and the ids may appear in any order.
    let request = parallel(&task_line(
        r#"{"id":"dup","command":"a"},{"id":"other","command":"b"}"#,
    ))
    .expect("distinct ids");
    assert_eq!(request.tasks[1].id, "other");
}

#[test]
fn parallel_bounds_the_task_count() {
    let tasks = |count: usize| {
        let body: Vec<String> = (0..count)
            .map(|index| format!(r#"{{"id":"t{index}","command":"true"}}"#))
            .collect();
        task_line(&body.join(","))
    };

    assert_eq!(
        parallel_rejected(&tasks(0)).code,
        ErrorCode::InvalidRequest
    );
    assert_eq!(parallel(&tasks(128)).expect("at the limit").tasks.len(), 128);
    let error = parallel_rejected(&tasks(129));
    assert_eq!(error.code, ErrorCode::TooManyTasks);
    assert_eq!(error.id.as_deref(), Some("p1"));
    assert!(!error.code.is_fatal());

    // A lowered limit is enforced, and the message names both numbers.
    let limits = json::Limits {
        max_parallel_tasks: 2,
        ..json::Limits::default()
    };
    let error = protocol::decode_parallel(&parse(&tasks(3)), &limits, "p1".to_string())
        .expect_err("over the lowered limit");
    assert_eq!(error.code, ErrorCode::TooManyTasks);
    assert!(error.message.contains('3') && error.message.contains('2'));
}

#[test]
fn parallel_bounds_max_concurrency() {
    for value in [1, 4, 32] {
        let line = format!(
            r#"{{"schema_version":1,"id":"p1","op":"parallel","max_concurrency":{value},"tasks":[{{"id":"t1","command":"true"}}]}}"#
        );
        assert_eq!(
            parallel(&line).expect("within range").max_concurrency,
            value as usize
        );
    }
    let error = parallel_rejected(
        r#"{"schema_version":1,"id":"p1","op":"parallel","max_concurrency":0,"tasks":[{"id":"t1","command":"true"}]}"#,
    );
    assert!(error.message.contains("max_concurrency"), "{}", error.message);
    assert!(error.message.contains("32"), "{}", error.message);

    // The bound follows `limits.max_parallel`, not a hardcoded literal.
    let limits = json::Limits {
        max_parallel: 2,
        ..json::Limits::default()
    };
    let line = r#"{"schema_version":1,"id":"p1","op":"parallel","max_concurrency":3,"tasks":[{"id":"t1","command":"true"}]}"#;
    let error = protocol::decode_parallel(&parse(line), &limits, "p1".to_string())
        .expect_err("over the lowered window");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
}

#[test]
fn parallel_decodes_task_stdin_encodings() {
    let request = parallel(&task_line(
        r#"{"id":"a","command":"cat","stdin":"hi\n"},{"id":"b","command":"cat","stdin":"aGkK","stdin_encoding":"base64"}"#,
    ))
    .expect("valid stdin");
    assert_eq!(request.tasks[0].stdin, Some(b"hi\n".to_vec()));
    assert_eq!(request.tasks[1].stdin, Some(b"hi\n".to_vec()));
    // The design keeps `exec`'s rule: an empty command is a valid no-op.
    let request = parallel(&task_line(r#"{"id":"a","command":""}"#)).expect("empty command");
    assert_eq!(request.tasks[0].command(), Some(""));

    for line in [
        task_line(r#"{"id":"a","command":"cat","stdin_encoding":"base64"}"#),
        task_line(r#"{"id":"a","command":"cat","stdin":5}"#),
    ] {
        assert_eq!(
            parallel_rejected(&line).code,
            ErrorCode::InvalidRequest,
            "{line}"
        );
    }
    for line in [
        task_line(r#"{"id":"a","command":"cat","stdin":"aGk","stdin_encoding":"base64"}"#),
        task_line(r#"{"id":"a","command":"cat","stdin":"!!!!","stdin_encoding":"base64"}"#),
        task_line(r#"{"id":"a","command":"cat","stdin":"hi","stdin_encoding":"latin1"}"#),
        task_line(r#"{"id":"a","command":"cat","stdin":"hi","stdin_encoding":7}"#),
    ] {
        assert_eq!(
            parallel_rejected(&line).code,
            ErrorCode::InvalidEncoding,
            "{line}"
        );
    }

    let limits = json::Limits {
        max_stdin_bytes: 4,
        ..json::Limits::default()
    };
    let line = task_line(r#"{"id":"a","command":"cat","stdin":"hello"}"#);
    let error = protocol::decode_parallel(&parse(&line), &limits, "p1".to_string())
        .expect_err("too large");
    assert_eq!(error.code, ErrorCode::StdinTooLarge);
    assert_eq!(error.id.as_deref(), Some("p1"));
}

#[test]
fn parallel_ignores_unknown_keys_for_forward_compatibility() {
    let request = parallel(
        r#"{"schema_version":1,"id":"p1","op":"parallel","stream":true,"isolation":"process",
            "tasks":[{"id":"t1","command":"true","priority":9,"tty":true}]}"#,
    )
    .expect("unknown keys are ignored");
    assert_eq!(request.tasks.len(), 1);
    assert_eq!(request.tasks[0].id, "t1");
}

#[test]
fn parallel_is_advertised_once_the_executor_exists() {
    // `ready` and the driver are gated on the same constant: the op is listed
    // exactly when the dispatch answers it, and per-task cancellation is listed
    // exactly when the supervisor's parallel table honors it.
    const {
        assert!(json::PARALLEL_AVAILABLE);
        assert!(json::PARALLEL_TASK_CANCEL_AVAILABLE);
    }

    let ready = parse(&Response::Ready(Ready::new("s-1", 1, "3.7.0")).encode());
    let protocol = get(&ready, "protocol");
    let ops = get(protocol, "ops");
    assert!(
        ops.as_array()
            .unwrap()
            .iter()
            .any(|entry| entry.as_str() == Some("parallel")),
        "ops must list parallel: {ops:?}"
    );
    let features = get(protocol, "features");
    let features = features.as_array().unwrap();
    for feature in ["parallel", "parallel_env_delta", "parallel_task_cancel"] {
        assert!(
            features
                .iter()
                .any(|entry| entry.as_str() == Some(feature)),
            "features must list {feature}: {features:?}"
        );
    }

    let value = parse(
        r#"{"schema_version":1,"id":"p1","op":"parallel","tasks":[{"id":"t1","command":"true"}]}"#,
    );
    let request = protocol::decode(&value, &json::Limits::default()).expect("dispatched");
    assert_eq!(request.id(), "p1");
    assert!(matches!(request, protocol::DecodedRequest::Parallel(_)));
}

#[test]
fn the_parallel_encoder_and_decoder_agree() {
    let line = Request::Parallel(json::ParallelRequest {
        id: "p1".to_string(),
        policy: ParallelPolicy::FailFast,
        max_concurrency: 3,
        timeout_ms: Some(120_000),
        tasks: vec![
            json::TaskRequest {
                id: "status".to_string(),
                command: "git status --short".to_string(),
                stdin: None,
                timeout_ms: None,
                cwd: None,
                env: Vec::new(),
                env_delta: false,
                tty: false,
                sandbox: None,
            },
            json::TaskRequest {
                id: "diff".to_string(),
                command: "cat".to_string(),
                stdin: Some(json::Stdin::Base64(b"hi\n".to_vec())),
                timeout_ms: Some(30_000),
                cwd: Some("/repo".to_string()),
                env: vec![
                    ("GIT_PAGER".to_string(), Some("cat".to_string())),
                    ("DROP".to_string(), None),
                ],
                env_delta: true,
                tty: false,
                sandbox: None,
            },
        ],
    })
    .encode();
    let request = parallel(&line).unwrap_or_else(|error| panic!("{line}: {error:?}"));
    assert_eq!(request.id, "p1");
    assert_eq!(request.policy, ParallelPolicy::FailFast);
    assert_eq!(request.max_concurrency, 3);
    assert_eq!(request.timeout_ms, Some(120_000));
    assert_eq!(request.tasks[1].stdin, Some(b"hi\n".to_vec()));
    assert_eq!(request.tasks[1].cwd.as_deref(), Some("/repo"));
    assert!(request.tasks[1].env_delta);
}

#[test]
fn encodes_a_parallel_result() {
    let mut ok = json::TaskResult::new("status", "/repo");
    ok.duration_ms = 12;
    ok.duration_us = 12_345;
    ok.stdout = Captured {
        data: StreamData::Utf8(" M a.rs\n".to_string()),
        bytes: 7,
        truncated: false,
    };
    ok.pgid = Some(4301);

    let mut slow = json::TaskResult::new("diff", "/repo");
    slow.state = json::TaskState::Timeout;
    slow.termination = Termination::Timeout;
    slow.status = 124;
    slow.reason = Some(json::TaskReason::GroupTimeout);
    slow.duration_ms = 30_011;
    slow.stderr = Captured {
        data: StreamData::Base64(vec![0x00, 0xff]),
        bytes: 9,
        truncated: true,
    };

    let group = json::ParallelResult {
        id: "p1".to_string(),
        policy: ParallelPolicy::WaitAll,
        state: json::GroupState::Completed,
        status: 124,
        duration_ms: 30_037,
        duration_us: 30_037_500,
        max_concurrency: 3,
        started_tasks: 3,
        output_truncated: false,
        tasks: vec![ok, slow],
    };
    let value = parse(&Response::Parallel(group).encode());
    assert_eq!(get(&value, "schema_version"), &Value::Int(1));
    assert_eq!(get(&value, "id"), &Value::String("p1".to_string()));
    assert_eq!(get(&value, "type"), &Value::String("parallel".to_string()));
    assert_eq!(get(&value, "op"), &Value::String("parallel".to_string()));
    assert_eq!(
        get(&value, "policy"),
        &Value::String("wait-all".to_string())
    );
    assert_eq!(
        get(&value, "state"),
        &Value::String("completed".to_string())
    );
    assert_eq!(get(&value, "status"), &Value::Int(124));
    assert_eq!(get(&value, "duration_ms"), &Value::Int(30_037));
    assert_eq!(get(&value, "duration_us"), &Value::Int(30_037_500));
    assert_eq!(get(&value, "max_concurrency"), &Value::Int(3));
    assert_eq!(get(&value, "started_tasks"), &Value::Int(3));
    assert_eq!(get(&value, "output_truncated"), &Value::Bool(false));

    let tasks = get(&value, "tasks").as_array().expect("tasks is an array");
    assert_eq!(tasks.len(), 2);
    let first = &tasks[0];
    assert_eq!(get(first, "id"), &Value::String("status".to_string()));
    assert_eq!(get(first, "state"), &Value::String("completed".to_string()));
    assert_eq!(
        get(first, "termination"),
        &Value::String("exited".to_string())
    );
    assert_eq!(get(first, "status"), &Value::Int(0));
    assert_eq!(get(first, "duration_ms"), &Value::Int(12));
    assert_eq!(get(first, "duration_us"), &Value::Int(12_345));
    assert_eq!(get(first, "cwd"), &Value::String("/repo".to_string()));
    assert_eq!(
        get(first, "stdout"),
        &Value::String(" M a.rs\n".to_string())
    );
    assert_eq!(
        get(first, "stdout_encoding"),
        &Value::String("utf8".to_string())
    );
    assert_eq!(get(first, "stdout_bytes"), &Value::Int(7));
    assert_eq!(get(first, "stdout_truncated"), &Value::Bool(false));
    assert_eq!(get(first, "stderr_bytes"), &Value::Int(0));
    assert_eq!(get(first, "pgid"), &Value::Int(4301));
    assert!(first.get("signal").is_none());
    assert!(first.get("reason").is_none());
    assert!(first.get("error").is_none());
    assert!(first.get("env_delta").is_none());

    let second = &tasks[1];
    assert_eq!(get(second, "state"), &Value::String("timeout".to_string()));
    assert_eq!(
        get(second, "termination"),
        &Value::String("timeout".to_string())
    );
    assert_eq!(
        get(second, "reason"),
        &Value::String("group_timeout".to_string())
    );
    assert_eq!(
        get(second, "stderr_encoding"),
        &Value::String("base64".to_string())
    );
    assert_eq!(get(second, "stderr"), &Value::String("AP8=".to_string()));
    assert_eq!(get(second, "stderr_bytes"), &Value::Int(9));
    assert_eq!(get(second, "stderr_truncated"), &Value::Bool(true));
}

#[test]
fn a_parallel_result_has_a_stable_key_order() {
    let mut task = json::TaskResult::new("t1", "/");
    task.signal = Some("SIGTERM".to_string());
    task.reason = Some(json::TaskReason::FailFast);
    task.error = Some(json::TaskError {
        code: ErrorCode::SpawnFailed,
        message: "fork failed".to_string(),
    });
    task.env_delta = Some(vec![("X".to_string(), Some("1".to_string()))]);
    task.pgid = Some(7);
    let group = json::ParallelResult {
        id: "p1".to_string(),
        policy: ParallelPolicy::FailFast,
        state: json::GroupState::Cancelled,
        status: 143,
        duration_ms: 1,
        duration_us: 1_000,
        max_concurrency: 2,
        started_tasks: 1,
        output_truncated: true,
        tasks: vec![task],
    };
    let value = parse(&Response::Parallel(group).encode());
    assert_eq!(
        object_keys(&value),
        vec![
            "schema_version",
            "id",
            "type",
            "op",
            "policy",
            "state",
            "status",
            "duration_ms",
            "duration_us",
            "max_concurrency",
            "started_tasks",
            "output_truncated",
            "tasks",
        ]
    );
    let task = &get(&value, "tasks").as_array().unwrap()[0];
    assert_eq!(
        object_keys(task),
        vec![
            "id",
            "state",
            "termination",
            "status",
            "signal",
            "reason",
            "error",
            "duration_ms",
            "duration_us",
            "cwd",
            "stdout",
            "stdout_encoding",
            "stdout_bytes",
            "stdout_truncated",
            "stderr",
            "stderr_encoding",
            "stderr_bytes",
            "stderr_truncated",
            "env_delta",
            "pgid",
        ]
    );
    assert_eq!(
        get(task, "signal").as_str(),
        Some("SIGTERM")
    );
    assert_eq!(get(task, "reason").as_str(), Some("fail_fast"));
    let error = get(task, "error");
    assert_eq!(object_keys(error), vec!["code", "message"]);
    assert_eq!(get(error, "code").as_str(), Some("SPAWN_FAILED"));
    assert_eq!(
        get(get(task, "env_delta"), "X").as_str(),
        Some("1")
    );
    assert_eq!(get(task, "pgid"), &Value::Int(7));
}

#[test]
fn parallel_states_and_terminations_cover_the_failure_taxonomy() {
    for (state, name) in [
        (json::TaskState::Completed, "completed"),
        (json::TaskState::Cancelled, "cancelled"),
        (json::TaskState::Timeout, "timeout"),
        (json::TaskState::SpawnError, "spawn_error"),
    ] {
        assert_eq!(state.as_str(), name);
    }
    for (state, name) in [
        (json::GroupState::Completed, "completed"),
        (json::GroupState::Cancelled, "cancelled"),
    ] {
        assert_eq!(state.as_str(), name);
    }
    for (policy, name) in [
        (ParallelPolicy::WaitAll, "wait-all"),
        (ParallelPolicy::FailFast, "fail-fast"),
    ] {
        assert_eq!(policy.as_str(), name);
    }
    for (reason, name) in [
        (json::TaskReason::FailFast, "fail_fast"),
        (json::TaskReason::GroupCancel, "group_cancel"),
        (json::TaskReason::TaskCancel, "task_cancel"),
        (json::TaskReason::GroupTimeout, "group_timeout"),
    ] {
        assert_eq!(reason.as_str(), name);
    }
    assert_eq!(Termination::SpawnError.as_str(), "spawn_error");
}

#[test]
fn encodes_an_exec_result() {
    let mut result = ExecResult::new("r1", 0, 3, "/tmp");
    result.duration_us = 3_250;
    result.stdout = Captured {
        data: StreamData::Utf8("hi\n".to_string()),
        bytes: 3,
        truncated: false,
    };
    result.env_delta = Some(vec![
        ("X".to_string(), Some("1".to_string())),
        ("GONE".to_string(), None),
    ]);
    result.pgid = Some(4243);
    let value = parse(&Response::Result(result).encode());
    assert_eq!(get(&value, "type"), &Value::String("result".to_string()));
    assert_eq!(get(&value, "status"), &Value::Int(0));
    assert_eq!(get(&value, "termination"), &Value::String("exited".to_string()));
    assert_eq!(get(&value, "duration_ms"), &Value::Int(3));
    assert_eq!(get(&value, "duration_us"), &Value::Int(3_250));
    assert_eq!(get(&value, "stdout"), &Value::String("hi\n".to_string()));
    assert_eq!(get(&value, "stdout_encoding"), &Value::String("utf8".to_string()));
    assert_eq!(get(&value, "stdout_bytes"), &Value::Int(3));
    assert_eq!(get(&value, "stdout_truncated"), &Value::Bool(false));
    assert_eq!(get(&value, "stderr_encoding"), &Value::String("utf8".to_string()));
    assert_eq!(get(&value, "pgid"), &Value::Int(4243));
    let delta = get(&value, "env_delta");
    assert_eq!(get(delta, "X"), &Value::String("1".to_string()));
    assert_eq!(get(delta, "GONE"), &Value::Null);
}

#[test]
fn encodes_a_base64_result_and_a_signaled_termination() {
    let mut result = ExecResult::new("r2", 143, 5, "/");
    result.termination = Termination::Signaled;
    result.signal = Some("SIGTERM".to_string());
    result.stdout = Captured {
        data: StreamData::Base64(vec![0x00, 0xff]),
        bytes: 2,
        truncated: true,
    };
    let value = parse(&Response::Result(result).encode());
    assert_eq!(get(&value, "status"), &Value::Int(143));
    assert_eq!(get(&value, "termination"), &Value::String("signaled".to_string()));
    assert_eq!(get(&value, "signal"), &Value::String("SIGTERM".to_string()));
    assert_eq!(get(&value, "stdout"), &Value::String("AP8=".to_string()));
    assert_eq!(get(&value, "stdout_encoding"), &Value::String("base64".to_string()));
    assert_eq!(get(&value, "stdout_truncated"), &Value::Bool(true));
}

#[test]
fn encodes_ack_closed_and_error_responses() {
    let value = parse(&Response::Ack(json::Ack::delivered("r4", "cancel")).encode());
    assert_eq!(get(&value, "type"), &Value::String("ack".to_string()));
    assert_eq!(get(&value, "op"), &Value::String("cancel".to_string()));
    assert_eq!(get(&value, "outcome"), &Value::String("delivered".to_string()));

    let value = parse(
        &Response::Closed(Closed {
            id: Some("r5".to_string()),
            status: 0,
        })
        .encode(),
    );
    assert_eq!(get(&value, "type"), &Value::String("closed".to_string()));
    assert_eq!(get(&value, "status"), &Value::Int(0));

    let value = parse(
        &Response::Error(ErrorResponse::new(
            None,
            ErrorCode::InvalidJson,
            "line is not a single well-formed JSON value",
        ))
        .encode(),
    );
    assert_eq!(get(&value, "type"), &Value::String("error".to_string()));
    assert_eq!(get(&value, "id"), &Value::Null);
    let error = get(&value, "error");
    assert_eq!(get(error, "code"), &Value::String("INVALID_JSON".to_string()));
    assert_eq!(get(error, "fatal"), &Value::Bool(false));
}

#[test]
fn error_codes_match_the_stable_taxonomy() {
    let expected = [
        (ErrorCode::InvalidJson, "INVALID_JSON", false),
        (ErrorCode::InvalidRequest, "INVALID_REQUEST", false),
        (ErrorCode::UnsupportedSchemaVersion, "UNSUPPORTED_SCHEMA_VERSION", false),
        (ErrorCode::UnknownOp, "UNKNOWN_OP", false),
        (ErrorCode::DuplicateId, "DUPLICATE_ID", false),
        (ErrorCode::InvalidId, "INVALID_ID", false),
        (ErrorCode::MessageTooLarge, "MESSAGE_TOO_LARGE", false),
        (ErrorCode::StdinTooLarge, "STDIN_TOO_LARGE", false),
        (ErrorCode::InvalidEncoding, "INVALID_ENCODING", false),
        (ErrorCode::QueueFull, "QUEUE_FULL", false),
        (ErrorCode::DuplicateTaskId, "DUPLICATE_TASK_ID", false),
        (ErrorCode::TooManyTasks, "TOO_MANY_TASKS", false),
        (ErrorCode::SpawnFailed, "SPAWN_FAILED", false),
        (ErrorCode::PartialRequest, "PARTIAL_REQUEST", true),
        (ErrorCode::CancelUnknownId, "CANCEL_UNKNOWN_ID", false),
        (ErrorCode::CancelTooLate, "CANCEL_TOO_LATE", false),
        (ErrorCode::SessionClosed, "SESSION_CLOSED", true),
        (ErrorCode::ProtocolFdLost, "PROTOCOL_FD_LOST", true),
        (ErrorCode::Internal, "INTERNAL", true),
    ];
    for (code, name, fatal) in expected {
        assert_eq!(code.as_str(), name);
        assert_eq!(code.is_fatal(), fatal);
    }
}

#[test]
fn a_parse_failure_maps_to_invalid_json() {
    let error = parse_line(b"{").unwrap_err();
    assert_eq!(error.kind, JsonErrorKind::UnexpectedEnd);
    assert_eq!(error.code(), "INVALID_JSON");
    assert!(!error.to_string().is_empty());
}

#[test]
fn encodes_a_state_snapshot() {
    let state = json::StateResult {
        id: "r3".to_string(),
        cwd: "/tmp".to_string(),
        last_status: 0,
        pid: 4242,
        jobs: vec![json::JobSummary {
            id: 1,
            pgid: 4244,
            pids: vec![4244],
            command: "sleep 100".to_string(),
            state: "running".to_string(),
        }],
        options: json::ShellOptions::default(),
        sections: vec![(
            "env".to_string(),
            Value::Object(vec![("PATH".to_string(), Value::String("/bin".to_string()))]),
        )],
    };
    let value = parse(&Response::State(state).encode());
    assert_eq!(get(&value, "type"), &Value::String("state".to_string()));
    assert_eq!(get(&value, "cwd"), &Value::String("/tmp".to_string()));
    assert_eq!(get(&value, "last_status"), &Value::Int(0));
    assert_eq!(get(&value, "pid"), &Value::Int(4242));
    let job = &get(&value, "jobs").as_array().unwrap()[0];
    assert_eq!(get(job, "pgid"), &Value::Int(4244));
    assert_eq!(
        get(&value, "options").get("errexit"),
        Some(&Value::Bool(false))
    );
    assert_eq!(get(get(&value, "env"), "PATH"), &Value::String("/bin".to_string()));
}

#[test]
fn encodes_streams_that_need_escaping() {
    let mut result = ExecResult::new("r9", 0, 1, "/");
    result.stdout = Captured {
        data: StreamData::Utf8("a\"b\\c\n".to_string()),
        bytes: 6,
        truncated: false,
    };
    let value = parse(&Response::Result(result).encode());
    assert_eq!(get(&value, "stdout"), &Value::String("a\"b\\c\n".to_string()));
}
