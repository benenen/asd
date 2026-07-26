//! Windows transport to the local daemon. Selected by [`super`]; see there for
//! the shared surface.

use crate::conn::{BoxRead, BoxWrite};

/// No local connection yet: the GUI reaches sessions through SSH remotes here.
///
/// This predates the Windows daemon and is now merely unimplemented rather than
/// impossible — the daemon does listen on `\\.\pipe\asd-<user>`, and a
/// `ClientOptions::open` here would be the whole change. Wiring it up is a
/// behaviour change, not a move, so it is left for its own commit; note that
/// the transport must stay a plain named-pipe client, since this crate must
/// never gain portable-pty or any process management.
pub(crate) async fn connect_local() -> anyhow::Result<(BoxRead, BoxWrite)> {
    anyhow::bail!("no local daemon connection on this platform yet — connect an SSH remote instead")
}
