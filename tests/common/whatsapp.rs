//! A fake WhatsApp gateway for suites that assert on what was sent.
//!
//! `delivery::whatsapp::send_message` posts to `WHATSAPP_SERVICE_URL`; this
//! points that variable at a local listener that keeps every message body.
//! nextest runs each test in its own process, so setting the variable is
//! scoped to the test that calls [`gateway`].

pub type Messages = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

/// A WhatsApp gateway (`WHATSAPP_SERVICE_URL`) that keeps every message it
/// is asked to send.
pub async fn gateway() -> Messages {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    unsafe { std::env::set_var("WHATSAPP_SERVICE_URL", url) };
    let seen = Messages::default();
    let log = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let log = log.clone();
            tokio::spawn(async move {
                let mut req = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&req).to_string();
                    let Some(end) = text.find("\r\n\r\n") else {
                        continue;
                    };
                    let len = text[..end]
                        .lines()
                        .find_map(|l| {
                            let l = l.to_ascii_lowercase();
                            l.strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if req.len() >= end + 4 + len {
                        log.lock().unwrap().push(text[end + 4..].to_string());
                        break;
                    }
                }
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
                    )
                    .await;
            });
        }
    });
    seen
}

/// Waits (5 s at most) for a WhatsApp message to a number ending in `digits`.
pub async fn whatsapp_to(gateway: &Messages, digits: &str) {
    for _ in 0..100 {
        if gateway.lock().unwrap().iter().any(|m| m.contains(digits)) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("no WhatsApp to …{digits}: {:?}", gateway.lock().unwrap());
}

/// How many kept messages contain `needle`, after giving fire-and-forget
/// sends `settle_ms` to arrive.
pub async fn count_containing(gateway: &Messages, needle: &str, settle_ms: u64) -> usize {
    tokio::time::sleep(std::time::Duration::from_millis(settle_ms)).await;
    gateway
        .lock()
        .unwrap()
        .iter()
        .filter(|m| m.contains(needle))
        .count()
}
