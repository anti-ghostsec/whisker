//! Fetches files like matrix-sdk does, but falls back to the older media
//! addresses (`/_matrix/media/v3/…`) when the newer, logged-in ones
//! (`/_matrix/client/v1/media/…`) are turned down. Some servers advertise
//! the new ones and then answer "Unrecognized request" for some files, or
//! can't fetch a file from another server that way, while the old address
//! still works.

use matrix_sdk::media::{DefaultMediaFetcher, MediaFetcher, MediaFormat, MediaRequestParameters};
use matrix_sdk::ruma::api::error::ErrorKind;
use matrix_sdk::ruma::events::room::MediaSource;
use matrix_sdk::{BoxFuture, Client};
use std::io::Read;
use std::time::Duration;

#[derive(Debug, Default)]
pub struct WithFallback;

impl MediaFetcher for WithFallback {
    fn fetch_media_content<'a>(&'a self, client: &'a Client, request: &'a MediaRequestParameters) -> BoxFuture<'a, Result<Vec<u8>, matrix_sdk::Error>> {
        Box::pin(async move {
            match DefaultMediaFetcher.fetch_media_content(client, request).await {
                Err(e) if turned_down(&e) => match old_address(client, request).await {
                    Ok(data) => Ok(data),
                    Err(again) => {
                        tracing::debug!("the old media address failed too: {again}");
                        Err(e)
                    }
                },
                other => other,
            }
        })
    }
}

/// The server didn't take the request itself (rather than, say, being down).
fn turned_down(e: &matrix_sdk::Error) -> bool {
    let Some(api) = e.as_client_api_error() else { return false };
    matches!(api.error_kind(), Some(ErrorKind::Unrecognized | ErrorKind::NotFound)) || matches!(api.status_code.as_u16(), 400 | 404 | 405 | 501)
}

#[allow(deprecated)]
async fn old_address(client: &Client, request: &MediaRequestParameters) -> Result<Vec<u8>, matrix_sdk::Error> {
    use matrix_sdk::ruma::api::client::media;
    // A file's size and the connection are unknown: no time limit.
    let config = client.request_config().timeout(Some(Duration::MAX));
    match &request.source {
        MediaSource::Encrypted(file) => {
            let data = client.send(media::get_content::v3::Request::from_url(&file.url)?).with_request_config(config).await?.file;
            let mut cursor = std::io::Cursor::new(data);
            let mut reader = matrix_sdk_base::crypto::AttachmentDecryptor::new(&mut cursor, file.as_ref().clone().into())?;
            let mut out = Vec::new();
            reader.read_to_end(&mut out)?;
            Ok(out)
        }
        MediaSource::Plain(uri) => match &request.format {
            MediaFormat::Thumbnail(s) => {
                let mut r = media::get_content_thumbnail::v3::Request::from_url(uri, s.width, s.height)?;
                r.method = Some(s.method.clone());
                r.animated = Some(s.animated);
                Ok(client.send(r).with_request_config(config).await?.file)
            }
            MediaFormat::File => Ok(client.send(media::get_content::v3::Request::from_url(uri)?).with_request_config(config).await?.file),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matrix_sdk::ruma::OwnedMxcUri;

    /// A server that answers "Unrecognized" on the new address still gives
    /// the file on the old one.
    #[tokio::test]
    async fn falls_back_to_the_old_address() {
        let mock = matrix_sdk::test_utils::mocks::MatrixMockServer::new().await;
        let server = mock.server();
        wiremock::Mock::given(wiremock::matchers::path_regex("^/_matrix/client/v1/media/"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_json(serde_json::json!({"errcode": "M_UNRECOGNIZED", "error": "Unrecognized request"})))
            .mount(server)
            .await;
        wiremock::Mock::given(wiremock::matchers::path("/_matrix/media/v3/download/other.org/cat"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_raw(b"meow".to_vec(), "image/png"))
            .mount(server)
            .await;
        let client = mock.client_builder().build().await;
        client.set_media_fetcher(std::sync::Arc::new(WithFallback)).await;
        let req = MediaRequestParameters { source: MediaSource::Plain(OwnedMxcUri::from("mxc://other.org/cat")), format: MediaFormat::File };
        assert_eq!(client.media().get_media_content(&req, false).await.unwrap(), b"meow");
    }
}
