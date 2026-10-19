//! Test-only helpers shared by the modules that record and read fixtures.

use serde_json::{Value, json};

/// Replaces what identifies the recording machine in a recorded response: the address a
/// stream url is bound to (`ip=` and `mip=` parameters, whatever the family) and every
/// `visitorData` value, including the `visitor_data` key/value pair of a service's tracking
/// params. Run it over a response before it is written as a fixture.
pub(crate) fn scrub(value: &mut Value) {
    match value {
        Value::String(text) => *text = scrub_ip(text),
        Value::Array(items) => items.iter_mut().for_each(scrub),
        Value::Object(map) => {
            if map.get("key").and_then(Value::as_str) == Some("visitor_data")
                && let Some(inner) = map.get_mut("value")
            {
                *inner = json!("VISITOR_DATA");
            }
            for (key, inner) in map.iter_mut() {
                match key.as_str() {
                    "visitorData" => *inner = json!("VISITOR_DATA"),
                    _ => scrub(inner),
                }
            }
        }
        _ => {}
    }
}

/// `text` with the value of every `ip=` parameter, and the segment after every `/ip/` path
/// component, set to `0.0.0.0`.
fn scrub_ip(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((at, marker)) = ["ip=", "/ip/"]
        .iter()
        .filter_map(|marker| rest.find(marker).map(|at| (at, marker.len())))
        .min()
    {
        let (head, tail) = rest.split_at(at + marker);
        out.push_str(head);
        let value = tail
            .find(|c: char| !(c.is_ascii_hexdigit() || c == '.' || c == ':' || c == '%'))
            .unwrap_or(tail.len());
        let numeric = tail[..value].contains(['.', ':']);
        match numeric {
            true => out.push_str("0.0.0.0"),
            false => out.push_str(&tail[..value]),
        }
        rest = &tail[value..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_replaces_the_address_and_visitor() {
        let mut value = json!({
            "visitorData": "abc",
            "params": [{ "key": "visitor_data", "value": "Cgsx" }, { "key": "other", "value": "keep" }],
            "url": "https://h/p?ei=x&ip=203.0.113.7&id=o&mip=2001:db8::1&ipbits=0&sparams=ip,id",
        });
        scrub(&mut value);
        assert_eq!(value["visitorData"], "VISITOR_DATA");
        assert_eq!(value["params"][0]["value"], "VISITOR_DATA");
        assert_eq!(value["params"][1]["value"], "keep");
        assert_eq!(
            value["url"],
            "https://h/p?ei=x&ip=0.0.0.0&id=o&mip=0.0.0.0&ipbits=0&sparams=ip,id"
        );
    }
}
