//! End-to-end: the server offers RDP-UDP multitransport, a client establishes the sideband
//! transport, and the server migrates EGFX onto it (Soft-Sync).

#[path = "support/client.rs"]
mod client;

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clawtank_core::access::AccessPolicy;
use clawtank_core::events::CoreEvent;
use clawtank_core::server::{self, ServerConfig, ServerHandle};
use clawtank_core::tls::TlsIdentity;
use ironrdp_async::FramedWrite as _;
use ironrdp_connector::{ClientConnector, ConnectionResult, MultitransportResult};
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_pdu::gcc::MultiTransportFlags;
use ironrdp_pdu::rdp::multitransport::MultitransportResponsePdu;
use ironrdp_rdpeudp_tokio::multitransport::MultitransportBootstrap;
use ironrdp_rdpeudp_tokio::transport::UdpTlsConfig;
use ironrdp_session::image::DecodedImage;
use ironrdp_session::{ActiveStage, ActiveStageOutput};
use tokio::net::TcpStream;

const USER: &str = "tester";
const PASSWORD: &str = "correct horse battery staple";
const WIDTH: u16 = 1024;
const HEIGHT: u16 = 768;

fn start_server() -> (ServerHandle, Arc<Mutex<Vec<CoreEvent>>>) {
    let events: Arc<Mutex<Vec<CoreEvent>>> = Arc::default();
    let log = Arc::clone(&events);
    let identity = TlsIdentity::generate("clawtank-test.local").unwrap();
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        username: USER.to_owned(),
        password: PASSWORD.to_owned(),
        identity,
        policy: AccessPolicy::lan_only(),
        udp: true,
    };
    let handle = server::start(
        config,
        Arc::new(move |e| log.lock().unwrap().push(e)),
        Arc::new(|_| {}),
    )
    .unwrap();
    (handle, events)
}

struct Client {
    result: ConnectionResult,
    framed: ironrdp_tokio::TokioFramed<tokio_rustls::client::TlsStream<TcpStream>>,
    transport: ironrdp_rdpeudp_tokio::transport::UdpTransport,
}

/// Connects, performing the multitransport UDP bootstrap the server requests.
async fn connect(addr: SocketAddr) -> anyhow::Result<Client> {
    let mut cfg = client::client_config(USER, PASSWORD, WIDTH, HEIGHT);
    cfg.multitransport_flags = Some(
        MultiTransportFlags::TRANSPORT_TYPE_UDP_FECR | MultiTransportFlags::SOFT_SYNC_TCP_TO_UDP,
    );
    let egfx = client::EgfxRecorder::default();
    let tcp = TcpStream::connect(addr).await?;
    tcp.set_nodelay(true)?;
    let client_addr = tcp.local_addr()?;
    let mut framed = ironrdp_tokio::TokioFramed::new(tcp);
    let mut connector = ClientConnector::new(cfg, client_addr);
    let gfx = ironrdp_egfx::client::GraphicsPipelineClient::new(
        Box::new(client::RecordingHandler(egfx.clone())),
        Some(Box::new(client::RecordingDecoder(egfx.clone()))),
    );
    connector.attach_static_channel(
        ironrdp_dvc::DrdynvcClient::new()
            .with_dynamic_channel(gfx)
            .with_dynamic_channel(ironrdp_displaycontrol::client::DisplayControlClient::new(
                |_| Ok(Vec::new()),
            )),
    );

    let should_upgrade = ironrdp_async::connect_begin(&mut framed, &mut connector).await?;
    let tcp = framed.into_inner_no_leftover();
    let tls_config = tokio_rustls::rustls::ClientConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(client::AcceptAnyCert))
    .with_no_client_auth();
    let tls = tokio_rustls::TlsConnector::from(Arc::new(tls_config))
        .connect("clawtank-server".try_into()?, tcp)
        .await?;
    let cert_der = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|c| c.first())
        .ok_or_else(|| anyhow::anyhow!("no server certificate"))?
        .to_vec();
    let server_public_key = TlsIdentity {
        cert_der,
        key_pkcs8_der: Vec::new(),
    }
    .public_key()
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let upgraded = ironrdp_async::mark_as_upgraded(should_upgrade, &mut connector);
    let mut framed = ironrdp_tokio::TokioFramed::new(tls);
    let transport: Arc<Mutex<Option<ironrdp_rdpeudp_tokio::transport::UdpTransport>>> =
        Arc::default();
    let slot = Arc::clone(&transport);
    let outcome = ironrdp_async::connect_finalize_with_multitransport(
        upgraded,
        connector,
        &mut framed,
        &mut ironrdp_tokio::reqwest::ReqwestNetworkClient::new(),
        "clawtank-server".into(),
        server_public_key,
        None,
        |request, _soft_sync| async {
            let mut bootstrap = MultitransportBootstrap::new(request);
            let udp_addr = SocketAddr::new(addr.ip(), addr.port());
            let connected = bootstrap
                .connect(
                    udp_addr,
                    "clawtank-server".into(),
                    Default::default(),
                    UdpTlsConfig::new(addr.to_string()),
                )
                .await
                .is_ok();
            if connected {
                *slot.lock().unwrap() = bootstrap.take_transport();
                Ok(MultitransportResult::Success)
            } else {
                Ok(MultitransportResult::Failure(
                    MultitransportResponsePdu::E_ABORT,
                ))
            }
        },
    )
    .await?
    .into_connected()?;
    let transport = transport
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| anyhow::anyhow!("no UDP transport was established"))?;
    Ok(Client {
        result: outcome,
        framed,
        transport,
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn egfx_moves_to_the_udp_transport() {
    let (server, events) = start_server();
    let addr = server.local_addr();
    let mut client = connect(addr).await.expect("connect with UDP");

    let (w, h) = (
        client.result.desktop_size.width,
        client.result.desktop_size.height,
    );
    let mut stage: ActiveStage = client::active_stage(client.result);
    stage.enable_reliable_udp_dvc_tunnel().unwrap();
    let mut image = DecodedImage::new(PixelFormat::RgbA32, w, h);

    // Wait for the EGFX channel to be ready before submitting.
    let ready = Instant::now() + Duration::from_secs(10);
    loop {
        if events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, CoreEvent::VideoStart { .. }))
        {
            break;
        }
        assert!(Instant::now() < ready, "EGFX never became ready");
        let read = tokio::time::timeout(Duration::from_millis(200), client.framed.read_pdu()).await;
        let Ok(read) = read else { continue };
        let (action, payload) = read.unwrap();
        for out in stage.process(&mut image, action, &payload, None).unwrap() {
            if let ActiveStageOutput::ResponseFrame(bytes) = out {
                client.framed.write_all(&bytes).await.unwrap();
            }
        }
    }

    // A dummy keyframe makes the server create its EGFX surface and dispatch, which is what
    // triggers the Soft-Sync request that moves EGFX to the tunnel.
    let frame = [0u8, 0, 0, 1, 0x65, 0x88, 0x84, 0x00];
    assert!(matches!(
        server.video().submit_h264(&frame, true, w, h),
        clawtank_core::video::SubmitResult::Sent
    ));

    let deadline = Instant::now() + Duration::from_secs(10);
    while !stage.reliable_udp_dvc_tunnel_in_use() {
        assert!(
            Instant::now() < deadline,
            "Soft-Sync never switched EGFX to the tunnel"
        );
        let read = tokio::time::timeout(Duration::from_millis(200), client.framed.read_pdu()).await;
        let Ok(read) = read else { continue };
        let (action, payload) = read.unwrap();
        for out in stage.process(&mut image, action, &payload, None).unwrap() {
            if let ActiveStageOutput::ResponseFrame(bytes) = out {
                client.framed.write_all(&bytes).await.unwrap();
            }
        }
    }
    assert!(
        events
            .lock()
            .unwrap()
            .iter()
            .any(|e| matches!(e, CoreEvent::ClientConnected(_)))
    );
    let _ = client.transport.shutdown().await;
}
