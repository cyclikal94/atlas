//! The coarse client label recorded with each new session (BE-Q25).
//!
//! The channel comes from the route that issues the session, never from a header. Browser and
//! platform families come from the request's `User-Agent` alone, matched against a short ordered
//! list of tokens; only the resulting closed values are stored. The header is self-reported and
//! easily changed, so a label is a hint that helps a person recognise a sign-in, not identity, and
//! metadata never blocks a sign-in: anything unusable is `unknown`.
use atlas_core::devices::{Browser, Channel, ClientLabel, Platform};
use axum::http::HeaderMap;

/// Longer headers are not inspected. Real browser `User-Agent`s are well under this.
const MAX_USER_AGENT: usize = 512;

/// First match wins; the order matters because browsers embed each other's tokens (Edge, Opera
/// and Samsung Internet all also say `Chrome/`, Chrome says `Safari/`).
const BROWSERS: &[(&[&str], Browser)] = &[
    (&["Edg/", "EdgA/", "EdgiOS/"], Browser::Edge),
    (&["OPR/"], Browser::Opera),
    (&["SamsungBrowser/"], Browser::SamsungInternet),
    (&["Firefox/", "FxiOS/"], Browser::Firefox),
    (&["CriOS/", "Chrome/", "Chromium/"], Browser::Chrome),
];

/// First match wins: Chrome OS and Android also say `Linux`, iOS says `like Mac OS X`.
const PLATFORMS: &[(&[&str], Platform)] = &[
    (&["CrOS"], Platform::Chromeos),
    (&["Android"], Platform::Android),
    (&["iPhone", "iPad", "iPod"], Platform::Ios),
    (&["Windows"], Platform::Windows),
    (&["Macintosh", "Mac OS X"], Platform::Macos),
    (&["Linux", "X11"], Platform::Linux),
];

/// The single `User-Agent`, if there is exactly one, of printable ASCII, no longer than
/// [`MAX_USER_AGENT`]. Anything else is treated as absent.
fn user_agent(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all("user-agent").iter();
    let value = values.next()?;
    if values.next().is_some() || value.len() > MAX_USER_AGENT {
        return None;
    }
    let bytes = value.as_bytes();
    if !bytes.iter().all(|b| (0x20..=0x7e).contains(b)) {
        return None;
    }
    std::str::from_utf8(bytes).ok()
}

fn browser(agent: &str) -> Browser {
    for (tokens, browser) in BROWSERS {
        if tokens.iter().any(|token| agent.contains(token)) {
            return *browser;
        }
    }
    if agent.contains("Safari/") && agent.contains("Version/") {
        return Browser::Safari;
    }
    Browser::Unknown
}

fn platform(agent: &str) -> Platform {
    PLATFORMS
        .iter()
        .find(|(tokens, _)| tokens.iter().any(|token| agent.contains(token)))
        .map_or(Platform::Unknown, |(_, platform)| *platform)
}

/// The label for a session issued on `channel` by a request carrying `headers`.
pub(super) fn client(headers: &HeaderMap, channel: Channel) -> ClientLabel {
    let agent = user_agent(headers);
    ClientLabel {
        channel,
        browser: match channel {
            Channel::Browser => Some(agent.map_or(Browser::Unknown, browser)),
            Channel::Native => None,
        },
        platform: agent.map_or(Platform::Unknown, platform),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn label(agents: &[&[u8]], channel: Channel) -> ClientLabel {
        let mut headers = HeaderMap::new();
        for agent in agents {
            headers.append("user-agent", HeaderValue::from_bytes(agent).unwrap());
        }
        client(&headers, channel)
    }

    fn browser_label(agent: &str) -> (Option<Browser>, Platform) {
        let label = label(&[agent.as_bytes()], Channel::Browser);
        (label.browser, label.platform)
    }

    /// U1: real-world `User-Agent`s of each supported family, and the documented limitation that
    /// iPadOS in desktop mode reports macOS.
    #[test]
    fn classifies_each_supported_family() {
        use Browser::*;
        use Platform::*;
        let table: &[(&str, Browser, Platform)] = &[
            (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36",
                Chrome,
                Windows,
            ),
            (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36 Edg/129.0.0.0",
                Edge,
                Windows,
            ),
            (
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 14.6; rv:131.0) Gecko/20100101 Firefox/131.0",
                Firefox,
                Macos,
            ),
            (
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15",
                Safari,
                Macos,
            ),
            (
                "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1",
                Safari,
                Ios,
            ),
            (
                "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/129.0.6668.69 Mobile/15E148 Safari/604.1",
                Chrome,
                Ios,
            ),
            (
                "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) FxiOS/131.0 Mobile/15E148 Safari/605.1.15",
                Firefox,
                Ios,
            ),
            (
                "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 EdgiOS/129.2792.84 Mobile/15E148 Safari/605.1.15",
                Edge,
                Ios,
            ),
            (
                "Mozilla/5.0 (iPad; CPU OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1",
                Safari,
                Ios,
            ),
            (
                "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.6668.81 Mobile Safari/537.36",
                Chrome,
                Android,
            ),
            (
                "Mozilla/5.0 (Linux; Android 14; SM-S921B) AppleWebKit/537.36 (KHTML, like Gecko) SamsungBrowser/26.0 Chrome/122.0.0.0 Mobile Safari/537.36",
                SamsungInternet,
                Android,
            ),
            (
                "Mozilla/5.0 (Linux; Android 14) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Mobile Safari/537.36 EdgA/129.0.0.0",
                Edge,
                Android,
            ),
            (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36 OPR/114.0.0.0",
                Opera,
                Windows,
            ),
            (
                "Mozilla/5.0 (X11; CrOS x86_64 14541.0.0) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36",
                Chrome,
                Chromeos,
            ),
            (
                "Mozilla/5.0 (X11; Linux x86_64; rv:131.0) Gecko/20100101 Firefox/131.0",
                Firefox,
                Linux,
            ),
            (
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chromium/129.0.0.0 Chrome/129.0.0.0 Safari/537.36",
                Chrome,
                Linux,
            ),
            // iPadOS in desktop mode sends a Mac Safari string: indistinguishable, so macOS.
            (
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.6 Safari/605.1.15",
                Safari,
                Macos,
            ),
            // Safari/ without Version/ is a WebKit embedder, not Safari.
            (
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Safari/605.1.15",
                Browser::Unknown,
                Macos,
            ),
            ("curl/8.7.1", Browser::Unknown, Platform::Unknown),
        ];
        for (agent, expected_browser, expected_platform) in table {
            assert_eq!(
                browser_label(agent),
                (Some(*expected_browser), *expected_platform),
                "{agent}"
            );
        }
    }

    /// A string carrying several families' tokens resolves by the fixed order, never by position.
    #[test]
    fn the_first_rule_wins_over_embedded_tokens() {
        assert_eq!(
            browser_label("Chrome/1 Edg/1 Firefox/1").0,
            Some(Browser::Edge)
        );
        assert_eq!(
            browser_label("Firefox/1 SamsungBrowser/1").0,
            Some(Browser::SamsungInternet)
        );
        assert_eq!(browser_label("Linux Android").1, Platform::Android);
        assert_eq!(browser_label("Mac OS X iPhone").1, Platform::Ios);
    }

    /// Unusable metadata never fails or blocks a sign-in: it is simply `unknown`.
    #[test]
    fn absent_repeated_oversized_or_non_ascii_headers_are_unknown() {
        let unknown = ClientLabel {
            channel: Channel::Browser,
            browser: Some(Browser::Unknown),
            platform: Platform::Unknown,
        };
        let chrome = "Mozilla/5.0 (Windows NT 10.0) Chrome/129.0.0.0 Safari/537.36";
        assert_eq!(label(&[], Channel::Browser), unknown, "absent");
        assert_eq!(label(&[b""], Channel::Browser), unknown, "empty");
        assert_eq!(
            label(&[chrome.as_bytes(), chrome.as_bytes()], Channel::Browser),
            unknown,
            "repeated"
        );
        assert_eq!(
            label(&["Chrome/1 Windows \u{e9}".as_bytes()], Channel::Browser),
            unknown,
            "non-ASCII"
        );
        assert_eq!(
            label(&[b"Chrome/1 Windows\t"], Channel::Browser),
            unknown,
            "control character"
        );
        let at_limit = format!("Chrome/1 Windows {}", "a".repeat(MAX_USER_AGENT - 17));
        assert_eq!(at_limit.len(), MAX_USER_AGENT);
        assert_eq!(
            label(&[at_limit.as_bytes()], Channel::Browser).browser,
            Some(Browser::Chrome),
            "512 bytes is inspected"
        );
        let over = format!("{at_limit}a");
        assert_eq!(
            label(&[over.as_bytes()], Channel::Browser),
            unknown,
            "513 bytes"
        );
    }

    /// The native channel never carries a browser, whatever the header says.
    #[test]
    fn a_native_session_has_no_browser() {
        let native = label(
            &[b"Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) Version/18.0 Safari/604.1"],
            Channel::Native,
        );
        assert_eq!(
            native,
            ClientLabel {
                channel: Channel::Native,
                browser: None,
                platform: Platform::Ios,
            }
        );
        assert_eq!(
            label(&[], Channel::Native),
            ClientLabel {
                channel: Channel::Native,
                browser: None,
                platform: Platform::Unknown,
            }
        );
    }
}
