//! Telegram channel: one Bot API `sendMessage` call to one chat.
//!
//! The message is plain text (no `parse_mode`), so titles and product names
//! never need escaping. Telegram turns the link on the last line into a
//! clickable URL by itself.

use std::time::Duration;

use super::{Notification, Notifier, NotifyError, read_capped, safe_transport_detail, summarize};

const TELEGRAM: &str = "Telegram";
const API: &str = "https://api.telegram.org";
/// Same cap and reason as Bark's: a hung server must not hold an alert.
const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BODY: usize = 8 << 10;

/// Sends alerts through a Telegram bot.
///
/// Not `Debug`: the bot token is a full credential for the bot.
#[derive(Clone)]
pub struct Telegram {
    token: String,
    chat_id: String,
    http: reqwest::Client,
    api: String,
}

impl Telegram {
    /// `token` is what @BotFather gives (`123456:ABC...`). `chat_id` is the
    /// numeric chat ID (negative for groups) or `@channelname`. Both empty
    /// means "not configured" and [`Notifier::notify`] sends nothing.
    pub fn new(token: String, chat_id: String, http: reqwest::Client) -> Self {
        Self {
            token: token.trim().to_string(),
            chat_id: chat_id.trim().to_string(),
            http,
            api: API.to_string(),
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.token.is_empty() && !self.chat_id.is_empty()
    }

    fn error(&self, detail: &str) -> NotifyError {
        NotifyError::Config {
            channel: TELEGRAM.to_string(),
            detail: detail.to_string(),
        }
    }
}

impl Notifier for Telegram {
    fn name(&self) -> &str {
        TELEGRAM
    }

    async fn notify(&self, n: &Notification) -> Result<(), NotifyError> {
        if self.token.is_empty() && self.chat_id.is_empty() {
            return Ok(());
        }
        if !self.is_configured() {
            return Err(self.error("set both the bot token and the chat ID"));
        }
        let text = [
            n.title.trim(),
            n.body.trim(),
            n.url.as_deref().unwrap_or("").trim(),
        ]
        .into_iter()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
        if text.is_empty() {
            return Err(self.error("the notification has no text"));
        }

        // The token is part of the URL path, so transport errors go through
        // safe_transport_detail, which never quotes the URL.
        let mut resp = self
            .http
            .post(format!("{}/bot{}/sendMessage", self.api, self.token))
            .timeout(TIMEOUT)
            .json(&serde_json::json!({ "chat_id": self.chat_id, "text": text }))
            .send()
            .await
            .map_err(|e| NotifyError::Transport {
                channel: TELEGRAM.to_string(),
                detail: safe_transport_detail(&e),
            })?;
        let status = resp.status();
        let body = read_capped(&mut resp, MAX_BODY, TELEGRAM).await?;
        if status.is_success() {
            return Ok(());
        }

        // Errors look like {"ok":false,"error_code":400,"description":"Bad
        // Request: chat not found"}; the description is the useful part.
        let description = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("description")?.as_str().map(str::to_string))
            .unwrap_or_else(|| summarize(&body));
        Err(NotifyError::Rejected {
            channel: TELEGRAM.to_string(),
            status: status.as_u16(),
            body: description.replace(&self.token, "***"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const TOKEN: &str = "123456:SECRET-token";

    /// Answers one request with `status` and a JSON `body`; the handle yields
    /// the raw request.
    async fn serve_once(
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        let join = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut raw = Vec::new();
            let mut buf = [0; 4096];
            loop {
                let read = stream.read(&mut buf).await.unwrap();
                raw.extend_from_slice(&buf[..read]);
                let text = String::from_utf8_lossy(&raw);
                let complete = text.split_once("\r\n\r\n").is_some_and(|(head, body)| {
                    let length = head.lines().find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")?
                            .trim()
                            .parse::<usize>()
                            .ok()
                    });
                    body.len() >= length.unwrap_or(0)
                });
                if complete || read == 0 {
                    break;
                }
            }
            stream.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(raw).unwrap()
        });
        (api, join)
    }

    fn telegram(api: &str) -> Telegram {
        let mut telegram = Telegram::new(
            format!(" {TOKEN} "),
            "-10042".to_string(),
            reqwest::Client::new(),
        );
        telegram.api = api.to_string();
        telegram
    }

    fn hit() -> Notification {
        Notification::new("In stock", "Store A iPhone 17 & 256GB / black")
            .with_url("https://www.apple.com.cn/shop/bag?a=1&b=2")
    }

    #[tokio::test]
    async fn sends_title_body_and_link_to_the_chat() {
        let (api, server) = serve_once("200 OK", r#"{"ok":true}"#).await;
        telegram(&api).notify(&hit()).await.unwrap();

        let raw = server.await.unwrap();
        assert!(
            raw.starts_with(&format!("POST /bot{TOKEN}/sendMessage HTTP/1.1")),
            "{raw}"
        );
        let body: serde_json::Value =
            serde_json::from_str(raw.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["chat_id"], "-10042");
        assert_eq!(
            body["text"],
            "In stock\nStore A iPhone 17 & 256GB / black\nhttps://www.apple.com.cn/shop/bag?a=1&b=2"
        );
    }

    #[tokio::test]
    async fn rejection_reports_the_description_without_the_token() {
        let (api, server) = serve_once(
            "401 Unauthorized",
            r#"{"ok":false,"error_code":401,"description":"Unauthorized: 123456:SECRET-token"}"#,
        )
        .await;
        let err = telegram(&api).notify(&hit()).await.unwrap_err();
        server.await.unwrap();

        assert!(
            matches!(err, NotifyError::Rejected { status: 401, .. }),
            "{err:?}"
        );
        let text = err.to_string();
        assert!(text.contains("Unauthorized: ***"), "{text}");
        assert!(!text.contains("SECRET"), "{text}");
    }

    #[tokio::test]
    async fn transport_errors_do_not_quote_the_url() {
        // Bind then drop: nothing listens on this port any more.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);

        let err = telegram(&api).notify(&hit()).await.unwrap_err();
        assert!(matches!(err, NotifyError::Transport { .. }), "{err:?}");
        assert!(!err.to_string().contains("SECRET"), "{err}");
    }

    #[tokio::test]
    async fn unconfigured_sends_nothing_and_half_configured_is_an_error() {
        let http = reqwest::Client::new();
        let off = Telegram::new(" ".into(), String::new(), http.clone());
        assert!(!off.is_configured());
        off.notify(&hit()).await.unwrap();

        let half = Telegram::new(TOKEN.into(), String::new(), http);
        let err = half.notify(&hit()).await.unwrap_err();
        assert!(matches!(err, NotifyError::Config { .. }), "{err:?}");
    }
}
