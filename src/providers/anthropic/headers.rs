use http::{HeaderMap, HeaderName, header};

pub(super) fn forwarded_headers(headers: &HeaderMap) -> HeaderMap {
    let connection_headers: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    let mut forwarded = HeaderMap::new();
    for (name, value) in headers {
        if !connection_headers.contains(name)
            && !matches!(
                name.as_str(),
                "host"
                    | "connection"
                    | "keep-alive"
                    | "proxy-authenticate"
                    | "proxy-authorization"
                    | "te"
                    | "trailer"
                    | "transfer-encoding"
                    | "upgrade"
                    | "content-length"
            )
        {
            forwarded.append(name.clone(), value.clone());
        }
    }
    forwarded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_connection_headers_but_preserves_credentials_and_encoding() {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("connection", "keep-alive, X-Private"),
            ("x-private", "hop"),
            ("host", "localhost"),
            ("content-length", "12"),
            ("authorization", "Bearer token"),
            ("x-api-key", "key"),
            ("anthropic-beta", "beta"),
            ("content-encoding", "gzip"),
        ] {
            headers.insert(HeaderName::from_static(name), value.parse().unwrap());
        }
        headers.append(header::CONNECTION, "X-Other".parse().unwrap());
        headers.insert("x-other", "hop".parse().unwrap());
        let forwarded = forwarded_headers(&headers);
        for name in [
            "connection",
            "x-private",
            "x-other",
            "host",
            "content-length",
        ] {
            assert!(!forwarded.contains_key(name));
        }
        for name in [
            "authorization",
            "x-api-key",
            "anthropic-beta",
            "content-encoding",
        ] {
            assert_eq!(forwarded[name], headers[name]);
        }
    }
}
