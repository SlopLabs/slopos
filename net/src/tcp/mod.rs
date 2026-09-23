//! TCP — RFC 793 + RFC 7413. Protocol logic only; packet I/O belongs to the
//! caller.

pub mod actions;
pub mod buffer;
pub mod challenge_ack;
pub mod checksum;
pub mod chunk;
pub use crate::clock;
pub mod cong;
pub mod header;
pub mod isn;
pub mod listener;
pub mod pcb;
pub mod reasm;
pub mod retx;
pub mod rtt;
pub mod segment;
pub mod seq;
pub mod siphash;
pub mod table;
pub mod tuple;

pub use actions::{Actions, MAX_SEGMENTS, MAX_TIMER_OPS, SocketNotify, TimerOp};
pub use tuple::{TcpError, TcpTuple};

use buffer::SegmentSource;
pub use buffer::{
    DELAYED_ACK_MS, DELAYED_ACK_SEGMENTS, TcpBufferPair, TcpRecvState, TcpSendState, ZcSource,
};
pub use checksum::{tcp_checksum, verify_checksum};
pub use chunk::Spares;
pub use header::{
    DEFAULT_MSS, DEFAULT_WINDOW_SIZE, ParsedTcpOptions, TCP_FLAG_ACK, TCP_FLAG_FIN, TCP_FLAG_PSH,
    TCP_FLAG_RST, TCP_FLAG_SYN, TCP_FLAG_URG, TCP_HEADER_LEN, TCP_HEADER_MAX_LEN, TCP_OPT_END,
    TCP_OPT_MSS, TCP_OPT_MSS_LEN, TCP_OPT_NOP, TCP_OPT_WINDOW_SCALE, TCP_OPT_WINDOW_SCALE_LEN,
    TcpHeader, build_header, our_window_scale, parse_header, parse_tcp_options, write_header,
    write_mss_option, write_window_scale_option,
};
pub use pcb::data::{ClosePhase, DataState};
pub use pcb::{ObservedSocketState, PcbState, TcpState};
pub use pcb::{Pcb, SocketId};
pub use reasm::Assembler;
pub use segment::{SegmentBuilder, TcpOutSegment, write_tcp_segment};
pub use seq::{SeqDelta, SeqNum, seq_ge, seq_gt, seq_le, seq_lt};
pub use table::ConnId;

use self::cong::CongestionControl;
use crate::timer::{NET_TIMER_WHEEL, TimerKind, TimerToken};
use crate::types::{Ipv4Addr, Port, SockAddr};

use slopos_ostd::klog_debug;
use slopos_ostd::mm::uframe::KeepaliveFrames;
use slopos_ostd::{KBox, KVec, ZcNotifToken};

/// RFC 6298 recommends 1 s.
pub const INITIAL_RTO_MS: u32 = 1000;

pub const MAX_RTO_MS: u32 = 60_000;

/// 2 × MSL, MSL = 30 s.
pub const TIME_WAIT_MS: u64 = 60_000;

pub const MAX_RETRANSMITS: u8 = 8;
/// Zero-window probes a connection no socket owns may send before it is
/// reset: Linux's `tcp_orphan_retries` default.
pub const MAX_ORPHAN_PROBES: u8 = 8;

/// SYN retransmissions on an active open before the attempt is abandoned.
pub const ACTIVE_SYN_RETRIES_MAX: u8 = 5;

/// 60 s, matching Linux's `tcp_fin_timeout` default.
pub const FIN_WAIT2_TIMEOUT_MS: u64 = 60_000;

/// Keepalive idle period before the first probe (RFC 1122 default 2 h).
const TCP_KEEPALIVE_IDLE_MS: u64 = 7_200 * 1_000;
const TCP_KEEPALIVE_INTERVAL_MS: u64 = 75 * 1_000;
const TCP_KEEPALIVE_PROBES_MAX: u8 = 9;

pub(crate) use isn::generate_isn;

/// Process an incoming TCP segment into the `Actions` the caller drains.
pub fn input(
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    hdr: &TcpHeader,
    options: &[u8],
    payload: &[u8],
    now_ms: u64,
) -> Actions {
    // A reply to an unspecified source is routed by the default route, so it
    // leaves over the NIC addressed to 0.0.0.0. Drop rather than answer: no
    // legitimate segment carries one, and the RST is what reaches the wire.
    if Ipv4Addr(src_ip).is_unspecified() {
        klog_debug!("tcp: segment from an unspecified source, dropping");
        return Actions::new();
    }

    let incoming_tuple = TcpTuple {
        local_ip: dst_ip,
        local_port: hdr.dst_port,
        remote_ip: src_ip,
        remote_port: hdr.src_port,
    };

    let Some(id) = table::find(&incoming_tuple) else {
        return if hdr.is_rst() {
            Actions::new()
        } else {
            input_no_match_rst(hdr, dst_ip, src_ip)
        };
    };

    if id.is_listener() {
        let (mut actions, parent) =
            input_process_listener(id, &incoming_tuple, hdr, options, now_ms);
        // Installing the child runs outside the listener's per-slot lock so it
        // can take the matching shard's write lock.
        if actions.accepted.is_some()
            && let Some(child_id) = install_accepted_child(&incoming_tuple, &actions, hdr, parent)
        {
            // The child's own SynRecv handler makes the Data transition, so
            // buffers and NEW_ESTABLISHED come from the normal segment path.
            let mut child_actions =
                input_process_established(child_id, &incoming_tuple, hdr, options, payload, now_ms);
            child_actions.merge_segments_from(&mut actions);
            return child_actions;
        }
        if actions.accepted.is_some() {
            // The table had no room, but the peer believes the handshake
            // completed.
            actions.accepted = None;
            actions.notify = SocketNotify::empty();
            actions.push_segment(SegmentBuilder::rst_for(hdr, dst_ip, src_ip));
        }
        actions.conn_id = Some(id);
        actions
    } else {
        input_process_established(id, &incoming_tuple, hdr, options, payload, now_ms)
    }
}

#[derive(Clone, Copy)]
struct Parent {
    socket: Option<pcb::SocketId>,
    rcvbuf: u32,
    sndbuf: u32,
}

/// Run the listener state machine on `id` under its per-slot lock.
#[inline(never)]
fn input_process_listener(
    id: ConnId,
    incoming: &TcpTuple,
    hdr: &TcpHeader,
    options: &[u8],
    now_ms: u64,
) -> (Actions, Parent) {
    let orphan = Parent {
        socket: None,
        rcvbuf: 0,
        sndbuf: 0,
    };
    table::with_pcb_mut(id, |pcb| {
        let mut actions = pcb.on_segment(None, incoming, hdr, options, &[], now_ms);
        actions.conn_id = Some(id);
        let parent = Parent {
            socket: pcb.socket_id,
            rcvbuf: pcb.rcvbuf,
            sndbuf: pcb.sndbuf,
        };
        (actions, parent)
    })
    .unwrap_or((Actions::new(), orphan))
}

/// Build a single-RST `Actions` for the no-matching-connection path. Separate
/// so the 400 B `Actions` return slot stays out of `tcp::input`'s frame.
#[inline(never)]
fn input_no_match_rst(hdr: &TcpHeader, dst_ip: [u8; 4], src_ip: [u8; 4]) -> Actions {
    let mut actions = Actions::new();
    actions.push_segment(SegmentBuilder::rst_for(hdr, dst_ip, src_ip));
    actions
}

/// Install a child PCB accepted by the listener phase. `SynRecvState` is
/// ~80 B; isolating this path keeps `tcp::input`'s frame small.
#[inline(never)]
fn install_accepted_child(
    incoming_tuple: &TcpTuple,
    actions: &Actions,
    hdr: &TcpHeader,
    parent: Parent,
) -> Option<ConnId> {
    let accepted = match &actions.accepted {
        Some(a) => a,
        None => return None,
    };
    let child_iss = SeqNum::new(accepted.iss);
    let child_irs = SeqNum::new(accepted.irs);
    let mut child_state = pcb::SynRecvState::new(child_iss, child_irs);
    child_state.peer_mss = accepted.peer_mss;
    child_state.sack_permitted = accepted.sack_permitted;
    child_state.snd_wnd = hdr.window_size as u32;
    if let Some(shift) = accepted.peer_wscale {
        child_state.wscale_enabled = true;
        child_state.snd_wscale = shift;
        child_state.our_wscale = our_window_scale();
    }
    if let Some(tsval) = accepted.peer_tsval {
        child_state.ts_enabled = true;
        child_state.peer_tsval = tsval;
    }

    table::install_established(*incoming_tuple, PcbState::SynRecv(child_state), |child| {
        child.socket_id = parent.socket;
        child.rcvbuf = parent.rcvbuf;
        child.sndbuf = parent.sndbuf;
    })
    .ok()
}

/// A SYN-ACK retransmit timer fired for `key`.
///
/// The key belongs to a SYN-queue entry, not to a `ConnId`, so the owning
/// listener is found by scanning the listener slots.
pub fn on_syn_ack_retransmit(key: u32) -> Option<TcpOutSegment> {
    for slot in 0..table::MAX_LISTENERS {
        let found = table::with_listener_slot_mut(slot, |pcb| {
            let PcbState::Listen(listen) = &mut pcb.state else {
                return None;
            };
            if !listen.syn_queue().has_key(key) {
                return None;
            }
            listen.syn_queue_mut().on_retransmit(key)
        });
        if let Some(Some(seg)) = found {
            return Some(seg);
        }
    }
    None
}

/// Release every established child a closing listener still owns.
///
/// A child holds a shard slot nothing else reclaims once the listener is gone;
/// the returned tuples let the caller reset the peers it was still speaking to.
pub fn release_children_of(socket_id: pcb::SocketId) -> KVec<(TcpTuple, u32)> {
    let mut ids = [None; table::TOTAL_PCB_SLOTS];
    let count = table::snapshot_shard_conn_ids(&mut ids);
    let mut orphans = KVec::new();
    for id in ids.iter().take(count).flatten() {
        let owned = table::with_pcb(*id, |pcb| {
            (pcb.socket_id == Some(socket_id)).then(|| (pcb.tuple, pcb.state.snd_nxt_raw()))
        })
        .flatten();
        if let Some(entry) = owned {
            if orphans.push(entry).is_err() {
                break;
            }
            table::release(*id);
        }
    }
    orphans
}

/// Give `id` the send/receive rings its next transition assumes.
///
/// The pair is allocated outside the PCB lock: a slab refill under the
/// `TCP_PCB_SLOTS` cli-spinlock — a lock a remote peer drives — deadlocks.
/// A PCB never re-enters `SynRecv`, so a stale peek can only waste an
/// allocation.
fn ensure_connection_buffer(id: ConnId) -> Result<(), TcpError> {
    let wanted = table::with_pcb_and_bufs(id, |pcb, buf| {
        (buf.is_none() && matches!(pcb.state, PcbState::SynRecv(_) | PcbState::SynSent(_)))
            .then_some((pcb.rcvbuf, pcb.sndbuf))
    });
    let Some(Some((rcvbuf, sndbuf))) = wanted else {
        return Ok(());
    };
    let pair = TcpBufferPair::boxed(buffer_size(rcvbuf), buffer_size(sndbuf))?;
    table::with_pcb_and_bufs(id, |_, slot| {
        if slot.is_none() {
            *slot = Some(pair);
        }
    });
    Ok(())
}

/// A `TimeWait` connection keeps its rings only while its socket still has
/// bytes to read: the peer's FIN can arrive before the reader catches up.
fn release_time_wait_bufs(pcb: &Pcb, slot: &mut Option<KBox<TcpBufferPair>>) {
    if matches!(pcb.state, PcbState::TimeWait(_))
        && slot
            .as_ref()
            .is_some_and(|b| pcb.socket_id.is_none() || b.recv.available() == 0)
    {
        *slot = None;
    }
}

fn buffer_size(requested: u32) -> usize {
    if requested == 0 {
        chunk::buffer_max()
    } else {
        (requested as usize).min(chunk::buffer_max())
    }
}

/// Abandon a connection that reached `Data` with no rings to serve it: the
/// peer is reset rather than left holding a handshake this side cannot honour.
#[inline(never)]
fn reset_for_no_buffer(actions: &mut Actions, tuple: &TcpTuple, hdr: &TcpHeader) {
    klog_debug!("tcp: no memory for a new connection's buffers; resetting peer");
    for seg in actions.segments.iter_mut() {
        *seg = None;
    }
    actions.segments_len = 0;
    actions.notify = SocketNotify::empty();
    actions.accepted = None;
    actions.push_segment(SegmentBuilder::rst_for(
        hdr,
        tuple.local_ip,
        tuple.remote_ip,
    ));
    actions.release = true;
}

/// Process a segment for an established/transient connection. The per-slot
/// lock is dropped before `table::release`, which re-acquires it.
/// `#[inline(never)]` keeps the ~400 B `Actions` return value out of
/// `tcp::input`'s frame.
#[inline(never)]
fn input_process_established(
    id: ConnId,
    incoming: &TcpTuple,
    hdr: &TcpHeader,
    options: &[u8],
    payload: &[u8],
    now_ms: u64,
) -> Actions {
    let _ = ensure_connection_buffer(id);
    let held = table::with_pcb_and_bufs(id, |pcb, bufs| {
        let ring = &bufs.as_ref()?.recv.buf;
        let in_order = matches!(&pcb.state, PcbState::Data(d) if d.rcv_nxt.raw() == hdr.seq_num);
        Some(if in_order {
            ring.stream_chunks()
        } else {
            ring.chunks_held()
        })
    })
    .flatten()
    .unwrap_or(0);
    let mut spares = Spares::for_bytes(payload.len(), held);

    let actions = table::with_pcb_and_bufs(id, |pcb, buffer_slot| {
        if let Some(bufs) = buffer_slot.as_mut() {
            core::mem::swap(&mut bufs.spares, &mut spares);
        }
        let mut actions = pcb.on_segment(
            buffer_slot.as_deref_mut(),
            incoming,
            hdr,
            options,
            payload,
            now_ms,
        );
        if let Some(bufs) = buffer_slot.as_mut() {
            core::mem::swap(&mut bufs.spares, &mut spares);
            #[cfg(debug_assertions)]
            if let PcbState::Data(d) = &pcb.state {
                d.debug_assert_sendmap(&bufs.send.sendmap);
            }
        }
        actions.conn_id = Some(id);

        // State handlers emit `key: 0` as a sentinel; the real ConnId is
        // substituted here.
        for i in 0..actions.timer_ops_len as usize {
            if let Some(ref op) = actions.timer_ops[i] {
                match *op {
                    TimerOp::Schedule {
                        kind,
                        key: _,
                        delay_ms,
                    } => {
                        let token = NET_TIMER_WHEEL.schedule(delay_ms, kind, id.raw());
                        match kind {
                            TimerKind::TcpRetransmit => {
                                set_retransmit_token(pcb, Some(token));
                            }
                            TimerKind::TcpTimeWait => {
                                if let PcbState::TimeWait(tw) = &mut pcb.state {
                                    tw.expire_token = Some(token);
                                }
                            }
                            TimerKind::TcpKeepalive => {
                                if let PcbState::Data(d) = &mut pcb.state {
                                    d.keepalive_token = Some(token);
                                }
                            }
                            TimerKind::TcpFinWait2 => {
                                if let PcbState::Data(d) = &mut pcb.state {
                                    d.fin_wait2_token = Some(token);
                                }
                            }
                            _ => {}
                        }
                    }
                    TimerOp::Cancel { token } => {
                        NET_TIMER_WHEEL.cancel(token);
                    }
                }
            }
        }

        if actions.notify.contains(SocketNotify::NEW_ESTABLISHED) && buffer_slot.is_none() {
            reset_for_no_buffer(&mut actions, &pcb.tuple, hdr);
        }

        release_time_wait_bufs(pcb, buffer_slot);

        if !actions.release
            && actions.notify.intersects(
                SocketNotify::RECV_WAKE | SocketNotify::SEND_WAKE | SocketNotify::PEER_HEARD,
            )
        {
            if let PcbState::Data(d) = &mut pcb.state {
                if let Some((old_token, delay)) = d.reset_keepalive_on_activity() {
                    NET_TIMER_WHEEL.cancel(old_token);
                    let token = NET_TIMER_WHEEL.schedule(delay, TimerKind::TcpKeepalive, id.raw());
                    d.keepalive_token = Some(token);
                }
            }
        }

        actions
    });

    // The socket layer takes the socket table before calling down into a PCB,
    // so reading the keepalive option under the PCB lock inverts that order.
    if actions
        .as_ref()
        .is_some_and(|a| a.notify.contains(SocketNotify::NEW_ESTABLISHED))
    {
        let socket_id = table::with_pcb(id, |pcb| pcb.socket_id).flatten();
        let keepalive_enabled = socket_id
            .map(|sid| crate::socket::socket_keepalive_enabled_by_index(sid.0 as usize))
            .unwrap_or(false);
        table::with_pcb_mut(id, |pcb| {
            if let PcbState::Data(d) = &mut pcb.state {
                if let Some(delay) = d.schedule_initial_keepalive(keepalive_enabled) {
                    let token = NET_TIMER_WHEEL.schedule(delay, TimerKind::TcpKeepalive, id.raw());
                    d.keepalive_token = Some(token);
                }
            }
        });
    }

    let actions = actions.unwrap_or_else(|| {
        let mut a = Actions::new();
        a.conn_id = Some(id);
        a
    });

    if actions.release {
        table::release(id);
    }

    actions
}

fn set_retransmit_token(pcb: &mut Pcb, token: Option<TimerToken>) {
    match &mut pcb.state {
        PcbState::SynSent(s) => s.retransmit_token = token,
        PcbState::SynRecv(s) => s.retransmit_token = token,
        PcbState::Data(d) => d.retransmit_token = token,
        _ => {}
    }
}

/// Open an active connection (client: SYN → SYN_SENT).
pub fn connect(
    local_ip: [u8; 4],
    remote_ip: [u8; 4],
    remote_port: u16,
) -> Result<(ConnId, TcpOutSegment), TcpError> {
    let local_port = table::alloc_ephemeral_port().ok_or(TcpError::AddrInUse)?;
    let tuple = TcpTuple {
        local_ip,
        local_port,
        remote_ip,
        remote_port,
    };
    let iss = generate_isn(&tuple);

    let wscale = our_window_scale();
    let mut syn_sent = pcb::SynSentState::new(SeqNum::new(iss));
    syn_sent.our_wscale = wscale;

    let id = table::install_established(tuple, PcbState::SynSent(syn_sent), |_| {})?;

    klog_debug!(
        "tcp: CONNECT {}:{} -> {}:{} ISS={} id={}",
        local_ip[0],
        local_ip[1],
        local_port,
        remote_port,
        iss,
        id
    );

    let seg =
        SegmentBuilder::active_syn(tuple, iss, wscale).with_timestamp(clock::now_ms() as u32, 0);
    Ok((id, seg))
}

/// Call after the SYN is on the wire; idempotent, a second call retires the first timer.
pub fn arm_syn_retransmit(id: ConnId) {
    let stale = table::with_pcb_mut(id, |pcb| {
        let PcbState::SynSent(s) = &mut pcb.state else {
            return None;
        };
        let stale = s.retransmit_token.take();
        let token = NET_TIMER_WHEEL.schedule(s.rto_ms as u64, TimerKind::TcpRetransmit, id.raw());
        s.retransmit_token = Some(token);
        stale
    });
    if let Some(Some(token)) = stale {
        NET_TIMER_WHEEL.cancel(token);
    }
}

/// Open a passive connection (server: → LISTEN).
pub fn listen(local_ip: [u8; 4], local_port: u16) -> Result<ConnId, TcpError> {
    if table::port_in_use(local_ip, local_port) {
        return Err(TcpError::AddrInUse);
    }

    let tuple = TcpTuple {
        local_ip,
        local_port,
        remote_ip: [0; 4],
        remote_port: 0,
    };
    // Built before the listener slot lock is taken: growing it inside `on_syn`
    // would run the allocator beneath a cli-spinlock a remote peer drives.
    let local = SockAddr::new(Ipv4Addr(local_ip), Port(local_port));
    let syn_queue = listener::SynQueue::with_capacity(local)?;
    let id = table::install_listener(
        tuple,
        PcbState::Listen(pcb::ListenState::with_syn_queue(syn_queue)),
        |_| {},
    )?;

    klog_debug!("tcp: LISTEN on port {} id={}", local_port, id);
    Ok(id)
}

/// Close a connection (initiate graceful teardown), returning the FIN to send.
pub fn close(id: ConnId) -> Result<Option<TcpOutSegment>, TcpError> {
    if id.is_listener() {
        let name = table::with_pcb(id, |pcb| pcb.state.name()).ok_or(TcpError::NotFound)?;
        table::release(id);
        klog_debug!("tcp: CLOSE id={} from {} — released", id, name);
        return Ok(None);
    }

    enum Outcome {
        Release(&'static str),
        Segment(TcpOutSegment),
        NoOp,
    }

    ensure_connection_buffer(id)?;

    let result = table::with_pcb_and_bufs(id, |pcb, buffer_slot| -> Result<Outcome, TcpError> {
        if matches!(pcb.state, PcbState::Listen(_) | PcbState::SynSent(_)) {
            return Ok(Outcome::Release(pcb.state.name()));
        }
        if let PcbState::TimeWait(tw) = &pcb.state {
            if clock::now_ms().saturating_sub(tw.entry_ms) >= TIME_WAIT_MS {
                return Ok(Outcome::Release("TIME_WAIT"));
            }
            release_time_wait_bufs(pcb, buffer_slot);
            klog_debug!("tcp: CLOSE id={} TIME_WAIT — no-op", id);
            return Ok(Outcome::NoOp);
        }
        // SynRecv → Data(FinWait1) needs the rings the transition assumes.
        if matches!(pcb.state, PcbState::SynRecv(_)) && buffer_slot.is_none() {
            return Err(TcpError::OutOfMemory);
        }
        match &mut pcb.state {
            PcbState::SynRecv(_) => fin_from_syn_recv(pcb, id, "CLOSE")
                .map(|s| s.map_or(Outcome::NoOp, Outcome::Segment)),
            PcbState::Data(d) => {
                let tuple = pcb.tuple;
                let fin = fin_or_queue(d, buffer_slot.as_deref(), tuple, id);
                pcb.assert_invariants();
                Ok(fin.map_or(Outcome::NoOp, Outcome::Segment))
            }
            _ => Err(TcpError::InvalidState),
        }
    });

    match result {
        None => Err(TcpError::NotFound),
        Some(Err(e)) => Err(e),
        Some(Ok(Outcome::Release(name))) => {
            table::release(id);
            klog_debug!("tcp: CLOSE id={} from {} — released", id, name);
            Ok(None)
        }
        Some(Ok(Outcome::NoOp)) => Ok(None),
        Some(Ok(Outcome::Segment(s))) => Ok(Some(s)),
    }
}

/// SYN_RECEIVED → FIN_WAIT_1 on a close or a write shutdown. Out of line so
/// the `DataState` initialiser's frame stays out of its callers', which the
/// stack gate would refuse.
#[inline(never)]
fn fin_from_syn_recv(
    pcb: &mut pcb::Pcb,
    id: ConnId,
    what: &str,
) -> Result<Option<TcpOutSegment>, TcpError> {
    let s = match &pcb.state {
        PcbState::SynRecv(s) => s,
        _ => unreachable!("fin_from_syn_recv called on non-SynRecv pcb"),
    };
    let tuple = pcb.tuple;
    let seq = s.snd_nxt.raw();
    let ack = s.rcv_nxt.raw();
    let now_ms = clock::now_ms();
    let ts_enabled = s.ts_enabled;
    let handshake_timer = s.retransmit_token;
    let mut ds = slopos_ostd::KBox::try_init(DataState::init_from_syn_recv(s))?;
    ds.close_phase = ClosePhase::FinWait1;
    ds.snd_nxt = ds.snd_nxt.wrapping_add(1);
    ds.retransmit_token = Some(NET_TIMER_WHEEL.schedule(
        (ds.rtt.rto_ms() as u64).max(1),
        TimerKind::TcpRetransmit,
        id.raw(),
    ));
    let ts = if ts_enabled {
        Some((now_ms as u32, ds.ts_recent))
    } else {
        None
    };
    let window = ds.advertised();
    pcb.state = PcbState::Data(ds);
    if let Some(token) = handshake_timer {
        NET_TIMER_WHEEL.cancel(token);
    }
    pcb.assert_invariants();
    let mut seg = SegmentBuilder::fin_ack(tuple, seq, ack, window);
    seg.timestamp = ts;
    klog_debug!("tcp: {} id={} SYN_RECV -> FIN_WAIT_1", what, id);
    Ok(Some(seg))
}

/// Abort a connection (send RST, release immediately).
pub fn abort(id: ConnId) -> Result<Option<TcpOutSegment>, TcpError> {
    if id.is_listener() {
        let name = table::with_pcb(id, |pcb| pcb.state.name()).ok_or(TcpError::NotFound)?;
        klog_debug!("tcp: ABORT id={} from {}", id, name);
        table::release(id);
        return Ok(None);
    }

    let seg = table::with_pcb(id, |pcb| {
        klog_debug!("tcp: ABORT id={} from {}", id, pcb.state.name());
        match &pcb.state {
            PcbState::Listen(_) => None,
            PcbState::SynSent(s) => Some(SegmentBuilder::bare_rst(pcb.tuple, s.snd_nxt.raw())),
            PcbState::SynRecv(s) => Some(SegmentBuilder::bare_rst(pcb.tuple, s.snd_nxt.raw())),
            PcbState::Data(d) => Some(SegmentBuilder::bare_rst(pcb.tuple, d.snd_nxt.raw())),
            PcbState::TimeWait(tw) => {
                Some(SegmentBuilder::bare_rst(pcb.tuple, tw.last_snd_nxt.raw()))
            }
        }
    })
    .ok_or(TcpError::NotFound)?;

    table::release(id);
    Ok(seg)
}

/// Shutdown the write half of a connection (send FIN without releasing).
pub fn shutdown_write(id: ConnId) -> Result<Option<TcpOutSegment>, TcpError> {
    if id.is_listener() {
        return Err(TcpError::InvalidState);
    }

    ensure_connection_buffer(id)?;

    let result = table::with_pcb_and_bufs(
        id,
        |pcb, buffer_slot| -> Result<Option<TcpOutSegment>, TcpError> {
            if matches!(pcb.state, PcbState::SynRecv(_)) && buffer_slot.is_none() {
                return Err(TcpError::OutOfMemory);
            }
            match &mut pcb.state {
                PcbState::Data(d) => {
                    let tuple = pcb.tuple;
                    let fin = fin_or_queue(d, buffer_slot.as_deref(), tuple, id);
                    pcb.assert_invariants();
                    Ok(fin)
                }
                PcbState::SynRecv(_) => fin_from_syn_recv(pcb, id, "SHUTDOWN_WR"),
                _ => Err(TcpError::InvalidState),
            }
        },
    );

    match result {
        None => Err(TcpError::NotFound),
        Some(r) => r,
    }
}

/// Discard all data in the receive buffer (for SHUT_RD).
pub fn recv_discard(id: ConnId) {
    if id.is_listener() {
        return;
    }
    let cleared = table::with_pcb_and_bufs(id, |_pcb, buf| {
        if let Some(b) = buf.as_mut() {
            b.recv.clear();
            b.ooo.clear();
            true
        } else {
            false
        }
    })
    .unwrap_or(false);
    if cleared {
        klog_debug!("tcp: RECV_DISCARD id={} — recv buffer cleared", id);
    }
}

fn send_fin(d: &mut DataState, tuple: TcpTuple, id: ConnId) -> Option<TcpOutSegment> {
    let next = match d.close_phase {
        ClosePhase::Established => ClosePhase::FinWait1,
        ClosePhase::CloseWait => ClosePhase::LastAck,
        _ => return None,
    };
    let seq = d.snd_nxt.raw();
    d.snd_nxt = d.snd_nxt.wrapping_add(1);
    d.close_phase = next;
    d.fin_queued = false;
    cancel_keepalive(d);
    if d.retransmit_token.is_none() {
        d.retransmit_token = Some(NET_TIMER_WHEEL.schedule(
            (d.rtt.rto_ms() as u64).max(1),
            TimerKind::TcpRetransmit,
            id.raw(),
        ));
    }
    let mut seg = SegmentBuilder::fin_ack(tuple, seq, d.rcv_nxt.raw(), d.advertised());
    d.stamp(&mut seg, clock::now_ms());
    klog_debug!("tcp: id={} -> {:?}, FIN seq={}", id, next, seq);
    Some(seg)
}

/// Our FIN now, or once the send buffer drains: it takes the sequence number
/// after the last byte, so it cannot overtake bytes still waiting to go.
fn fin_or_queue(
    d: &mut DataState,
    bufs: Option<&TcpBufferPair>,
    tuple: TcpTuple,
    id: ConnId,
) -> Option<TcpOutSegment> {
    if bufs.is_some_and(|b| b.send.unsent_len() > 0) {
        d.fin_queued = matches!(
            d.close_phase,
            ClosePhase::Established | ClosePhase::CloseWait
        );
        return None;
    }
    send_fin(d, tuple, id)
}

fn cancel_keepalive(d: &mut DataState) {
    if let Some(token) = d.keepalive_token.take() {
        NET_TIMER_WHEEL.cancel(token);
    }
}

pub fn get_state(id: ConnId) -> Option<TcpState> {
    table::with_pcb(id, |pcb| pcb.state.tcp_state())
}

pub fn active_count() -> usize {
    table::active_count()
}

pub fn find(tuple: &TcpTuple) -> Option<ConnId> {
    table::find(tuple)
}

pub fn set_socket_idx(id: ConnId, socket_id: Option<SocketId>) {
    table::with_pcb_mut(id, |pcb| {
        pcb.socket_id = socket_id;
    });
}

pub fn is_peer_closed(id: ConnId) -> bool {
    table::with_pcb(id, |pcb| match &pcb.state {
        PcbState::Data(d) => d.peer_closed,
        PcbState::TimeWait(_) => true,
        _ => false,
    })
    .unwrap_or(false)
}

pub fn is_reset(id: ConnId) -> bool {
    table::with_pcb(id, |pcb| match &pcb.state {
        PcbState::Data(d) => d.reset_received,
        _ => false,
    })
    .unwrap_or(false)
}

pub fn send_buffer_space(id: ConnId) -> usize {
    table::with_bufs(id, |b| b.send.writable()).unwrap_or(0)
}

pub fn recv_available(id: ConnId) -> usize {
    table::with_bufs(id, |b| b.recv.available()).unwrap_or(0)
}

pub fn has_pending_data(id: ConnId) -> bool {
    table::with_bufs(id, |b| b.send.unsent_len() > 0).unwrap_or(false)
}

pub fn has_pending_output(id: ConnId) -> bool {
    table::with_pcb_and_bufs(id, |pcb, buf| {
        let fin = matches!(&pcb.state, PcbState::Data(d) if d.fin_queued);
        fin || buf
            .as_ref()
            .is_some_and(|b| b.send.unsent_len() > 0 || b.send.sendmap.has_lost())
    })
    .unwrap_or(false)
}

pub fn with_pcb<T>(id: ConnId, f: impl FnOnce(&Pcb) -> T) -> Option<T> {
    table::with_pcb(id, f)
}

pub fn with_pcb_mut<T>(id: ConnId, f: impl FnOnce(&mut Pcb) -> T) -> Option<T> {
    table::with_pcb_mut(id, f)
}

/// Set SO_SNDBUF, capped at [`chunk::buffer_max`]; a connection whose buffers
/// do not exist yet takes it when they are made.
pub fn set_sndbuf(id: ConnId, bytes: usize) {
    let size = buffer_size(bytes.min(u32::MAX as usize) as u32);
    table::with_pcb_and_bufs(id, |pcb, buf| {
        pcb.sndbuf = size as u32;
        if let Some(b) = buf.as_mut() {
            b.send.set_capacity(size);
        }
    });
}

/// Set SO_RCVBUF, capped at [`chunk::buffer_max`]; see [`set_sndbuf`].
pub fn set_rcvbuf(id: ConnId, bytes: usize) {
    let size = buffer_size(bytes.min(u32::MAX as usize) as u32);
    table::with_pcb_and_bufs(id, |pcb, buf| {
        pcb.rcvbuf = size as u32;
        if let Some(b) = buf.as_mut() {
            b.recv.set_capacity(size);
        }
    });
}

/// Set or clear TCP_NODELAY (disables/enables Nagle algorithm).
pub fn set_nodelay(id: ConnId, nodelay: bool) {
    table::with_pcb_mut(id, |pcb| {
        if let PcbState::Data(d) = &mut pcb.state {
            d.nagle_enabled = !nodelay;
        }
    });
}

fn send_spares(id: ConnId, len: usize) -> Spares {
    let (held, writable) =
        table::with_bufs(id, |b| (b.send.chunks_held(), b.send.writable())).unwrap_or((0, 0));
    Spares::for_bytes(len.min(writable), held)
}

/// Retried once when no chunk was found, for room freed after the reservation
/// was sized; a second miss is the heap's.
fn send_reserved(
    id: ConnId,
    len: usize,
    mut enqueue: impl FnMut(&mut TcpSendState, &mut Spares) -> usize,
) -> Result<usize, TcpError> {
    if id.is_listener() {
        return Err(TcpError::InvalidState);
    }
    for _ in 0..2 {
        let mut spares = send_spares(id, len);
        let result = table::with_pcb_and_bufs(id, |pcb, buf| -> Result<Option<usize>, TcpError> {
            match &pcb.state {
                PcbState::Data(d)
                    if matches!(
                        d.close_phase,
                        ClosePhase::Established | ClosePhase::CloseWait
                    ) => {}
                _ => return Err(TcpError::InvalidState),
            }
            let Some(bufs) = buf.as_mut() else {
                return Err(TcpError::InvalidState);
            };
            let wrote = enqueue(&mut bufs.send, &mut spares);
            let found_no_chunk = wrote == 0 && len > 0 && bufs.send.writable() > 0;
            Ok((!found_no_chunk).then_some(wrote))
        });
        match result {
            None => return Err(TcpError::NotFound),
            Some(Ok(Some(wrote))) => return Ok(wrote),
            Some(Ok(None)) => {}
            Some(Err(e)) => return Err(e),
        }
    }
    Err(TcpError::OutOfMemory)
}

pub fn send(id: ConnId, data: &[u8]) -> Result<usize, TcpError> {
    send_reserved(id, data.len(), |send, spares| send.enqueue(data, spares))
}

/// Single-direct-copy [`send`]: the payload goes from the pinned user pages
/// (via `reader`) into the send ring in one volatile copy, no kernel scratch.
pub fn send_from(
    id: ConnId,
    reader: &mut slopos_ostd::mm::VmReader<'_>,
) -> Result<usize, TcpError> {
    let len = reader.remain();
    send_reserved(id, len, |send, spares| send.enqueue_from(reader, spares))
}

pub fn recv(id: ConnId, out: &mut [u8]) -> Result<usize, TcpError> {
    if id.is_listener() {
        return Err(TcpError::InvalidState);
    }
    let result = table::with_pcb_and_bufs(id, |pcb, buf| -> Result<usize, TcpError> {
        let Some(bufs) = buf.as_mut() else {
            return match pcb.state {
                PcbState::TimeWait(_) => Ok(0),
                _ => Err(TcpError::InvalidState),
            };
        };
        let read = bufs.recv.dequeue(out);
        if read == 0 && bufs.recv.available() == 0 {
            if let PcbState::Data(d) = &pcb.state {
                if d.reset_received {
                    return Err(TcpError::ConnectionReset);
                }
            }
        }
        release_time_wait_bufs(pcb, buf);
        Ok(read)
    });
    match result {
        None => Err(TcpError::NotFound),
        Some(r) => r,
    }
}

/// Single-direct-copy [`recv`]: received bytes go straight into the pinned
/// user pages (via `writer`) in one volatile copy, no kernel scratch.
pub fn recv_into(
    id: ConnId,
    writer: &mut slopos_ostd::mm::VmWriter<'_>,
) -> Result<usize, TcpError> {
    if id.is_listener() {
        return Err(TcpError::InvalidState);
    }
    let result = table::with_pcb_and_bufs(id, |pcb, buf| -> Result<usize, TcpError> {
        let Some(bufs) = buf.as_mut() else {
            return match pcb.state {
                PcbState::TimeWait(_) => Ok(0),
                _ => Err(TcpError::InvalidState),
            };
        };
        let read = bufs.recv.dequeue_into(writer);
        if read == 0 && bufs.recv.available() == 0 {
            if let PcbState::Data(d) = &pcb.state {
                if d.reset_received {
                    return Err(TcpError::ConnectionReset);
                }
            }
        }
        release_time_wait_bufs(pcb, buf);
        Ok(read)
    });
    match result {
        None => Err(TcpError::NotFound),
        Some(r) => r,
    }
}

/// The ACK announcing a window a read has reopened, once it is worth one:
/// receiver-side silly window avoidance (RFC 1122 §4.2.3.3), as Linux shapes it.
pub fn window_update(id: ConnId) -> Option<TcpOutSegment> {
    if id.is_listener() {
        return None;
    }
    table::with_pcb_and_bufs(id, |pcb, buf| {
        let bufs = buf.as_mut()?;
        let tuple = pcb.tuple;
        let PcbState::Data(d) = &mut pcb.state else {
            return None;
        };
        let old = d.rcv_wnd;
        let new = bufs.recv.window();
        let cap = bufs.recv.effective_capacity() as u32;
        let worth_it = (2 * old).max((DEFAULT_MSS as u32).min(cap / 2));
        if old > cap / 2 || new < worth_it {
            return None;
        }
        let window = d.advertise(new);
        let mut seg = SegmentBuilder::ack(tuple, d.snd_nxt.raw(), d.rcv_nxt.raw(), window);
        d.stamp(&mut seg, clock::now_ms());
        bufs.recv.ack_sent();
        Some(seg)
    })
    .flatten()
}

/// Append a TCP `MSG_ZEROCOPY` piece to the send queue: the NIC DMAs `len`
/// bytes straight from the pinned pages `keepalive` (data at the pin's
/// `base_off`), held across retransmits until they are cumulatively ACKed.
/// `token` owns the piece's notification reference; the ring posts `F_NOTIF`
/// when it reaches zero. `None` drops `keepalive` and `token` here and leaves
/// the caller the single-direct-copy leaf.
pub fn enqueue_zerocopy(
    id: ConnId,
    keepalive: KeepaliveFrames,
    base_off: usize,
    len: usize,
    token: ZcNotifToken,
) -> Option<usize> {
    if id.is_listener() {
        return None;
    }
    table::with_pcb_and_bufs(id, |pcb, buf| -> Option<usize> {
        match &pcb.state {
            PcbState::Data(d)
                if matches!(
                    d.close_phase,
                    ClosePhase::Established | ClosePhase::CloseWait
                ) => {}
            _ => return None,
        }
        let bufs = buf.as_mut()?;
        // The copy leaf is what handles SO_SNDBUF blocking correctly.
        if bufs.send.zc_free_space() < len {
            return None;
        }
        if bufs
            .send
            .enqueue_zerocopy(keepalive, base_off, len as u32, token)
        {
            Some(len)
        } else {
            None
        }
    })
    .flatten()
}

/// Resolve the source of one segment at stream offset `off` (≤ `max_len` bytes,
/// never crossing a piece boundary): copy inline bytes into `payload_buf`, or
/// carry a [`ZcSource`] the caller DMAs straight from the pinned pages. `None`
/// = nothing buffered there. `#[inline(never)]` keeps its source temporaries
/// off `poll_transmit`'s frame.
#[inline(never)]
fn resolve_segment(
    send: &TcpSendState,
    off: usize,
    max_len: usize,
    payload_buf: &mut [u8],
) -> Option<(usize, Option<ZcSource>)> {
    match send.segment_source(off, max_len) {
        SegmentSource::Empty => None,
        SegmentSource::Inline { len } => {
            let copied = send.peek_retransmit(off, &mut payload_buf[..len]);
            if copied == 0 {
                None
            } else {
                Some((copied, None))
            }
        }
        SegmentSource::Zerocopy {
            keepalive,
            byte_start,
            len,
            token,
        } => Some((
            len,
            Some(ZcSource {
                keepalive,
                byte_start,
                len,
                token,
            }),
        )),
    }
}

fn arm_retransmit(d: &mut DataState, deadline_ms: &mut u64, id: ConnId, now_ms: u64) {
    if d.retransmit_token.is_some() {
        return;
    }
    let rto_ms = (d.rtt.rto_ms() as u64).max(1);
    *deadline_ms = now_ms.saturating_add(rto_ms);
    d.retransmit_token = Some(NET_TIMER_WHEEL.schedule(rto_ms, TimerKind::TcpRetransmit, id.raw()));
}

/// Generate the next outgoing data segment for a connection.
///
/// When the bytes live in a zero-copy piece the returned [`ZcSource`] is what
/// the caller DMAs from; a `None` source means the payload was copied into
/// `payload_buf`.
pub fn poll_transmit(
    id: ConnId,
    payload_buf: &mut [u8],
    now_ms: u64,
) -> Option<(TcpOutSegment, usize, Option<ZcSource>)> {
    if id.is_listener() {
        return None;
    }
    table::with_pcb_and_bufs(
        id,
        |pcb, buf| -> Option<(TcpOutSegment, usize, Option<ZcSource>)> {
            let bufs = buf.as_mut()?;

            let PcbState::Data(d) = &mut pcb.state else {
                return None;
            };
            // Project once through the KBox `DerefMut` so the borrow checker
            // can split the disjoint field borrows below.
            let d: &mut DataState = &mut **d;
            if d.close_phase == ClosePhase::FinWait2 {
                return None;
            }

            let tuple = pcb.tuple;
            let peer_mss = d.peer_mss as usize;
            let snd_wnd = d.snd_wnd as usize;

            // The congestion window is against pipe (RFC 6675); the peer's
            // bounds the sequence range past `snd_una`, sent or lost alike.
            let pipe = bufs.send.sendmap.pipe() as usize;
            let outstanding = d.snd_una.distance_to(d.snd_nxt) as usize;
            let wnd_avail = snd_wnd.saturating_sub(outstanding);
            let cwnd_avail = (d.cc.cwnd() as usize).saturating_sub(pipe);
            let effective_wnd = core::cmp::min(wnd_avail, cwnd_avail);
            // A shut window with nothing in flight hears no ACK to reopen it
            // unless it is asked (RFC 9293 §3.8.6.1).
            let must_probe = snd_wnd == 0 && pipe == 0;

            if let Some(&lost) = bufs.send.sendmap.next_lost() {
                let len = lost.len as usize;
                let offset = d.snd_una.distance_to(lost.seq) as usize;
                let fits = offset + len <= snd_wnd && len <= cwnd_avail;
                if (fits || must_probe) && len <= payload_buf.len() {
                    let seq = lost.seq.raw();
                    if let Some((seg_len, zc)) =
                        resolve_segment(&bufs.send, offset, len, payload_buf)
                    {
                        bufs.send.sendmap.mark_retransmitted(lost.seq);
                        let window = d.advertise(bufs.recv.window());
                        let mut seg =
                            SegmentBuilder::data_push(tuple, seq, d.rcv_nxt.raw(), window);
                        d.stamp(&mut seg, now_ms);
                        arm_retransmit(d, &mut bufs.send.rto_deadline_ms, id, now_ms);
                        pcb.assert_invariants();
                        return Some((seg, seg_len, zc));
                    }
                }
            }

            if bufs.send.sendmap.capacity_remaining() == 0 {
                return None;
            }

            let unsent = bufs.send.unsent_len();
            if unsent == 0 && d.fin_queued {
                return send_fin(d, tuple, id).map(|seg| (seg, 0, None));
            }
            if unsent > 0 && must_probe {
                arm_retransmit(d, &mut bufs.send.rto_deadline_ms, id, now_ms);
                return None;
            }
            let mut max_send = core::cmp::min(unsent, peer_mss);
            max_send = core::cmp::min(max_send, effective_wnd);
            max_send = core::cmp::min(max_send, payload_buf.len());

            // Nagle (RFC 896): defer sub-MSS segments when data is in flight.
            if d.nagle_enabled && max_send < peer_mss && pipe > 0 {
                return None;
            }

            if max_send == 0 {
                return None;
            }

            let seq = d.snd_nxt.raw();
            let unsent_off = bufs.send.inflight;
            let Some((payload_len, zc)) =
                resolve_segment(&bufs.send, unsent_off, max_send, payload_buf)
            else {
                return None;
            };

            bufs.send.mark_sent(payload_len);
            d.snd_nxt = d.snd_nxt.wrapping_add(payload_len as u32);

            let _ = bufs
                .send
                .sendmap
                .push_sent(SeqNum::new(seq), payload_len as u32, now_ms);
            arm_retransmit(d, &mut bufs.send.rto_deadline_ms, id, now_ms);

            let window = d.advertise(bufs.recv.window());
            let mut seg = SegmentBuilder::data_push(tuple, seq, d.rcv_nxt.raw(), window);
            d.stamp(&mut seg, now_ms);
            #[cfg(debug_assertions)]
            d.debug_assert_sendmap(&bufs.send.sendmap);
            pcb.assert_invariants();

            Some((seg, payload_len, zc))
        },
    )
    .flatten()
}

/// A retransmit timer fired on a half-open connection, in either direction.
///
/// `None` means neither handshake state: fall through to the data path.
/// `Some(GaveUp(None))` means the attempt is exhausted and the PCB is gone.
///
/// `SynRecv` is reached here only through a simultaneous open; a listener's
/// half-open entries retransmit from `SynQueue` instead. One budget spans both
/// arms, since a crossed SYN answers the SYN already sent.
fn on_handshake_retransmit(id: ConnId, fired: Option<TimerToken>) -> Option<RetransmitAction> {
    enum SynOutcome {
        NotHandshake,
        Stale,
        Exhausted(&'static str),
        Resend(TcpOutSegment),
    }
    let stale = |armed: Option<TimerToken>| fired.is_some_and(|token| armed != Some(token));

    let outcome = table::with_pcb_mut(id, |pcb| {
        let tuple = pcb.tuple;
        let now_ms = clock::now_ms();
        match &mut pcb.state {
            PcbState::SynSent(s) if stale(s.retransmit_token) => SynOutcome::Stale,
            PcbState::SynRecv(s) if stale(s.retransmit_token) => SynOutcome::Stale,
            PcbState::SynSent(s) => {
                s.retransmit_token = None;
                if s.retransmits >= ACTIVE_SYN_RETRIES_MAX {
                    return SynOutcome::Exhausted("SYN");
                }
                s.retransmits = s.retransmits.saturating_add(1);
                s.rto_ms = s.rto_ms.saturating_mul(2);

                let seg = SegmentBuilder::active_syn(tuple, s.iss.raw(), s.our_wscale)
                    .with_timestamp(now_ms as u32, 0);
                let token =
                    NET_TIMER_WHEEL.schedule(s.rto_ms as u64, TimerKind::TcpRetransmit, id.raw());
                s.retransmit_token = Some(token);
                SynOutcome::Resend(seg)
            }
            PcbState::SynRecv(s) => {
                s.retransmit_token = None;
                if s.retransmits >= ACTIVE_SYN_RETRIES_MAX {
                    return SynOutcome::Exhausted("SYN-ACK");
                }
                s.retransmits = s.retransmits.saturating_add(1);
                s.rto_ms = s.rto_ms.saturating_mul(2);

                let seg = s.syn_ack(tuple, now_ms);
                let token =
                    NET_TIMER_WHEEL.schedule(s.rto_ms as u64, TimerKind::TcpRetransmit, id.raw());
                s.retransmit_token = Some(token);
                SynOutcome::Resend(seg)
            }
            _ => SynOutcome::NotHandshake,
        }
    })?;

    match outcome {
        SynOutcome::NotHandshake => None,
        SynOutcome::Stale => Some(RetransmitAction::Nothing),
        SynOutcome::Exhausted(what) => {
            klog_debug!(
                "tcp: {} retransmits exhausted id={} -> releasing",
                what,
                id.raw()
            );
            table::release(id);
            Some(RetransmitAction::GaveUp(None))
        }
        SynOutcome::Resend(seg) => Some(RetransmitAction::Segment(seg)),
    }
}

/// What a fired retransmit or keepalive timer asks the caller to do.
pub enum RetransmitAction {
    Nothing,
    Data(ConnId),
    Segment(TcpOutSegment),
    /// A timer gave up on the connection and released it; a segment here
    /// resets it.
    GaveUp(Option<TcpOutSegment>),
}

pub fn on_retransmit(conn_id: u32) -> RetransmitAction {
    retransmit_fired(conn_id, None)
}

/// [`on_retransmit`] for the wheel's `token`, which an ACK may have replaced
/// while it was being dispatched.
pub fn on_retransmit_timer(conn_id: u32, token: TimerToken) -> RetransmitAction {
    retransmit_fired(conn_id, Some(token))
}

fn retransmit_fired(conn_id: u32, fired: Option<TimerToken>) -> RetransmitAction {
    let id = ConnId::from_raw(conn_id);
    if id.is_listener() {
        return RetransmitAction::Nothing;
    }

    if let Some(action) = on_handshake_retransmit(id, fired) {
        return action;
    }

    enum Outcome {
        Released,
        Reset(TcpOutSegment),
        Retransmitted,
        Resend(TcpOutSegment),
        Skip,
    }

    let outcome = table::with_pcb_and_bufs(id, |pcb, buf| -> Outcome {
        let Some(bufs) = buf.as_mut() else {
            return Outcome::Skip;
        };
        let orphan = pcb.socket_id.is_none();
        let tuple = pcb.tuple;
        let PcbState::Data(d) = &mut pcb.state else {
            return Outcome::Skip;
        };
        let d: &mut DataState = &mut **d;
        if fired.is_some_and(|token| d.retransmit_token != Some(token)) {
            return Outcome::Skip;
        }
        d.retransmit_token = None;
        let now_ms = clock::now_ms();

        let (segment, rto_ms) = if !bufs.send.sendmap.is_empty() {
            if d.rtt.consecutive_timeouts >= MAX_RETRANSMITS {
                return Outcome::Released;
            }
            d.cc.on_timeout(bufs.send.sendmap.pipe());
            bufs.send.sendmap.mark_all_lost();
            d.rtt.back_off();
            (None, d.rtt.rto_ms() as u64)
        } else if d.snd_una != d.snd_nxt {
            if d.rtt.consecutive_timeouts >= MAX_RETRANSMITS {
                return Outcome::Released;
            }
            let fin_seq = d.snd_nxt.raw().wrapping_sub(1);
            let mut fin = SegmentBuilder::fin_ack(tuple, fin_seq, d.rcv_nxt.raw(), d.advertised());
            d.stamp(&mut fin, now_ms);
            d.rtt.back_off();
            (Some(fin), d.rtt.rto_ms() as u64)
        } else if d.snd_wnd == 0 && bufs.send.unsent_len() > 0 {
            if orphan {
                d.orphan_probes = d.orphan_probes.saturating_add(1);
            }
            if d.persist_probes >= MAX_RETRANSMITS || d.orphan_probes > MAX_ORPHAN_PROBES {
                return Outcome::Reset(SegmentBuilder::bare_rst(tuple, d.snd_nxt.raw()));
            }
            d.persist_probes = d.persist_probes.saturating_add(1);
            d.persist_backoff = d.persist_backoff.saturating_add(1);
            let mut probe = SegmentBuilder::keepalive_probe(
                tuple,
                d.snd_una.raw(),
                d.rcv_nxt.raw(),
                d.advertised(),
            );
            d.stamp(&mut probe, now_ms);
            let interval = (d.rtt.rto_ms() as u64) << d.persist_backoff.min(16);
            (Some(probe), interval.min(u64::from(MAX_RTO_MS)))
        } else {
            bufs.send.rto_deadline_ms = 0;
            return Outcome::Skip;
        };

        d.retransmit_token =
            Some(NET_TIMER_WHEEL.schedule(rto_ms.max(1), TimerKind::TcpRetransmit, conn_id));
        bufs.send.rto_deadline_ms = now_ms.saturating_add(rto_ms);
        pcb.assert_invariants();
        klog_debug!("tcp: retransmit fired id={} rto_ms={}", conn_id, rto_ms);
        match segment {
            Some(seg) => Outcome::Resend(seg),
            None => Outcome::Retransmitted,
        }
    });

    match outcome {
        None | Some(Outcome::Skip) => RetransmitAction::Nothing,
        Some(Outcome::Released) => {
            klog_debug!("tcp: retransmit timeout id={} -> releasing", conn_id);
            table::release(id);
            RetransmitAction::GaveUp(None)
        }
        Some(Outcome::Reset(rst)) => {
            klog_debug!("tcp: zero-window probes unanswered id={} -> reset", conn_id);
            table::release(id);
            RetransmitAction::GaveUp(Some(rst))
        }
        Some(Outcome::Retransmitted) => RetransmitAction::Data(id),
        Some(Outcome::Resend(seg)) => RetransmitAction::Segment(seg),
    }
}

pub fn on_keepalive(conn_id: u32) -> RetransmitAction {
    keepalive_fired(conn_id, None)
}

/// [`on_keepalive`] for the wheel's `token`, which a probe or a peer's
/// activity may already have replaced.
pub fn on_keepalive_timer(conn_id: u32, token: TimerToken) -> RetransmitAction {
    keepalive_fired(conn_id, Some(token))
}

fn keepalive_fired(conn_id: u32, fired: Option<TimerToken>) -> RetransmitAction {
    let id = ConnId::from_raw(conn_id);
    if id.is_listener() {
        return RetransmitAction::Nothing;
    }

    enum Outcome {
        Released,
        Probe(TcpOutSegment),
        Skip,
    }

    let outcome = table::with_pcb_mut(id, |pcb| -> Outcome {
        let PcbState::Data(d) = &mut pcb.state else {
            return Outcome::Skip;
        };
        if d.close_phase != ClosePhase::Established
            || fired.is_some_and(|token| d.keepalive_token != Some(token))
        {
            return Outcome::Skip;
        }
        d.keepalive_token = None;
        if d.keepalive_probes_sent >= TCP_KEEPALIVE_PROBES_MAX {
            return Outcome::Released;
        }
        let mut probe_seg = SegmentBuilder::keepalive_probe(
            pcb.tuple,
            d.snd_una.raw(),
            d.rcv_nxt.raw(),
            d.advertised(),
        );
        d.stamp(&mut probe_seg, clock::now_ms());

        d.keepalive_probes_sent = d.keepalive_probes_sent.saturating_add(1);
        let token =
            NET_TIMER_WHEEL.schedule(TCP_KEEPALIVE_INTERVAL_MS, TimerKind::TcpKeepalive, conn_id);
        d.keepalive_token = Some(token);
        Outcome::Probe(probe_seg)
    });

    match outcome {
        None | Some(Outcome::Skip) => RetransmitAction::Nothing,
        Some(Outcome::Released) => {
            klog_debug!(
                "tcp: keepalive max probes reached id={} -> releasing",
                conn_id
            );
            table::release(id);
            RetransmitAction::GaveUp(None)
        }
        Some(Outcome::Probe(seg)) => RetransmitAction::Segment(seg),
    }
}

/// Release a TIME_WAIT slot `2 × MSL` after the peer's last FIN; one whose
/// socket still has bytes to read re-arms, since nothing else releases it.
pub fn on_time_wait_expire(conn_id: u32) {
    let id = ConnId::from_raw(conn_id);
    if id.is_listener() {
        return;
    }
    let now_ms = clock::now_ms();
    let expired = table::with_pcb_and_bufs(id, |pcb, bufs| {
        let owed = pcb.socket_id.is_some() && bufs.as_ref().is_some_and(|b| b.recv.available() > 0);
        let PcbState::TimeWait(tw) = &mut pcb.state else {
            return false;
        };
        let waited = now_ms.saturating_sub(tw.entry_ms);
        if waited >= TIME_WAIT_MS && !owed {
            return true;
        }
        let delay_ms = match TIME_WAIT_MS.checked_sub(waited) {
            Some(left) if left > 0 => left,
            _ => TIME_WAIT_MS,
        };
        tw.expire_token = Some(NET_TIMER_WHEEL.schedule(delay_ms, TimerKind::TcpTimeWait, conn_id));
        false
    })
    .unwrap_or(false);
    if expired {
        klog_debug!("tcp: TIME_WAIT timer expired id={}", conn_id);
        table::release(id);
    }
}

/// Only an orphan is timed out of FIN_WAIT_2: a socket may still be reading
/// the peer's half, so its timer waits for the close.
pub fn on_fin_wait2_timeout(conn_id: u32) {
    let id = ConnId::from_raw(conn_id);
    if id.is_listener() {
        return;
    }
    let orphan = table::with_pcb_mut(id, |pcb| {
        let orphan = pcb.socket_id.is_none();
        let PcbState::Data(d) = &mut pcb.state else {
            return false;
        };
        if d.close_phase != ClosePhase::FinWait2 {
            return false;
        }
        if !orphan {
            d.fin_wait2_token = Some(NET_TIMER_WHEEL.schedule(
                FIN_WAIT2_TIMEOUT_MS,
                TimerKind::TcpFinWait2,
                conn_id,
            ));
        }
        orphan
    });
    if orphan == Some(true) {
        klog_debug!("tcp: FIN_WAIT_2 timeout id={}", conn_id);
        table::release(id);
    }
}

/// Deterministic retransmit probe — test-only. Triggers retransmit for the
/// first connection whose RTO deadline has expired at `now_ms`.
#[cfg(feature = "test-hooks")]
pub fn retransmit_check(now_ms: u64) -> Option<ConnId> {
    let mut ids = [None; table::TOTAL_PCB_SLOTS];
    let n = table::snapshot_shard_conn_ids(&mut ids);
    for entry in &ids[..n] {
        let Some(id) = *entry else { continue };

        #[derive(Clone, Copy)]
        enum Outcome {
            Released,
            Retransmitted,
            Skip,
        }

        let outcome = table::with_pcb_and_bufs(id, |pcb, buf| -> Outcome {
            let Some(bufs) = buf.as_mut() else {
                return Outcome::Skip;
            };
            let send = &bufs.send;
            if send.inflight == 0 || send.rto_deadline_ms == 0 || now_ms < send.rto_deadline_ms {
                return Outcome::Skip;
            }
            let PcbState::Data(d) = &mut pcb.state else {
                return Outcome::Skip;
            };
            let d: &mut DataState = &mut **d;
            if d.rtt.consecutive_timeouts >= MAX_RETRANSMITS {
                return Outcome::Released;
            }
            d.cc.on_timeout(bufs.send.sendmap.pipe());
            bufs.send.sendmap.mark_all_lost();
            d.rtt.back_off();
            bufs.send.rto_deadline_ms = now_ms.saturating_add(d.rtt.rto_ms() as u64);
            pcb.assert_invariants();
            Outcome::Retransmitted
        });

        match outcome {
            None | Some(Outcome::Skip) => continue,
            Some(Outcome::Released) => {
                table::release(id);
                return None;
            }
            Some(Outcome::Retransmitted) => return Some(id),
        }
    }
    None
}

pub fn delayed_ack_check(now_ms: u64) -> Option<(ConnId, TcpOutSegment)> {
    let mut ids = [None; table::TOTAL_PCB_SLOTS];
    let n = table::snapshot_shard_conn_ids(&mut ids);
    for entry in &ids[..n] {
        let Some(id) = *entry else { continue };
        if let Some(seg) = table::with_pcb_and_bufs(id, |pcb, buf| {
            let bufs = buf.as_mut()?;
            let tuple = pcb.tuple;
            let PcbState::Data(d) = &mut pcb.state else {
                return None;
            };
            d.check_delayed_ack(tuple, bufs, now_ms)
        })
        .flatten()
        {
            return Some((id, seg));
        }
    }
    None
}

pub fn reset_all() {
    table::clear_all();
    isn::reset_for_tests();
}
