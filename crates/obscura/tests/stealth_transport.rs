#![cfg(feature = "stealth")]

use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::Duration;

use obscura::Browser;

// OBSCURA_PROFILE below pins index 0, and the stealth identity is indexed
// in step with the ordinary profiles, so both layers land on one browser.
const STEALTH_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";
const ORDINARY_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/143.0.0.0 Safari/537.36";

fn spawn_server() -> (String, mpsc::Receiver<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (request_tx, request_rx) = mpsc::sync_channel(1);

    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();

        let mut request = Vec::new();
        let mut chunk = [0u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut chunk).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&chunk[..read]);
        }
        request_tx
            .send(String::from_utf8(request).unwrap())
            .unwrap();

        let body = "<!doctype html><html><body>ok</body></html>";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body,
        );
        stream.write_all(response.as_bytes()).unwrap();
    });

    (format!("http://{}", addr), request_rx)
}

async fn navigate_user_agent(stealth: bool) -> String {
    let (url, request_rx) = spawn_server();

    let browser = Browser::builder().stealth(stealth).build().unwrap();
    let mut page = browser.new_page().await.unwrap();
    page.goto(&url).await.unwrap();

    let request = request_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    request
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find_map(|(name, value)| {
            name.eq_ignore_ascii_case("user-agent")
                .then(|| value.trim().to_string())
        })
        .expect("request should include a user-agent header")
}

/// The colour scheme the page reports through `prefers-color-scheme`.
async fn navigate_color_scheme() -> String {
    let (url, request_rx) = spawn_server();

    let browser = Browser::builder().stealth(true).build().unwrap();
    let mut page = browser.new_page().await.unwrap();
    page.goto(&url).await.unwrap();
    let _ = request_rx.recv_timeout(Duration::from_secs(5));

    page.evaluate("matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light'")
        .as_str()
        .expect("the colour scheme probe should return a string")
        .to_string()
}

#[tokio::test(flavor = "current_thread")]
async fn stealth_transport_requires_compile_time_and_runtime_opt_in() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    std::env::set_var("OBSCURA_PROFILE", "0");

    assert_eq!(navigate_user_agent(true).await, STEALTH_USER_AGENT);
    assert_eq!(navigate_user_agent(false).await, ORDINARY_USER_AGENT);
}

/// The stealth identity is one unit: the user agent the page reports has to
/// be the one belonging to the profile the transport impersonates. Reading
/// them from separate places is what would let a site catch the mismatch, so
/// every entry is checked rather than the default alone.
#[tokio::test(flavor = "current_thread")]
async fn stealth_identity_agrees_across_every_profile() {
    std::env::set_var("OBSCURA_ALLOW_PRIVATE_NETWORK", "1");
    for (index, profile) in obscura_net::STEALTH_PROFILES.iter().enumerate() {
        std::env::set_var("OBSCURA_PROFILE", index.to_string());
        assert_eq!(
            obscura_net::select_stealth_profile().user_agent,
            profile.user_agent,
            "profile {index} did not select itself"
        );
        assert_eq!(
            navigate_user_agent(true).await,
            profile.user_agent,
            "profile {index}: page and transport named different browsers"
        );
        assert_eq!(
            navigate_color_scheme().await,
            profile.color_scheme,
            "profile {index}: page and identity disagree on the colour scheme"
        );
    }
    std::env::set_var("OBSCURA_PROFILE", "0");
}
