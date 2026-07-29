mod config;
mod egfx;
mod keymap;
mod protocol;
mod rdp;

use std::sync::Arc;

use anyhow::Context as _;
use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use clap::Parser as _;
use futures_util::{SinkExt as _, StreamExt as _};
use log::{info, warn};
use tokio::sync::{mpsc, oneshot};

use crate::config::{Args, Target};
use crate::protocol::{ClientMsg, GatewayEvent};

const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const STYLE_CSS: &str = include_str!("../web/style.css");

#[derive(Clone)]
struct AppState {
    target: Arc<Target>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    tokio_rustls::rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    let args = Args::parse();
    let listen = args.listen;
    let target = Arc::new(args.resolve_target()?);
    let target_name = target
        .name
        .clone()
        .unwrap_or_else(|| "RDP target".to_owned());

    let state = AppState { target };
    let app = Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .route("/ws", get(ws_upgrade))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind HTTP listener to {listen}"))?;

    info!("serving {target_name:?} at http://{listen}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("HTTP server")?;

    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn app_js() -> impl IntoResponse {
    ([("content-type", "text/javascript; charset=utf-8")], APP_JS)
}

async fn style_css() -> impl IntoResponse {
    ([("content-type", "text/css; charset=utf-8")], STYLE_CSS)
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| ws_session(socket, state.target))
}

async fn ws_session(socket: WebSocket, target: Arc<Target>) {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let (input_tx, input_rx) = mpsc::unbounded_channel();
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let mut rdp_done = spawn_rdp(target, input_rx, event_tx);
    let mut event_channel_open = true;

    loop {
        tokio::select! {
            incoming = ws_rx.next() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMsg>(&text) {
                            Ok(input) => {
                                if input_tx.send(input).is_err() {
                                    warn!("RDP input channel closed");
                                    break;
                                }
                            }
                            Err(error) => warn!("ignoring invalid browser input: {error}"),
                        }
                    }
                    Some(Ok(Message::Close(frame))) => {
                        info!("browser WebSocket closed: {frame:?}");
                        break;
                    }
                    None => {
                        info!("browser WebSocket stream ended");
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        warn!("browser WebSocket read failed: {error}");
                        break;
                    }
                }
            }
            event = event_rx.recv(), if event_channel_open => {
                let Some(event) = event else {
                    event_channel_open = false;
                    continue;
                };
                let message = match event {
                    GatewayEvent::Control(control) => {
                        match serde_json::to_string(&control) {
                            Ok(json) => Message::Text(json.into()),
                            Err(error) => {
                                warn!("could not serialize control message: {error}");
                                continue;
                            }
                        }
                    }
                    GatewayEvent::Video(packet) => Message::Binary(packet.encode().into()),
                };
                if let Err(error) = ws_tx.send(message).await {
                    warn!("browser WebSocket write failed: {error}");
                    break;
                }
            }
            result = &mut rdp_done => {
                match result {
                    Ok(Ok(())) => info!("RDP session ended"),
                    Ok(Err(error)) => {
                        warn!("RDP session failed: {error:#}");
                        let control = protocol::ControlMsg::Error {
                            message: format!("{error:#}"),
                        };
                        if let Ok(json) = serde_json::to_string(&control) {
                            let _ = ws_tx.send(Message::Text(json.into())).await;
                        }
                    }
                    Err(error) => warn!("RDP thread ended without a result: {error}"),
                }
                break;
            }
        }
    }

    drop(input_tx);
}

fn spawn_rdp(
    target: Arc<Target>,
    input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    event_tx: mpsc::UnboundedSender<GatewayEvent>,
) -> oneshot::Receiver<anyhow::Result<()>> {
    let (done_tx, done_rx) = oneshot::channel();
    std::thread::Builder::new()
        .name("rdp-session".to_owned())
        .spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("build RDP runtime")
                .and_then(|runtime| runtime.block_on(rdp::run(target, input_rx, event_tx)));
            let _ = done_tx.send(result);
        })
        .expect("spawn RDP session thread");
    done_rx
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
