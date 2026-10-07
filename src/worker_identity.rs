//! Bounded, versioned local worker handshake. Never accepted on a QUIC stream.
use anyhow::{Context, Result, ensure};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

const MAGIC: &[u8; 8] = b"ASTRAWK2";
const VERSION: u32 = 2;
pub const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WorkerIdentity {
    pub uid: u32,
    pub pid: u32,
    pub epoch: Uuid,
}

impl WorkerIdentity {
    pub fn new(uid: u32) -> Self {
        Self {
            uid,
            pid: std::process::id(),
            epoch: Uuid::new_v4(),
        }
    }
}

pub(crate) async fn discover<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    expected_uid: u32,
) -> Result<WorkerIdentity> {
    let challenge = Uuid::new_v4();
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        stream.write_all(MAGIC).await?;
        stream.write_all(challenge.as_bytes()).await?;
        let mut response = [0u8; 60];
        stream.read_exact(&mut response).await?;
        ensure!(
            &response[..8] == MAGIC,
            "incompatible worker handshake; explicit worker upgrade required"
        );
        ensure!(
            u32::from_be_bytes(response[8..12].try_into()?) == VERSION,
            "unsupported internal worker protocol; explicit worker upgrade required"
        );
        ensure!(
            &response[44..60] == challenge.as_bytes(),
            "worker challenge mismatch"
        );
        let identity = WorkerIdentity {
            uid: u32::from_be_bytes(response[12..16].try_into()?),
            pid: u32::from_be_bytes(response[16..20].try_into()?),
            epoch: Uuid::from_slice(&response[20..36])?,
        };
        ensure!(response[36..44] == [0; 8], "unsupported worker flags");
        ensure!(
            identity.uid == expected_uid && identity.pid > 0 && !identity.epoch.is_nil(),
            "worker identity mismatch"
        );
        Ok(identity)
    })
    .await
    .context("worker identity handshake timed out; existing worker was not replaced")?
}

pub(crate) async fn introduce<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    identity: WorkerIdentity,
) -> Result<()> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let mut request = [0u8; 24];
        stream.read_exact(&mut request).await?;
        ensure!(
            &request[..8] == MAGIC,
            "unsupported gateway worker protocol"
        );
        let mut response = [0u8; 60];
        response[..8].copy_from_slice(MAGIC);
        response[8..12].copy_from_slice(&VERSION.to_be_bytes());
        response[12..16].copy_from_slice(&identity.uid.to_be_bytes());
        response[16..20].copy_from_slice(&identity.pid.to_be_bytes());
        response[20..36].copy_from_slice(identity.epoch.as_bytes());
        response[44..60].copy_from_slice(&request[8..24]);
        stream.write_all(&response).await?;
        Ok(())
    })
    .await
    .context("gateway worker handshake timed out")?
}

pub(crate) fn pin(current: &mut Option<WorkerIdentity>, candidate: WorkerIdentity) -> Result<()> {
    if let Some(identity) = current {
        ensure!(
            *identity == candidate,
            "worker epoch invalidated; reconnect and enumerate sessions"
        );
    } else {
        *current = Some(candidate);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn identity_is_stable_and_fenced_per_connection() -> Result<()> {
        let identity = WorkerIdentity::new(501);
        for _ in 0..8 {
            let (mut gateway, mut worker) = tokio::io::duplex(128);
            let (found, served) = tokio::join!(
                discover(&mut gateway, 501),
                introduce(&mut worker, identity)
            );
            served?;
            assert_eq!(found?, identity);
        }
        let mut connection = None;
        pin(&mut connection, identity)?;
        pin(&mut connection, identity)?;
        assert!(pin(&mut connection, WorkerIdentity::new(501)).is_err());
        assert!(pin(&mut connection, WorkerIdentity::new(502)).is_err());
        assert_eq!(connection, Some(identity));
        let mut other_connection = None;
        pin(&mut other_connection, WorkerIdentity::new(502))?;
        Ok(())
    }
    #[tokio::test]
    async fn rejects_wrong_uid_and_unversioned_worker() {
        let (mut gateway, mut worker) = tokio::io::duplex(128);
        let (found, _) = tokio::join!(
            discover(&mut gateway, 501),
            introduce(&mut worker, WorkerIdentity::new(502))
        );
        assert!(found.is_err());
        let (mut gateway, mut worker) = tokio::io::duplex(128);
        let (found, _) = tokio::join!(discover(&mut gateway, 501), async {
            let mut query = [0; 24];
            worker.read_exact(&mut query).await.unwrap();
            worker.write_all(&[0; 60]).await.unwrap();
        });
        assert!(found.is_err());
    }
}
