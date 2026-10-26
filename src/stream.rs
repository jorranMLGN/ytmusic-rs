//! A track's audio read in order while it downloads, one bounded range at a time.
//!
//! The stream host serves a plain GET at a trickle and drops long connections halfway without
//! saying so, but it answers a bounded `Range` request at full speed. So the body is asked for
//! a mebibyte at a time, each read has a deadline, and a range that stalls or breaks is asked
//! for again from the byte it stopped at. A url the host refuses partway through, which is what
//! a cold start token gets after its first megabyte or so, is renewed through the client that
//! handed it out, with a minted token when that client needs one, and read on from the same
//! byte.

use std::sync::{Arc, Weak};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use bytes::Bytes;

use crate::client::YtMusic;
use crate::context::Client;
use crate::models::AudioFormat;

/// How much one request asks for.
const CHUNK: u64 = 1024 * 1024;
/// How long a request or a read may go without a byte before the range is asked for again.
const STALL: Duration = Duration::from_secs(8);
/// How many times in a row the same byte may be asked for before the stream gives up.
const RETRIES: u32 = 3;

/// The body of one audio format, handed out chunk by chunk in file order.
pub struct AudioStream {
    http: reqwest::Client,
    url: String,
    agent: &'static str,
    total: Option<u64>,
    /// The next byte to hand out.
    offset: u64,
    /// The last byte the open request covers.
    end: u64,
    response: Option<reqwest::Response>,
    /// How many attempts in a row have failed at `offset`.
    failures: u32,
    /// How to ask for a fresh url once the host refuses this one. `None` for a stream opened
    /// without a client, which then ends at the first refusal.
    renewal: Option<Renewal>,
}

/// Which of a video id's streams a renewal asks the player for.
#[derive(Clone, Copy)]
enum Kind {
    Audio,
    Video,
}

/// What a stream needs to ask the player for a fresh url of the same file.
struct Renewal {
    client: Weak<YtMusic>,
    video_id: String,
    itag: u32,
    kind: Kind,
    /// The client that handed out the url.
    from: Client,
    /// The byte the last renewal was made at. A renewed url refused at that same byte ends the
    /// stream instead of renewing forever.
    at: Option<u64>,
}

impl AudioStream {
    /// Sends the first range request and checks the stream host accepts it. A refusal here is
    /// the same answer a probe would have got, so a caller can fall back to another url.
    pub async fn open(http: reqwest::Client, format: &AudioFormat) -> Result<Self> {
        Self::open_url(
            http,
            format.url.clone(),
            format.user_agent,
            format.content_length,
        )
        .await
    }

    /// The same as [`open`](Self::open) for any file, given its url, the agent the url is bound
    /// to and its length when known. A video stream goes through here.
    pub(crate) async fn open_url(
        http: reqwest::Client,
        url: String,
        agent: &'static str,
        total: Option<u64>,
    ) -> Result<Self> {
        let mut stream = Self {
            http,
            url,
            agent,
            total,
            offset: 0,
            end: 0,
            response: None,
            failures: 0,
            renewal: None,
        };
        stream.request().await?;
        Ok(stream)
    }

    /// Lets the stream renew its url through `client` when the host refuses it partway through.
    /// `from` is the client that handed the url out, which decides how a new one is asked for.
    pub(crate) fn renewing(
        self,
        client: &Arc<YtMusic>,
        video_id: &str,
        itag: u32,
        from: Client,
    ) -> Self {
        self.renewing_kind(client, video_id, itag, Kind::Audio, from)
    }

    /// Like [`renewing`](Self::renewing), for the video-only stream `itag`, which only the
    /// signed-in client renews.
    pub(crate) fn renewing_video(self, client: &Arc<YtMusic>, video_id: &str, itag: u32) -> Self {
        self.renewing_kind(client, video_id, itag, Kind::Video, Client::Music)
    }

    fn renewing_kind(
        mut self,
        client: &Arc<YtMusic>,
        video_id: &str,
        itag: u32,
        kind: Kind,
        from: Client,
    ) -> Self {
        self.renewal = Some(Renewal {
            client: Arc::downgrade(client),
            video_id: video_id.to_string(),
            itag,
            kind,
            from,
            at: None,
        });
        self
    }

    /// A new stream over the same file that starts at byte `at`, for a seek. It keeps the url,
    /// agent, length and renewal (with a fresh handle on the client), opens nothing until its
    /// first `chunk`, and leaves this stream untouched.
    pub fn resumed(&self, at: u64) -> AudioStream {
        AudioStream {
            http: self.http.clone(),
            url: self.url.clone(),
            agent: self.agent,
            total: self.total,
            offset: at,
            end: 0,
            response: None,
            failures: 0,
            renewal: self.renewal.as_ref().map(|renewal| Renewal {
                client: Weak::clone(&renewal.client),
                video_id: renewal.video_id.clone(),
                itag: renewal.itag,
                kind: renewal.kind,
                from: renewal.from,
                at: None,
            }),
        }
    }

    /// The length of the body, when the player response gave one.
    pub fn total(&self) -> Option<u64> {
        self.total
    }

    /// The next part of the body, or `None` once all of it has been handed out. Fails when the
    /// stream host refuses a range and the url cannot be renewed, or the same byte has stalled
    /// `RETRIES` times.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>> {
        loop {
            if self.total.is_some_and(|total| self.offset >= total) {
                return Ok(None);
            }
            if self.response.is_none() {
                match self.request().await {
                    Ok(()) => {}
                    Err(error) if error.is::<Refused>() => {
                        self.renew(error).await?;
                        continue;
                    }
                    Err(error) => {
                        self.retry(error)?;
                        continue;
                    }
                }
            }
            let Some(response) = self.response.as_mut() else {
                continue;
            };
            match tokio::time::timeout(STALL, response.chunk()).await {
                Ok(Ok(Some(chunk))) => {
                    self.offset += chunk.len() as u64;
                    self.failures = 0;
                    return Ok(Some(chunk));
                }
                Ok(Ok(None)) => {
                    self.response = None;
                    // Without a length, a range that ended early is the end of the file.
                    if self.total.is_none() && self.offset <= self.end {
                        return Ok(None);
                    }
                }
                Ok(Err(error)) => self.retry(anyhow::Error::new(error))?,
                Err(elapsed) => self.retry(anyhow::Error::new(elapsed))?,
            }
        }
    }

    /// Asks for the next range from `offset`.
    async fn request(&mut self) -> Result<()> {
        let end = match self.total {
            Some(total) => (self.offset + CHUNK - 1).min(total.saturating_sub(1)),
            None => self.offset + CHUNK - 1,
        };
        let sent = request(&self.http, &self.url, self.agent, self.offset, end);
        let response = tokio::time::timeout(STALL, sent)
            .await
            .context("the stream host did not answer")??;
        self.end = end;
        self.response = Some(response);
        Ok(())
    }

    /// Swaps in a fresh url for the same file after the host refused this one, or hands back
    /// `refused` when there is no way to renew or a renewed url was already refused here.
    async fn renew(&mut self, refused: anyhow::Error) -> Result<()> {
        let Some(renewal) = self.renewal.as_mut() else {
            return Err(refused);
        };
        if renewal.at == Some(self.offset) {
            return Err(refused);
        }
        let Some(client) = renewal.client.upgrade() else {
            return Err(refused);
        };
        renewal.at = Some(self.offset);
        log::debug!(
            "player: {} was refused at byte {}, renewing the url",
            renewal.video_id,
            self.offset
        );
        let (url, agent, length) = match renewal.kind {
            Kind::Audio => client
                .renewed_audio(&renewal.video_id, renewal.itag, renewal.from)
                .await
                .map(|format| (format.url, format.user_agent, format.content_length)),
            Kind::Video => client
                .renewed_video(&renewal.video_id, renewal.itag)
                .await
                .map(|format| (format.url, format.user_agent, format.content_length)),
        }
        .map_err(|error| refused.context(format!("cannot renew the url: {error:#}")))?;
        if length != self.total {
            bail!(
                "the renewed url for {} is {:?} bytes long, not {:?}",
                renewal.video_id,
                length,
                self.total
            );
        }
        self.url = url;
        self.agent = agent;
        Ok(())
    }

    /// Drops the broken range so the next read asks again from `offset`, or gives up.
    fn retry(&mut self, error: anyhow::Error) -> Result<()> {
        self.response = None;
        self.failures += 1;
        if self.failures > RETRIES {
            return Err(error.context(format!(
                "the stream stalled at byte {} {RETRIES} times",
                self.offset
            )));
        }
        log::debug!(
            "player: the stream broke at byte {}, asking again: {error:#}",
            self.offset
        );
        Ok(())
    }
}

/// The stream host answered a range with an error status. Asking again gets the same answer.
#[derive(Debug)]
pub(crate) struct Refused(reqwest::StatusCode);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "stream host refused the download with status {} \
             (the proof-of-origin token it carried was not accepted)",
            self.0
        )
    }
}

impl std::error::Error for Refused {}

/// Sends one range request and hands back the response once its status says the body follows.
pub(crate) async fn request(
    http: &reqwest::Client,
    url: &str,
    agent: &'static str,
    start: u64,
    end: u64,
) -> Result<reqwest::Response> {
    let response = http
        .get(url)
        .header("Range", format!("bytes={start}-{end}"))
        .header("User-Agent", agent)
        .header("Accept-Encoding", "identity")
        .header("Origin", "https://www.youtube.com")
        .header("Referer", "https://www.youtube.com/")
        .send()
        .await
        .context("cannot reach stream host")?;
    let status = response.status();
    if !status.is_success() {
        bail!(Refused(status));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    /// Serves `body` with range support on a local port and records each `Range` header.
    async fn serve(body: Arc<Vec<u8>>) -> (String, Arc<std::sync::Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/audio", listener.local_addr().unwrap());
        let ranges = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = ranges.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let (body, seen) = (body.clone(), seen.clone());
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 8192];
                    let read = socket.read(&mut buffer).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                    let range = request
                        .lines()
                        .find_map(|line| {
                            line.to_lowercase()
                                .strip_prefix("range: bytes=")
                                .map(str::to_string)
                        })
                        .unwrap_or_default();
                    seen.lock().unwrap().push(range.clone());
                    let (start, end) = range.split_once('-').unwrap_or(("0", ""));
                    let start: usize = start.parse().unwrap_or(0);
                    let last = body.len() - 1;
                    let end: usize = end.parse().unwrap_or(last).min(last);
                    let part = &body[start..=end];
                    let head = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\
                         Content-Range: bytes {start}-{end}/{}\r\nConnection: close\r\n\r\n",
                        part.len(),
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(part).await;
                });
            }
        });
        (url, ranges)
    }

    #[tokio::test]
    async fn a_resumed_stream_asks_from_its_offset() {
        let body: Arc<Vec<u8>> = Arc::new((0..3 * 1024 * 1024).map(|n| (n % 251) as u8).collect());
        let (url, ranges) = serve(body.clone()).await;
        let format = AudioFormat {
            itag: 140,
            url,
            mime: "audio/mp4".into(),
            codec: "mp4a.40.2".into(),
            bitrate: 128_000,
            duration: None,
            content_length: Some(body.len() as u64),
            loudness_db: None,
            user_agent: "test",
        };
        let first = AudioStream::open(reqwest::Client::new(), &format)
            .await
            .unwrap();
        let at = 2 * 1024 * 1024u64;
        let mut resumed = first.resumed(at);
        let chunk = resumed.chunk().await.unwrap().unwrap();
        let at = at as usize;
        assert_eq!(&chunk[..], &body[at..at + chunk.len()]);
        let ranges = ranges.lock().unwrap();
        assert!(ranges.last().unwrap().starts_with("2097152-"), "{ranges:?}");
    }
}
