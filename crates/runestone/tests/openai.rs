//! OpenAI-compatible gateway support, against a local mock server.

use runestone::agent::model::Provider;
use runestone::agent::openai::{self, OpenAi};
use runestone::agent::{verify_requests, Config, Permission, Quiet, Session};
use runestone::Engine;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;

/// Serve canned HTTP responses in order; return the request bodies received.
fn serve(responses: Vec<String>) -> (u16, std::thread::JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let h = std::thread::spawn(move || {
        let mut bodies = Vec::new();
        for resp in responses {
            let (mut sock, _) = listener.accept().unwrap();
            let mut r = BufReader::new(sock.try_clone().unwrap());
            let mut len = 0usize;
            loop {
                let mut l = String::new();
                r.read_line(&mut l).unwrap();
                if l == "\r\n" || l.is_empty() {
                    break;
                }
                if let Some(v) = l.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            bodies.push(body);
            sock.write_all(resp.as_bytes()).unwrap();
        }
        bodies
    });
    (port, h)
}

fn sse(chunks: &[Value]) -> String {
    let mut s = String::from("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n");
    for c in chunks {
        s += &format!("data: {c}\n\n");
    }
    s += "data: [DONE]\n\n";
    s
}

fn json_resp(v: &Value) -> String {
    let b = v.to_string();
    format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{b}", b.len())
}

#[test]
fn renders_tool_turns_in_openai_shape() {
    let cfg = json!({ "model": "m1", "max_tokens": 1000, "effort": "high" });
    let tools = json!([{ "name": "Read", "description": "Read a file", "input_schema": { "type": "object" } }]);
    let messages = vec![
        json!({ "role": "user", "content": [{ "type": "text", "text": "fix it" }] }),
        json!({ "role": "assistant", "content": [
            { "type": "thinking", "thinking": "hmm", "signature": "s" },
            { "type": "text", "text": "Reading." },
            { "type": "tool_use", "id": "call_1", "name": "Read", "input": { "file_path": "a.py" } }
        ]}),
        json!({ "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "call_1", "content": "no such file", "is_error": true },
            { "type": "text", "text": "[harness event, not from the user] note" }
        ]}),
    ];
    let b = openai::render(&cfg, "SYS", &tools, &messages);
    let m = b["messages"].as_array().unwrap();
    assert_eq!(m[0], json!({ "role": "system", "content": "SYS" }));
    assert_eq!(m[1], json!({ "role": "user", "content": "fix it" }));
    assert_eq!(m[2]["content"], "Reading.");
    assert_eq!(m[2]["tool_calls"][0]["function"]["arguments"], "{\"file_path\":\"a.py\"}");
    assert_eq!(m[3], json!({ "role": "tool", "tool_call_id": "call_1", "content": "Error: no such file" }));
    assert_eq!(m[4], json!({ "role": "user", "content": "[harness event, not from the user] note" }));
    assert!(!b.to_string().contains("hmm"), "thinking is not sent to this API");
    assert_eq!(b["tools"][0]["function"]["parameters"], json!({ "type": "object" }));
    assert_eq!(b["reasoning_effort"], "high");
    assert_eq!(b["stream_options"]["include_usage"], true);
}

#[test]
fn accumulates_streamed_tool_calls_and_usage() {
    let (port, h) = serve(vec![sse(&[
        json!({ "id": "c1", "model": "m1", "choices": [{ "index": 0, "delta": { "role": "assistant", "content": "" } }] }),
        json!({ "id": "c1", "choices": [{ "index": 0, "delta": { "reasoning_content": "think" } }] }),
        json!({ "id": "c1", "choices": [{ "index": 0, "delta": { "content": "Let me " } }] }),
        json!({ "id": "c1", "choices": [{ "index": 0, "delta": { "content": "look." } }] }),
        json!({ "id": "c1", "choices": [{ "index": 0, "delta": { "tool_calls": [{ "index": 0, "id": "call_a", "type": "function", "function": { "name": "Read", "arguments": "{\"file_" } }] } }] }),
        json!({ "id": "c1", "choices": [{ "index": 0, "delta": { "tool_calls": [{ "index": 0, "function": { "arguments": "path\": \"x.py\"}" } }] } }] }),
        json!({ "id": "c1", "choices": [{ "index": 0, "delta": { "tool_calls": [{ "index": 1, "id": "call_b", "function": { "name": "Bash", "arguments": "{\"command\":\"ls\"}" } }] } }] }),
        json!({ "id": "c1", "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
        json!({ "id": "c1", "choices": [], "usage": { "prompt_tokens": 100, "completion_tokens": 7, "prompt_tokens_details": { "cached_tokens": 80 } } }),
    ])]);
    let p = OpenAi::new(&format!("http://127.0.0.1:{port}"), Some("k".into()), false);
    let (mut text, mut thinking) = (String::new(), String::new());
    let m = p
        .send(b"{}", &mut |d| match d {
            runestone::agent::model::Delta::Text(t) => text.push_str(t),
            runestone::agent::model::Delta::Thinking(t) => thinking.push_str(t),
            _ => {}
        })
        .unwrap();
    h.join().unwrap();
    assert_eq!((text.as_str(), thinking.as_str()), ("Let me look.", "think"));
    assert_eq!(m["stop_reason"], "tool_use");
    assert_eq!(m["content"][0], json!({ "type": "text", "text": "Let me look." }));
    assert_eq!(m["content"][1], json!({ "type": "tool_use", "id": "call_a", "name": "Read", "input": { "file_path": "x.py" } }));
    assert_eq!(m["content"][2]["input"], json!({ "command": "ls" }));
    assert_eq!(m["usage"], json!({ "input_tokens": 20, "cache_read_input_tokens": 80, "output_tokens": 7 }));
}

#[test]
fn handles_a_server_that_does_not_stream() {
    let (port, h) = serve(vec![json_resp(&json!({
        "id": "x", "model": "m",
        "choices": [{ "index": 0, "finish_reason": "stop", "message": { "role": "assistant", "content": "plain answer" } }],
        "usage": { "prompt_tokens": 5, "completion_tokens": 2 }
    }))]);
    let p = OpenAi::new(&format!("http://127.0.0.1:{port}/v1"), None, false);
    let m = p.send(b"{}", &mut |_| {}).unwrap();
    h.join().unwrap();
    assert_eq!(m["content"][0]["text"], "plain answer");
    assert_eq!(m["stop_reason"], "end_turn");
}

#[test]
fn reads_cached_tokens_under_either_name() {
    let (port, h) = serve(vec![sse(&[
        json!({ "id": "c", "choices": [{ "index": 0, "delta": { "content": "hi" }, "finish_reason": "stop" }] }),
        json!({ "id": "c", "choices": [], "usage": { "prompt_tokens": 50, "completion_tokens": 3, "input_tokens_details": { "cached_tokens": 40 } } }),
    ])]);
    let m = OpenAi::new(&format!("http://127.0.0.1:{port}"), None, false).send(b"{}", &mut |_| {}).unwrap();
    h.join().unwrap();
    assert_eq!(m["usage"], json!({ "input_tokens": 10, "cache_read_input_tokens": 40, "output_tokens": 3 }));
}

#[test]
fn lists_models() {
    let (port, h) = serve(vec![json_resp(&json!({ "data": [{ "id": "qwen3-coder" }, { "id": "gpt-oss-120b" }] }))]);
    let ids = openai::list_models(&format!("http://127.0.0.1:{port}"), None).unwrap();
    h.join().unwrap();
    assert_eq!(ids, vec!["qwen3-coder", "gpt-oss-120b"]);
}

#[test]
fn agent_loop_through_a_gateway_is_exact() {
    let home = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    std::fs::write(wt.path().join("a.py"), "x = 1\n").unwrap();
    let eng = Engine::open_in(home.path(), wt.path()).unwrap();
    let b = eng.branch("s/gw");
    let cfg = Config { api: "openai".into(), model: "qwen3-coder".into(), ..Config::default() };
    Session::spawn(&eng, &b, &cfg, "2026-11-11").unwrap();

    let (port, h) = serve(vec![
        sse(&[
            json!({ "id": "r1", "model": "qwen3-coder", "choices": [{ "index": 0, "delta": { "tool_calls": [{ "index": 0, "id": "call_1", "function": { "name": "Read", "arguments": "{\"file_path\":\"a.py\"}" } }] } }] }),
            json!({ "id": "r1", "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
        ]),
        sse(&[
            json!({ "id": "r2", "model": "qwen3-coder", "choices": [{ "index": 0, "delta": { "content": "a.py sets x to 1." } }] }),
            json!({ "id": "r2", "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }] }),
        ]),
    ]);
    let mut s = Session {
        eng: &eng,
        branch: b.clone(),
        provider: Box::new(OpenAi::new(&format!("http://127.0.0.1:{port}"), None, false)),
        ui: Box::new(Quiet),
        perm: Permission::Dry,
        max_steps: 10,
        checks: None,
    };
    let r = s.turn("what does a.py do?").unwrap();
    let bodies = h.join().unwrap();
    assert_eq!(r.text, "a.py sets x to 1.");

    // The gateway received the tool round trip in its own format…
    let second: Value = serde_json::from_slice(&bodies[1]).unwrap();
    let msgs = second["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[1], json!({ "role": "user", "content": "what does a.py do?" }));
    assert_eq!(msgs[2]["tool_calls"][0]["id"], "call_1");
    assert_eq!(msgs[3]["role"], "tool");
    assert!(msgs[3]["content"].as_str().unwrap().contains("x = 1"));
    assert_eq!(second["model"], "qwen3-coder");

    // …the record stored exactly those bytes, and re-renders them.
    let stored: Vec<Vec<u8>> = eng
        .lineage(&b)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "model.call")
        .map(|e| eng.store.blob(e.str("request").unwrap()).unwrap())
        .collect();
    assert_eq!(stored, bodies);
    assert!(verify_requests(&eng, &b).unwrap().iter().all(|x| x.1));
}

fn send_one(chunks: &[Value]) -> Value {
    let (port, h) = serve(vec![sse(chunks)]);
    let m = OpenAi::new(&format!("http://127.0.0.1:{port}"), None, false).send(b"{}", &mut |_| {}).unwrap();
    h.join().unwrap();
    m
}

#[test]
fn quirk_every_call_at_index_zero() {
    // Told apart only by id.
    let m = send_one(&[
        json!({ "id": "c", "choices": [{ "index": 0, "delta": { "tool_calls": [{ "index": 0, "id": "a", "function": { "name": "Read", "arguments": "{\"file_path\":\"x\"}" } }] } }] }),
        json!({ "id": "c", "choices": [{ "index": 0, "delta": { "tool_calls": [{ "index": 0, "id": "b", "function": { "name": "Glob", "arguments": "{\"pattern\":\"*\"}" } }] } }] }),
        json!({ "id": "c", "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }] }),
    ]);
    let c = m["content"].as_array().unwrap();
    assert_eq!(c.len(), 2);
    assert_eq!((c[0]["id"].as_str(), c[0]["name"].as_str()), (Some("a"), Some("Read")));
    assert_eq!((c[1]["id"].as_str(), c[1]["input"].clone()), (Some("b"), json!({ "pattern": "*" })));
}

#[test]
fn quirk_arguments_as_an_object() {
    let m = send_one(&[json!({ "id": "c", "choices": [{ "index": 0, "delta": { "tool_calls": [{ "index": 0, "id": "a", "function": { "name": "Read", "arguments": { "file_path": "x" } } }] }, "finish_reason": "tool_calls" }] })]);
    assert_eq!(m["content"][0]["input"], json!({ "file_path": "x" }));
}

#[test]
fn quirk_stop_or_no_finish_reason_next_to_calls() {
    for finish in [json!("stop"), Value::Null] {
        let m = send_one(&[json!({ "id": "c", "choices": [{ "index": 0, "delta": { "content": "", "tool_calls": [{ "index": 0, "id": "a", "function": { "name": "Glob", "arguments": "{}" } }] }, "finish_reason": finish }] })]);
        assert_eq!(m["stop_reason"], "tool_use");
        assert_eq!(m["content"].as_array().unwrap().len(), 1, "empty content adds no text block");
    }
}

#[test]
fn quirk_missing_and_reused_ids_stay_unique_in_the_conversation() {
    // A gateway that never sends ids, two turns in a row: both calls would be call_0.
    let home = tempfile::tempdir().unwrap();
    let wt = tempfile::tempdir().unwrap();
    std::fs::write(wt.path().join("a.py"), "x = 1\n").unwrap();
    let eng = Engine::open_in(home.path(), wt.path()).unwrap();
    let b = eng.branch("s/ids");
    let cfg = Config { api: "openai".into(), model: "m".into(), ..Config::default() };
    Session::spawn(&eng, &b, &cfg, "2026-11-12").unwrap();
    let call = |args: &str| sse(&[json!({ "id": "r", "choices": [{ "index": 0, "delta": { "tool_calls": [{ "index": 0, "function": { "name": "Read", "arguments": args } }] }, "finish_reason": "tool_calls" }] })]);
    let done = sse(&[json!({ "id": "r", "choices": [{ "index": 0, "delta": { "content": "ok" }, "finish_reason": "stop" }] })]);
    let (port, h) = serve(vec![call("{\"file_path\":\"a.py\"}"), call("{\"file_path\":\"a.py\"}"), done]);
    let mut s = Session {
        eng: &eng,
        branch: b.clone(),
        provider: Box::new(OpenAi::new(&format!("http://127.0.0.1:{port}"), None, false)),
        ui: Box::new(Quiet),
        perm: Permission::Dry,
        max_steps: 10,
        checks: None,
    };
    assert_eq!(s.turn("read it twice").unwrap().text, "ok");
    let bodies = h.join().unwrap();
    let last: Value = serde_json::from_slice(&bodies[2]).unwrap();
    let ids: Vec<&str> = last["messages"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["tool_calls"].as_array().into_iter().flatten())
        .map(|c| c["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
    let results: Vec<&str> = last["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "tool").map(|m| m["tool_call_id"].as_str().unwrap()).collect();
    assert_eq!(results, ids, "each result answers its own call");
    assert!(verify_requests(&eng, &b).unwrap().iter().all(|x| x.1));
}
