//! Unix transport to the local daemon. Selected by [`super`]; see there for the
//! shared surface.

use crate::conn::{BoxRead, BoxWrite};

/// Open the local daemon's Unix socket and box the halves.
pub(crate) async fn connect_local() -> anyhow::Result<(BoxRead, BoxWrite)> {
    let socket = asd_proto::paths::socket_path();
    let stream = tokio::net::UnixStream::connect(&socket)
        .await
        .map_err(|e| anyhow::anyhow!("connect {}: {e}", socket.display()))?;
    let (r, w) = tokio::io::split(stream);
    Ok((Box::new(r), Box::new(w)))
}
