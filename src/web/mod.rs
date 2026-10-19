//! Plain youtube.com (WEB client) models, parsers and requests, as opposed to the YouTube Music
//! ones in the rest of the crate.

pub mod api;
pub mod parse;

use std::time::Duration;

use crate::models::Thumbnail;

/// A channel named on a video or playlist card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelRef {
    pub id: String,
    pub name: String,
}

/// A playable video. `duration` is `None` only when the card did not carry a length.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Video {
    pub id: String,
    pub title: String,
    pub channel: Option<ChannelRef>,
    pub duration: Option<Duration>,
    pub thumbnails: Vec<Thumbnail>,
}

/// A channel card. `subscribed` is `None` for a guest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Channel {
    pub id: String,
    pub name: String,
    pub thumbnails: Vec<Thumbnail>,
    pub subscribed: Option<bool>,
}

/// A playlist card. The id has no `VL` browse prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebPlaylist {
    pub id: String,
    pub title: String,
    pub owner: Option<ChannelRef>,
    pub thumbnails: Vec<Thumbnail>,
    pub count: Option<u32>,
}

/// What a youtube.com search found, split by kind in page order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WebSearch {
    pub videos: Vec<Video>,
    pub channels: Vec<Channel>,
    pub playlists: Vec<WebPlaylist>,
}

/// A channel's page: who it is and the first page of its uploads. `continuation` leads to the
/// next page through `YtMusic::web_continuation`. `channel.subscribed` is always `None` here;
/// callers read subscription state from `YtMusic::web_subscriptions`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelPage {
    pub channel: Channel,
    pub description: Option<String>,
    pub uploads: Vec<Video>,
    pub continuation: Option<String>,
}

/// A playlist's page: the playlist and the first page of its videos. `continuation` leads to
/// the next page through `YtMusic::web_continuation`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlaylistPage {
    pub playlist: WebPlaylist,
    pub videos: Vec<Video>,
    pub continuation: Option<String>,
}
