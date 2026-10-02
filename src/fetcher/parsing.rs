//! The parsing half of the fetcher's work: everything that interprets
//! untrusted bytes once they have been downloaded.
//!
//! Downloading and parsing are kept apart so they can run in different
//! processes. In the isolated fetcher, the worker that holds the network
//! clients hands each [`ParseTask`] to a parser process, which carries it
//! out on a pool of threads and has no network access, no filesystem
//! access and no credentials (see
//! [`crate::process::feed_fetcher`]), so a bug in an XML or HTML parser
//! cannot be used to reach the network or read another feed's secrets.
//! [`Parsers`] hides where the work happens, so the callers that download
//! things do not change.

use super::{parse_feed, svg::sanitize_svg, ParseOutcome};
use serde::{Deserialize, Serialize};

/// Work for a parser: bytes that were downloaded (or read from a file),
/// and what to find in them.
#[derive(Debug, Serialize, Deserialize)]
pub enum ParseTask {
    /// Parse a feed body as Atom or RSS.
    Feed {
        feed_id: i64,
        /// Encoded as one length and the raw bytes, not byte by byte.
        #[serde(with = "serde_bytes")]
        body: Vec<u8>,
    },

    /// Rebuild an SVG image so that it is safe to serve; see
    /// [`sanitize_svg`].
    Svg {
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
    },

    /// Find the icons a web page links to; see
    /// [`super::assets::extract_icon_links`].
    PageIcons {
        #[serde(with = "serde_bytes")]
        html: Vec<u8>,
        /// The URL the page was served from, for resolving relative links.
        page_url: String,
    },

    /// Find the images an entry's HTML `content` shows, resolved against
    /// `base`; see [`super::assets::extract_asset_urls`].
    Images { content: String, base: String },
}

/// The result of a [`ParseTask`], variant for variant.
#[derive(Debug, Serialize, Deserialize)]
pub enum ParseOutput {
    Feed(ParseOutcome),
    /// The rebuilt image, or `None` if it could not be made safe.
    Svg(#[serde(with = "serde_bytes")] Option<Vec<u8>>),
    PageIcons(Vec<String>),
    Images(Vec<String>),
}

/// What the parser process sends back for a task, or what its supervisor
/// sends in its place.
#[derive(Debug, Serialize, Deserialize)]
pub enum ParseReply {
    /// The task was completed.
    Done(ParseOutput),

    /// The task could not be completed, for a reason that says nothing
    /// about its input: the parser panicked on something other than a
    /// feed, the reply was too large to send, or the parser was not
    /// available in time.
    Failed { message: String },

    /// The parser process died, or was killed for taking too long over a
    /// task, while it had this task and `in_flight - 1` others in hand;
    /// `why` says how. Sent by the supervisor, never by the parser.
    ///
    /// Any one of those tasks may be to blame, so this alone blames none
    /// of them: the task is retried on its own to find out.
    Exited { in_flight: u32, why: String },
}

/// Why [`Parsers::run`] produced no output.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ParseFailure {
    /// The task killed the parser process while it was the only task in
    /// hand, even when retried: its input is to blame. Carries how the
    /// parser died.
    #[error("{0}")]
    Crashed(String),

    /// The parser could not serve the task; see [`ParseReply::Failed`].
    #[error("{0}")]
    Unavailable(String),
}

/// Carry out `task` in the calling thread.
///
/// A panic while parsing a feed is reported as a body that is not a feed,
/// as it always has been; any other panic is left to the caller.
pub fn run(task: ParseTask) -> ParseOutput {
    match task {
        ParseTask::Feed { feed_id, body } => {
            let start = std::time::Instant::now();
            let feed =
                std::panic::catch_unwind(|| parse_feed(feed_id, &body)).unwrap_or_else(|_| {
                    tracing::warn!("Feed {}: parser panicked", feed_id);
                    None
                });
            ParseOutput::Feed(ParseOutcome {
                feed,
                seconds: start.elapsed().as_secs_f64(),
            })
        }
        ParseTask::Svg { bytes } => ParseOutput::Svg(sanitize_svg(&bytes)),
        ParseTask::PageIcons { html, page_url } => {
            let icons = match reqwest::Url::parse(&page_url) {
                Ok(url) => super::assets::extract_icon_links(&html, &url)
                    .into_iter()
                    .map(String::from)
                    .collect(),
                Err(_) => Vec::new(),
            };
            ParseOutput::PageIcons(icons)
        }
        ParseTask::Images { content, base } => {
            let urls = match reqwest::Url::parse(&base) {
                Ok(base) => super::assets::extract_asset_urls(&content, &base)
                    .into_iter()
                    .map(String::from)
                    .collect(),
                Err(_) => Vec::new(),
            };
            ParseOutput::Images(urls)
        }
    }
}

/// Where [`ParseTask`]s are carried out.
#[derive(Clone)]
pub enum Parsers {
    /// On this process's blocking thread pool.
    InProcess,

    /// In the feed fetcher's parser process, reached through the
    /// supervisor.
    #[cfg(unix)]
    Pool(crate::process::feed_fetcher::ParserClient),
}

impl Parsers {
    /// Carry out `task`.
    ///
    /// # Errors
    ///
    /// [`ParseFailure::Crashed`] if the task killed the parser that had
    /// it, and [`ParseFailure::Unavailable`] if it could not be served at
    /// all.
    pub async fn run(&self, task: ParseTask) -> Result<ParseOutput, ParseFailure> {
        match self {
            Parsers::InProcess => tokio::task::spawn_blocking(move || run(task))
                .await
                .map_err(|e| ParseFailure::Unavailable(format!("parser failed: {e}"))),
            #[cfg(unix)]
            Parsers::Pool(pool) => pool.run(task).await,
        }
    }

    /// Parse a feed body.
    ///
    /// # Errors
    ///
    /// As for [`Self::run`].
    pub async fn feed(&self, feed_id: i64, body: Vec<u8>) -> Result<ParseOutcome, ParseFailure> {
        match self.run(ParseTask::Feed { feed_id, body }).await? {
            ParseOutput::Feed(outcome) => Ok(outcome),
            other => Err(mismatch(&other)),
        }
    }

    /// Rebuild an SVG image so it is safe to serve, or `None` if it
    /// cannot be.
    ///
    /// # Errors
    ///
    /// As for [`Self::run`].
    pub async fn svg(&self, bytes: Vec<u8>) -> Result<Option<Vec<u8>>, ParseFailure> {
        match self.run(ParseTask::Svg { bytes }).await? {
            ParseOutput::Svg(clean) => Ok(clean),
            other => Err(mismatch(&other)),
        }
    }

    /// The icons the page `html`, served from `page_url`, links to, best
    /// first.
    ///
    /// # Errors
    ///
    /// As for [`Self::run`].
    pub async fn page_icons(
        &self,
        html: Vec<u8>,
        page_url: String,
    ) -> Result<Vec<String>, ParseFailure> {
        match self.run(ParseTask::PageIcons { html, page_url }).await? {
            ParseOutput::PageIcons(icons) => Ok(icons),
            other => Err(mismatch(&other)),
        }
    }

    /// The images an entry's HTML `content` shows, resolved against
    /// `base`.
    ///
    /// # Errors
    ///
    /// As for [`Self::run`].
    pub async fn images(&self, content: String, base: String) -> Result<Vec<String>, ParseFailure> {
        match self.run(ParseTask::Images { content, base }).await? {
            ParseOutput::Images(urls) => Ok(urls),
            other => Err(mismatch(&other)),
        }
    }
}

fn mismatch(output: &ParseOutput) -> ParseFailure {
    ParseFailure::Unavailable(format!("the parser answered a different task: {output:?}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    const RSS: &[u8] = br#"<rss version="2.0"><channel><title>t</title><link>http://x/</link>
        <description>d</description><item><title>hi</title><guid>g1</guid></item>
        </channel></rss>"#;

    #[tokio::test]
    async fn in_process_parsers_serve_every_task() {
        let parsers = Parsers::InProcess;
        let outcome = parsers.feed(1, RSS.to_vec()).await.unwrap();
        assert_eq!(outcome.feed.unwrap().entry_count(), 1);
        assert!(parsers
            .feed(1, b"not xml".to_vec())
            .await
            .unwrap()
            .feed
            .is_none());

        let clean = parsers
            .svg(br#"<svg><script>x</script><rect/></svg>"#.to_vec())
            .await
            .unwrap()
            .unwrap();
        assert!(!String::from_utf8(clean).unwrap().contains("script"));
        assert!(parsers.svg(b"<html/>".to_vec()).await.unwrap().is_none());

        let icons = parsers
            .page_icons(
                br#"<link rel="icon" href="/i.png">"#.to_vec(),
                "http://x/".into(),
            )
            .await
            .unwrap();
        assert_eq!(icons, ["http://x/i.png"]);

        let images = parsers
            .images(r#"<img src="a.png">"#.into(), "http://x/p/".into())
            .await
            .unwrap();
        assert_eq!(images, ["http://x/p/a.png"]);
    }

    #[test]
    fn tasks_with_bad_urls_find_nothing() {
        let out = run(ParseTask::Images {
            content: r#"<img src="a.png">"#.into(),
            base: "not a url".into(),
        });
        assert!(matches!(out, ParseOutput::Images(v) if v.is_empty()));
        let out = run(ParseTask::PageIcons {
            html: br#"<link rel="icon" href="/i.png">"#.to_vec(),
            page_url: "not a url".into(),
        });
        assert!(matches!(out, ParseOutput::PageIcons(v) if v.is_empty()));
    }
}
