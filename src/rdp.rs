use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Context as _;
use ironrdp::core::decode;
use ironrdp::connector::{
    ClientConnector, Config, ConnectionResult, Credentials, DesktopSize, ServerName,
};
use ironrdp::dvc::DrdynvcClient;
use ironrdp::graphics::image_processing::PixelFormat;
use ironrdp::pdu::gcc::{ClientEarlyCapabilityFlags, ConferenceCreateRequest};
use ironrdp::pdu::gcc::KeyboardType;
use ironrdp::pdu::input::MousePdu;
use ironrdp::pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
use ironrdp::pdu::input::mouse::PointerFlags;
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp::pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp::pdu::x224::{X224, X224Data};
use ironrdp::pdu::mcs;
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStageBuilder, ActiveStageOutput};
use ironrdp_tokio::reqwest::ReqwestNetworkClient;
use ironrdp_tokio::bytes::BytesMut;
use ironrdp_tokio::{
    Framed, FramedRead, FramedWrite, StreamWrapper, TokioFramed,
};
use log::{debug, info};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::sync::mpsc;

use crate::config::Target;
use crate::egfx::EgfxPassthrough;
use crate::keymap;
use crate::protocol::{ClientMsg, ControlMsg, GatewayEvent, MouseButton};

trait AsyncReadWrite: AsyncRead + AsyncWrite {}
impl<T> AsyncReadWrite for T where T: AsyncRead + AsyncWrite {}
type ErasedStream = Box<dyn AsyncReadWrite + Unpin + Send + Sync>;
type UpgradedFramed = Framed<GfxFlagStream<ErasedStream>>;

struct GfxFlagStream<S> {
    inner: S,
    flag_sent: bool,
}

impl<S> StreamWrapper for GfxFlagStream<S> {
    type InnerStream = S;

    fn from_inner(inner: Self::InnerStream) -> Self {
        Self {
            inner,
            flag_sent: false,
        }
    }

    fn into_inner(self) -> Self::InnerStream {
        self.inner
    }

    fn get_inner(&self) -> &Self::InnerStream {
        &self.inner
    }

    fn get_inner_mut(&mut self) -> &mut Self::InnerStream {
        &mut self.inner
    }
}

impl<S> FramedRead for GfxFlagStream<S>
where
    S: Send + Sync + Unpin + AsyncRead,
{
    type ReadFut<'read>
        = Pin<Box<dyn Future<Output = io::Result<usize>> + Send + Sync + 'read>>
    where
        Self: 'read;

    fn read<'a>(&'a mut self, buf: &'a mut BytesMut) -> Self::ReadFut<'a> {
        Box::pin(async { self.inner.read_buf(buf).await })
    }
}

impl<S> FramedWrite for GfxFlagStream<S>
where
    S: Send + Sync + Unpin + AsyncWrite,
{
    type WriteAllFut<'write>
        = Pin<Box<dyn Future<Output = io::Result<()>> + Send + Sync + 'write>>
    where
        Self: 'write;

    fn write_all<'a>(&'a mut self, buf: &'a [u8]) -> Self::WriteAllFut<'a> {
        Box::pin(async move {
            let patched = if self.flag_sent {
                None
            } else {
                match add_gfx_capability_flag(buf) {
                    Ok(Some(packet)) => {
                        self.flag_sent = true;
                        Some(packet)
                    }
                    Ok(None) => None,
                    Err(error) => return Err(io::Error::other(error)),
                }
            };
            self.inner.write_all(patched.as_deref().unwrap_or(buf)).await?;
            self.inner.flush().await
        })
    }
}

fn add_gfx_capability_flag(packet: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
    let Ok(X224(x224_data)) = decode::<X224<X224Data<'_>>>(packet) else {
        return Ok(None);
    };
    let Ok(mut connect_initial) = decode::<mcs::ConnectInitial>(x224_data.data.as_ref()) else {
        return Ok(None);
    };

    let mut gcc_blocks = connect_initial
        .conference_create_request
        .into_gcc_blocks();
    let flags = gcc_blocks
        .core
        .optional_data
        .early_capability_flags
        .get_or_insert(ClientEarlyCapabilityFlags::empty());
    flags.insert(ClientEarlyCapabilityFlags::SUPPORT_DYN_VC_GFX_PROTOCOL);
    connect_initial.conference_create_request =
        ConferenceCreateRequest::new(gcc_blocks).context("rebuild GCC conference request")?;

    let mut patched = ironrdp::core::WriteBuf::new();
    let written = ironrdp::connector::encode_x224_packet(&connect_initial, &mut patched)
        .context("encode EGFX-capable MCS Connect Initial")?;
    Ok(Some(patched.filled()[..written].to_vec()))
}

pub async fn run(
    target: Arc<Target>,
    mut input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    event_tx: mpsc::UnboundedSender<GatewayEvent>,
) -> anyhow::Result<()> {
    control(
        &event_tx,
        ControlMsg::Status {
            phase: "connecting",
            message: format!("connecting to {}", target.host),
        },
    );

    let destination = host_port(&target.host, target.port);
    let stream = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        tokio::net::TcpStream::connect(&destination),
    )
    .await
    .with_context(|| format!("TCP connect to {destination} timed out"))?
    .with_context(|| format!("TCP connect to {destination}"))?;
    stream.set_nodelay(true).ok();

    let (connection, mut framed) = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        connect(&target, stream, event_tx.clone()),
    )
    .await
    .context("RDP handshake timed out")??;

    info!(
        "RDP connected at {}x{}",
        connection.desktop_size.width, connection.desktop_size.height
    );
    control(
        &event_tx,
        ControlMsg::Status {
            phase: "connected",
            message: "RDP active; waiting for EGFX/AVC420".to_owned(),
        },
    );
    control(
        &event_tx,
        ControlMsg::Resize {
            width: u32::from(connection.desktop_size.width),
            height: u32::from(connection.desktop_size.height),
        },
    );

    let mut image = DecodedImage::new(
        PixelFormat::RgbA32,
        connection.desktop_size.width,
        connection.desktop_size.height,
    );
    let mut active_stage = ActiveStageBuilder {
        static_channels: connection.static_channels,
        user_channel_id: connection.user_channel_id,
        io_channel_id: connection.io_channel_id,
        message_channel_id: connection.message_channel_id,
        share_id: connection.share_id,
        compression_type: connection.compression_type,
        enable_server_pointer: connection.enable_server_pointer,
        pointer_software_rendering: connection.pointer_software_rendering,
    }
    .build();
    let mut last_pointer = (
        connection.desktop_size.width / 2,
        connection.desktop_size.height / 2,
    );

    loop {
        let outputs = tokio::select! {
            frame = framed.read_pdu() => {
                let (action, payload) = frame.context("read RDP frame")?;
                active_stage
                    .process(&mut image, action, &payload)
                    .context("process RDP frame")?
            }
            input = input_rx.recv() => {
                let Some(input) = input else {
                    break;
                };
                let events = translate_input(input, &mut last_pointer);
                if events.is_empty() {
                    continue;
                }
                active_stage
                    .process_fastpath_input(&mut image, &events)
                    .context("encode RDP input")?
            }
        };

        for output in outputs {
            match output {
                ActiveStageOutput::ResponseFrame(frame) => {
                    framed
                        .write_all(&frame)
                        .await
                        .context("write RDP response")?;
                }
                ActiveStageOutput::Terminate(reason) => {
                    info!("RDP server terminated the session: {reason:?}");
                    return Ok(());
                }
                ActiveStageOutput::DeactivateAll => {
                    control(
                        &event_tx,
                        ControlMsg::Warning {
                            message: "server deactivated the session; resize/reactivation is outside this POC"
                                .to_owned(),
                        },
                    );
                }
                ActiveStageOutput::GraphicsUpdate(_) => {
                    debug!("classic graphics update ignored while waiting for EGFX");
                }
                _ => {}
            }
        }
    }

    Ok(())
}

async fn connect(
    target: &Target,
    stream: tokio::net::TcpStream,
    event_tx: mpsc::UnboundedSender<GatewayEvent>,
) -> anyhow::Result<(ConnectionResult, UpgradedFramed)> {
    let client_addr = stream.local_addr().context("get local socket address")?;
    let egfx = EgfxPassthrough::new(event_tx);
    let drdynvc = DrdynvcClient::new().with_dynamic_channel(egfx);
    let mut connector = ClientConnector::new(build_connector_config(target), client_addr)
        .with_static_channel(drdynvc);
    let mut framed = TokioFramed::new(stream);

    let should_upgrade = ironrdp_tokio::connect_begin(&mut framed, &mut connector)
        .await
        .context("RDP negotiation")?;
    let (plain_stream, leftover) = framed.into_inner();
    let (tls_stream, tls_certificate) = ironrdp_tls::upgrade(plain_stream, &target.host)
        .await
        .context("RDP TLS upgrade")?;
    let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);
    let erased: ErasedStream = Box::new(tls_stream);
    let mut framed: UpgradedFramed = Framed::new_with_leftover(erased, leftover);
    let server_public_key = ironrdp_tls::extract_tls_server_public_key(&tls_certificate)
        .context("extract RDP TLS public key")?
        .to_owned();

    let result = ironrdp_tokio::connect_finalize(
        upgraded,
        connector,
        &mut framed,
        &mut ReqwestNetworkClient::new(),
        ServerName::new(&target.host),
        server_public_key,
        None,
    )
    .await
    .context("RDP activation/CredSSP")?;

    Ok((result, framed))
}

fn build_connector_config(target: &Target) -> Config {
    Config {
        credentials: Credentials::UsernamePassword {
            username: target.username.clone(),
            password: target.password.clone(),
        },
        domain: None,
        enable_tls: true,
        enable_credssp: true,
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: DesktopSize {
            width: target.width,
            height: target.height,
        },
        bitmap: None,
        client_build: 0,
        client_name: "rdpweb".to_owned(),
        client_dir: "C:\\Windows\\System32\\mstscax.dll".to_owned(),
        #[cfg(windows)]
        platform: MajorPlatformType::WINDOWS,
        #[cfg(target_os = "macos")]
        platform: MajorPlatformType::MACINTOSH,
        #[cfg(target_os = "linux")]
        platform: MajorPlatformType::UNIX,
        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        platform: MajorPlatformType::UNIX,
        enable_server_pointer: false,
        pointer_software_rendering: false,
        request_data: None,
        autologon: false,
        enable_audio_playback: false,
        compression_type: None,
        multitransport_flags: None,
        desktop_scale_factor: 0,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        performance_flags: PerformanceFlags::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
    }
}

fn translate_input(input: ClientMsg, last_pointer: &mut (u16, u16)) -> Vec<FastPathInputEvent> {
    match input {
        ClientMsg::MouseMove { x, y } => {
            let position = (clamp_u16(x), clamp_u16(y));
            *last_pointer = position;
            vec![FastPathInputEvent::MouseEvent(MousePdu {
                flags: PointerFlags::MOVE,
                number_of_wheel_rotation_units: 0,
                x_position: position.0,
                y_position: position.1,
            })]
        }
        ClientMsg::MouseButton { button, pressed } => {
            let mut flags = match button {
                MouseButton::Left => PointerFlags::LEFT_BUTTON,
                MouseButton::Middle => PointerFlags::MIDDLE_BUTTON_OR_WHEEL,
                MouseButton::Right => PointerFlags::RIGHT_BUTTON,
            };
            if pressed {
                flags |= PointerFlags::DOWN;
            }
            vec![FastPathInputEvent::MouseEvent(MousePdu {
                flags,
                number_of_wheel_rotation_units: 0,
                x_position: last_pointer.0,
                y_position: last_pointer.1,
            })]
        }
        ClientMsg::Wheel { dx, dy } => {
            let mut events = Vec::new();
            if dy != 0.0 {
                events.push(FastPathInputEvent::MouseEvent(MousePdu {
                    flags: PointerFlags::VERTICAL_WHEEL,
                    number_of_wheel_rotation_units: if dy > 0.0 { -120 } else { 120 },
                    x_position: last_pointer.0,
                    y_position: last_pointer.1,
                }));
            }
            if dx != 0.0 {
                events.push(FastPathInputEvent::MouseEvent(MousePdu {
                    flags: PointerFlags::HORIZONTAL_WHEEL,
                    number_of_wheel_rotation_units: if dx > 0.0 { 120 } else { -120 },
                    x_position: last_pointer.0,
                    y_position: last_pointer.1,
                }));
            }
            events
        }
        ClientMsg::Key { code, pressed } => {
            let Some((scancode, extended)) = keymap::scancode(&code) else {
                debug!("unmapped browser key: {code}");
                return Vec::new();
            };
            let mut flags = KeyboardFlags::empty();
            if !pressed {
                flags |= KeyboardFlags::RELEASE;
            }
            if extended {
                flags |= KeyboardFlags::EXTENDED;
            }
            vec![FastPathInputEvent::KeyboardEvent(flags, scancode)]
        }
    }
}

fn clamp_u16(value: i32) -> u16 {
    value.clamp(0, i32::from(u16::MAX)) as u16
}

fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn control(tx: &mpsc::UnboundedSender<GatewayEvent>, message: ControlMsg) {
    let _ = tx.send(GatewayEvent::Control(message));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brackets_ipv6_destination() {
        assert_eq!(host_port("2001:db8::20", 3389), "[2001:db8::20]:3389");
        assert_eq!(host_port("desktop", 3389), "desktop:3389");
    }

    #[test]
    fn clamps_pointer_coordinates() {
        assert_eq!(clamp_u16(-1), 0);
        assert_eq!(clamp_u16(42), 42);
        assert_eq!(clamp_u16(100_000), u16::MAX);
    }

    #[test]
    fn adds_dynamic_graphics_pipeline_capability_to_connector_packet() {
        let config = build_connector_config(&Target {
            protocol: Some("rdp".to_owned()),
            name: None,
            host: "desktop".to_owned(),
            port: 3389,
            username: "user".to_owned(),
            password: "secret".to_owned(),
            width: 1280,
            height: 800,
        });
        let mut connector = ClientConnector::new(
            config,
            "127.0.0.1:49152".parse().expect("client address"),
        );
        connector.state = ironrdp::connector::ClientConnectorState::BasicSettingsExchangeSendInitial {
            selected_protocol: ironrdp::pdu::nego::SecurityProtocol::HYBRID_EX,
        };
        let mut packet = ironrdp::core::WriteBuf::new();
        let written = ironrdp::connector::Sequence::step_no_input(
            &mut connector,
            &mut packet,
        )
        .expect("build connector packet")
        .size()
        .expect("connector wrote a packet");

        let patched = add_gfx_capability_flag(&packet.filled()[..written])
            .expect("patch connector packet")
            .expect("packet is MCS Connect Initial");
        let X224(x224_data) =
            decode::<X224<X224Data<'_>>>(&patched).expect("decode patched X.224 packet");
        let connect_initial = decode::<mcs::ConnectInitial>(x224_data.data.as_ref())
            .expect("decode patched MCS Connect Initial");
        assert!(
            connect_initial
                .conference_create_request
                .gcc_blocks()
                .core
                .optional_data
                .early_capability_flags
                .expect("early capability flags")
                .contains(ClientEarlyCapabilityFlags::SUPPORT_DYN_VC_GFX_PROTOCOL)
        );
    }
}
