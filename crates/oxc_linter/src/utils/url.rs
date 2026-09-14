/// Finds the first value of a query parameter in a URL. Is not guaranteed to be accurate
/// with the URL standard and is just meant to be a simple helper that doesn't require
/// fully parsing the URL. The returned bytes preserve WTF-8; callers comparing
/// fixed query tokens can inspect them without converting the complete URL to UTF-8.
///
/// # Example
///
/// ```ignore
/// find_url_query_value("https://example.com/?foo=bar&baz=qux", "baz") // => Some(b"qux".as_slice())
/// ```
pub fn find_url_query_value<'url>(
    url: impl Into<oxc_str::JSStr<'url>>,
    key: &str,
) -> Option<&'url [u8]> {
    let url = url.into().as_wtf8();
    // Return None right away if this doesn't look like a URL at all.
    if !url.starts_with(b"http://") && !url.starts_with(b"https://") {
        return None;
    }
    // Skip everything up to the first `?` as we're not parsing the host/path/etc.
    let url = url.split(|&byte| byte == b'?').nth(1)?;
    // Now parse the query string in pairs of `key=value`, we don't need
    // to be too strict about this as we're not trying to be spec-compliant.
    for pair in url.split(|&byte| byte == b'&') {
        if let Some(index) = memchr::memchr(b'=', pair)
            && &pair[..index] == key.as_bytes()
        {
            return Some(&pair[index + 1..]);
        }
    }
    None
}

mod test {
    #[test]
    fn test_find_url_query_value() {
        use super::find_url_query_value;
        assert_eq!(find_url_query_value("something", "q"), None);
        assert_eq!(
            find_url_query_value("https://example.com/?foo=bar", "foo"),
            Some(b"bar".as_slice())
        );
        assert_eq!(find_url_query_value("https://example.com/?foo=bar", "baz"), None);
        assert_eq!(
            find_url_query_value("https://example.com/?foo=bar&baz=qux", "baz"),
            Some(b"qux".as_slice())
        );
        assert_eq!(
            find_url_query_value("https://example.com/?foo=bar&foo=qux", "foo"),
            Some(b"bar".as_slice())
        );
        assert_eq!(
            find_url_query_value(
                "https://polyfill.io/v3/polyfill.min.js?features=WeakSet%2CPromise%2CPromise.prototype.finally%2Ces2015%2Ces5%2Ces6",
                "features"
            ),
            Some(b"WeakSet%2CPromise%2CPromise.prototype.finally%2Ces2015%2Ces5%2Ces6".as_slice())
        );
    }
}
