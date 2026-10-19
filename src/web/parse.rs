use std::time::Duration;

use serde_json::Value;

use super::{Channel, ChannelRef, Video, WebPlaylist, WebSearch};
use crate::models::Thumbnail;
use crate::nav::{Nav as _, find_all};
use crate::parse::{find_renderers, runs_text, thumbnails};
use crate::util::parse_clock;

const LOCKUP_PLAYLIST: &str = "LOCKUP_CONTENT_TYPE_PLAYLIST";
const LOCKUP_VIDEO: &str = "LOCKUP_CONTENT_TYPE_VIDEO";

/// Reads a WEB `search` response into videos, channels and playlists, each list in page order
/// and grouped by kind. Shorts and videos without a length (live, upcoming) are left out, and a video that shows up
/// twice is kept once.
pub fn search(response: &Value) -> WebSearch {
    let mut found = WebSearch::default();
    let mut push_video = |video: Video| {
        if !found.videos.iter().any(|seen| seen.id == video.id) {
            found.videos.push(video);
        }
    };
    for renderer in find_renderers(response, "videoRenderer") {
        if let Some(video) = video_renderer(renderer) {
            push_video(video);
        }
    }
    for lockup in find_renderers(response, "lockupViewModel") {
        if lockup.str_at(&["contentType"]) == Some(LOCKUP_VIDEO)
            && let Some(video) = video_lockup(lockup)
        {
            push_video(video);
        }
    }
    for renderer in find_renderers(response, "channelRenderer") {
        if let Some(channel) = channel_renderer(renderer) {
            found.channels.push(channel);
        }
    }
    for renderer in find_renderers(response, "playlistRenderer") {
        if let Some(playlist) = playlist_renderer(renderer) {
            found.playlists.push(playlist);
        }
    }
    for lockup in find_renderers(response, "lockupViewModel") {
        if lockup.str_at(&["contentType"]) == Some(LOCKUP_PLAYLIST)
            && let Some(playlist) = playlist_lockup(lockup)
        {
            found.playlists.push(playlist);
        }
    }
    found
}

/// A `videoRenderer` as a [`Video`]; `None` for a short or a card without a length.
fn video_renderer(renderer: &Value) -> Option<Video> {
    let id = renderer.str_at(&["videoId"])?;
    if is_short(renderer) {
        return None;
    }
    let length = renderer
        .at(&["lengthText"])
        .and_then(runs_text)
        .or_else(|| {
            find_renderers(renderer, "thumbnailOverlayTimeStatusRenderer")
                .first()
                .and_then(|overlay| overlay.run_text(&["text"]))
        })
        .and_then(|text| clock(&text));
    Some(Video {
        id: id.to_string(),
        title: renderer.run_text(&["title"])?,
        channel: ["ownerText", "longBylineText", "shortBylineText"]
            .iter()
            .find_map(|key| renderer.at(&[key]).and_then(channel_in)),
        duration: length?.into(),
        thumbnails: clean(thumbnails(renderer.at(&["thumbnail"])?)),
    })
}

/// A video `lockupViewModel`. The length sits in a thumbnail badge as `m:ss` text.
fn video_lockup(lockup: &Value) -> Option<Video> {
    let meta = lockup.at(&["metadata", "lockupMetadataViewModel"])?;
    let length = badge_texts(lockup)
        .into_iter()
        .find_map(|text| clock(&text))?;
    Some(Video {
        id: lockup.str_at(&["contentId"])?.to_string(),
        title: meta.str_at(&["title", "content"])?.to_string(),
        channel: meta
            .at(&["metadata", "contentMetadataViewModel", "metadataRows"])
            .and_then(channel_in),
        duration: Some(length),
        thumbnails: clean(source_thumbnails(lockup.at(&["contentImage"])?)),
    })
}

/// A `channelRenderer` as a [`Channel`]. `subscribed` stays `None`, a guest cannot be one.
fn channel_renderer(renderer: &Value) -> Option<Channel> {
    Some(Channel {
        id: renderer.str_at(&["channelId"])?.to_string(),
        name: renderer.run_text(&["title"])?,
        thumbnails: clean(thumbnails(renderer.at(&["thumbnail"])?)),
        subscribed: None,
    })
}

/// A `playlistRenderer` as a [`WebPlaylist`].
fn playlist_renderer(renderer: &Value) -> Option<WebPlaylist> {
    let owner = ["longBylineText", "shortBylineText"]
        .iter()
        .find_map(|key| renderer.at(&[key]).and_then(channel_in));
    Some(WebPlaylist {
        id: playlist_id(renderer.str_at(&["playlistId"])?),
        title: renderer.run_text(&["title"])?,
        owner,
        thumbnails: clean(thumbnails(renderer.at(&["thumbnails", "0"])?)),
        count: renderer
            .run_text(&["videoCount"])
            .and_then(|text| leading_count(&text)),
    })
}

/// A playlist `lockupViewModel`. The count is the thumbnail badge text, such as `30 videos`.
fn playlist_lockup(lockup: &Value) -> Option<WebPlaylist> {
    let meta = lockup.at(&["metadata", "lockupMetadataViewModel"])?;
    Some(WebPlaylist {
        id: playlist_id(lockup.str_at(&["contentId"])?),
        title: meta.str_at(&["title", "content"])?.to_string(),
        owner: meta
            .at(&["metadata", "contentMetadataViewModel", "metadataRows"])
            .and_then(channel_in),
        thumbnails: clean(source_thumbnails(lockup.at(&["contentImage"])?)),
        count: badge_texts(lockup)
            .iter()
            .find_map(|text| leading_count(text)),
    })
}

/// True for a card that opens the Shorts player rather than the watch page.
fn is_short(renderer: &Value) -> bool {
    let mut urls = Vec::new();
    find_all(renderer, "url", &mut urls);
    urls.iter()
        .filter_map(|url| url.as_str())
        .any(|url| url.starts_with("/shorts/"))
}

/// The first channel a node links to: a `browseId` that starts with `UC`, paired with the text
/// of the run it hangs on.
fn channel_in(node: &Value) -> Option<ChannelRef> {
    let mut ids = Vec::new();
    find_all(node, "browseId", &mut ids);
    let id = ids
        .iter()
        .filter_map(|id| id.as_str())
        .find(|id| id.starts_with("UC"))?;
    let name = match node.at(&["runs"]).is_some() {
        true => node.runs(&[]).iter().find_map(|run| {
            let own = run.str_at(&["navigationEndpoint", "browseEndpoint", "browseId"])?;
            (own == id)
                .then(|| run.str_at(&["text"]).map(str::to_string))
                .flatten()
        }),
        false => first_content(node),
    }?;
    Some(ChannelRef {
        id: id.to_string(),
        name,
    })
}

/// The first `content` string of a view-model text, which is where its visible words live.
fn first_content(node: &Value) -> Option<String> {
    let mut found = Vec::new();
    find_all(node, "content", &mut found);
    found
        .first()
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

/// Every thumbnail badge text on a lockup: the length of a video, the count of a playlist.
fn badge_texts(lockup: &Value) -> Vec<String> {
    find_renderers(lockup, "thumbnailBadgeViewModel")
        .into_iter()
        .filter_map(|badge| badge.str_at(&["text"]).map(str::to_string))
        .collect()
}

/// The image `sources` of a view-model, smallest first. A source without a size gets 0.
fn source_thumbnails(node: &Value) -> Vec<Thumbnail> {
    let mut found = Vec::new();
    find_all(node, "sources", &mut found);
    let mut thumbs: Vec<Thumbnail> = found
        .first()
        .and_then(|value| value.as_array())
        .map(|list| {
            list.iter()
                .filter_map(|thumb| {
                    Some(Thumbnail {
                        url: thumb.str_at(&["url"])?.to_string(),
                        width: thumb.at(&["width"]).and_then(Value::as_u64).unwrap_or(0) as u32,
                        height: thumb.at(&["height"]).and_then(Value::as_u64).unwrap_or(0) as u32,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    thumbs.sort_by_key(|thumb| thumb.width);
    thumbs
}

/// Thumbnails with protocol-relative urls (`//yt3.ggpht.com/...`) made absolute.
fn clean(mut thumbs: Vec<Thumbnail>) -> Vec<Thumbnail> {
    for thumb in &mut thumbs {
        if thumb.url.starts_with("//") {
            thumb.url.insert_str(0, "https:");
        }
    }
    thumbs
}

/// A playlist id without the `VL` prefix a browse id carries.
fn playlist_id(id: &str) -> String {
    id.strip_prefix("VL").unwrap_or(id).to_string()
}

/// The leading number of text such as `30 videos` or `1,204`.
fn leading_count(text: &str) -> Option<u32> {
    text.split_whitespace()
        .next()?
        .replace(',', "")
        .parse()
        .ok()
}

/// A length such as `3:50` or `1:02:03`; `None` for words (`LIVE`) and for zero.
fn clock(text: &str) -> Option<Duration> {
    parse_clock(text).filter(|length| !length.is_zero())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(name: &str) -> Value {
        let path = format!("{}/tests/fixtures/web/{name}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn a_search_lists_videos_channels_and_playlists() {
        let found = search(&fixture("search_boiler_room.json"));
        assert!(found.videos.len() >= 10, "{} videos", found.videos.len());
        for video in &found.videos {
            assert_eq!(video.id.len(), 11, "{video:?}");
            assert!(video.duration.is_some(), "{video:?}");
            assert!(video.channel.is_some(), "{video:?}");
        }
        let channel = found
            .channels
            .iter()
            .find(|channel| channel.name == "Boiler Room")
            .expect("the Boiler Room channel");
        assert!(channel.id.starts_with("UC"), "{channel:?}");
        assert!(!found.playlists.is_empty());
        for playlist in &found.playlists {
            assert!(!playlist.id.starts_with("VL"), "{playlist:?}");
            assert!(!playlist.title.is_empty(), "{playlist:?}");
        }
    }

    #[test]
    fn shorts_and_live_items_are_dropped() {
        let response = fixture("search_shorts.json");
        let seen = find_renderers(&response, "shortsLockupViewModel").len()
            + find_renderers(&response, "reelItemRenderer").len();
        assert!(seen >= 1, "the fixture carries no shorts");
        let found = search(&response);
        for video in &found.videos {
            let length = video.duration.expect("a length");
            assert!(length >= Duration::from_secs(1), "{video:?}");
        }
    }

    #[test]
    fn a_video_renderer_linking_to_shorts_is_dropped() {
        let response = json!({ "contents": [{ "videoRenderer": {
            "videoId": "abcdefghijk",
            "title": { "runs": [{ "text": "Short" }] },
            "lengthText": { "simpleText": "0:30" },
            "navigationEndpoint": { "commandMetadata": { "webCommandMetadata": {
                "url": "/shorts/abcdefghijk" } } },
            "thumbnail": { "thumbnails": [{ "url": "https://i.ytimg.com/a.jpg" }] },
        } }] });
        assert!(search(&response).videos.is_empty());
    }

    #[test]
    fn a_live_or_upcoming_card_is_dropped() {
        let card = |id: &str, label: &str| {
            json!({ "videoRenderer": {
                "videoId": id,
                "title": { "runs": [{ "text": "Stream" }] },
                "thumbnail": { "thumbnails": [{ "url": "https://i.ytimg.com/a.jpg" }] },
                "thumbnailOverlays": [{ "thumbnailOverlayTimeStatusRenderer": {
                    "style": label, "text": { "simpleText": label } } }],
            } })
        };
        let response =
            json!({ "contents": [card("liveliveliv", "LIVE"), card("soonsoonsoo", "UPCOMING")] });
        assert!(search(&response).videos.is_empty());
    }

    #[test]
    fn a_video_lockup_parses_with_its_badge_length() {
        let response = json!({ "contents": [{ "lockupViewModel": {
            "contentId": "abcdefghijk",
            "contentType": "LOCKUP_CONTENT_TYPE_VIDEO",
            "contentImage": { "thumbnailViewModel": {
                "image": { "sources": [{ "url": "//i.ytimg.com/a.jpg", "width": 360, "height": 202 }] },
                "overlays": [{ "thumbnailOverlayBadgeViewModel": { "thumbnailBadges": [
                    { "thumbnailBadgeViewModel": { "text": "1:02:03" } }] } }],
            } },
            "metadata": { "lockupMetadataViewModel": {
                "title": { "content": "A long set" },
                "metadata": { "contentMetadataViewModel": { "metadataRows": [{ "metadataParts": [{
                    "text": { "content": "Boiler Room", "commandRuns": [{ "onTap": { "innertubeCommand": {
                        "browseEndpoint": { "browseId": "UCGBpxWJr9FNOcFYA5GkKrMg" } } } }] } }] }] } },
            } },
        } }] });
        let videos = search(&response).videos;
        assert_eq!(videos.len(), 1);
        let video = &videos[0];
        assert_eq!(video.id, "abcdefghijk");
        assert_eq!(video.title, "A long set");
        assert_eq!(video.duration, Some(Duration::from_secs(3723)));
        let channel = video.channel.as_ref().unwrap();
        assert_eq!(
            (channel.id.as_str(), channel.name.as_str()),
            ("UCGBpxWJr9FNOcFYA5GkKrMg", "Boiler Room")
        );
        assert_eq!(video.thumbnails[0].url, "https://i.ytimg.com/a.jpg");
    }

    #[test]
    fn a_video_without_a_length_is_dropped() {
        let live = json!({ "videoRenderer": { "videoId": "abcdefghijk", "title": {"runs": [{"text": "Live"}]} } });
        let response = json!({ "contents": [live] });
        assert!(search(&response).videos.is_empty());
    }

    #[test]
    fn a_playlist_renderer_loses_the_browse_prefix() {
        let response = json!({ "contents": [{ "playlistRenderer": {
            "playlistId": "VLPL123",
            "title": { "simpleText": "Mix" },
            "videoCount": "1,204",
            "shortBylineText": { "runs": [{ "text": "Owner", "navigationEndpoint": {
                "browseEndpoint": { "browseId": "UCowner" } } }] },
            "thumbnails": [{ "thumbnails": [{ "url": "//i.ytimg.com/x.jpg", "width": 10, "height": 5 }] }],
        } }] });
        let found = search(&response);
        let playlist = &found.playlists[0];
        assert_eq!(playlist.id, "PL123");
        assert_eq!(playlist.count, Some(1204));
        assert_eq!(playlist.owner.as_ref().unwrap().id, "UCowner");
        assert_eq!(playlist.thumbnails[0].url, "https://i.ytimg.com/x.jpg");
    }

    #[test]
    fn clock_text_reads_hours_and_minutes() {
        assert_eq!(clock("1:02:03"), Some(Duration::from_secs(3723)));
        assert_eq!(clock("3:50"), Some(Duration::from_secs(230)));
        assert_eq!(clock("LIVE"), None);
        assert_eq!(clock("3:50 Now playing"), None);
    }
}
