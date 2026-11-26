//! Where a file was downloaded from, as recorded by macOS.
//!
//! Browsers and other downloaders set the `com.apple.metadata:kMDItemWhereFroms`
//! extended attribute: a binary plist array of URLs (the file URL, then usually the
//! page it was linked from). These URLs can embed purchase keys and tokens, so only
//! host names ever leave this module.

use std::path::Path;

pub const WHERE_FROMS_ATTR: &str = "com.apple.metadata:kMDItemWhereFroms";

/// Host names of the URLs the file was downloaded from, in recorded order.
/// Empty when nothing was recorded or the attribute can't be read.
pub fn download_hosts(path: &Path) -> Vec<String> {
    let Ok(Some(bytes)) = xattr::get(path, WHERE_FROMS_ATTR) else {
        return Vec::new();
    };
    let Ok(urls) = plist::from_bytes::<Vec<String>>(&bytes) else {
        return Vec::new();
    };
    let mut hosts: Vec<String> = urls.iter().filter_map(|u| host_of(u)).collect();
    hosts.dedup();
    hosts
}

/// `https://user@dl.example.com:8443/x?y` → `dl.example.com`. None for non-network URLs.
fn host_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if !matches!(
        scheme.to_ascii_lowercase().as_str(),
        "http" | "https" | "ftp"
    ) {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    let host = host
        .split(':')
        .next()?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

/// Whether `host` is `domain` or a subdomain of it.
pub fn host_matches(host: &str, domain: &str) -> bool {
    let domain = domain.trim_start_matches('.').to_ascii_lowercase();
    host == domain
        || host
            .strip_suffix(&domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

#[cfg(test)]
pub(crate) fn set_download_urls(path: &Path, urls: &[&str]) {
    let mut buf = Vec::new();
    plist::to_writer_binary(&mut buf, &urls).unwrap();
    xattr::set(path, WHERE_FROMS_ATTR, &buf).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts_from_urls() {
        assert_eq!(
            host_of("https://dl.humble.com/x/a.dmg?gamekey=SECRET").as_deref(),
            Some("dl.humble.com")
        );
        assert_eq!(
            host_of("ftp://user@FTP.Epson.com:21/d.dmg").as_deref(),
            Some("ftp.epson.com")
        );
        assert_eq!(host_of("file:///Users/x/a.dmg"), None);
        assert_eq!(host_of("not a url"), None);
    }

    #[test]
    fn domain_matching() {
        assert!(host_matches("dl.google.com", "google.com"));
        assert!(host_matches("google.com", "google.com"));
        assert!(!host_matches("notgoogle.com", "google.com"));
        assert!(!host_matches("google.com.evil.net", "google.com"));
    }

    #[test]
    fn reads_the_attribute() {
        let f = std::env::temp_dir().join(format!("diskwise-prov-{}", std::process::id()));
        std::fs::write(&f, b"x").unwrap();
        assert!(download_hosts(&f).is_empty());
        set_download_urls(
            &f,
            &[
                "https://dl.humble.com/a.dmg?gamekey=SECRET",
                "https://www.humblebundle.com/downloads",
            ],
        );
        assert_eq!(
            download_hosts(&f),
            vec!["dl.humble.com", "www.humblebundle.com"]
        );
        std::fs::remove_file(&f).unwrap();
    }
}
