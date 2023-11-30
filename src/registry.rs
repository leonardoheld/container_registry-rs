mod auth;
mod storage;
mod www_authenticate;

use std::{
    fmt::{self, Display},
    sync::Arc,
};

use self::{
    auth::{AuthProvider, UnverifiedCredentials, ValidUser},
    storage::{FilesystemStorage, ImageLocation, RegistryStorage},
};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{
        header::{CONTENT_LENGTH, CONTENT_TYPE, LOCATION, RANGE},
        StatusCode,
    },
    response::{IntoResponse, Response},
    routing::{get, head, patch, post, put},
    Router,
};
use futures::stream::StreamExt;
use hex::FromHex;
use serde::{Deserialize, Deserializer};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

// TODO: Return error as:
// {
//     "errors:" [{
//             "code": <error identifier>,
//             "message": <message describing condition>,
//             "detail": <unstructured>
//         },
//         ...
//     ]
// }

#[derive(Debug)]
struct AppError(anyhow::Error);

impl Display for AppError {
    #[inline(always)]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    #[inline(always)]
    fn from(err: E) -> Self {
        AppError(err.into())
    }
}

impl IntoResponse for AppError {
    #[inline(always)]
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, self.0.to_string()).into_response()
    }
}

pub(crate) struct DockerRegistry {
    realm: String,
    auth_provider: Box<dyn AuthProvider>,
    storage: Box<dyn RegistryStorage>,
}

impl DockerRegistry {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(DockerRegistry {
            realm: "TODO REGISTRY".to_string(),
            auth_provider: Box::new(()),
            storage: Box::new(
                FilesystemStorage::new("./rockslide-storage").expect("inaccessible storage"),
            ),
        })
    }

    pub(crate) fn make_router(self: Arc<DockerRegistry>) -> Router {
        Router::new()
            .route("/v2/", get(index_v2))
            .route("/v2/:repository/:image/blobs/:digest", head(blob_check))
            .route("/v2/:repository/:image/blobs/uploads/", post(upload_new))
            .route(
                "/v2/:repository/:image/uploads/:upload",
                patch(upload_add_chunk),
            )
            .route(
                "/v2/:repository/:image/uploads/:upload",
                put(upload_finalize),
            )
            .route("/v2/:repository/:image/manifests/latest", put(manifest_put))
            .with_state(self)
    }
}

async fn index_v2(
    State(registry): State<Arc<DockerRegistry>>,
    credentials: Option<UnverifiedCredentials>,
) -> Response<Body> {
    let realm = &registry.realm;

    if let Some(creds) = credentials {
        if registry.auth_provider.check_credentials(&creds).await {
            return Response::builder()
                .status(StatusCode::OK)
                .header("WWW-Authenticate", format!("Basic realm=\"{realm}\""))
                .body(Body::empty())
                .unwrap();
        }
    }

    // Return `UNAUTHORIZED`, since we want the client to supply credentials.
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("WWW-Authenticate", format!("Basic realm=\"{realm}\""))
        .body(Body::empty())
        .unwrap()
}

async fn blob_check(
    State(registry): State<Arc<DockerRegistry>>,
    Path(image): Path<ImageDigest>,
    _auth: ValidUser,
) -> Result<Response, AppError> {
    if let Some(metadata) = registry.storage.get_blob_metadata(image.digest).await? {
        Ok(Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_LENGTH, metadata.size())
            .header("Docker-Content-Digest", image.to_string())
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(Body::empty())
            .unwrap())
    } else {
        Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap())
    }
}

async fn upload_new(
    State(registry): State<Arc<DockerRegistry>>,
    Path(location): Path<ImageLocation>,
    _auth: ValidUser,
) -> Result<UploadState, AppError> {
    // Initiate a new upload
    let upload = registry.storage.begin_new_upload().await?;

    Ok(UploadState {
        location,
        completed: None,
        upload,
    })
}

fn mk_upload_location(location: &ImageLocation, uuid: Uuid) -> String {
    let repository = &location.repository();
    let image = &location.image();
    format!("/v2/{repository}/{image}/uploads/{uuid}")
}

#[derive(Debug)]
struct UploadState {
    location: ImageLocation,
    completed: Option<u64>,
    upload: Uuid,
}

impl IntoResponse for UploadState {
    fn into_response(self) -> Response {
        let mut builder = Response::builder()
            .header(LOCATION, mk_upload_location(&self.location, self.upload))
            .header(CONTENT_LENGTH, 0)
            .header("Docker-Upload-UUID", self.upload.to_string());

        if let Some(completed) = self.completed {
            builder = builder
                .header(RANGE, format!("0-{}", completed))
                .status(StatusCode::NO_CONTENT)
        } else {
            builder = builder
                .header(CONTENT_LENGTH, 0)
                .status(StatusCode::ACCEPTED);
            // The spec says to use `CREATED`, but only `ACCEPTED` works?
        }

        builder.body(Body::empty()).unwrap()
    }
}

#[derive(Copy, Clone, Debug, Deserialize)]
struct UploadId {
    upload: Uuid,
}

#[derive(Debug, Deserialize)]
struct ImageDigest {
    #[serde(deserialize_with = "deserialize_sha256_hexdigest")]
    digest: storage::Digest,
}

impl Display for ImageDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", self.digest)
    }
}

const SHA256_LEN: usize = 32;

fn deserialize_sha256_hexdigest<'de, D>(deserializer: D) -> Result<storage::Digest, D::Error>
where
    D: Deserializer<'de>,
{
    const PREFIX_LEN: usize = 7;
    const DIGEST_HEX_LEN: usize = SHA256_LEN * 2;

    let raw = String::deserialize(deserializer)?;

    if raw.len() != PREFIX_LEN + DIGEST_HEX_LEN {
        return Err(serde::de::Error::custom("wrong length"));
    }

    if !raw.starts_with("sha256:") {
        return Err(serde::de::Error::custom("wrong prefix"));
    }

    let hex_encoded = &raw[PREFIX_LEN..];
    debug_assert_eq!(hex_encoded.len(), DIGEST_HEX_LEN);

    let digest = <[u8; SHA256_LEN]>::from_hex(hex_encoded).map_err(serde::de::Error::custom)?;

    Ok(storage::Digest::new(digest))
}

async fn upload_add_chunk(
    State(registry): State<Arc<DockerRegistry>>,
    Path(location): Path<ImageLocation>,
    Path(UploadId { upload }): Path<UploadId>,
    _auth: ValidUser,
    request: axum::extract::Request,
) -> Result<UploadState, AppError> {
    // Check if we have a range - if so, its an unsupported feature, namely monolit uploads.
    if request.headers().contains_key(RANGE) {
        return Err(anyhow::anyhow!("unsupport feature: chunked uploads").into());
    }

    let mut writer = registry.storage.get_writer(0, upload).await?;

    // We'll get the entire file in one go, no range header == monolithic uploads.
    let mut body = request.into_body().into_data_stream();

    let mut completed: u64 = 0;
    while let Some(result) = body.next().await {
        let chunk = result?;
        completed += chunk.len() as u64;
        writer.write_all(chunk.as_ref()).await?;
    }

    writer.flush().await?;

    Ok(UploadState {
        location,
        completed: Some(completed),
        upload,
    })
}

async fn upload_finalize(
    State(registry): State<Arc<DockerRegistry>>,
    //Path(location): Path<ImageLocation>,
    Path(UploadId { upload }): Path<UploadId>,
    Query(image_digest): Query<ImageDigest>,
    _auth: ValidUser,
    request: axum::extract::Request,
) -> Result<Response<Body>, AppError> {
    // We do not support the final chunk in the `PUT` call, so ensure that's not the case.
    match request.headers().get(CONTENT_LENGTH) {
        Some(value) => {
            let num_bytes: u64 = value.to_str()?.parse()?;
            if num_bytes != 0 {
                return Err(anyhow::anyhow!("missing content length not implemented").into());
            }

            // 0 is the only acceptable value here.
        }
        None => return Err(anyhow::anyhow!("missing content length not implemented").into()),
    }

    registry
        .storage
        .finalize_upload(upload, image_digest.digest)
        .await?;

    Ok(Response::builder()
        .status(StatusCode::CREATED)
        .header("Docker-Content-Digest", image_digest.to_string())
        .body(Body::empty())?)
}

async fn manifest_put(
    State(registry): State<Arc<DockerRegistry>>,
    //Path(location): Path<ImageLocation>,
    // Path(UploadId { upload }): Path<UploadId>,
    // Query(image_digest): Query<ImageDigest>,
    _auth: ValidUser,
    // request: axum::extract::Request,
    body: String,
) -> Result<Response<Body>, AppError> {
    println!("{}", body);
    todo!()
}
