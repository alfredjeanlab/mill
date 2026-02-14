use std::io;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Maximum frame size: 16 MiB.
const MAX_FRAME_SIZE: u32 = 16 * 1024 * 1024;

/// Write a length-prefixed JSON frame: `[u32 big-endian length][JSON payload]`.
pub async fn write_frame<W: AsyncWrite + Unpin, T: Serialize>(
    w: &mut W,
    val: &T,
) -> io::Result<()> {
    let payload =
        serde_json::to_vec(val).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let len = payload.len() as u32;
    if len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame too large: {len} bytes (max {MAX_FRAME_SIZE})"),
        ));
    }
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&payload).await?;
    w.flush().await?;
    Ok(())
}

/// Read a length-prefixed JSON frame. Returns `Ok(None)` on clean EOF.
pub async fn read_frame<R: AsyncRead + Unpin, T: DeserializeOwned>(
    r: &mut R,
) -> io::Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame too large: {len} bytes (max {MAX_FRAME_SIZE})"),
        ));
    }
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).await?;
    let val =
        serde_json::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some(val))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mill_config::AllocId;
    use tokio::io::duplex;

    use crate::rpc::{NodeCommand, NodeReport};

    #[tokio::test]
    async fn round_trip_node_command() {
        let (mut client, mut server) = duplex(4096);
        let cmd = NodeCommand::Stop { alloc_id: AllocId("test-1".into()) };
        write_frame(&mut client, &cmd).await.unwrap();
        let back: Option<NodeCommand> = read_frame(&mut server).await.unwrap();
        assert!(back.is_some());
        let back = back.unwrap();
        assert!(matches!(back, NodeCommand::Stop { alloc_id } if alloc_id.0 == "test-1"));
    }

    #[tokio::test]
    async fn round_trip_node_report() {
        let (mut client, mut server) = duplex(4096);
        let report = NodeReport::Stopped { alloc_id: AllocId("a".into()), exit_code: 42 };
        write_frame(&mut client, &report).await.unwrap();
        let back: Option<NodeReport> = read_frame(&mut server).await.unwrap();
        assert!(back.is_some());
        let back = back.unwrap();
        assert!(matches!(back, NodeReport::Stopped { exit_code, .. } if exit_code == 42));
    }

    #[tokio::test]
    async fn oversize_frame_rejected_on_read() {
        let (mut client, mut server) = duplex(64);
        // Write a length header claiming a huge payload.
        let fake_len: u32 = MAX_FRAME_SIZE + 1;
        client.write_all(&fake_len.to_be_bytes()).await.unwrap();
        let result: io::Result<Option<NodeCommand>> = read_frame(&mut server).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn eof_returns_none() {
        let (client, mut server) = duplex(64);
        drop(client); // close the write half
        let result: io::Result<Option<NodeCommand>> = read_frame(&mut server).await;
        assert!(matches!(result, Ok(None)));
    }

    #[tokio::test]
    async fn multiple_frames_round_trip() {
        let (mut client, mut server) = duplex(8192);
        let cmd1 = NodeCommand::Stop { alloc_id: AllocId("a".into()) };
        let cmd2 = NodeCommand::Pull { image: "nginx:latest".into(), auth: None };
        let cmd3 = NodeCommand::LogTail { alloc_id: AllocId("b".into()), lines: 50 };

        write_frame(&mut client, &cmd1).await.unwrap();
        write_frame(&mut client, &cmd2).await.unwrap();
        write_frame(&mut client, &cmd3).await.unwrap();

        let r1: NodeCommand = read_frame(&mut server).await.unwrap().unwrap();
        let r2: NodeCommand = read_frame(&mut server).await.unwrap().unwrap();
        let r3: NodeCommand = read_frame(&mut server).await.unwrap().unwrap();

        assert!(matches!(r1, NodeCommand::Stop { .. }));
        assert!(matches!(r2, NodeCommand::Pull { .. }));
        assert!(matches!(r3, NodeCommand::LogTail { lines: 50, .. }));
    }
}
