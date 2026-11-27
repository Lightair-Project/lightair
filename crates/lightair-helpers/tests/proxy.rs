//! A helper's only way out is its proxy lane: what it lets through, what it refuses, and that
//! every attempt is reported. Only loopback servers are reached; refusals never connect anywhere.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use lightair_helpers::{Event, Lane, Rules, events};

/// A server on 127.0.0.1 that answers each connection with `reply` after reading a request head.
fn server(reply: &'static [u8]) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut c in l.incoming().flatten() {
            let mut buf = [0u8; 4096];
            let mut got = Vec::new();
            while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                match c.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                }
            }
            // echo what arrived after the reply, so a test can see what was forwarded
            let _ = c.write_all(reply);
            let _ = c.write_all(&got);
        }
    });
    port
}

fn ask(lane: &Lane, request: &str) -> String {
    let mut c = TcpStream::connect(("127.0.0.1", lane.port())).unwrap();
    c.write_all(request.as_bytes()).unwrap();
    let _ = c.shutdown(std::net::Shutdown::Write);
    let mut out = String::new();
    let _ = c.read_to_string(&mut out);
    out
}

fn lane(rules: Rules) -> (Lane, lightair_helpers::Receiver<Event>) {
    let (tx, rx) = events();
    (Lane::open("io.test.helper", rules, tx).unwrap(), rx)
}

#[test]
fn a_declared_local_port_is_reached_through_connect() {
    let port = server(b"HELLO");
    let (l, rx) = lane(Rules { localhost_ports: vec![port], ..Rules::default() });
    let out = ask(&l, &format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\nping\r\n\r\n"));
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert!(out.contains("HELLO"), "nothing came back through the tunnel: {out}");
    let e = rx.recv().unwrap();
    assert!(e.allowed && e.helper == "io.test.helper", "{e:?}");
}

#[test]
fn a_local_port_it_didnt_declare_is_refused_and_reported() {
    let port = server(b"SECRET");
    let (l, rx) = lane(Rules { localhost_ports: vec![port.wrapping_add(1)], ..Rules::default() });
    let out = ask(&l, &format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\n"));
    assert!(out.starts_with("HTTP/1.1 403") && !out.contains("SECRET"), "{out}");
    let e = rx.recv().unwrap();
    assert!(!e.allowed && e.port == port, "{e:?}");
}

#[test]
fn a_declared_name_that_turns_out_to_be_this_computer_still_needs_the_port() {
    let port = server(b"SECRET");
    // "localhost" declared as a host: it resolves to loopback, so the local-port rule decides
    let (l, _rx) = lane(Rules { hosts: vec!["localhost".into()], ..Rules::default() });
    let out = ask(&l, &format!("CONNECT localhost:{port} HTTP/1.1\r\n\r\n"));
    assert!(out.starts_with("HTTP/1.1 403"), "{out}");
}

#[test]
fn a_home_network_address_needs_the_home_network_permission() {
    let (l, rx) = lane(Rules { any_host: true, ..Rules::default() });
    let out = ask(&l, "CONNECT 192.168.1.20:80 HTTP/1.1\r\n\r\n");
    assert!(out.starts_with("HTTP/1.1 403"), "{out}");
    assert!(!rx.recv().unwrap().allowed);
}

#[test]
fn a_public_address_it_didnt_declare_is_refused() {
    let (l, rx) = lane(Rules { hosts: vec!["api.example.com".into()], ..Rules::default() });
    let out = ask(&l, "CONNECT 203.0.113.9:443 HTTP/1.1\r\n\r\n");
    assert!(out.starts_with("HTTP/1.1 403"), "{out}");
    let e = rx.recv().unwrap();
    assert!(!e.allowed && e.host == "203.0.113.9", "{e:?}");
}

#[test]
fn plain_http_is_forwarded_without_the_proxy_headers() {
    let port = server(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
    let (l, _rx) = lane(Rules { localhost_ports: vec![port], ..Rules::default() });
    let out = ask(
        &l,
        &format!("GET http://127.0.0.1:{port}/x HTTP/1.1\r\nHost: 127.0.0.1\r\nProxy-Authorization: secret\r\n\r\n"),
    );
    assert!(out.starts_with("HTTP/1.1 200"), "{out}");
    assert!(out.contains("GET /x HTTP/1.1"), "not forwarded in origin form: {out}");
    assert!(!out.contains("secret"), "a proxy header reached the server: {out}");
}

#[test]
fn something_that_isnt_a_proxy_request_is_refused() {
    let (l, _rx) = lane(Rules { any_host: true, ..Rules::default() });
    assert!(ask(&l, "GET /etc/passwd HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 400"));
}
