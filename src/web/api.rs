use anyhow::Result;
use serde_json::json;

use super::WebSearch;
use super::parse;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::scrub;

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
    }
}
