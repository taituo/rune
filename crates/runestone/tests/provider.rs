//! The Anthropic provider against a local mock server: retry on 529, then an SSE stream with thinking,
//! text and a tool call, accumulated into the same shape as a non-streaming response.

use runestone::agent::model::{Anthropic, Delta, Provider};
use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;

fn sse(events: &[serde_json::Value]) -> String {
    events.iter().map(|e| format!("event: {}\ndata: {}\n\n", e["type"].as_str().unwrap(), e)).collect()
}

fn serve(listener: TcpListener, responses: Vec<String>) -> std::thread::JoinHandle<Vec<String>> {
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        for resp in responses {
            let (mut sock, _) = listener.accept().unwrap();
            let mut r = BufReader::new(sock.try_clone().unwrap());
            let mut len = 0usize;
            let mut headers = String::new();
            loop {
                let mut l = String::new();
                r.read_line(&mut l).unwrap();
                if l == "\r\n" || l.is_empty() {
                    break;
                }
                if let Some(v) = l.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                headers.push_str(&l);
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            seen.push(format!("{headers}\n{}", String::from_utf8_lossy(&body)));
            sock.write_all(resp.as_bytes()).unwrap();
        }
        seen
    })
}

#[test]
fn streams_and_retries() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let ob = r#"{"type":"error","error":{"type":"overloaded_error"}}"#;
    let overloaded = format!("HTTP/1.1 529 Overloaded\r\ncontent-type: application/json\r\nretry-after: 0\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{ob}", ob.len());
    let stream = sse(&[
        json!({"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5-5","content":[],"stop_reason":null,"usage":{"input_tokens":10,"output_tokens":1}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"Check the file first."}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"SIG=="}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"ping"}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Let me "}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"look."}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"Read","input":{}}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"file_"}}),
        json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"path\": \"a.rs\"}"}}),
        json!({"type":"content_block_stop","index":2}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":42}}),
        json!({"type":"message_stop"}),
    ]);
    let ok = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{stream}");
    let server = serve(listener, vec![overloaded, ok]);

    let p = Anthropic::new(&format!("http://127.0.0.1:{port}"), "test-key", false);
    let mut text = String::new();
    let mut retries = 0;
    let msg = p
        .send(br#"{"model":"claude-opus-5-5","stream":true}"#, &mut |d| match d {
            Delta::Text(t) => text.push_str(t),
            Delta::Retry(_) => retries += 1,
            _ => {}
        })
        .unwrap();
    let seen = server.join().unwrap();

    assert_eq!(retries, 1);
    assert_eq!(seen.len(), 2);
    assert!(seen[1].to_ascii_lowercase().contains("x-api-key: test-key"));
    assert!(seen[1].contains("anthropic-version: 2023-06-01"));
    assert_eq!(text, "Let me look.");
    assert_eq!(msg["stop_reason"], "tool_use");
    assert_eq!(msg["usage"]["output_tokens"], 42);
    assert_eq!(msg["usage"]["input_tokens"], 10);
    let c = msg["content"].as_array().unwrap();
    assert_eq!(c[0], json!({"type":"thinking","thinking":"Check the file first.","signature":"SIG=="}));
    assert_eq!(c[1], json!({"type":"text","text":"Let me look."}));
    assert_eq!(c[2], json!({"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"a.rs"}}));
}

#[test]
fn non_retryable_error_surfaces() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"}}"#;
    let resp = format!("HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
    let server = serve(listener, vec![resp]);
    let p = Anthropic::new(&format!("http://127.0.0.1:{port}"), "k", false);
    let err = p.send(b"{}", &mut |_| {}).unwrap_err().to_string();
    server.join().unwrap();
    assert!(err.contains("400") && err.contains("invalid_request_error"), "{err}");
}
