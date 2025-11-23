use url::Url;

pub fn get_canonical_mailbox_address_identifier(url: &Url) -> String {
    let scheme = url.scheme();
    let username = url.username();
    let host = url.host_str().unwrap_or("");
    let path = url.path();

    if !username.is_empty() {
        format!("{}:{}@{}{}", scheme, username, host, path)
    } else {
        format!("{}:{}{}", scheme, host, path)
    }
}
