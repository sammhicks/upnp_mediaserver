use std::{collections::HashMap, fmt, path::PathBuf, sync::Arc};

mod shutdown;

use anyhow::{Context, Result};
use askama::Template;
use askama_axum::IntoResponse;
use axum::{
    extract::{FromRequestParts, State},
    http::StatusCode,
    routing::{get, post},
};
use axum_extra::routing::RouterExt;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use futures_util::FutureExt;
use strum::IntoEnumIterator;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

#[derive(Debug, strum::Display, strum::EnumIter, strum::IntoStaticStr)]
enum DeviceName {
    #[strum(to_string = "urn:schemas-upnp-org:device:MediaServer:1")]
    MediaServer,
}

#[derive(Debug, strum::Display, strum::EnumIter, strum::IntoStaticStr)]
enum ServiceName {
    #[strum(to_string = "urn:schemas-upnp-org:service:ConnectionManager:1")]
    ConnectionManager,
    #[strum(to_string = "urn:schemas-upnp-org:service:ContentDirectory:1")]
    ContentDirectory,
}

#[derive(Debug)]
enum Advertisement {
    #[allow(clippy::enum_variant_names)]
    RootDevice {
        id: Uuid,
    },
    Uuid {
        id: Uuid,
    },
    Device {
        id: Uuid,
        name: DeviceName,
    },
    Service {
        id: Uuid,
        name: ServiceName,
    },
}

impl Advertisement {
    fn notification_type(&self) -> String {
        match self {
            Self::RootDevice { .. } => "upnp:rootdevice".into(),
            Self::Uuid { id } => format!("uuid:{id}"),
            Self::Device { name, .. } => name.to_string(),
            Self::Service { name, .. } => name.to_string(),
        }
    }

    fn service_name(&self) -> String {
        match self {
            Self::RootDevice { id } => format!("uuid:{id}::upnp:rootdevice"),
            Self::Uuid { id } => format!("uuid:{id}"),
            Self::Device { id, name } => format!("uuid:{id}::{name}"),
            Self::Service { id, name } => format!("uuid:{id}::{name}"),
        }
    }

    fn all(id: Uuid) -> [Advertisement; 5] {
        [
            Advertisement::RootDevice { id },
            Advertisement::Uuid { id },
            Advertisement::Device {
                id,
                name: DeviceName::MediaServer,
            },
            Advertisement::Service {
                id,
                name: ServiceName::ConnectionManager,
            },
            Advertisement::Service {
                id,
                name: ServiceName::ContentDirectory,
            },
        ]
    }
}

#[derive(Debug)]
enum NotificationSubType {
    Alive,
    ByeBye,
}

impl fmt::Display for NotificationSubType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Alive => "ssdp:alive",
            Self::ByeBye => "ssdp:byebye",
        })
    }
}

#[derive(Debug, Template)]
#[template(path = "ssdp_notify.http", escape = "none")]
struct SsdpNotify<'a> {
    hostname: &'a str,
    port: u16,
    advertisement: Advertisement,
    notification_sub_type: NotificationSubType,
}

#[derive(Debug, Template)]
#[template(path = "ssdp_response.http", escape = "none")]
struct SsdpResponse<'a> {
    hostname: &'a str,
    port: u16,
    advertisement: Advertisement,
    date: chrono::DateTime<chrono::Local>,
}

impl<'a> SsdpResponse<'a> {
    fn date(&self) -> impl fmt::Display + '_ {
        self.date.format("%a, %d %b %Y %T %Z")
    }
}

async fn parse_discover_request(request: &mut [u8]) -> Result<Option<&str>> {
    let request = std::str::from_utf8_mut(request).context("Request is not UTF-8")?;
    request.make_ascii_lowercase();

    let mut lines = request.trim_end().lines();

    if !lines
        .next()
        .context("No request line")?
        .starts_with("m-search")
    {
        return Ok(None);
    }

    let headers = Result::<HashMap<_, _>>::from_iter(lines.map(|line| {
        line.split_once(':')
            .context("Invalid Header")
            .map(|(name, value)| (name.trim(), value.trim()))
    }))?;

    let man = headers.get("man").copied();

    if man != Some(r#""ssdp:discover""#) {
        return Ok(None);
    }

    let &search_target = headers.get("st").context("No search target")?;

    Ok(Some(search_target))
}

const SSDP_MULTICAST_ADDRESS: std::net::Ipv4Addr = std::net::Ipv4Addr::new(239, 255, 255, 250);
const SSDP_PORT: u16 = 1900;

async fn run_ssdp_notify(
    id: Uuid,
    hostname: String,
    port: u16,
    socket: Arc<tokio::net::UdpSocket>,
    shutdown_signal: futures_util::future::Shared<shutdown::Signal>,
) {
    let hostname = &hostname;

    let notifications = async {
        loop {
            for advertisement in Advertisement::all(id) {
                let notify = SsdpNotify {
                    hostname,
                    port,
                    advertisement,
                    notification_sub_type: NotificationSubType::Alive,
                };

                tracing::debug!(?notify, "SSDP Notification");

                socket
                    .send_to(
                        notify.render().unwrap().as_bytes(),
                        (SSDP_MULTICAST_ADDRESS, SSDP_PORT),
                    )
                    .await
                    .unwrap();
            }

            tokio::time::sleep(std::time::Duration::from_secs(1800)).await;
        }
    };

    let ((), _) = futures_util::future::select(shutdown_signal, std::pin::pin!(notifications))
        .await
        .factor_first();

    for device in Advertisement::all(id) {
        let notify = SsdpNotify {
            hostname,
            port,
            advertisement: device,
            notification_sub_type: NotificationSubType::ByeBye,
        };

        tracing::debug!(?notify, "SSDP Notification");

        socket
            .send_to(
                notify.render().unwrap().as_bytes(),
                (SSDP_MULTICAST_ADDRESS, SSDP_PORT),
            )
            .await
            .unwrap();
    }
}

async fn run_ssdp(
    id: Uuid,
    hostname: String,
    port: u16,
    shutdown_signal: futures_util::future::Shared<shutdown::Signal>,
) {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )
    .unwrap();
    socket.set_reuse_address(true).unwrap();
    socket.set_reuse_port(true).unwrap();
    socket.set_nonblocking(true).unwrap();
    socket
        .bind(&std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, SSDP_PORT)).into())
        .unwrap();

    let socket = Arc::new(tokio::net::UdpSocket::from_std(socket.into()).unwrap());

    socket
        .join_multicast_v4(SSDP_MULTICAST_ADDRESS, std::net::Ipv4Addr::UNSPECIFIED)
        .unwrap();

    let notify_task = tokio::spawn(run_ssdp_notify(
        id,
        hostname.clone(),
        port,
        socket.clone(),
        shutdown_signal.clone(),
    ));

    let hostname = &hostname;

    let mut buffer = [0; 4096];

    let process_search = async {
        loop {
            let (message_length, remote_address) = socket.recv_from(&mut buffer).await.unwrap();

            let discover = match parse_discover_request(&mut buffer[..message_length]).await {
                Ok(Some(discover)) => discover,
                Ok(None) => continue,
                Err(err) => {
                    tracing::error!("{err}");
                    continue;
                }
            };

            let advertisement = if discover == "upnp:rootdevice" {
                Advertisement::RootDevice { id }
            } else if let Some(Ok(search_uuid)) =
                discover.strip_prefix("uuid:").map(Uuid::parse_str)
            {
                if search_uuid == id {
                    Advertisement::Uuid { id }
                } else {
                    tracing::trace!(%search_uuid, "Ignoring id");
                    continue;
                }
            } else if let Some(name) =
                DeviceName::iter().find(|name| discover.eq_ignore_ascii_case(name.into()))
            {
                Advertisement::Device { id, name }
            } else if let Some(name) =
                ServiceName::iter().find(|name| discover.eq_ignore_ascii_case(name.into()))
            {
                Advertisement::Service { id, name }
            } else {
                tracing::trace!(?discover, "Ignoring discover");
                continue;
            };

            tracing::debug!(?advertisement, "SSDP Message");

            let response = SsdpResponse {
                hostname,
                port,
                advertisement,
                date: chrono::Local::now(),
            };

            tracing::debug!(?response, "SSDP Response");

            let response = response.render().unwrap();

            tracing::debug!(?response, "SSDP Response");

            socket
                .send_to(response.as_bytes(), remote_address)
                .await
                .unwrap();
        }
    };

    let ((), _) = futures_util::future::select(shutdown_signal, std::pin::pin!(process_search))
        .await
        .factor_first();

    notify_task.await.unwrap();
}

#[derive(Template)]
#[template(path = "media_server_description.xml")]
struct MediaServerDescription {
    id: Uuid,
}

#[derive(Template)]
#[template(path = "content_directory_description.xml")]
struct ContentDirectoryDescription;

struct SoapAction<T: serde::de::DeserializeOwned>(T);

#[axum::async_trait]
impl<T: serde::de::DeserializeOwned, S: Send + Sync> axum::extract::FromRequest<S>
    for SoapAction<T>
{
    type Rejection = axum::response::Response;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, Self::Rejection> {
        #[derive(serde::Deserialize)]
        struct SoapEnvelopeBody<T> {
            #[serde(rename = "$value")]
            value: T,
        }

        #[derive(serde::Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct SoapEnvelope<T> {
            body: SoapEnvelopeBody<T>,
        }

        let (mut parts, body) = req.into_parts();

        let axum_extra::TypedHeader(content_type) = axum_extra::TypedHeader::<
            axum_extra::headers::ContentType,
        >::from_request_parts(&mut parts, state)
        .await
        .map_err(|_| StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response())?;

        if mime::Mime::from(content_type).essence_str() != "text/xml" {
            return Err(StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response());
        }

        let SoapEnvelope {
            body: SoapEnvelopeBody { value },
        } = quick_xml::de::from_str(
            &String::from_request(axum::extract::Request::from_parts(parts, body), state)
                .await
                .map_err(|_| StatusCode::BAD_REQUEST.into_response())?,
        )
        .map_err(|_| StatusCode::BAD_REQUEST.into_response())?;

        Ok(Self(value))
    }
}

#[derive(serde::Deserialize)]
enum ConnectionManagerAction {
    GetProtocolInfo,
}

mod content_directory_browse {
    use std::{fmt, path::Path};

    use askama::Template;
    use axum::{http::StatusCode, response::IntoResponse};

    use super::{Engine, BASE64};

    #[derive(Debug, Clone)]
    pub enum ObjectID {
        Root,
        FolderRoot,
        Folder { root_name: String, path: String },
    }

    impl ObjectID {
        pub fn parent(&self) -> Self {
            match &self {
                Self::Root | Self::FolderRoot => Self::Root,
                Self::Folder { root_name, path } => {
                    match path.rfind('/').map(|index| &path[0..index]) {
                        None => Self::FolderRoot,
                        Some(parent) => Self::Folder {
                            root_name: root_name.clone(),
                            path: parent.into(),
                        },
                    }
                }
            }
        }

        pub fn title(&self) -> String {
            match self {
                ObjectID::Root => "Root".into(),
                ObjectID::FolderRoot => "Folder".into(),
                ObjectID::Folder { root_name, path } => {
                    match path.rfind('/').map(|index| &path[(index + 1)..]) {
                        None => root_name.clone(),
                        Some(title) => title.into(),
                    }
                }
            }
        }
    }

    impl fmt::Display for ObjectID {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Root => write!(f, "0"),
                Self::FolderRoot => write!(f, "folder"),
                Self::Folder { root_name, path } => {
                    write!(f, "folder_{root_name}_{}", BASE64.encode(path))
                }
            }
        }
    }

    impl serde::Serialize for ObjectID {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            serializer.collect_str(self)
        }
    }

    impl<'de> serde::Deserialize<'de> for ObjectID {
        fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            use serde::de::Error;

            let id = String::deserialize(deserializer)?;

            Ok(if id == "0" {
                Self::Root
            } else if id == "folder" {
                Self::FolderRoot
            } else if let Some((root_name, path)) = id
                .strip_prefix("folder_")
                .and_then(|path| path.split_once('_'))
            {
                let path = String::from_utf8(
                    BASE64
                        .decode(path)
                        .map_err(|err| Error::custom(format!("Not base64: {err}")))?,
                )
                .map_err(|err| Error::custom(format!("Not UTF-8: {err}")))?;

                if Path::new(&path).is_absolute() {
                    return Err(Error::custom("Path must be relative"));
                }

                if Path::new(&path).iter().any(|item| item == "..") {
                    return Err(Error::custom("Paths may not have parent segments"));
                }

                Self::Folder {
                    root_name: root_name.into(),
                    path,
                }
            } else {
                return Err(Error::custom(format!("Bad ObjectID {id}")));
            })
        }
    }

    #[derive(serde::Deserialize)]
    pub enum BrowseFlag {
        BrowseMetadata,
        BrowseDirectChildren,
    }

    pub enum Child {
        Container {
            id: ObjectID,
            parent_id: ObjectID,
            title: String,
        },
        Item {
            id: ObjectID,
            parent_id: ObjectID,
            root_name: String,
            base64_path: String,
            artist: String,
            album: String,
            title: String,
            genre: String,
            year: i32,
            track_number: u16,
            mime_type: String,
        },
    }

    #[derive(Template)]
    #[template(path = "content_directory_browse_response_payload.xml")]
    pub enum ResponseResult {
        DirectChildren {
            hostname: String,
            port: u16,
            children: Vec<Child>,
        },
        Metadata {
            id: ObjectID,
            parent_id: ObjectID,
            title: String,
            child_count: usize,
        },
    }

    impl serde::Serialize for ResponseResult {
        fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            tracing::info!("Render");
            self.render()
                .map_err(<S::Error as serde::ser::Error>::custom)?
                .serialize(serializer)
        }
    }

    #[derive(Template)]
    #[template(path = "content_directory_browse_response.xml")]
    pub struct Response {
        pub result: ResponseResult,
        pub number_returned: usize,
        pub total_matches: usize,
    }

    pub enum Error {
        NotFound,
        IO,
    }

    impl IntoResponse for Error {
        fn into_response(self) -> askama_axum::Response {
            match self {
                Error::NotFound => StatusCode::NOT_FOUND,
                Error::IO => StatusCode::INTERNAL_SERVER_ERROR,
            }
            .into_response()
        }
    }

    impl From<std::io::Error> for Error {
        fn from(_: std::io::Error) -> Self {
            Self::IO
        }
    }

    impl From<quick_xml::DeError> for Error {
        fn from(_: quick_xml::DeError) -> Self {
            Self::IO
        }
    }
}

#[derive(serde::Deserialize)]

enum ContentDirectoryAction {
    #[serde(rename_all = "PascalCase")]
    Browse {
        #[serde(rename = "ObjectID")]
        object_id: content_directory_browse::ObjectID,
        browse_flag: content_directory_browse::BrowseFlag,
        filter: String,
        starting_index: usize,
        requested_count: usize,
    },
}

#[derive(Clone)]
struct Hostname {
    hostname: String,
}

#[derive(Clone)]
struct Port {
    port: u16,
}

#[derive(Clone)]
struct FolderRoots(Arc<HashMap<String, PathBuf>>);

#[derive(Clone, axum::extract::FromRef)]
struct AppState {
    id: Uuid,
    hostname: Hostname,
    port: Port,
    folder_roots: FolderRoots,
}

async fn handle_content_direction_action(
    State(Hostname { hostname }): State<Hostname>,
    State(Port { port }): State<Port>,
    State(FolderRoots(folder_roots)): State<FolderRoots>,
    SoapAction(action): SoapAction<ContentDirectoryAction>,
) -> Result<impl IntoResponse, content_directory_browse::Error> {
    use content_directory_browse::{BrowseFlag, Child, Error, ObjectID, Response, ResponseResult};

    Ok(match action {
        ContentDirectoryAction::Browse {
            object_id,
            browse_flag,
            filter,
            starting_index,
            requested_count,
        } => {
            tracing::info!(?object_id, "Browse");
            let mut children = match object_id {
                ObjectID::Root => vec![Child::Container {
                    id: ObjectID::FolderRoot,
                    parent_id: object_id.clone(),
                    title: "Folder".into(),
                }],

                ObjectID::FolderRoot => folder_roots
                    .keys()
                    .map(|root_name| Child::Container {
                        id: ObjectID::Folder {
                            root_name: root_name.clone(),
                            path: String::new(),
                        },
                        parent_id: object_id.clone(),
                        title: root_name.clone(),
                    })
                    .collect(),
                ObjectID::Folder {
                    ref root_name,
                    ref path,
                } => {
                    let mut directory_path =
                        folder_roots.get(root_name).ok_or(Error::NotFound)?.clone();

                    directory_path.push(path);

                    std::fs::read_dir(directory_path)?
                        .map(|entry| {
                            let entry = entry?;

                            let entry_name = entry.file_name().to_string_lossy().into_owned();
                            let entry_type = entry.file_type()?;
                            let entry_path = format!(
                                "{path}{}{entry_name}",
                                if path.is_empty() { "" } else { "/" }
                            );

                            Ok::<_, Error>(Some(if entry_type.is_dir() {
                                Child::Container {
                                    id: ObjectID::Folder {
                                        root_name: root_name.clone(),
                                        path: entry_path,
                                    },
                                    parent_id: object_id.clone(),
                                    title: entry_name,
                                }
                            } else if entry_type.is_file() {
                                let Ok(tags) = audiotags::Tag::new().read_from_path(entry.path())
                                else {
                                    return Ok(None);
                                };

                                let mime_type = mime_guess::from_path(entry.path())
                                    .first_or_octet_stream()
                                    .essence_str()
                                    .into();

                                Child::Item {
                                    id: ObjectID::Folder {
                                        root_name: root_name.clone(),
                                        path: entry_path.clone(),
                                    },
                                    parent_id: object_id.clone(),
                                    root_name: root_name.clone(),
                                    base64_path: BASE64.encode(&entry_path),
                                    artist: tags.artist().unwrap_or_default().into(),
                                    album: tags.album_title().unwrap_or_default().into(),
                                    title: tags.title().unwrap_or_default().into(),
                                    genre: tags.genre().unwrap_or_default().into(),
                                    year: tags.year().unwrap_or_default(),
                                    track_number: tags.track_number().unwrap_or_default(),
                                    mime_type,
                                }

                                /*if !entry_name.ends_with(".mp3") {
                                    return Ok(None);
                                }

                                Child::Item {
                                    id: ObjectID::Folder {
                                        root_name: root_name.clone(),
                                        path: entry_path.clone(),
                                    },
                                    parent_id: object_id.clone(),
                                    root_name: root_name.clone(),
                                    base64_path: BASE64.encode(&entry_path),
                                    artist: "My Artist".into(),
                                    album: "My Album".into(),
                                    title: entry_name,
                                }*/
                            } else {
                                return Ok(None);
                            }))
                        })
                        .filter_map(Result::transpose)
                        .collect::<Result<Vec<_>, _>>()?
                }
            };

            children.sort_by_key(|child| match child {
                Child::Container { title, .. } | Child::Item { title, .. } => title.clone(),
            });

            let total_matches = children.len();

            let children = children
                .into_iter()
                .skip(starting_index)
                .take(requested_count)
                .collect::<Vec<_>>();

            let number_returned = children.len();

            Response {
                result: match browse_flag {
                    BrowseFlag::BrowseMetadata => {
                        let parent_id = object_id.parent();
                        let title = object_id.title();

                        ResponseResult::Metadata {
                            id: object_id,
                            parent_id,
                            title,
                            child_count: children.len(),
                        }
                    }
                    BrowseFlag::BrowseDirectChildren => ResponseResult::DirectChildren {
                        hostname,
                        port,
                        children,
                    },
                },
                number_returned,
                total_matches,
            }
            .into_response()
        }
    })
}

#[derive(axum_extra::routing::TypedPath, serde::Deserialize)]
#[typed_path("/media/:root_name/:path")]
struct MediaRequest {
    root_name: String,
    path: String,
}

async fn handle_media_request(
    MediaRequest { root_name, path }: MediaRequest,
    State(FolderRoots(folder_roots)): State<FolderRoots>,
    range: Option<axum_extra::TypedHeader<axum_extra::headers::Range>>,
) -> Result<impl IntoResponse, axum::response::Response> {
    let decoded_path = String::from_utf8(BASE64.decode(&path).map_err(|err| {
        tracing::error!("Bad path: {err}");
        StatusCode::BAD_REQUEST.into_response()
    })?)
    .map_err(|err| {
        tracing::error!("Bad path: {err}");
        StatusCode::BAD_REQUEST.into_response()
    })?;

    tracing::debug!(root_name, path, decoded_path, "Media Request");

    drop(path);

    let root_path = folder_roots
        .get(&root_name)
        .ok_or_else(|| {
            tracing::error!("Folder root {root_name:?} not found");
            StatusCode::NOT_FOUND.into_response()
        })?
        .as_path();

    let file = tokio::fs::read(PathBuf::from_iter([root_path, decoded_path.as_ref()]))
        .await
        .map_err(|err| {
            tracing::error!("{err}");
            StatusCode::NOT_FOUND.into_response()
        })?;

    let headers = [
        (axum::http::header::CONTENT_TYPE, "audio/mpeg"),
        (axum::http::header::ACCEPT_RANGES, "bytes"),
    ];

    Ok(
        if let Some((start, end)) =
            range.and_then(|range| range.satisfiable_ranges(file.len() as u64).next())
        {
            let start = match start {
                std::ops::Bound::Included(start) => start,
                std::ops::Bound::Excluded(start) => start + 1,
                std::ops::Bound::Unbounded => 0,
            } as usize;

            let end = match end {
                std::ops::Bound::Included(end) => (end + 1) as usize,
                std::ops::Bound::Excluded(end) => end as usize,
                std::ops::Bound::Unbounded => file.len(),
            };

            (
                headers,
                [(
                    axum::http::header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{}", file.len()),
                )],
                axum::body::Body::from(Vec::from(file.get(start..end).unwrap_or_default())),
            )
                .into_response()
        } else {
            (headers, axum::body::Body::from(file)).into_response()
        },
    )
}

#[derive(axum_extra::routing::TypedPath, serde::Deserialize)]
#[typed_path("/album_art/:root_name/:path")]
struct AlbumArtRequest {
    root_name: String,
    path: String,
}

async fn handle_album_art_request(
    AlbumArtRequest { root_name, path }: AlbumArtRequest,
    State(FolderRoots(folder_roots)): State<FolderRoots>,
) -> Result<impl IntoResponse, axum::response::Response> {
    let decoded_path = String::from_utf8(BASE64.decode(&path).map_err(|err| {
        tracing::error!("Bad path: {err}");
        StatusCode::BAD_REQUEST.into_response()
    })?)
    .map_err(|err| {
        tracing::error!("Bad path: {err}");
        StatusCode::BAD_REQUEST.into_response()
    })?;

    tracing::debug!(root_name, path, decoded_path, "Media Request");

    let root_path = folder_roots
        .get(&root_name)
        .ok_or_else(|| {
            tracing::error!("Folder root {root_name:?} not found");
            StatusCode::NOT_FOUND.into_response()
        })?
        .as_path();

    let tags = audiotags::Tag::new()
        .read_from_path(PathBuf::from_iter([root_path, decoded_path.as_ref()]))
        .map_err(|_| StatusCode::NOT_FOUND.into_response())?;

    let album_art = tags
        .album_cover()
        .ok_or_else(|| StatusCode::NOT_FOUND.into_response())?;

    Ok((
        [(
            axum::http::header::CONTENT_TYPE,
            match album_art.mime_type {
                audiotags::MimeType::Png => "image/png",
                audiotags::MimeType::Jpeg => "image/jpeg",
                audiotags::MimeType::Tiff => "image/tiff",
                audiotags::MimeType::Bmp => "image/bmp",
                audiotags::MimeType::Gif => "image/gif",
            },
        )],
        axum::body::Body::from(Vec::from(album_art.data)),
    ))
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum LevelFilter {
    Trace,
    Debug,
    Warn,
    Info,
    Error,
    Off,
}

impl From<LevelFilter> for tracing_subscriber::filter::LevelFilter {
    fn from(value: LevelFilter) -> Self {
        match value {
            LevelFilter::Trace => Self::TRACE,
            LevelFilter::Debug => Self::DEBUG,
            LevelFilter::Warn => Self::WARN,
            LevelFilter::Info => Self::INFO,
            LevelFilter::Error => Self::ERROR,
            LevelFilter::Off => Self::OFF,
        }
    }
}

#[config_manager::config(file(
    format = "toml",
    clap(long = "config_file"),
    default = "config.toml"
))]
struct Config {
    #[source(clap(long), env, config)]
    id: Uuid,
    #[source(clap(long), env, config)]
    hostname: String,
    #[source(clap(long, short), env, config)]
    port: u16,
    #[source(config, default(init_from = "LevelFilter::Info"))]
    default_log: LevelFilter,
    #[source(config, default)]
    log: HashMap<String, LevelFilter>,
    #[source(config, default)]
    folders: HashMap<String, PathBuf>,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let Config {
        id,
        hostname,
        port,
        default_log,
        log,
        folders,
    } = config_manager::ConfigInit::parse().unwrap();

    tracing_subscriber::registry()
        .with(tracing_subscriber::filter::Targets::from_iter(log).with_default(default_log))
        .with(tracing_subscriber::fmt::Layer::default())
        .init();

    let http_listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port))
        .await
        .unwrap();

    let http_address = http_listener.local_addr().unwrap();
    let port = http_address.port();

    let (shutdown_handle, shutdown_signal) = shutdown::Signal::new();
    let shutdown_signal = shutdown_signal.shared();

    let ssdp_task = tokio::spawn(run_ssdp(id, hostname.clone(), port, shutdown_signal));

    tracing::info!(%http_address, "Listening");

    let http_app = axum::Router::new()
        .route(
            "/description.xml",
            get(|State(id)| async move { MediaServerDescription { id } }),
        )
        .nest(
            "/ContentDirectory",
            axum::Router::new()
                .route(
                    "/description.xml",
                    get(|| async { ContentDirectoryDescription }),
                )
                .route("/action", post(handle_content_direction_action)),
        )
        .typed_get(handle_media_request)
        .typed_get(handle_album_art_request)
        .with_state(AppState {
            id,
            hostname: Hostname { hostname },
            port: Port { port },
            folder_roots: FolderRoots(Arc::new(folders)),
        })
        .layer(tower_http::trace::TraceLayer::new_for_http());

    tokio::spawn(async move { axum::serve(http_listener, http_app).await.unwrap() });

    tokio::signal::ctrl_c().await.unwrap();

    shutdown_handle.signal_shutdown();

    ssdp_task.await.unwrap();
}
