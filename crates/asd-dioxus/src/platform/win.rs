//! Windows transport to the local daemon. Selected by [`super`]; see there for
//! the shared surface.

use crate::conn::{BoxRead, BoxWrite};

/// Connect to the local daemon's named pipe and split it for the framed codec.
pub(crate) async fn connect_local() -> anyhow::Result<(BoxRead, BoxWrite)> {
    use tokio::net::windows::named_pipe::ClientOptions;

    let socket = asd_proto::paths::socket_path();
    let name = socket
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("pipe path is not valid UTF-8"))?;
    let stream = ClientOptions::new()
        .open(name)
        .map_err(|e| anyhow::anyhow!("connect {name}: {e}"))?;
    let (r, w) = tokio::io::split(stream);
    Ok((Box::new(r), Box::new(w)))
}
