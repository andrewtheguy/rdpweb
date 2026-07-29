use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context as _, bail};
use clap::Parser;
use ironrdp::dvc::encode_dvc_messages;
use ironrdp::svc::ChannelFlags;
use ironrdp_egfx::pdu::{Avc420Region, CapabilitiesAdvertisePdu, CapabilitySet, annex_b_to_avc};
use ironrdp_egfx::server::{GraphicsPipelineHandler, GraphicsPipelineServer};
use ironrdp_server::{
    Credentials, DesktopSize, DisplayUpdate, EgfxServerMessage, GfxDvcBridge, GfxServerFactory,
    GfxServerHandle, RdpServer, RdpServerDisplay, RdpServerDisplayUpdates, ServerEvent,
    ServerEventSender, TlsIdentityCtx,
};
use tokio::sync::mpsc;

const WIDTH: u16 = 320;
const HEIGHT: u16 = 180;

#[derive(Parser)]
#[command(about = "Disposable local RDP server that emits an EGFX/AVC420 test frame")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:3390")]
    listen: SocketAddr,

    #[arg(long)]
    cert: PathBuf,

    #[arg(long)]
    key: PathBuf,

    /// One H.264 access unit in Annex B format, including SPS, PPS, and IDR.
    #[arg(long)]
    h264: PathBuf,

    #[arg(long, default_value = "test")]
    username: String,

    #[arg(long, default_value = "test")]
    password: String,
}

struct FixtureHandler {
    ready_tx: mpsc::UnboundedSender<CapabilitySet>,
}

impl GraphicsPipelineHandler for FixtureHandler {
    fn capabilities_advertise(&mut self, _: &CapabilitiesAdvertisePdu) {}

    fn on_ready(&mut self, negotiated: &CapabilitySet) {
        let _ = self.ready_tx.send(negotiated.clone());
    }
}

struct FixtureGfxFactory {
    handle: GfxServerHandle,
    ready_tx: mpsc::UnboundedSender<CapabilitySet>,
}

impl GfxServerFactory for FixtureGfxFactory {
    fn build_gfx_handler(&self) -> Box<dyn GraphicsPipelineHandler> {
        Box::new(FixtureHandler {
            ready_tx: self.ready_tx.clone(),
        })
    }

    fn build_server_with_handle(&self) -> Option<(GfxDvcBridge, GfxServerHandle)> {
        Some((
            GfxDvcBridge::new(Arc::clone(&self.handle)),
            Arc::clone(&self.handle),
        ))
    }
}

impl ServerEventSender for FixtureGfxFactory {
    fn set_sender(&mut self, _: mpsc::UnboundedSender<ServerEvent>) {}
}

struct FixedDisplay;

#[async_trait::async_trait]
impl RdpServerDisplay for FixedDisplay {
    async fn size(&mut self) -> DesktopSize {
        DesktopSize {
            width: WIDTH,
            height: HEIGHT,
        }
    }

    async fn updates(&mut self) -> anyhow::Result<Box<dyn RdpServerDisplayUpdates>> {
        Ok(Box::new(NoDisplayUpdates))
    }
}

struct NoDisplayUpdates;

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for NoDisplayUpdates {
    async fn next_update(&mut self) -> anyhow::Result<Option<DisplayUpdate>> {
        std::future::pending().await
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tokio_rustls::rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    let args = Args::parse();
    let identity = TlsIdentityCtx::init_from_paths(&args.cert, &args.key)?;
    let acceptor = identity.make_acceptor()?;
    let annex_b = std::fs::read(&args.h264)
        .with_context(|| format!("read H.264 fixture {}", args.h264.display()))?;
    let avc = annex_b_to_avc(&annex_b);
    if avc.is_empty() {
        bail!("the H.264 fixture contained no Annex B NAL units");
    }

    let (ready_tx, ready_rx) = mpsc::unbounded_channel();
    let gfx_handle = Arc::new(Mutex::new(GraphicsPipelineServer::new(Box::new(
        FixtureHandler {
            ready_tx: ready_tx.clone(),
        },
    ))));
    let factory = FixtureGfxFactory {
        handle: Arc::clone(&gfx_handle),
        ready_tx,
    };

    let mut server = RdpServer::builder()
        .with_addr(args.listen)
        .with_hybrid(acceptor, identity.pub_key)
        .with_no_input()
        .with_display_handler(FixedDisplay)
        .with_gfx_factory(Some(Box::new(factory)))
        .build();
    server.set_credentials(Some(Credentials {
        username: args.username,
        password: args.password,
        domain: None,
    }));

    let event_tx = server.event_sender().clone();
    tokio::spawn(stream_fixture(gfx_handle, ready_rx, event_tx, avc));

    println!("EGFX fixture RDP server listening on {}", args.listen);
    server.run().await
}

async fn stream_fixture(
    gfx_handle: GfxServerHandle,
    mut ready_rx: mpsc::UnboundedReceiver<CapabilitySet>,
    event_tx: mpsc::UnboundedSender<ServerEvent>,
    avc: Vec<u8>,
) {
    let Some(negotiated) = ready_rx.recv().await else {
        return;
    };
    println!("EGFX negotiated {negotiated:?}");

    // Let the capability-confirm response reach the client before surface setup.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let surface_id = {
        let mut gfx = gfx_handle.lock().expect("EGFX server mutex poisoned");
        if !gfx.supports_avc420() {
            eprintln!("client did not negotiate AVC420");
            return;
        }
        gfx.set_output_dimensions(WIDTH, HEIGHT);
        let Some(surface_id) = gfx.create_surface(WIDTH, HEIGHT) else {
            eprintln!("EGFX server was not ready to create a surface");
            return;
        };
        if !gfx.map_surface_to_output(surface_id, 0, 0) {
            eprintln!("failed to map EGFX surface");
            return;
        }
        surface_id
    };

    for frame_number in 0..12u32 {
        let encoded = {
            let mut gfx = gfx_handle.lock().expect("EGFX server mutex poisoned");
            let regions = [Avc420Region::full_frame(WIDTH, HEIGHT, 20)];
            if let Some(frame_id) =
                gfx.send_avc420_frame(surface_id, &avc, &regions, frame_number * 250)
            {
                let Some(channel_id) = gfx.channel_id() else {
                    eprintln!("EGFX channel closed before frame {frame_id}");
                    return;
                };
                let messages = match encode_dvc_messages(
                    channel_id,
                    gfx.drain_output(),
                    ChannelFlags::SHOW_PROTOCOL,
                ) {
                    Ok(messages) => messages,
                    Err(error) => {
                        eprintln!("could not encode EGFX frame {frame_id}: {error}");
                        return;
                    }
                };
                Some((frame_id, messages))
            } else {
                None
            }
        };
        let Some(encoded) = encoded else {
            eprintln!("frame {frame_number} was backpressured");
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        };

        if event_tx
            .send(ServerEvent::Egfx(EgfxServerMessage::SendMessages {
                messages: encoded.1,
            }))
            .is_err()
        {
            return;
        }
        println!("sent AVC420 frame {}", encoded.0);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}
