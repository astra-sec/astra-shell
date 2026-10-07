//! Opt-in real managed gateway test, launched only by the disposable Linux fixture.
use super::*;
use crate::protocol::{ForwardFrame, OpenTcpForwardRequest};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn open(client: &AstraClient, port: u16) -> Result<(FramedSendStream, FramedRecvStream)> {
    let (send, mut recv, id) = client
        .open_request_stream(request::Command::OpenTcpForward(OpenTcpForwardRequest {
            mapping_id: uuid::Uuid::new_v4().to_string(),
            host: "127.0.0.1".into(),
            port: port.into(),
        }))
        .await?;
    match require_response(&mut recv, &id).await?.result {
        Some(response::Result::Ack(_)) => Ok((send, recv)),
        Some(response::Result::Error(e)) => Err(server_response_error(e)),
        _ => bail!("wrong forwarding response"),
    }
}

async fn exchange(
    (mut send, mut recv): (FramedSendStream, FramedRecvStream),
    payload: &[u8],
) -> Result<()> {
    let upload = async {
        for bytes in payload.chunks(32 * 1024) {
            write_message(
                &mut send,
                &WireMessage::new(wire_message::Body::ForwardFrame(ForwardFrame {
                    data: bytes.to_vec(),
                    eof: false,
                })),
            )
            .await?;
        }
        write_message(
            &mut send,
            &WireMessage::new(wire_message::Body::ForwardFrame(ForwardFrame {
                data: vec![],
                eof: true,
            })),
        )
        .await?;
        Ok::<(), anyhow::Error>(())
    };
    let download = async {
        let mut received = Vec::new();
        loop {
            let wire =
                crate::protocol::read_message_limited(&mut recv, crate::forwarding::MAX_WIRE)
                    .await?
                    .context("missing forward EOF")?;
            let Some(wire_message::Body::ForwardFrame(frame)) = wire.body else {
                bail!("wrong forward frame")
            };
            received.extend_from_slice(&frame.data);
            if frame.eof {
                break;
            }
        }
        anyhow::ensure!(received == payload, "forward payload differs");
        Ok::<(), anyhow::Error>(())
    };
    tokio::try_join!(upload, download)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires scripts/test-managed-identity.py in an isolated Linux container"]
async fn managed_uid_quota_and_lifecycle() -> Result<()> {
    anyhow::ensure!(cfg!(target_os = "linux") && Path::new("/.dockerenv").exists());
    let fixture = std::env::var("ASTRA_MANAGED_FORWARD_FIXTURE")?;
    let profile: Vec<_> = fixture.split('|').collect();
    anyhow::ensure!(profile.len() == 6, "invalid fixture profile");
    let root = PathBuf::from(profile[0]);
    anyhow::ensure!(
        root.starts_with("/tmp")
            && root
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("astra-worker-test-")
    );
    let port = profile[1];
    let mut clients = Vec::new();
    for user in &profile[2..4] {
        clients.push(
            AstraClient::connect_with_support(
                format!("127.0.0.1:{port}").parse()?,
                "localhost",
                &ServerTrust::PinnedCertificate(root.join("state/host-cert.der")),
                &root.join(format!("key-{user}")),
                user,
                ProtocolSupport::command_line_client(),
            )
            .await?,
        );
    }
    let target = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let target_port = target.local_addr()?.port();
    let echo = tokio::spawn(async move {
        loop {
            let (mut socket, _) = target.accept().await?;
            tokio::spawn(async move {
                let mut buf = [0; 32768];
                loop {
                    let n = socket.read(&mut buf).await?;
                    if n == 0 {
                        socket.shutdown().await?;
                        break;
                    }
                    socket.write_all(&buf[..n]).await?;
                }
                Ok::<(), std::io::Error>(())
            });
        }
        #[allow(unreachable_code)]
        Ok::<(), std::io::Error>(())
    });
    let a1 = open(&clients[0], target_port).await?;
    let a2 = open(&clients[0], target_port).await?;
    let rejected = open(&clients[0], target_port).await;
    anyhow::ensure!(rejected.is_err());
    anyhow::ensure!(
        rejected
            .err()
            .unwrap()
            .downcast_ref::<ServerResponseError>()
            .is_some_and(|e| e.code == "quota")
    );
    let b1 = open(&clients[1], target_port).await?;
    // Inspect the kernel TCP table: outgoing target sockets must belong to each worker UID, never root.
    let tcp = fs::read_to_string("/proc/net/tcp")?;
    let mut socket_uids: Vec<u32> = tcp
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            (fields[2].ends_with(&format!(":{target_port:04X}")) && fields[3] == "01")
                .then(|| fields[7].parse().unwrap())
        })
        .collect();
    socket_uids.sort();
    let uid_a = profile[4].parse::<u32>()?;
    let uid_b = profile[5].parse::<u32>()?;
    let mut expected = vec![uid_a, uid_a, uid_b];
    expected.sort();
    anyhow::ensure!(
        socket_uids == expected,
        "TCP socket UID mismatch: {socket_uids:?}"
    );
    // Idle accepted flows keep quota, while control remains live and other UID remains independent.
    tokio::time::sleep(Duration::from_secs(2)).await;
    clients[0].list().await?;
    clients[1].list().await?;
    drop(a1);
    drop(a2);
    exchange(b1, b"other UID remains usable").await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let payload: Vec<u8> = (0..1_048_576).map(|i| (i % 251) as u8).collect();
    for _ in 0..20 {
        let (a, b) = tokio::try_join!(
            open(&clients[0], target_port),
            open(&clients[1], target_port)
        )?;
        tokio::try_join!(exchange(a, &payload), exchange(b, &payload))?;
        clients[0].list().await?;
    }
    // Killing A retires only A's epoch; the old authenticated connection must fail,
    // while B can still forward. The Python owner subsequently handles recovery/cleanup.
    let uid_state = root.join(format!("state/users/{uid_a}"));
    let mut worker = tokio::net::UnixStream::connect(uid_state.join("session.sock")).await?;
    let identity = crate::worker_identity::discover(&mut worker, uid_a).await?;
    anyhow::ensure!(worker.peer_cred()?.pid() == Some(identity.pid as i32));
    drop(worker);
    // Only the kernel-verified worker of this disposable fixture is signalled.
    anyhow::ensure!(unsafe { nix::libc::kill(identity.pid as i32, nix::libc::SIGKILL) } == 0);
    tokio::time::sleep(Duration::from_millis(200)).await;
    anyhow::ensure!(
        open(&clients[0], target_port).await.is_err(),
        "old connection entered replacement worker"
    );
    exchange(
        open(&clients[1], target_port).await?,
        b"survives other worker failure",
    )
    .await?;
    let recovered = clients[0].reconnect().await?;
    exchange(
        open(&recovered, target_port).await?,
        b"new epoch accepts fresh flow",
    )
    .await?;
    recovered.finish_connection().await;
    for client in clients {
        client.finish_connection().await;
    }
    echo.abort();
    println!(
        "managed forwarding: kernel UID verified, quota=2 isolated/reclaimed, 40 MiB checked, idle/control, epoch fence and fresh reconnect passed"
    );
    Ok(())
}
