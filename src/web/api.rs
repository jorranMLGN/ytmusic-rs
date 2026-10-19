use anyhow::{Context as _, Result};
use serde_json::{Value, json};

use super::parse;
use super::{Channel, ChannelPage, PlaylistPage, Video, WebPlaylist, WebSearch};
use crate::client::YtMusic;
use crate::context::Client;

impl YtMusic {
    /// Searches youtube.com as the WEB client. Shorts and items without a length are dropped.
    pub async fn web_search(&self, query: &str) -> Result<WebSearch> {
        let response = self
            .execute("search", Client::Web, json!({ "query": query }))
            .await?;
        Ok(parse::search(&response))
    }

    /// A channel's Videos tab. It takes two requests, as the tab's `params` are only known
    /// from the channel's own page.
    pub async fn web_channel(&self, channel_id: &str) -> Result<ChannelPage> {
        let home = self
            .execute("browse", Client::Web, json!({ "browseId": channel_id }))
            .await?;
        let params = parse::videos_tab_params(&home).context("cannot find the videos tab")?;
        let tab = self
            .execute(
                "browse",
                Client::Web,
                json!({ "browseId": channel_id, "params": params }),
            )
            .await?;
        parse::channel(&tab).context("cannot read the channel page")
    }

    /// The first page of a playlist, Watch later (`WL`) and Liked videos (`LL`) included. The
    /// id may carry the `VL` browse prefix. Follow `continuation` with [`Self::web_continuation`].
    pub async fn web_playlist(&self, playlist_id: &str) -> Result<PlaylistPage> {
        let id = playlist_id.strip_prefix("VL").unwrap_or(playlist_id);
        let response = self
            .execute(
                "browse",
                Client::Web,
                json!({ "browseId": format!("VL{id}") }),
            )
            .await?;
        parse::playlist(&response).context("cannot read the playlist page")
    }

    /// The page of videos a continuation token leads to, with the token after it, if any.
    pub async fn web_continuation(&self, token: &str) -> Result<(Vec<Video>, Option<String>)> {
        let response = self
            .execute("browse", Client::Web, json!({ "continuation": token }))
            .await?;
        Ok(parse::continuation(&response))
    }

    /// The channels the signed-in account subscribes to. Empty for a guest, without a request.
    pub async fn web_subscriptions(&self) -> Result<Vec<Channel>> {
        if !self.is_authenticated() {
            return Ok(Vec::new());
        }
        let response = self
            .execute("browse", Client::Web, json!({ "browseId": "FEchannels" }))
            .await?;
        Ok(parse::subscriptions(&response))
    }

    /// The signed-in account's playlists, Watch later and Liked videos among them. Empty for a
    /// guest, without a request: a guest browse of the page fails with 401.
    pub async fn web_library_playlists(&self) -> Result<Vec<WebPlaylist>> {
        if !self.is_authenticated() {
            return Ok(Vec::new());
        }
        let response = self
            .execute(
                "browse",
                Client::Web,
                json!({ "browseId": "FEplaylist_aggregation" }),
            )
            .await?;
        Ok(parse::library_playlists(&response))
    }

    /// Subscribes to or unsubscribes from a channel. Needs a signed-in account.
    pub async fn set_subscribed(&self, channel_id: &str, subscribed: bool) -> Result<()> {
        let (endpoint, payload) = subscription_request(channel_id, subscribed);
        self.execute(endpoint, Client::Web, payload).await?;
        Ok(())
    }
}

/// The endpoint and body that subscribe to or unsubscribe from a channel.
// Real WEB clients also send a `params` field with this body; verify live whether it is needed.
fn subscription_request(channel_id: &str, subscribed: bool) -> (&'static str, Value) {
    let endpoint = match subscribed {
        true => "subscription/subscribe",
        false => "subscription/unsubscribe",
    };
    (endpoint, json!({ "channelIds": [channel_id] }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::scrub;
    use serde_json::Value;

    /// Boiler Room, the channel whose videos tab is recorded.
    const BOILER_ROOM: &str = "UCGBpxWJr9FNOcFYA5GkKrMg";
    /// A public playlist of 180 videos (100 on its first page, 80 behind the token).
    const LONG_PLAYLIST: &str = "PLMC9KNkIncKvYin_USF1qoJQnIyMAfRxl";

    /// Records the guest WEB responses the `web` parser tests read. Live and read-only, so it
    /// is ignored and run by exact name.
    #[tokio::test]
    #[ignore]
    async fn record_web_fixtures() {
        let api = YtMusic::anonymous();
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/web");
        std::fs::create_dir_all(dir).unwrap();
        for (name, query) in [
            // A plain "boiler room" search carries no playlist cards for a guest; this one does.
            ("search_boiler_room", "boiler room playlist"),
            ("search_shorts", "shorts funny"),
        ] {
            let mut response = api
                .execute("search", Client::Web, json!({ "query": query }))
                .await
                .unwrap();
            scrub(&mut response);
            let text = serde_json::to_string_pretty(&response).unwrap();
            std::fs::write(format!("{dir}/{name}.json"), text).unwrap();
        }
        let write = |name: &str, mut response: Value| {
            scrub(&mut response);
            let text = serde_json::to_string_pretty(&response).unwrap();
            std::fs::write(format!("{dir}/{name}.json"), text).unwrap();
        };
        // The Boiler Room channel, its videos tab (the params come from the tab's endpoint).
        let home = api
            .execute("browse", Client::Web, json!({ "browseId": BOILER_ROOM }))
            .await
            .unwrap();
        let params = parse::videos_tab_params(&home).expect("a videos tab");
        let videos = api
            .execute(
                "browse",
                Client::Web,
                json!({ "browseId": BOILER_ROOM, "params": params }),
            )
            .await
            .unwrap();
        write("channel_boiler_room", videos);
        // A public playlist longer than one page, and the page its token leads to.
        let first = api
            .execute(
                "browse",
                Client::Web,
                json!({ "browseId": format!("VL{LONG_PLAYLIST}") }),
            )
            .await
            .unwrap();
        let token = parse::continuation(&first).1.expect("a continuation token");
        let next = api
            .execute("browse", Client::Web, json!({ "continuation": token }))
            .await
            .unwrap();
        write("playlist_long", first);
        write("playlist_long_continuation", next);
        // What a guest gets from the signed-in library page.
        let library = api
            .execute("browse", Client::Web, json!({ "browseId": "FEchannels" }))
            .await
            .unwrap();
        write("signed_out_library", library);
        // A guest browse of FEplaylist_aggregation is not recorded: it fails with
        // `401 Unauthorized` ("Request is missing required authentication credential"), which
        // is why `web_library_playlists` returns early for a guest.
    }

    #[tokio::test]
    async fn a_guest_library_is_empty_without_a_request() {
        let api = YtMusic::anonymous();
        assert!(api.web_library_playlists().await.unwrap().is_empty());
        assert!(api.web_subscriptions().await.unwrap().is_empty());
    }

    #[test]
    fn subscribing_posts_the_channel_id_to_the_matching_endpoint() {
        let (endpoint, body) = subscription_request("UCabc", true);
        assert_eq!(endpoint, "subscription/subscribe");
        assert_eq!(body, json!({ "channelIds": ["UCabc"] }));
        let (endpoint, body) = subscription_request("UCabc", false);
        assert_eq!(endpoint, "subscription/unsubscribe");
        assert_eq!(body, json!({ "channelIds": ["UCabc"] }));
    }
}
