//! Content-free X11 focus accounting for physical desktop actions.
//!
//! `_NET_ACTIVE_WINDOW` changes do not contain the new window id, so sampling
//! before and after an action misses a temporary takeover that is restored.
//! This retained observer reads the property at each PropertyNotify event and
//! brackets actions through the registry's single dispatch chokepoint.

use std::{
    collections::VecDeque,
    io::{ErrorKind, Read, Write},
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use cua_driver_contract::FocusEffect;
use cua_driver_core::{
    focus_effect::{classify_focus_epoch, indeterminate},
    tool::{ActionObservationEpoch, ActionObserver},
};
use serde_json::Value;
use x11rb::{
    connection::Connection,
    protocol::{xproto::*, Event},
    rust_connection::RustConnection,
};

const JOURNAL_CAP: usize = 4096;

#[derive(Clone, Copy)]
struct Transition {
    sequence: u64,
    at: Instant,
    active: Option<u64>,
}

#[derive(Clone, Copy)]
struct EpochStart {
    sequence: u64,
    at: Instant,
    active: Option<u64>,
    complete: bool,
}

enum Command {
    Begin(mpsc::Sender<EpochStart>),
    Finish {
        start: EpochStart,
        target: Option<u64>,
        reply: mpsc::Sender<FocusEffect>,
    },
}

pub struct X11FocusObserver {
    commands: mpsc::Sender<Command>,
    wake: Arc<Mutex<UnixStream>>,
}

impl X11FocusObserver {
    pub fn start() -> Option<Self> {
        let (commands, receiver) = mpsc::channel();
        let (wake_read, wake_write) = UnixStream::pair().ok()?;
        wake_read.set_nonblocking(true).ok()?;
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("cua-x11-focus-audit".to_owned())
            .spawn(move || run_monitor(receiver, ready_tx, wake_read))
            .ok()?;
        ready_rx.recv_timeout(Duration::from_secs(1)).ok()??;
        Some(Self {
            commands,
            wake: Arc::new(Mutex::new(wake_write)),
        })
    }

    fn send(&self, command: Command) -> bool {
        self.commands.send(command).is_ok()
            && self
                .wake
                .lock()
                .ok()
                .is_some_and(|mut wake| wake.write_all(&[1]).is_ok())
    }
}

struct FocusEpoch {
    commands: mpsc::Sender<Command>,
    wake: Arc<Mutex<UnixStream>>,
    start: EpochStart,
    target: Option<u64>,
}

impl ActionObservationEpoch for FocusEpoch {
    fn finish(self: Box<Self>) -> Option<FocusEffect> {
        let (reply, receive) = mpsc::channel();
        let sent = self
            .commands
            .send(Command::Finish {
                start: self.start,
                target: self.target,
                reply,
            })
            .is_ok()
            && self
                .wake
                .lock()
                .ok()
                .is_some_and(|mut wake| wake.write_all(&[1]).is_ok());
        Some(if sent {
            receive
                .recv_timeout(Duration::from_millis(100))
                .unwrap_or_else(|_| indeterminate())
        } else {
            indeterminate()
        })
    }
}

impl ActionObserver for X11FocusObserver {
    fn begin(&self, _tool: &str, args: &Value) -> Option<Box<dyn ActionObservationEpoch>> {
        let (reply, receive) = mpsc::channel();
        let start = self
            .send(Command::Begin(reply))
            .then(|| receive.recv_timeout(Duration::from_millis(100)).ok())
            .flatten()
            .unwrap_or(EpochStart {
                sequence: 0,
                at: Instant::now(),
                active: None,
                complete: false,
            });
        Some(Box::new(FocusEpoch {
            commands: self.commands.clone(),
            wake: self.wake.clone(),
            start,
            target: target_window(args),
        }))
    }
}

fn target_window(args: &Value) -> Option<u64> {
    if let Some(window) = args.get("window_id").and_then(Value::as_u64) {
        return Some(window);
    }
    let pid = args.get("pid").and_then(Value::as_i64)? as i32;
    let token = args.get("element_token").and_then(Value::as_str)?;
    match cua_driver_core::element_token::resolve_element_args(
        pid,
        args.get("element_index")
            .and_then(Value::as_u64)
            .map(|value| value as usize),
        Some(token),
        args.get("snapshot_id").and_then(Value::as_str),
        None,
        "focus_audit",
    ) {
        Ok(cua_driver_core::element_token::ResolvedElement::Element { window_id, .. }) => {
            window_id.map(u64::from)
        }
        _ => None,
    }
}

fn run_monitor(
    commands: mpsc::Receiver<Command>,
    ready: mpsc::SyncSender<Option<()>>,
    mut wake: UnixStream,
) {
    let Ok((conn, screen)) = RustConnection::connect(None) else {
        let _ = ready.send(None);
        return;
    };
    let root = conn.setup().roots[screen].root;
    let Ok(active_atom) = intern(&conn, "_NET_ACTIVE_WINDOW") else {
        let _ = ready.send(None);
        return;
    };
    let current_mask = conn
        .get_window_attributes(root)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map(|attributes| attributes.your_event_mask)
        .unwrap_or(EventMask::NO_EVENT);
    let subscribed = conn
        .change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(current_mask | EventMask::PROPERTY_CHANGE),
        )
        .ok()
        .is_some_and(|cookie| cookie.check().is_ok());
    if !subscribed || conn.flush().is_err() {
        let _ = ready.send(None);
        return;
    }

    let mut active = read_active(&conn, root, active_atom);
    let mut sequence = 0_u64;
    let mut journal = VecDeque::<Transition>::new();
    let mut healthy = true;
    let _ = ready.send(Some(()));

    let x11_fd = conn.stream().as_raw_fd();
    let wake_fd = wake.as_raw_fd();
    loop {
        let mut descriptors = [
            libc::pollfd {
                fd: x11_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // Both descriptors are process-owned and remain valid for this thread.
        let poll_result =
            unsafe { libc::poll(descriptors.as_mut_ptr(), descriptors.len() as _, -1) };
        if poll_result < 0 {
            if std::io::Error::last_os_error().kind() == ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if descriptors[1].revents & libc::POLLIN != 0 {
            let mut buffer = [0_u8; 64];
            loop {
                match wake.read(&mut buffer) {
                    Ok(0) => return,
                    Ok(_) => continue,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => return,
                }
            }
        }
        if !drain_events(
            &conn,
            root,
            active_atom,
            &mut active,
            &mut sequence,
            &mut journal,
        ) {
            healthy = false;
        }
        while let Ok(command) = commands.try_recv() {
            match command {
                Command::Begin(reply) => {
                    // A round trip on this same connection places every preceding
                    // focus event into x11rb's queue before the epoch snapshot.
                    healthy &= conn
                        .get_input_focus()
                        .ok()
                        .and_then(|cookie| cookie.reply().ok())
                        .is_some();
                    healthy &= drain_events(
                        &conn,
                        root,
                        active_atom,
                        &mut active,
                        &mut sequence,
                        &mut journal,
                    );
                    active = read_active(&conn, root, active_atom);
                    let _ = reply.send(EpochStart {
                        sequence,
                        at: Instant::now(),
                        active,
                        complete: healthy && active.is_some(),
                    });
                }
                Command::Finish {
                    start,
                    target,
                    reply,
                } => {
                    healthy &= conn
                        .get_input_focus()
                        .ok()
                        .and_then(|cookie| cookie.reply().ok())
                        .is_some();
                    healthy &= drain_events(
                        &conn,
                        root,
                        active_atom,
                        &mut active,
                        &mut sequence,
                        &mut journal,
                    );
                    active = read_active(&conn, root, active_atom);
                    let overrun = journal
                        .front()
                        .is_some_and(|entry| entry.sequence > start.sequence.saturating_add(1));
                    let transitions = journal
                        .iter()
                        .copied()
                        .filter(|entry| entry.sequence > start.sequence)
                        .collect::<Vec<_>>();
                    let timed_transitions = transitions
                        .iter()
                        .map(|transition| {
                            (
                                transition
                                    .at
                                    .saturating_duration_since(start.at)
                                    .as_millis()
                                    .min(u128::from(u64::MAX))
                                    as u64,
                                transition.active,
                            )
                        })
                        .collect::<Vec<_>>();
                    let effect = classify_focus_epoch(
                        start.active,
                        target,
                        active,
                        timed_transitions.as_slice(),
                        start.complete && healthy && !overrun,
                        Instant::now()
                            .saturating_duration_since(start.at)
                            .as_millis()
                            .min(u128::from(u64::MAX)) as u64,
                    );
                    let _ = reply.send(effect);
                }
            }
        }
    }
}

fn drain_events(
    conn: &RustConnection,
    root: Window,
    active_atom: Atom,
    active: &mut Option<u64>,
    sequence: &mut u64,
    journal: &mut VecDeque<Transition>,
) -> bool {
    loop {
        match conn.poll_for_event() {
            Ok(Some(Event::PropertyNotify(event)))
                if event.window == root && event.atom == active_atom =>
            {
                *active = read_active(conn, root, active_atom);
                *sequence = sequence.saturating_add(1);
                journal.push_back(Transition {
                    sequence: *sequence,
                    at: Instant::now(),
                    active: *active,
                });
                while journal.len() > JOURNAL_CAP {
                    journal.pop_front();
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => return true,
            Err(_) => return false,
        }
    }
}

fn read_active(conn: &RustConnection, root: Window, atom: Atom) -> Option<u64> {
    conn.get_property(false, root, atom, AtomEnum::WINDOW, 0, 1)
        .ok()?
        .reply()
        .ok()?
        .value32()?
        .next()
        .filter(|window| *window != x11rb::NONE)
        .map(u64::from)
}

fn intern(conn: &RustConnection, name: &str) -> Result<Atom, ()> {
    conn.intern_atom(false, name.as_bytes())
        .map_err(|_| ())?
        .reply()
        .map(|reply| reply.atom)
        .map_err(|_| ())
}
