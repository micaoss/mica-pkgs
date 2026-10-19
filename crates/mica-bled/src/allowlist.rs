//! The routes forwarded to mica-apid (protocol section 7.1). `{name}` matches
//! one non-empty path segment other than `.` and `..`.

const ROUTES: &[(&str, &str)] = &[
    ("GET", "/v1/system/info"),
    ("GET", "/v1/network/status"),
    ("GET", "/v1/network"),
    ("PUT", "/v1/network"),
    ("PUT", "/v1/network/{iface}"),
    ("DELETE", "/v1/network/{iface}"),
    ("GET", "/v1/wifi/client"),
    ("PUT", "/v1/wifi/client"),
    ("GET", "/v1/wifi/client/networks"),
    ("POST", "/v1/wifi/client/networks"),
    ("PUT", "/v1/wifi/client/networks/{ssid}"),
    ("DELETE", "/v1/wifi/client/networks/{ssid}"),
    ("POST", "/v1/wifi/client/scan"),
    ("GET", "/v1/wifi/ap"),
    ("PUT", "/v1/wifi/ap"),
];

/// Whether `method path` is forwarded.
pub fn allowed(method: &str, path: &str) -> bool {
    ROUTES
        .iter()
        .any(|(m, pattern)| *m == method && matches(pattern, path))
}

/// Whether `path` matches `pattern`, segment by segment.
pub fn matches(pattern: &str, path: &str) -> bool {
    let (Some(pattern), Some(path)) = (pattern.strip_prefix('/'), path.strip_prefix('/')) else {
        return false;
    };
    if path.contains(['?', '#', '\\']) || path.bytes().any(|b| b.is_ascii_control() || b == b' ') {
        return false;
    }
    let mut want = pattern.split('/');
    let mut got = path.split('/');
    loop {
        match (want.next(), got.next()) {
            (None, None) => return true,
            (Some(w), Some(g)) => {
                let ok = if w.starts_with('{') {
                    !g.is_empty() && g != "." && g != ".."
                } else {
                    w == g
                };
                if !ok {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_the_listed_routes() {
        assert!(allowed("GET", "/v1/network/status"));
        assert!(allowed("PUT", "/v1/network/eth0"));
        assert!(allowed("DELETE", "/v1/wifi/client/networks/My%20Wifi"));
        assert!(allowed("POST", "/v1/wifi/client/scan"));
        assert!(allowed("GET", "/v1/system/info"));
        assert!(!allowed("POST", "/v1/network/status"));
        assert!(!allowed("GET", "/v1/network/eth0"));
        assert!(!allowed("put", "/v1/network"));
        assert!(!allowed("GET", "/v1/bluetooth"));
        assert!(!allowed("POST", "/v1/power/reboot"));
        assert!(!allowed("GET", "/v1/settings/access"));
    }

    #[test]
    fn no_way_around_the_list() {
        for path in [
            "/v1/network/",
            "/v1/network//",
            "/v1/network/..",
            "/v1/network/.",
            "/v1/network/eth0/../../power",
            "/v1/network/status/",
            "/v1/network/status?x=1",
            "/v1/network/status#x",
            "v1/network",
            "//v1/network",
            "/v1/network/a b",
            "/v1/network/a\\b",
            "/v1/network/a\nb",
            "/v1/wifi/client/networks/a/b",
            "",
        ] {
            assert!(
                !allowed("GET", path) && !allowed("PUT", path) && !allowed("DELETE", path),
                "{path:?}"
            );
        }
    }
}
