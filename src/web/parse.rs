use std::time::Duration;

use serde_json::Value;

use super::{Channel, ChannelPage, ChannelRef, PlaylistPage, Video, WebPlaylist, WebSearch};
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

/// Elements of a list that are video cards, in any of the shapes a list page uses.
const VIDEO_CARDS: [&str; 5] = [
    "lockupViewModel",
    "richItemRenderer",
    "videoRenderer",
    "playlistVideoRenderer",
    "gridVideoRenderer",
];

/// The keys under which a list ends in the token for its next page.
const CONTINUATION_ITEMS: [&str; 2] = ["continuationItemRenderer", "continuationItemViewModel"];

/// Reads a channel's Videos tab into a [`ChannelPage`]. A card that does not name its channel
/// is given this one. `None` when the response carries no channel metadata.
pub fn channel(response: &Value) -> Option<ChannelPage> {
    let meta = find_renderers(response, "channelMetadataRenderer")
        .into_iter()
        .next()?;
    let id = meta.str_at(&["externalId"])?.to_string();
    let name = meta.str_at(&["title"])?.to_string();
    let items = video_list(response).unwrap_or_default();
    let mut uploads = items_videos(items);
    for video in &mut uploads {
        video.channel.get_or_insert_with(|| ChannelRef {
            id: id.clone(),
            name: name.clone(),
        });
    }
    Some(ChannelPage {
        channel: Channel {
            id,
            name,
            thumbnails: clean(thumbnails(meta.at(&["avatar"]).unwrap_or(&Value::Null))),
            subscribed: None,
        },
        description: meta
            .str_at(&["description"])
            .filter(|text| !text.is_empty())
            .map(str::to_string),
        uploads,
        continuation: items_token(items),
    })
}

/// Reads a `VL<id>` browse response, a playlist, Watch later and Liked videos alike, into a
/// [`PlaylistPage`]. The id comes from the browse id the response echoes back. Cards without
/// a length (deleted, private, live) are left out. `None` when the page is not a playlist.
pub fn playlist(response: &Value) -> Option<PlaylistPage> {
    let id = browse_id(response)?.strip_prefix("VL")?.to_string();
    let header = find_renderers(response, "pageHeaderViewModel")
        .into_iter()
        .next();
    let title = response
        .run_text(&["metadata", "playlistMetadataRenderer", "title"])
        .or_else(|| {
            header
                .and_then(|header| header.at(&["title"]))
                .and_then(first_content)
        })?;
    let rows = header.and_then(|header| header.at(&["metadata", "contentMetadataViewModel"]));
    let owner = rows
        .and_then(|rows| rows.at(&["metadataRows", "0"]).and_then(channel_in))
        .map(|owner| ChannelRef {
            name: owner
                .name
                .strip_prefix("by ")
                .map_or(owner.name.clone(), str::to_string),
            ..owner
        });
    let count = rows.and_then(|rows| {
        let mut texts = Vec::new();
        find_all(rows, "content", &mut texts);
        texts
            .iter()
            .filter_map(|text| text.as_str())
            .filter(|text| text.ends_with("video") || text.ends_with("videos"))
            .find_map(leading_count)
    });
    let items = video_list(response).unwrap_or_default();
    Some(PlaylistPage {
        playlist: WebPlaylist {
            id,
            title,
            owner,
            thumbnails: header
                .and_then(|header| header.at(&["heroImage"]))
                .map(|hero| clean(source_thumbnails(hero)))
                .unwrap_or_default(),
            count,
        },
        videos: items_videos(items),
        continuation: items_token(items),
    })
}

/// Reads a continuation response into the next videos and the token after them. The token is
/// `None` on the last page.
pub fn continuation(response: &Value) -> (Vec<Video>, Option<String>) {
    let items = video_list(response).unwrap_or_default();
    (items_videos(items), items_token(items))
}

/// The channels of the signed-in `FEchannels` page, all marked subscribed. A guest page has
/// none, so this is empty.
pub fn subscriptions(response: &Value) -> Vec<Channel> {
    let mut channels: Vec<Channel> = Vec::new();
    for renderer in find_renderers(response, "channelRenderer") {
        if let Some(channel) = channel_renderer(renderer)
            && !channels.iter().any(|seen| seen.id == channel.id)
        {
            channels.push(Channel {
                subscribed: Some(true),
                ..channel
            });
        }
    }
    channels
}

/// The playlists of the signed-in `FEplaylist_aggregation` page, Watch later (`WL`) and Liked
/// videos (`LL`) among them, each once. A guest page has none, so this is empty.
pub fn library_playlists(response: &Value) -> Vec<WebPlaylist> {
    let mut found: Vec<WebPlaylist> = Vec::new();
    let mut push = |playlist: WebPlaylist| {
        if !found.iter().any(|seen| seen.id == playlist.id) {
            found.push(playlist);
        }
    };
    for lockup in find_renderers(response, "lockupViewModel") {
        if lockup.str_at(&["contentType"]) == Some(LOCKUP_PLAYLIST)
            && let Some(playlist) = playlist_lockup(lockup)
        {
            push(playlist);
        }
    }
    for renderer in find_renderers(response, "gridPlaylistRenderer") {
        if let Some(playlist) = grid_playlist(renderer) {
            push(playlist);
        }
    }
    for renderer in find_renderers(response, "playlistRenderer") {
        if let Some(playlist) = playlist_renderer(renderer) {
            push(playlist);
        }
    }
    found
}

/// The `params` of the Videos tab of a channel page, which a browse needs to open that tab.
pub(super) fn videos_tab_params(response: &Value) -> Option<String> {
    find_renderers(response, "tabRenderer")
        .into_iter()
        .find(|tab| {
            tab.str_at(&["endpoint", "commandMetadata", "webCommandMetadata", "url"])
                .is_some_and(|url| url.ends_with("/videos"))
        })
        .and_then(|tab| tab.str_at(&["endpoint", "browseEndpoint", "params"]))
        .map(str::to_string)
}

/// The browse id a response echoes in its tracking params, such as `VLWL`.
fn browse_id(response: &Value) -> Option<&str> {
    find_renderers(response, "serviceTrackingParams")
        .into_iter()
        .flat_map(|services| services.as_array().map_or(&[][..], Vec::as_slice))
        .flat_map(|service| service.items(&["params"]))
        .find(|param| param.str_at(&["key"]) == Some("browse_id"))
        .and_then(|param| param.str_at(&["value"]))
}

/// The first list in a response that holds video cards: a playlist's items, a channel's grid
/// or a continuation's `continuationItems`. Lists that only hold other things are skipped, so
/// a token beside a header section is never mistaken for the list's own.
fn video_list(node: &Value) -> Option<&[Value]> {
    match node {
        Value::Array(list) => {
            let holds_videos = list.iter().any(|item| {
                item.as_object()
                    .is_some_and(|map| VIDEO_CARDS.iter().any(|key| map.contains_key(*key)))
            });
            match holds_videos {
                true => Some(list.as_slice()),
                false => list.iter().find_map(video_list),
            }
        }
        Value::Object(map) => map.values().find_map(video_list),
        _ => None,
    }
}

/// The videos in a list of cards, in order. Cards that are not videos or have no length are
/// skipped.
fn items_videos(items: &[Value]) -> Vec<Video> {
    items.iter().filter_map(card_video).collect()
}

/// The video a list element holds, whichever of the card shapes it is in.
fn card_video(item: &Value) -> Option<Video> {
    let lockup = find_renderers(item, "lockupViewModel")
        .into_iter()
        .find(|lockup| lockup.str_at(&["contentType"]) == Some(LOCKUP_VIDEO))
        .and_then(video_lockup);
    lockup.or_else(|| {
        [
            "videoRenderer",
            "playlistVideoRenderer",
            "gridVideoRenderer",
        ]
        .iter()
        .find_map(|key| {
            find_renderers(item, key)
                .into_iter()
                .next()
                .and_then(video_renderer)
        })
    })
}

/// The token in the continuation element of a list, which sits last among its cards.
fn items_token(items: &[Value]) -> Option<String> {
    items
        .iter()
        .filter(|item| {
            item.as_object()
                .is_some_and(|map| CONTINUATION_ITEMS.iter().any(|key| map.contains_key(*key)))
        })
        .find_map(|item| {
            find_renderers(item, "token")
                .into_iter()
                .find_map(|token| token.as_str().map(str::to_string))
        })
}

/// A `gridPlaylistRenderer` as a [`WebPlaylist`].
fn grid_playlist(renderer: &Value) -> Option<WebPlaylist> {
    Some(WebPlaylist {
        id: playlist_id(renderer.str_at(&["playlistId"])?),
        title: renderer.run_text(&["title"])?,
        owner: ["longBylineText", "shortBylineText"]
            .iter()
            .find_map(|key| renderer.at(&[key]).and_then(channel_in)),
        thumbnails: clean(thumbnails(renderer.at(&["thumbnail"])?)),
        count: ["videoCountText", "videoCountShortText"]
            .iter()
            .find_map(|key| renderer.run_text(&[key]))
            .and_then(|text| leading_count(&text)),
    })
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

    // `subscriptions.json`, `library_playlists.json`, `watch_later.json` and `liked.json` are
    // synthetic: hand-built on the renderer shapes of the guest responses (placeholder names
    // and ids only), because recording them needs a signed-in account and the fixtures must
    // carry no personal data. Re-verify them against real responses in a live check.

    #[test]
    fn a_channel_page_lists_its_uploads() {
        let page = channel(&fixture("channel_boiler_room.json")).expect("a channel page");
        assert_eq!(page.channel.name, "Boiler Room");
        assert!(page.channel.id.starts_with("UC"), "{:?}", page.channel);
        assert!(!page.channel.thumbnails.is_empty());
        assert!(page.description.is_some_and(|text| !text.is_empty()));
        assert!(page.uploads.len() >= 20, "{} uploads", page.uploads.len());
        for video in &page.uploads {
            assert!(video.duration.is_some(), "{video:?}");
            assert_eq!(
                video.channel.as_ref().map(|c| c.id.as_str()),
                Some(page.channel.id.as_str())
            );
        }
        assert!(page.continuation.is_some());
    }

    #[test]
    fn a_playlist_continuation_yields_the_next_page_and_token() {
        let page = playlist(&fixture("playlist_long.json")).expect("a playlist page");
        assert_eq!(page.videos.len(), 100);
        assert!(page.continuation.is_some());
        assert!(!page.playlist.id.starts_with("VL"), "{:?}", page.playlist);
        assert!(page.playlist.id.starts_with("PL"), "{:?}", page.playlist);
        assert!(!page.playlist.title.is_empty());
        assert!(
            page.playlist
                .owner
                .is_some_and(|owner| owner.id.starts_with("UC") && !owner.name.starts_with("by "))
        );
        assert!(page.playlist.count.is_some_and(|count| count >= 100));
        let (videos, token) = continuation(&fixture("playlist_long_continuation.json"));
        assert!(!videos.is_empty());
        assert!(videos.iter().all(|video| video.duration.is_some()));
        assert!(token.is_none(), "the last page carries no token");
    }

    #[test]
    fn a_channel_continuation_is_found_beside_the_uploads() {
        let response = json!({ "onResponseReceivedActions": [{ "appendContinuationItemsAction": {
            "continuationItems": [
                { "richItemRenderer": { "content": { "lockupViewModel": {
                    "contentId": "abcdefghijk", "contentType": "LOCKUP_CONTENT_TYPE_VIDEO",
                    "contentImage": { "thumbnailViewModel": { "image": { "sources": [
                        { "url": "https://i.ytimg.com/a.jpg", "width": 1, "height": 1 }] },
                        "overlays": [{ "thumbnailBadgeViewModel": { "text": "2:00" } }] } },
                    "metadata": { "lockupMetadataViewModel": { "title": { "content": "Next" } } },
                } } } },
                { "continuationItemRenderer": { "continuationEndpoint": { "continuationCommand": {
                    "token": "NEXT" } } } },
            ] } }] });
        let (videos, token) = continuation(&response);
        assert_eq!(videos.len(), 1);
        assert_eq!(videos[0].title, "Next");
        assert_eq!(token.as_deref(), Some("NEXT"));
    }

    #[test]
    fn subscriptions_parse_to_channels() {
        let response = fixture("subscriptions.json");
        let channels = subscriptions(&response);
        assert_eq!(
            channels.len(),
            find_renderers(&response, "channelRenderer").len()
        );
        assert!(channels.len() >= 3);
        for channel in &channels {
            assert!(channel.id.starts_with("UC"), "{channel:?}");
            assert!(!channel.name.is_empty());
            assert_eq!(channel.subscribed, Some(true));
            assert!(
                channel.thumbnails[0].url.starts_with("https://"),
                "{channel:?}"
            );
        }
    }

    #[test]
    fn the_library_lists_watch_later_and_own_playlists() {
        let playlists = library_playlists(&fixture("library_playlists.json"));
        assert!(playlists.len() >= 3);
        assert!(playlists.iter().any(|playlist| playlist.id == "WL"));
        assert!(playlists.iter().any(|playlist| playlist.id == "LL"));
        let own = playlists
            .iter()
            .find(|playlist| playlist.title == "Playlist 3")
            .expect("the grid card");
        assert_eq!(own.count, Some(7));
        assert_eq!(own.owner.as_ref().unwrap().id, "UC000000000000000000001");
        assert!(
            playlists
                .iter()
                .all(|playlist| !playlist.id.starts_with("VL"))
        );
    }

    #[test]
    fn liked_videos_parse_as_a_playlist() {
        let page = playlist(&fixture("liked.json")).expect("a playlist page");
        assert_eq!(page.playlist.id, "LL");
        assert_eq!(page.playlist.title, "Liked videos");
        assert_eq!(page.videos.len(), 4);
        assert!(page.continuation.is_none());
    }

    #[test]
    fn watch_later_drops_cards_without_a_length() {
        let page = playlist(&fixture("watch_later.json")).expect("a playlist page");
        assert_eq!(page.playlist.id, "WL");
        assert_eq!(page.videos.len(), 4, "the live card has no length");
        assert!(page.videos.iter().all(|video| video.duration.is_some()));
    }

    #[test]
    fn a_signed_out_browse_parses_to_nothing() {
        let response = fixture("signed_out_library.json");
        assert!(subscriptions(&response).is_empty());
        assert!(library_playlists(&response).is_empty());
    }
}
