mod editor;

use std::net::SocketAddr;

use axum::{
    extract::{ConnectInfo, WebSocketUpgrade},
    response::Response,
    routing::get,
    Extension, Router,
};
use axum_prometheus::PrometheusMetricLayer;
use editor::{spawn_client, spawn_server, ClientParams, Connection, ServerHandle, ServerParams};
use futures::StreamExt;
use tracing_subscriber::prelude::*;
use uuid::Uuid;

#[cfg(not(debug_assertions))]
static ASSETS: include_dir::Dir<'_> = include_dir::include_dir!("$CARGO_MANIFEST_DIR/../dist");

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "server=debug,tower_http=debug,axum::rejection=trace".into()),
        )
        .with(console_subscriber::spawn())
        .with(
            tracing_subscriber::fmt::layer()
                .compact()
                .with_file(true)
                .with_line_number(true)
                .with_thread_ids(true)
                .with_target(false),
        )
        .try_init()
        .ok();

    let (prometheus_layer, metric_handle) = PrometheusMetricLayer::pair();

    let (server, join) = spawn_server(ServerParams { capacity: None });

    let asset_router = Router::new();

    #[cfg(not(debug_assertions))]
    let asset_router = asset_router
        .route("/", get(index))
        .route("/assets/*path", get(serve_static));

    let app = Router::new()
        .merge(asset_router)
        .route("/websocket", get(handle_websocket))
        .route("/metrics", get(|| async move { metric_handle.render() }))
        .layer(prometheus_layer)
        .layer(Extension(server));
    let app = app.into_make_service_with_connect_info::<SocketAddr>();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await.unwrap();
    tracing::info!("listening on http://{}", listener.local_addr().unwrap());

    let serve = axum::serve(listener, app);

    _ = tokio::join!(join, async {
        serve.await.unwrap();
    });
}

#[cfg(not(debug_assertions))]
async fn index() -> impl axum::response::IntoResponse {
    return axum::response::Html(ASSETS.get_file("index.html").unwrap().contents().to_owned());
}

#[cfg(not(debug_assertions))]
async fn serve_static(axum::extract::Path(path): axum::extract::Path<String>) -> impl axum::response::IntoResponse {
    use axum::{
        body::Body,
        http::{header, Response, StatusCode},
        response::IntoResponse,
    };
    use std::ffi::OsStr;
    let file = ASSETS.get_file(format!("assets/{path}"));

    if let Some(file) = file {
        let mime_type = file
            .path()
            .extension()
            .and_then(OsStr::to_str)
            .map(mime_guess::from_ext)
            .map(|m| m.first_or_octet_stream());

        return Response::builder()
            .status(StatusCode::OK)
            .header(
                header::CONTENT_TYPE,
                &mime_type.unwrap_or(mime_guess::mime::TEXT_HTML).to_string(),
            )
            .body(Body::from(file.contents().to_owned()))
            .unwrap();
    }

    StatusCode::NOT_FOUND.into_response()
}

async fn handle_websocket(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Extension(server_handle): Extension<ServerHandle>,
) -> Response {
    ws.on_upgrade(move |socket| async move {
        let (sender, receiver) = socket.split();
        spawn_client(ClientParams {
            id: Uuid::now_v7(),
            ip: addr,
            server_handle,
            connection: Connection { sender, receiver },
            capacity: None,
        });
    })
}
