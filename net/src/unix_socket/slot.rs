//! Per-slot AF_UNIX socket state, encoded as a typestate enum.
//!
//! Slot occupancy and the generation counter are owned by the registry's
//! `HandleTable`, not by the slot.

use slopos_fs::vfs::{SocketName, SocketNode};
use slopos_ostd::process::Identity;

use super::handle::SocketHandle;
use super::pair::{PairHandle, PairSide};

/// Maximum path length; matches POSIX `sockaddr_un::sun_path`.
pub(super) const UNIX_PATH_MAX: usize = 108;

pub(super) const MAX_BACKLOG: usize = 32;

pub(super) enum SlotState {
    /// `unix_create()` succeeded. Also the neutral placeholder while a state
    /// transition moves data out of the previous variant.
    Created,
    /// `unix_bind()` succeeded, making `name`.
    Bound {
        path: slopos_ostd::KVec<u8>,
        name: SocketName,
    },
    /// `unix_listen()` succeeded. `KVecDeque` keeps `accept()`'s pop-front O(1).
    Listening {
        path: slopos_ostd::KVec<u8>,
        name: SocketName,
        backlog: slopos_ostd::KVecDeque<SocketHandle>,
        /// Who listened, which every connection's client is told.
        owner: Identity,
    },
    /// `unix_connect()` (caller side) or accept (peer side) succeeded.
    Connected {
        pair: PairHandle,
        side: PairSide,
        peer: SocketHandle,
        peer_closed: bool,
        /// Kept here rather than read from the peer's slot, which its close
        /// frees.
        peer_identity: Identity,
    },
}

pub(super) struct UnixSlot {
    pub(super) state: SlotState,
    /// Socket-level option; persists across state transitions.
    pub(super) nonblocking: bool,
    /// A `bind()` is making this socket's node, so nothing else may change
    /// its state meanwhile.
    pub(super) binding: bool,
}

impl UnixSlot {
    pub(super) fn created() -> Self {
        Self {
            state: SlotState::Created,
            nonblocking: false,
            binding: false,
        }
    }

    /// The node a connect to this socket must name, while it listens.
    pub(super) fn listening_at(&self, node: &SocketNode) -> bool {
        matches!(&self.state, SlotState::Listening { name, .. } if name.node().is(node))
    }
}
