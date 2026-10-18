use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use serde_json::{Value, json};

use crate::client::YtMusic;
use crate::context::{Client, random_string};
use crate::models::AudioFormat;
use crate::stream::AudioStream;

const CHUNK: u64 = 1024 * 1024;
const PARALLEL: usize = 4;
const MAX_STREAM_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct SignInRequired;

impl std::fmt::Display for SignInRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("youtube only serves this track to a signed-in listener")
    }
}

impl std::error::Error for SignInRequired {}

/// Which of the signed-in client's streams to take.
#[derive(Clone, Copy, Debug)]
enum Choice {
    /// Exactly this itag, as when a stream already playing needs a fresh url.
    Itag(u32),
    /// The best stream at or below this many bits per second, as [`choose`] picks it.
    Within(Option<u32>),
}

#[derive(Clone, Debug)]
pub struct Playability {
    pub status: String,
    pub reason: Option<String>,
}

impl Playability {
    pub fn ok(&self) -> bool {
        self.status == "OK"
    }

    pub fn gated(&self) -> bool {
        self.status == "LOGIN_REQUIRED"
    }
}

impl YtMusic {
    pub async fn player_response(&self, video_id: &str, client: Client) -> Result<Value> {
        let payload = json!({
            "videoId": video_id,
            "racyCheckOk": true,
            "contentCheckOk": true,
            "cpn": random_string(16),
            "playbackContext": {
                "contentPlaybackContext": {
                    "vis": 0,
                    "splay": false,
                    "lactMilliseconds": "-1",
                }
            },
        });
        self.execute_with("player", client, payload, !self.is_cookie_auth())
            .await
    }

    /// The proof-of-origin token the signed-in client sends, bound to this session rather
    /// than to one track, which is what YouTube expects from `WEB_REMIX`. Empty when the
    /// binding cannot be worked out: a token for the wrong identity is worse than none.
    async fn po_token(&self) -> String {
        let Some(binding) = self.session_binding().await else {
            log::warn!("potoken: no session binding, the stream goes out without a token");
            return String::new();
        };
        self.po_tokens.token(&binding).await
    }

    /// The stream to play `video_id` from.
    ///
    /// The guest client is asked first because its urls need no deciphering, and the url it
    /// hands back is checked before a track commits to it: a player response can name a
    /// stream the host then refuses, which is what an address YouTube has flagged looks like.
    /// Either failure falls through to the signed-in client, whose url carries a
    /// proof-of-origin token. That path works for a guest too, so a run without an account
    /// still gets the second try.
    pub async fn best_audio(&self, video_id: &str) -> Result<AudioFormat> {
        let refused = match self.guest_audio(video_id, None).await {
            Ok(format) => match self.serves(&format).await {
                true => return Ok(format),
                false => anyhow::anyhow!("the stream host refused the guest url"),
            },
            Err(error) => error,
        };
        log::debug!("player: the guest stream for {video_id} failed ({refused:#})");
        self.signed_audio(video_id, None)
            .await
            .with_context(|| format!("the guest stream failed too ({refused:#})"))
    }

    /// The best stream to play `video_id` from, already arriving, chosen as
    /// [`open_audio_within`](Self::open_audio_within) chooses with no ceiling.
    ///
    /// Unlike [`best_audio`](Self::best_audio), the first range of the body is what checks the
    /// url rather than a probe of its own, so a track that plays costs no round trip beyond the
    /// download. The stream holds the client weakly, so a url the host stops serving partway
    /// through can be renewed.
    pub async fn open_audio(
        self: &Arc<Self>,
        video_id: &str,
    ) -> Result<(AudioFormat, AudioStream)> {
        self.open_audio_within(video_id, None).await
    }

    /// The stream to play `video_id` from, already arriving, at no more than `ceiling` bits
    /// per second. A track whose every stream is above the ceiling plays its lowest one.
    ///
    /// `None` takes the best there is. For a signed-in session that asks the signed-in client
    /// first, since the guest client stops at 128 kbps and only the signed-in one offers a
    /// Premium account its 256 kbps stream. A failure there falls back to the guest client.
    pub async fn open_audio_within(
        self: &Arc<Self>,
        video_id: &str,
        ceiling: Option<u32>,
    ) -> Result<(AudioFormat, AudioStream)> {
        let signed_first = ceiling.is_none() && self.is_cookie_auth();
        if signed_first {
            match self.open_signed(video_id, None).await {
                Ok(opened) => return Ok(opened),
                Err(error) => {
                    log::debug!("player: the signed stream for {video_id} failed ({error:#})")
                }
            }
        }
        let refused = match self.guest_audio(video_id, ceiling).await {
            Ok(format) => match AudioStream::open(self.http.clone(), &format).await {
                Ok(stream) => {
                    let stream = stream.renewing(self, video_id, format.itag);
                    return Ok((format, stream));
                }
                Err(error) => error.context(format!("itag {} is not being served", format.itag)),
            },
            Err(error) => error,
        };
        if signed_first {
            return Err(refused.context("the signed stream failed too"));
        }
        log::debug!("player: the guest stream for {video_id} failed ({refused:#})");
        self.open_signed(video_id, ceiling)
            .await
            .with_context(|| format!("the guest stream failed too ({refused:#})"))
    }

    /// The signed-in client's stream for `video_id` within `ceiling`, already arriving.
    async fn open_signed(
        self: &Arc<Self>,
        video_id: &str,
        ceiling: Option<u32>,
    ) -> Result<(AudioFormat, AudioStream)> {
        let format = self.signed_audio(video_id, ceiling).await?;
        let stream = AudioStream::open(self.http.clone(), &format).await?;
        let stream = stream.renewing(self, video_id, format.itag);
        Ok((format, stream))
    }

    /// A fresh url for the `itag` stream of `video_id`, carrying a minted proof-of-origin token.
    ///
    /// This is for a stream the host stopped serving partway through, which is what happens to a
    /// cold start token after its first megabyte or so. It waits for the minter instead of sending
    /// another cold start token. It fails when no token arrives in time or the player no longer
    /// offers that itag, since a different file cannot carry on from the same byte.
    pub(crate) async fn renewed_audio(&self, video_id: &str, itag: u32) -> Result<AudioFormat> {
        let binding = self
            .session_binding()
            .await
            .context("no session binding to mint a token for")?;
        let token = self
            .po_tokens
            .await_minted(&binding)
            .await
            .context("no proof-of-origin token was minted in time")?;
        self.signed_format(video_id, &token, Choice::Itag(itag))
            .await
    }

    /// Whether the stream host will serve this url, asked with the smallest range there is.
    async fn serves(&self, format: &AudioFormat) -> bool {
        match range(&self.http, &format.url, format.user_agent, 0, 0).await {
            Ok(_) => true,
            Err(error) => {
                log::debug!(
                    "player: itag {} is not being served: {error:#}",
                    format.itag
                );
                false
            }
        }
    }

    pub async fn load_audio(&self, video_id: &str) -> Result<(AudioFormat, Vec<u8>)> {
        let started = std::time::Instant::now();
        let format = self.best_audio(video_id).await?;
        let data = self.download(&format).await?;
        log::debug!(
            "player: {video_id} loaded, {} in {:?}",
            describe(&format, data.len()),
            started.elapsed()
        );
        Ok((format, data))
    }

    async fn guest_audio(&self, video_id: &str, ceiling: Option<u32>) -> Result<AudioFormat> {
        let response = self.guest_player(video_id).await?;
        let audio: Vec<AudioFormat> = playable(&response, video_id)?
            .iter()
            .filter_map(|format| audio_format(format, Client::VisionOs))
            .collect();
        log::debug!(
            "player: {} offers {video_id} as {}",
            Client::VisionOs.name(),
            offered(audio.iter().map(|format| (format.itag, format.bitrate)))
        );
        choose(audio, ceiling, |format| (&format.mime, format.bitrate))
            .with_context(|| format!("no direct audio stream for {video_id}"))
    }

    async fn signed_audio(&self, video_id: &str, ceiling: Option<u32>) -> Result<AudioFormat> {
        let token = self.po_token().await;
        self.signed_format(video_id, &token, Choice::Within(ceiling))
            .await
    }

    /// The signed-in client's stream for `video_id`, with `token` on both the player request and
    /// the url, chosen as `choice` says.
    async fn signed_format(
        &self,
        video_id: &str,
        token: &str,
        choice: Choice,
    ) -> Result<AudioFormat> {
        let solver = self.solver().await?;
        log::debug!(
            "player: asking {} for {video_id}, player {} sts {}",
            Client::Music.name(),
            solver.id(),
            solver.sts()
        );
        let payload = json!({
            "videoId": video_id,
            "contentCheckOk": true,
            "racyCheckOk": true,
            "cpn": random_string(16),
            "serviceIntegrityDimensions": { "poToken": token },
            "playbackContext": {
                "contentPlaybackContext": {
                    "html5Preference": "HTML5_PREF_WANTS",
                    "signatureTimestamp": solver.sts(),
                }
            },
        });
        let response = self.execute("player", Client::Music, payload).await?;
        let audio: Vec<Ciphered> = playable(&response, video_id)?
            .iter()
            .filter_map(ciphered)
            .collect();
        log::debug!(
            "player: {} offers {video_id} as {}",
            Client::Music.name(),
            offered(audio.iter().map(|format| (format.itag, format.bitrate)))
        );
        let chosen = match choice {
            Choice::Itag(itag) => audio
                .into_iter()
                .find(|format| format.itag == itag)
                .with_context(|| format!("itag {itag} is no longer offered for {video_id}"))?,
            Choice::Within(ceiling) => {
                choose(audio, ceiling, |format| (&format.mime, format.bitrate))
                    .with_context(|| format!("no addressable audio stream for {video_id}"))?
            }
        };
        let url = decipher(&solver, &chosen).await?;
        Ok(AudioFormat {
            itag: chosen.itag,
            url: with_pot(&url, token),
            mime: chosen.mime,
            codec: chosen.codec,
            bitrate: chosen.bitrate,
            duration: chosen.duration,
            content_length: chosen.content_length,
            loudness_db: chosen.loudness_db,
            user_agent: Client::Music.user_agent(),
        })
    }

    async fn guest_player(&self, video_id: &str) -> Result<Value> {
        let response = self.guest_request(video_id, None).await?;
        if playability(&response).status != "LOGIN_REQUIRED" {
            return Ok(response);
        }
        let Some(issued) = response
            .pointer("/responseContext/visitorData")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            return Ok(response);
        };
        log::debug!("player: the stream visitor was refused, priming a fresh one");
        self.adopt_visitor(issued.clone()).await;
        self.guest_request(video_id, Some(&issued)).await
    }

    async fn guest_request(&self, video_id: &str, guest: Option<&str>) -> Result<Value> {
        let payload = json!({
            "videoId": video_id,
            "contentCheckOk": true,
            "racyCheckOk": true,
            "cpn": random_string(16),
            "playbackContext": {
                "contentPlaybackContext": { "html5Preference": "HTML5_PREF_WANTS" }
            },
        });
        self.execute_visiting("player", Client::VisionOs, payload, false, guest)
            .await
    }

    pub async fn download(&self, format: &AudioFormat) -> Result<Vec<u8>> {
        let Some(total) = format.content_length else {
            return self.download_serial(format).await;
        };
        let limit = std::sync::Arc::new(tokio::sync::Semaphore::new(PARALLEL));
        let mut tasks = tokio::task::JoinSet::new();
        let mut offset = 0u64;
        let mut index = 0usize;
        while offset < total {
            let end = (offset + CHUNK - 1).min(total - 1);
            let http = self.http.clone();
            let url = format.url.clone();
            let agent = format.user_agent;
            let limit = limit.clone();
            let start = offset;
            tasks.spawn(async move {
                let _permit = limit.acquire().await;
                (index, range(&http, &url, agent, start, end).await)
            });
            offset = end + 1;
            index += 1;
        }
        let mut parts: Vec<(usize, Vec<u8>)> = Vec::with_capacity(index);
        while let Some(joined) = tasks.join_next().await {
            let (index, chunk) = joined.context("download task did not finish")?;
            parts.push((index, chunk?));
        }
        parts.sort_by_key(|(index, _)| *index);
        let mut data = Vec::with_capacity(total as usize);
        for (_, chunk) in parts {
            data.extend_from_slice(&chunk);
        }
        Ok(data)
    }

    async fn download_serial(&self, format: &AudioFormat) -> Result<Vec<u8>> {
        let mut data = Vec::with_capacity((CHUNK * 2) as usize);
        let mut offset = 0u64;
        while offset < MAX_STREAM_BYTES {
            let end = offset + CHUNK - 1;
            let chunk = range(&self.http, &format.url, format.user_agent, offset, end).await?;
            let short = (chunk.len() as u64) < CHUNK;
            offset += chunk.len() as u64;
            data.extend_from_slice(&chunk);
            if short {
                break;
            }
        }
        Ok(data)
    }
}

async fn range(
    http: &reqwest::Client,
    url: &str,
    agent: &'static str,
    start: u64,
    end: u64,
) -> Result<Vec<u8>> {
    let response = crate::stream::request(http, url, agent, start, end).await?;
    let chunk = response.bytes().await.context("cannot read stream chunk")?;
    Ok(chunk.to_vec())
}

struct Ciphered {
    itag: u32,
    mime: String,
    codec: String,
    bitrate: u32,
    duration: Option<Duration>,
    content_length: Option<u64>,
    loudness_db: Option<f32>,
    url: String,
    signature: Option<(String, String)>,
}

fn ciphered(format: &Value) -> Option<Ciphered> {
    let mime = format.get("mimeType").and_then(Value::as_str)?;
    if !mime.starts_with("audio/") {
        return None;
    }
    let (url, signature) = match format.get("signatureCipher").and_then(Value::as_str) {
        Some(cipher) => {
            let held = reqwest::Url::parse(&format!("https://cipher/?{cipher}")).ok()?;
            let url = param(&held, "url")?;
            let sig = param(&held, "s")?;
            let into = param(&held, "sp").unwrap_or_else(|| "signature".to_string());
            (url, Some((into, sig)))
        }
        None => (format.get("url").and_then(Value::as_str)?.to_string(), None),
    };
    let codec = mime
        .split_once("codecs=\"")
        .map(|(_, tail)| tail.trim_end_matches('"'))
        .unwrap_or_default();
    Some(Ciphered {
        itag: format.get("itag").and_then(Value::as_u64)? as u32,
        mime: mime.to_string(),
        codec: codec.to_string(),
        bitrate: format.get("bitrate").and_then(Value::as_u64).unwrap_or(0) as u32,
        duration: format
            .get("approxDurationMs")
            .and_then(Value::as_str)
            .and_then(|ms| ms.parse::<u64>().ok())
            .map(Duration::from_millis),
        content_length: format
            .get("contentLength")
            .and_then(Value::as_str)
            .and_then(|length| length.parse().ok()),
        loudness_db: format
            .get("loudnessDb")
            .and_then(Value::as_f64)
            .map(|db| db as f32),
        url,
        signature,
    })
}

async fn decipher(solver: &crate::deobf::Solver, format: &Ciphered) -> Result<String> {
    let mut url = reqwest::Url::parse(&format.url).context("the stream url does not parse")?;
    let throttle = param(&url, "n");
    let signature = format.signature.as_ref().map(|(_, sig)| sig.as_str());
    let started = std::time::Instant::now();
    let solved = solver.solve(signature, throttle.as_deref()).await?;
    log::debug!(
        "deobf: itag {} solved in {:?}, signature {} chars, n {} -> {}",
        format.itag,
        started.elapsed(),
        solved.sig.as_deref().map_or(0, str::len),
        throttle.as_deref().unwrap_or("none"),
        solved.n.as_deref().unwrap_or("none")
    );
    let mut changes: Vec<(&str, &str)> = Vec::with_capacity(2);
    if let Some((into, _)) = &format.signature {
        let sig = solved
            .sig
            .as_deref()
            .context("the solver returned no signature")?;
        changes.push((into.as_str(), sig));
    }
    if throttle.is_some() {
        let n = solved
            .n
            .as_deref()
            .context("the solver returned no n parameter")?;
        changes.push(("n", n));
    }
    set_params(&mut url, &changes);
    Ok(url.to_string())
}

/// Adds the proof-of-origin parameter the stream host asks for, leaving a url that already
/// carries one alone. An empty token, or a url that does not parse, is handed back untouched,
/// so a token can never be the reason a stream fails to load.
fn with_pot(url: &str, token: &str) -> String {
    if token.is_empty() {
        return url.to_string();
    }
    let Ok(mut parsed) = reqwest::Url::parse(url) else {
        log::warn!("potoken: the stream url does not parse, sending it without a token");
        return url.to_string();
    };
    set_params(&mut parsed, &[("pot", token)]);
    parsed.to_string()
}

fn param(url: &reqwest::Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
}

fn set_params(url: &mut reqwest::Url, changes: &[(&str, &str)]) {
    let existing: Vec<(String, String)> = url
        .query_pairs()
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect();
    let mut query = url.query_pairs_mut();
    query.clear();
    for (name, value) in &existing {
        match changes.iter().find(|(key, _)| key == name) {
            Some((_, replacement)) => query.append_pair(name, replacement),
            None => query.append_pair(name, value),
        };
    }
    for (key, value) in changes {
        if !existing.iter().any(|(name, _)| name == key) {
            query.append_pair(key, value);
        }
    }
    query.finish();
}

fn playable<'a>(response: &'a Value, video_id: &str) -> Result<&'a Vec<Value>> {
    let playability = playability(response);
    if !playability.ok() {
        let refused = format!(
            "{} is not playable: {} ({})",
            video_id,
            playability.status,
            playability.reason.as_deref().unwrap_or("no reason")
        );
        return match playability.gated() {
            true => Err(anyhow::Error::new(SignInRequired).context(refused)),
            false => Err(anyhow::anyhow!(refused)),
        };
    }
    match response
        .pointer("/streamingData/adaptiveFormats")
        .and_then(Value::as_array)
    {
        Some(formats) => Ok(formats),
        None => match response
            .pointer("/streamingData/serverAbrStreamingUrl")
            .is_some()
        {
            true => bail!("{video_id} is served over sabr only"),
            false => bail!("{video_id} offers no audio stream"),
        },
    }
}

fn describe(format: &AudioFormat, bytes: usize) -> String {
    format!(
        "itag {} {} {} kbps, {:.1} MiB",
        format.itag,
        format.codec,
        format.bitrate / 1000,
        bytes as f64 / (1024.0 * 1024.0)
    )
}

/// The stream to play out of `formats`, read through `key` as its mime type and bitrate. AAC
/// wins over every other codec when there is any. Among the candidates it takes the highest
/// bitrate at or below `ceiling`, or the lowest of them when all are above it, and simply the
/// highest when there is no ceiling.
fn choose<T>(formats: Vec<T>, ceiling: Option<u32>, key: impl Fn(&T) -> (&str, u32)) -> Option<T> {
    let aac = formats
        .iter()
        .any(|format| key(format).0.starts_with("audio/mp4"));
    let candidates = formats
        .into_iter()
        .filter(|format| !aac || key(format).0.starts_with("audio/mp4"));
    let (within, above): (Vec<T>, Vec<T>) =
        candidates.partition(|format| ceiling.is_none_or(|ceiling| key(format).1 <= ceiling));
    match within.is_empty() {
        true => above.into_iter().min_by_key(|format| key(format).1),
        false => within.into_iter().max_by_key(|format| key(format).1),
    }
}

/// The streams a player answer offered, as `itag@kbps` pairs for a log line.
fn offered(formats: impl Iterator<Item = (u32, u32)>) -> String {
    formats
        .map(|(itag, bitrate)| format!("{itag}@{}", bitrate / 1000))
        .collect::<Vec<_>>()
        .join(" ")
}

fn playability(response: &Value) -> Playability {
    let status = response
        .get("playabilityStatus")
        .cloned()
        .unwrap_or_default();
    Playability {
        status: status
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("UNKNOWN")
            .to_string(),
        reason: status
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

fn audio_format(format: &Value, client: Client) -> Option<AudioFormat> {
    let mime = format.get("mimeType").and_then(Value::as_str)?;
    if !mime.starts_with("audio/") {
        return None;
    }
    let url = format.get("url").and_then(Value::as_str)?;
    let codec = mime
        .split_once("codecs=\"")
        .map(|(_, tail)| tail.trim_end_matches('"'))
        .unwrap_or_default();
    Some(AudioFormat {
        itag: format.get("itag").and_then(Value::as_u64)? as u32,
        url: url.to_string(),
        mime: mime.to_string(),
        codec: codec.to_string(),
        bitrate: format.get("bitrate").and_then(Value::as_u64).unwrap_or(0) as u32,
        duration: format
            .get("approxDurationMs")
            .and_then(Value::as_str)
            .and_then(|ms| ms.parse::<u64>().ok())
            .map(Duration::from_millis),
        content_length: format
            .get("contentLength")
            .and_then(Value::as_str)
            .and_then(|length| length.parse().ok()),
        loudness_db: format
            .get("loudnessDb")
            .and_then(Value::as_f64)
            .map(|db| db as f32),
        user_agent: client.user_agent(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_ciphered_formats() {
        let format = json!({
            "itag": 140,
            "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
            "signatureCipher": "s=abc&sp=sig&url=https%3A%2F%2Fx",
            "bitrate": 130000,
        });
        assert!(audio_format(&format, Client::VisionOs).is_none());
    }

    #[test]
    fn parses_direct_format() {
        let format = json!({
            "itag": 140,
            "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
            "url": "https://example.com/stream",
            "bitrate": 130000,
            "contentLength": "3200000",
            "approxDurationMs": "185000",
            "loudnessDb": -4.5,
        });
        let parsed = audio_format(&format, Client::VisionOs).unwrap();
        assert_eq!(parsed.itag, 140);
        assert_eq!(parsed.codec, "mp4a.40.2");
        assert_eq!(parsed.duration, Some(Duration::from_secs(185)));
        assert_eq!(parsed.content_length, Some(3_200_000));
    }

    #[test]
    fn prefers_aac() {
        let formats = vec![
            AudioFormat {
                itag: 251,
                url: "opus".into(),
                mime: "audio/webm; codecs=\"opus\"".into(),
                codec: "opus".into(),
                bitrate: 160_000,
                duration: None,
                content_length: None,
                loudness_db: None,
                user_agent: "",
            },
            AudioFormat {
                itag: 140,
                url: "aac".into(),
                mime: "audio/mp4; codecs=\"mp4a.40.2\"".into(),
                codec: "mp4a.40.2".into(),
                bitrate: 130_000,
                duration: None,
                content_length: None,
                loudness_db: None,
                user_agent: "",
            },
        ];
        assert_eq!(
            choose(formats, None, |format| (&format.mime, format.bitrate))
                .unwrap()
                .itag,
            140
        );
    }
}
