//! Sends the one terminal [`CiResultMessage`] a run's post-`Converged` tail
//! produces back to `warden`'s reverse socket (issue #15/ADR-0011). Mirrors
//! `relay::relay`'s discipline exactly: connect, write the payload, shut the
//! write half down -- no retry, no parsing here (that happens on `warden`'s
//! side, at its own boundary).

use std::path::Path;

use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;
use warden_core::CiResultMessage;

use crate::error::{GatedError, Result};

/// Delivers `message` to `warden`'s reverse-channel listener at
/// `socket_path`. A connection/write failure here is the "channel failure
/// semantics" case ADR-0011 calls out explicitly: it must surface visibly
/// (surfaced here as [`GatedError::CiResultDeliveryFailed`]) rather than
/// being swallowed -- exactly like the forward relay already surfaces an
/// undelivered push notification in the hook's exit code.
pub async fn send_ci_result(socket_path: &Path, message: &CiResultMessage) -> Result<()> {
    let json = message.to_json()?;
    deliver(socket_path, json.as_bytes())
        .await
        .map_err(|source| GatedError::CiResultDeliveryFailed {
            socket_path: socket_path.to_path_buf(),
            source: Box::new(source),
        })
}

/// The actual connect-and-send, isolated from the error-wrapping above so
/// `?` can be used freely across both fallible steps (connect, write) inside
/// it.
async fn deliver(socket_path: &Path, payload: &[u8]) -> Result<()> {
    let mut stream = UnixStream::connect(socket_path).await?;
    stream.write_all(payload).await?;
    stream.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;
    use warden_core::CiWatchOutcome;

    fn sample_message() -> CiResultMessage {
        CiResultMessage {
            run_id: "run-1".to_string(),
            pr_number: Some(3),
            outcome: CiWatchOutcome::checks_passed(),
        }
    }

    #[tokio::test]
    async fn send_ci_result_delivers_the_exact_message_a_listener_can_parse_back() {
        let dir = TempDir::new().unwrap();
        let socket_path = dir.path().join("warden.ci.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let message = sample_message();

        let receiver = async {
            let (mut stream, _addr) = listener.accept().await.unwrap();
            let mut buffer = String::new();
            stream.read_to_string(&mut buffer).await.unwrap();
            warden_core::parse_ci_result_message(&buffer).unwrap()
        };

        let (send_result, received) =
            tokio::join!(send_ci_result(&socket_path, &message), receiver);
        send_result.unwrap();
        assert_eq!(received, message);
    }

    #[tokio::test]
    async fn send_ci_result_surfaces_a_typed_error_when_nothing_is_listening() {
        let dir = TempDir::new().unwrap();
        let socket_path = dir.path().join("no-listener.sock");

        let result = send_ci_result(&socket_path, &sample_message()).await;

        assert!(matches!(
            result,
            Err(GatedError::CiResultDeliveryFailed { .. })
        ));
    }
}
