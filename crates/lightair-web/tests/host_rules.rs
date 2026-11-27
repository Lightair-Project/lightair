//! A page from a package reaches its own origin and the hosts its package declared, nothing else.

use lightair_web::{HostRules, PageAllow};

fn rules(page: &str, hosts: &[&str]) -> HostRules {
    HostRules::new(
        page,
        &PageAllow { owner: "io.github.nova.w".into(), hosts: hosts.iter().map(|h| h.to_string()).collect() },
    )
}

#[test]
fn a_page_reaches_its_own_origin() {
    let r = rules("http://127.0.0.1:4848/w/abc/index.html", &[]);
    assert!(r.allows("http://127.0.0.1:4848/w/abc/main.js"));
    assert!(r.allows("ws://127.0.0.1:4848/socket"), "its own server's socket");
}

#[test]
fn another_port_on_the_same_host_isnt_its_origin() {
    let r = rules("http://127.0.0.1:4848/w/index.html", &[]);
    assert!(!r.allows("http://127.0.0.1:4849/admin"));
}

#[test]
fn an_undeclared_host_is_refused() {
    let r = rules("http://127.0.0.1:4848/w/index.html", &["api.example.com"]);
    assert!(r.allows("https://api.example.com/v1/alerts"));
    assert!(!r.allows("https://collect.example.net/beacon"));
}

#[test]
fn a_wildcard_covers_subdomains_and_not_lookalikes() {
    let r = rules("http://127.0.0.1:4848/w/index.html", &["*.example.com"]);
    assert!(r.allows("https://cdn.example.com/x.png"));
    assert!(!r.allows("https://example.com.evil.net/x"));
    assert!(!r.allows("https://notexample.com/x"));
}

#[test]
fn a_star_allows_any_web_host_but_not_other_schemes() {
    let r = rules("http://127.0.0.1:4848/w/index.html", &["*"]);
    assert!(r.allows("https://anything.example/x"));
    assert!(!r.allows("file:///etc/passwd"));
    assert!(!r.allows("ftp://files.example/x"));
}

#[test]
fn inline_data_is_always_allowed() {
    let r = rules("http://127.0.0.1:4848/w/index.html", &[]);
    assert!(r.allows("data:image/png;base64,AAAA"));
    assert!(r.allows("blob:http://127.0.0.1:4848/1234"));
}

#[test]
fn a_file_page_reads_only_its_own_folder() {
    let r = rules("file:///Users/me/Packages/w/1.0.0/widgets/w/index.html", &[]);
    assert!(r.allows("file:///Users/me/Packages/w/1.0.0/widgets/w/style.css"));
    assert!(!r.allows("file:///Users/me/.ssh/id_ed25519"));
    assert!(!r.allows("file:///Users/me/Packages/w/1.0.0/widgets/w/../../../../../.ssh/id_ed25519"));
}

#[test]
fn userinfo_in_an_address_doesnt_disguise_the_host() {
    let r = rules("http://127.0.0.1:4848/w/index.html", &["api.example.com"]);
    assert!(!r.allows("https://api.example.com@collect.example.net/x"));
}
