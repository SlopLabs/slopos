//! Thread-safe protocol connection for compositor communication.
//!
//! The protocol client is confined to the UI thread via `Rc` (`!Send + !Sync`).
//! Background threads that need to post work or a message to the UI thread use
//! [`UiSender`], which is `Send + Sync + Clone`.

use core::cell::RefCell;
use std::any::Any;
use std::boxed::Box;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::vec::Vec;

use slopos_abi::handle::{DisplayHandle, HasDisplayHandle, RawDisplayHandle};
use slopos_abi::pixel::PixelFormat;
use slopos_abi::syscall::posix::POLLIN;
use slopos_abi::syscall::types::UserPollFd;
use slopos_protocol::client::Client;
use slopos_protocol::types::{ProtocolError, SurfaceId, ToplevelId};

/// Protocol connection state.
///
/// Confined to the UI thread by `Rc` (`!Send`, `!Sync`). `pending_destroys` is a
/// `RefCell` separate from the client so `Surface::drop` can always queue a
/// deferred destroy while the client is borrowed elsewhere.
pub struct Protocol {
    client: RefCell<Client>,
    pending_destroys: RefCell<PendingDestroys>,
    ui_queue: Arc<UiQueue>,
    compositor_fd: i32,
    display_format: PixelFormat,
    display_width: u32,
    display_height: u32,
}

/// Handle to the protocol connection; `Rc` confines it to the UI thread.
pub type ProtocolHandle = Rc<Protocol>;

/// Connect to the compositor protocol socket.
///
/// Retries while the compositor is still starting up; a compositor that
/// refuses this client's version refuses it every time.
pub fn connect() -> Result<ProtocolHandle, ()> {
    for _ in 0..10 {
        let connected = Client::connect(&slopos_protocol::socket_path(slopos_cabi::geteuid()))
            .and_then(|mut client| client.ensure_output_info().map(|()| client));
        let client = match connected {
            Ok(client) => client,
            Err(ProtocolError::VersionMismatch) => return Err(()),
            Err(_) => {
                slopos_cabi::sleep_ms(200);
                continue;
            }
        };
        let compositor_fd = client.fd();
        let display_format =
            PixelFormat::from_u32(client.display_format()).unwrap_or(PixelFormat::Argb8888);
        let display_width = client.display_width();
        let display_height = client.display_height();

        return Ok(Rc::new(Protocol {
            client: RefCell::new(client),
            pending_destroys: RefCell::new(PendingDestroys::new()),
            ui_queue: UiQueue::new()?,
            compositor_fd,
            display_format,
            display_width,
            display_height,
        }));
    }
    Err(())
}

impl HasDisplayHandle for Protocol {
    fn display_handle(&self) -> DisplayHandle<'_> {
        DisplayHandle::new(RawDisplayHandle {
            fd: self.compositor_fd,
            format: self.display_format,
            width: self.display_width,
            height: self.display_height,
        })
    }
}

impl Protocol {
    /// The compositor socket fd; stable for this connection's lifetime.
    #[inline]
    pub fn compositor_fd(&self) -> i32 {
        self.compositor_fd
    }

    /// The read end of the self-pipe a [`UiSender`] writes.
    ///
    /// Exposed so an async consumer can await readiness on it directly; it
    /// drains it with [`Protocol::drain_wakeup`], or no send writes it again.
    #[inline]
    pub fn wakeup_read_fd(&self) -> i32 {
        self.ui_queue.wakeup_fd()
    }

    /// Get exclusive access to the protocol client.
    ///
    /// Panics on reentrant borrow.
    #[inline]
    pub fn borrow_client(&self) -> core::cell::RefMut<'_, Client> {
        self.client.borrow_mut()
    }

    /// Non-panicking variant of [`borrow_client()`].
    #[inline]
    pub fn try_borrow_client(&self) -> Option<core::cell::RefMut<'_, Client>> {
        self.client.try_borrow_mut().ok()
    }

    /// Queue a deferred destroy for compositor-side objects.
    ///
    /// Called from `Surface::drop`, where the client `RefCell` is already borrowed.
    pub fn queue_destroy(&self, toplevel_id: ToplevelId, surface_id: SurfaceId) {
        if let Ok(mut q) = self.pending_destroys.try_borrow_mut() {
            q.push(toplevel_id, surface_id);
        }
    }

    /// Send any deferred destroy requests queued by `Surface::drop`.
    ///
    /// Call at the top of the event loop before any other client operations.
    pub fn flush_pending_destroys(&self) {
        let snapshot = {
            let Ok(mut q) = self.pending_destroys.try_borrow_mut() else {
                return;
            };
            q.take()
        };

        if snapshot.is_empty() {
            return;
        }

        let Some(mut client) = self.try_borrow_client() else {
            if let Ok(mut q) = self.pending_destroys.try_borrow_mut() {
                for &(toplevel_id, surface_id) in &snapshot {
                    q.push(toplevel_id, surface_id);
                }
            }
            return;
        };

        for (toplevel_id, surface_id) in snapshot {
            if toplevel_id != ToplevelId::NONE {
                let _ = client.toplevel_destroy(toplevel_id);
            }
            if surface_id != SurfaceId::NONE {
                let _ = client.surface_destroy(surface_id);
            }
        }
    }

    /// Create a sender handle for posting work from background threads.
    pub fn ui_sender(&self) -> UiSender {
        self.ui_queue.sender()
    }

    /// Execute all closures posted by background threads via [`UiSender`].
    ///
    /// Call at the top of the event loop, after `flush_pending_destroys`.
    pub fn drain_ui_queue(&self) {
        let mut client = match self.try_borrow_client() {
            Some(c) => c,
            None => return,
        };
        self.ui_queue.run_posted(&mut client);
    }

    /// The messages background threads sent through [`UiSender::send`], oldest
    /// first.
    pub fn take_messages(&self) -> Vec<UiMessage> {
        self.ui_queue.take_messages()
    }

    /// Sleep until the compositor sends data or `timeout_ms` elapses.
    ///
    /// Also woken by a [`UiSender`] from a background thread.
    ///
    /// * `timeout_ms > 0` — sleep up to that many milliseconds.
    /// * `timeout_ms == 0` — non-blocking check (returns immediately).
    /// * `timeout_ms < 0` — sleep indefinitely until an event arrives.
    pub fn wait_events(&self, timeout_ms: i64) {
        let mut fds = [
            UserPollFd {
                fd: self.compositor_fd,
                events: POLLIN,
                revents: 0,
            },
            UserPollFd {
                fd: self.ui_queue.wakeup_fd(),
                events: POLLIN,
                revents: 0,
            },
        ];
        let _ = slopos_cabi::poll(&mut fds, timeout_ms.clamp(-1, i32::MAX.into()) as i32);

        if fds[1].revents & POLLIN != 0 {
            self.drain_wakeup();
        }
    }

    /// Drain the self-pipe so a pending wakeup doesn't fire again next iteration.
    pub fn drain_wakeup(&self) {
        self.ui_queue.drain_wakeup();
    }
}

const PENDING_DESTROY_CAP: usize = 64;

struct PendingDestroys {
    entries: Vec<(ToplevelId, SurfaceId)>,
}

impl PendingDestroys {
    const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    fn push(&mut self, toplevel_id: ToplevelId, surface_id: SurfaceId) {
        if self.entries.len() < PENDING_DESTROY_CAP {
            self.entries.push((toplevel_id, surface_id));
        } else {
            eprintln!("warn: destroy queue full, surface leak");
        }
    }

    fn take(&mut self) -> Vec<(ToplevelId, SurfaceId)> {
        core::mem::take(&mut self.entries)
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

type UiCallback = Box<dyn FnOnce(&mut Client) + Send>;

/// A message a background thread sent the UI thread, typed by whoever reads it.
pub type UiMessage = Box<dyn Any + Send>;

/// What background threads hand the UI thread, and the self-pipe that wakes it
/// from `poll`.
///
/// Push from any thread; drain on the UI thread only. The queue owns both ends
/// of the pipe, so a sender that outlives the connection writes to its own
/// pipe rather than to whatever the descriptor's number names next.
pub struct UiQueue {
    posted: Mutex<Vec<UiCallback>>,
    messages: Mutex<Vec<UiMessage>>,
    /// Set from the first push after a drain until the next, so a burst
    /// writes the pipe once.
    woken: AtomicBool,
    wakeup_read_fd: i32,
    wakeup_write_fd: i32,
}

impl UiQueue {
    pub fn new() -> Result<Arc<UiQueue>, ()> {
        let (wakeup_read_fd, wakeup_write_fd) =
            slopos_cabi::pipe2(slopos_abi::syscall::posix::O_NONBLOCK as i32).map_err(drop)?;
        Ok(Arc::new(UiQueue {
            posted: Mutex::new(Vec::new()),
            messages: Mutex::new(Vec::new()),
            woken: AtomicBool::new(false),
            wakeup_read_fd,
            wakeup_write_fd,
        }))
    }

    pub fn sender(self: &Arc<Self>) -> UiSender {
        UiSender {
            queue: Arc::clone(self),
        }
    }

    /// Readable while something was pushed since the last [`Self::drain_wakeup`].
    pub fn wakeup_fd(&self) -> i32 {
        self.wakeup_read_fd
    }

    /// Answers whether a push was waiting. Read the queues after it: a push
    /// that lands while it drains writes nothing and is found there.
    pub fn drain_wakeup(&self) -> bool {
        let mut drain = [0u8; 64];
        let mut woken = false;
        while slopos_cabi::read(self.wakeup_read_fd, &mut drain).is_ok_and(|n| n > 0) {
            woken = true;
        }
        self.woken.store(false, Ordering::SeqCst);
        woken
    }

    pub fn take_messages(&self) -> Vec<UiMessage> {
        self.messages
            .lock()
            .map(|mut q| core::mem::take(&mut *q))
            .unwrap_or_default()
    }

    fn wake(&self) {
        if !self.woken.swap(true, Ordering::SeqCst)
            && slopos_cabi::write(self.wakeup_write_fd, &[1u8]).is_err()
        {
            self.woken.store(false, Ordering::SeqCst);
        }
    }

    fn run_posted(&self, client: &mut Client) {
        let pending = {
            let Ok(mut q) = self.posted.lock() else {
                return;
            };
            core::mem::take(&mut *q)
        };
        for cb in pending {
            cb(client);
        }
    }
}

impl Drop for UiQueue {
    fn drop(&mut self) {
        let _ = slopos_cabi::close(self.wakeup_read_fd);
        let _ = slopos_cabi::close(self.wakeup_write_fd);
    }
}

/// Handle for posting work or a message to the UI thread from any thread.
///
/// Posted closures run on the next [`Protocol::drain_ui_queue`]; messages wait
/// for [`Protocol::take_messages`].
pub struct UiSender {
    queue: Arc<UiQueue>,
}

impl UiSender {
    /// Post a closure to execute on the UI thread.
    pub fn post(&self, f: impl FnOnce(&mut Client) + Send + 'static) {
        if let Ok(mut q) = self.queue.posted.lock() {
            q.push(Box::new(f));
        }
        self.queue.wake();
    }

    /// Queues `message` for [`Protocol::take_messages`] and wakes the UI
    /// thread; appkit's loop takes them, [`crate::run`]'s never does.
    pub fn send(&self, message: impl Any + Send) {
        if let Ok(mut q) = self.queue.messages.lock() {
            q.push(Box::new(message));
        }
        self.queue.wake();
    }
}

impl Clone for UiSender {
    fn clone(&self) -> Self {
        Self {
            queue: Arc::clone(&self.queue),
        }
    }
}
