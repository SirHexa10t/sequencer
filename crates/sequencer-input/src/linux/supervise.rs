//! Keeping a grabbed keyboard the user's: a second process that holds every grab, and lets
//! each one go the moment the manager stops answering.
//!
//! While a keyboard is grabbed, it reaches nothing else — not the desktop, not a text
//! console's Ctrl+Alt+F-key, not Alt+SysRq, all of which the kernel and the compositor would
//! otherwise see. A crash is safe on its own (the kernel lets go of a closed device), but a
//! hang is not, and neither is a suspended manager: Ctrl+Z in its terminal would leave a
//! keyboard that can't even type `fg`. A thread can't watch for that, since it hangs or stops
//! with the process. So the manager starts [`Supervisor`]: the same program run again
//! ([`serve`] is its side), in a process group of its own so that what the terminal sends
//! the manager — Ctrl+C, Ctrl+Z — never reaches it.
//!
//! The grabs are the supervisor's. The manager hands it each device over a socket, as the
//! very open file the manager reads, and the supervisor takes the grab on it; a grab belongs
//! to the open file, so the manager reads every grabbed key through its own descriptor. The
//! manager beats on the socket, and the supervisor lets every grab go on the manager's
//! goodbye — and, saying so, when the beats stop for [`QUIET`] or the socket closes without
//! a goodbye. A manager that resumes after that finds its next beat refused, and stops.
//!
//! Why the supervisor takes the grab, rather than the manager: evdev ends a grab only through
//! the handle that took it — whether one is held is that handle's own record — so when the
//! manager grabbed and the supervisor ungrabbed what it was handed, the ungrab did nothing,
//! and a hung manager kept the keyboards (found 2026-11-03).
//!
//! Everything here is safe Rust: the descriptors arrive as owned files (rustix's
//! `SCM_RIGHTS`), and the grabs are evdev's own.

use std::collections::VecDeque;
use std::io::{self, IoSlice, IoSliceMut, Read as _, Write as _};
use std::mem::MaybeUninit;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rustix::io::Errno;
use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
};

/// How long the supervisor waits between beats before letting the keyboards go. Shorter
/// than a user waits before concluding their keyboard is dead; longer than any pause a
/// working manager takes, which beats every [`BEAT_EVERY`].
pub const QUIET: Duration = Duration::from_secs(2);

/// How often a working manager beats: a quarter of [`QUIET`], so three beats can go missing
/// — a slow moment, not a hang — before the supervisor acts.
pub const BEAT_EVERY: Duration = Duration::from_millis(500);

/// How long the manager waits for a started supervisor to say it is watching, and for its
/// answer to a grab.
const ANSWER_WITHIN: Duration = Duration::from_secs(2);

/// The supervisor, once it watches.
const READY: u8 = b'R';
/// The manager is still answering.
const BEAT: u8 = b'.';
/// Take the grab on the device riding along, as the message's one descriptor.
const GRAB: u8 = b'g';
/// The manager is stopping: let every grab go.
const GOODBYE: u8 = b'q';

/// The supervisor's answer to [`GRAB`]: one of these three, then the error number when the
/// grab failed — four bytes, little-endian, zero otherwise — in [`ANSWER_LEN`] bytes in all.
const GRABBED: u8 = b'y';
const BUSY: u8 = b'b';
const FAILED: u8 = b'n';
const ANSWER_LEN: usize = 5;

/// The most descriptors one message may carry. The manager sends one per message; the room
/// to spare means a sender that batched would lose none.
const FDS_PER_MESSAGE: usize = 8;

/// Why the supervisor would not take a grab.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GrabRefused {
    /// Another program holds the device exclusively.
    #[error("another program holds it")]
    Busy,
    /// The grab failed for another reason, which the supervisor reported.
    #[error(transparent)]
    Failed(io::Error),
    /// The supervisor is gone, or would not answer: nothing may be grabbed.
    #[error("the supervisor is gone: {0}")]
    Gone(#[source] io::Error),
}

/// The manager's end: a supervisor process holding every grab.
#[derive(Debug)]
pub struct Supervisor {
    link: UnixStream,
    child: Child,
}

impl Supervisor {
    /// Starts `command` as the supervisor — this program run again, with whatever marker
    /// routes it to [`serve`] — and waits until it says it is watching.
    ///
    /// # Errors
    ///
    /// The process could not start, or it never said it was watching: most likely the
    /// program does not route the marker to [`serve`], and then nothing may be grabbed.
    pub fn spawn(command: Command) -> io::Result<Self> {
        Self::spawn_within(command, ANSWER_WITHIN)
    }

    fn spawn_within(mut command: Command, wait: Duration) -> io::Result<Self> {
        let (link, theirs) = UnixStream::pair()?;
        // Its own process group: the terminal's Ctrl+C and Ctrl+Z go to the manager's group,
        // and a supervisor stopped alongside the manager would watch over nothing.
        command
            .stdin(Stdio::from(OwnedFd::from(theirs)))
            .process_group(0);
        let mut child = command.spawn()?;
        link.set_read_timeout(Some(wait))?;
        let mut answer = [0; 1];
        if !matches!((&link).read(&mut answer), Ok(1) if answer[0] == READY) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other(
                "the supervisor never said it was watching — does this program route its marker to the supervisor?",
            ));
        }
        link.set_read_timeout(None)?;
        // A beat must never hold the manager up: a socket full of unread beats is a
        // supervisor that already knows the manager answers.
        link.set_nonblocking(true)?;
        Ok(Self { link, child })
    }

    /// Hands `device` to the supervisor — the open device the manager reads — and has it
    /// take the grab, so that the supervisor can end it if the manager stops answering.
    ///
    /// # Errors
    ///
    /// [`GrabRefused::Busy`] when another program holds the device, [`GrabRefused::Failed`]
    /// when the grab failed otherwise, and [`GrabRefused::Gone`] when the supervisor is gone
    /// — then nothing may be grabbed.
    pub fn grab(&self, device: BorrowedFd<'_>) -> Result<(), GrabRefused> {
        send_device(&self.link, device).map_err(GrabRefused::Gone)?;
        decode(read_answer(&self.link, ANSWER_WITHIN).map_err(GrabRefused::Gone)?)
    }

    /// Tells the supervisor the manager still answers. Due every [`BEAT_EVERY`].
    ///
    /// # Errors
    ///
    /// The supervisor is gone — it let the keyboards go, or died — and the manager must stop
    /// rather than run on without it.
    pub fn beat(&self) -> io::Result<()> {
        match (&self.link).write(&[BEAT]) {
            Ok(_) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => Ok(()),
            Err(err) => Err(err),
        }
    }

    /// Ends the watch as the manager stops: the supervisor lets every grab go and exits,
    /// and is waited for, so the keyboards are the session's again on return.
    pub fn finish(self) {
        let _ = (&self.link).write(&[GOODBYE]);
    }
}

impl Drop for Supervisor {
    /// Closing the link without a goodbye is what makes the supervisor let go of whatever it
    /// holds, so dropping an unfinished supervisor is a safe way down too. Either way, it is
    /// reaped here, or killed if it outstays a moment.
    fn drop(&mut self) {
        let _ = self.link.shutdown(std::net::Shutdown::Both);
        let deadline = Instant::now() + ANSWER_WITHIN;
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_)) | Err(_)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Sends `device` over `link` with the request to grab it, as one message carrying it.
fn send_device(link: &UnixStream, device: BorrowedFd<'_>) -> io::Result<()> {
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    let fds = [device];
    if !control.push(SendAncillaryMessage::ScmRights(&fds)) {
        return Err(io::Error::other("no room for the device in the message"));
    }
    // A beat-full socket is non-blocking; a device must arrive, so this one waits its turn.
    loop {
        match sendmsg(
            link,
            &[IoSlice::new(&[GRAB])],
            &mut control,
            SendFlags::NOSIGNAL,
        ) {
            Ok(_) => return Ok(()),
            Err(Errno::AGAIN) => std::thread::sleep(Duration::from_millis(1)),
            Err(Errno::INTR) => {}
            Err(err) => return Err(err.into()),
        }
    }
}

/// The supervisor's answer to a grab, waited for up to `within`: the link doesn't block,
/// for the beats' sake.
fn read_answer(link: &UnixStream, within: Duration) -> io::Result<[u8; ANSWER_LEN]> {
    let mut answer = [0; ANSWER_LEN];
    let mut filled = 0;
    let deadline = Instant::now() + within;
    let mut reader = link;
    while filled < ANSWER_LEN {
        match reader.read(&mut answer[filled..]) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(read) => filled += read,
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the supervisor never answered the grab",
                    ));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(answer)
}

/// What an answer says about the grab.
fn decode(answer: [u8; ANSWER_LEN]) -> Result<(), GrabRefused> {
    let [kind, number @ ..] = answer;
    match kind {
        GRABBED => Ok(()),
        BUSY => Err(GrabRefused::Busy),
        FAILED => Err(GrabRefused::Failed(io::Error::from_raw_os_error(
            i32::from_le_bytes(number),
        ))),
        _ => Err(GrabRefused::Gone(io::Error::other(
            "the supervisor's answer made no sense",
        ))),
    }
}

/// How a grab went, as the supervisor answers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    Grabbed,
    Busy,
    /// Failed, with the error number.
    Failed(i32),
}

impl Answer {
    /// `err`, answered: busy when another program holds the device.
    fn of(err: &io::Error) -> Self {
        if Errno::from_io_error(err) == Some(Errno::BUSY) {
            Self::Busy
        } else {
            Self::Failed(err.raw_os_error().unwrap_or(0))
        }
    }

    fn bytes(self) -> [u8; ANSWER_LEN] {
        let (kind, number) = match self {
            Self::Grabbed => (GRABBED, 0),
            Self::Busy => (BUSY, 0),
            Self::Failed(number) => (FAILED, number),
        };
        let [a, b, c, d] = number.to_le_bytes();
        [kind, a, b, c, d]
    }
}

/// What the supervisor does with the devices it is handed: grabs them in earnest
/// ([`Grabs`]), or records them, in tests.
trait Hold {
    /// Takes the grab on `device`, answering how it went.
    fn grab(&mut self, device: OwnedFd) -> Answer;
    /// Lets go of every grab taken.
    fn release_all(&mut self);
}

/// The grabs in earnest: each device's own handle — the one that took the grab, which
/// alone can end it.
#[derive(Debug, Default)]
struct Grabs(Vec<evdev::Device>);

impl Hold for Grabs {
    fn grab(&mut self, device: OwnedFd) -> Answer {
        let mut device = match evdev::Device::from_fd(device) {
            Ok(device) => device,
            Err(err) => return Answer::of(&err),
        };
        match device.grab() {
            Ok(()) => {
                self.0.push(device);
                Answer::Grabbed
            }
            Err(err) => Answer::of(&err),
        }
    }

    fn release_all(&mut self) {
        // The supervisor's process ends next.
        self.0.drain(..).for_each(crate::linux::grab::let_go);
    }
}

/// Why a watch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ended {
    /// The manager said goodbye: every grab was let go at its word.
    Goodbye,
    /// No beat for [`QUIET`]: every grab was let go.
    Silent,
    /// The manager went without a goodbye — it crashed, or was killed: every grab was let
    /// go.
    Gone,
}

/// The supervisor's side, on `link` — its stdin, the socket [`Supervisor::spawn`] gave it:
/// says it is watching, takes the grab on every device the manager hands it, and lets them
/// all go on the manager's goodbye, when the beats stop, or when the manager goes without a
/// goodbye.
#[must_use]
pub fn serve(link: &UnixStream) -> Ended {
    let ended = serve_with(link, QUIET, &mut Grabs::default());
    // After the devices are back, so a message the terminal holds up can't keep them.
    match ended {
        Ended::Goodbye => {}
        Ended::Silent => eprintln!(
            "sequencer: the profile manager stopped answering, so the keyboards are back to normal; \
             its profiles no longer apply until it runs again"
        ),
        Ended::Gone => eprintln!(
            "sequencer: the profile manager ended without letting go of the keyboards; they are back to normal"
        ),
    }
    ended
}

/// [`serve`] with the wait and the holding given: the grabs in earnest, a recorder in tests.
fn serve_with(link: &UnixStream, quiet: Duration, hold: &mut impl Hold) -> Ended {
    // Every receive waits at most `quiet`: a beat or a grab resets the wait, silence ends it.
    let mut writer = link;
    if link.set_read_timeout(Some(quiet)).is_err() || writer.write_all(&[READY]).is_err() {
        hold.release_all();
        return Ended::Gone;
    }
    loop {
        let mut bytes = [0; 64];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(FDS_PER_MESSAGE))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let received = recvmsg(
            link,
            &mut [IoSliceMut::new(&mut bytes)],
            &mut control,
            RecvFlags::CMSG_CLOEXEC,
        );
        let mut handed: VecDeque<OwnedFd> = control
            .drain()
            .filter_map(|message| match message {
                RecvAncillaryMessage::ScmRights(fds) => Some(fds),
                _ => None,
            })
            .flatten()
            .collect();
        let ended = match received {
            Ok(message) if message.bytes == 0 => Some(Ended::Gone),
            Ok(message) => answer(&bytes[..message.bytes], &mut handed, hold, &mut writer),
            Err(Errno::INTR) => None,
            Err(Errno::AGAIN) => Some(Ended::Silent),
            Err(_) => Some(Ended::Gone),
        };
        if let Some(ended) = ended {
            hold.release_all();
            return ended;
        }
    }
}

/// Carries out what one receive asked, in order — each grab with the next device handed,
/// answered — and whether that ends the watch.
fn answer(
    asked: &[u8],
    handed: &mut VecDeque<OwnedFd>,
    hold: &mut impl Hold,
    writer: &mut &UnixStream,
) -> Option<Ended> {
    for &byte in asked {
        match byte {
            GRAB => {
                let answer = handed
                    .pop_front()
                    .map_or(Answer::Failed(Errno::BADF.raw_os_error()), |device| {
                        hold.grab(device)
                    });
                if writer.write_all(&answer.bytes()).is_err() {
                    return Some(Ended::Gone);
                }
            }
            GOODBYE => return Some(Ended::Goodbye),
            // A beat: the receive itself reset the wait.
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::os::fd::AsFd as _;
    use std::sync::mpsc;

    use super::*;

    const TEST_QUIET: Duration = Duration::from_millis(150);

    /// Holds what it is handed, answering each grab with the next of `answers` — grabbed
    /// once they run out — and hands what it held back into a channel when released.
    struct Recorder {
        answers: VecDeque<Answer>,
        held: Vec<OwnedFd>,
        released: mpsc::Sender<Vec<OwnedFd>>,
    }

    impl Hold for Recorder {
        fn grab(&mut self, device: OwnedFd) -> Answer {
            let answer = self.answers.pop_front().unwrap_or(Answer::Grabbed);
            if answer == Answer::Grabbed {
                self.held.push(device);
            }
            answer
        }

        fn release_all(&mut self) {
            let _ = self.released.send(std::mem::take(&mut self.held));
        }
    }

    /// A supervisor's loop on a thread of the test's own, its grabs answered by `answers`
    /// and what it held handed back into a channel.
    fn serving(
        answers: Vec<Answer>,
    ) -> (
        UnixStream,
        mpsc::Receiver<Vec<OwnedFd>>,
        std::thread::JoinHandle<Ended>,
    ) {
        let (manager, supervisor) = UnixStream::pair().unwrap();
        let (released, back) = mpsc::channel();
        let watching = std::thread::spawn(move || {
            let mut recorder = Recorder {
                answers: answers.into(),
                held: Vec::new(),
                released,
            };
            serve_with(&supervisor, TEST_QUIET, &mut recorder)
        });
        let mut ready = [0; 1];
        (&manager).read_exact(&mut ready).unwrap();
        assert_eq!(ready[0], READY);
        (manager, back, watching)
    }

    /// Asks the supervisor on `manager` to grab `device`, as [`Supervisor::grab`] does.
    fn grab(manager: &UnixStream, device: BorrowedFd<'_>) -> Result<(), GrabRefused> {
        send_device(manager, device).unwrap();
        decode(read_answer(manager, ANSWER_WITHIN).unwrap())
    }

    /// A grabbed device is the same open file the manager holds — what makes the
    /// supervisor's grab the manager's too — and silence lets it go.
    #[test]
    fn a_grabbed_device_is_the_managers_own_and_silence_lets_it_go() {
        let (manager, back, watching) = serving(Vec::new());
        let (reader, mut writer) = io::pipe().unwrap();
        grab(&manager, reader.as_fd()).expect("grabbed");
        assert_eq!(
            watching.join().unwrap(),
            Ended::Silent,
            "no beat, so the watch ends"
        );
        let devices = back.recv().unwrap();
        assert_eq!(devices.len(), 1);
        writer.write_all(b"same file").unwrap();
        drop(writer);
        let mut through = String::new();
        File::from(devices.into_iter().next().unwrap())
            .read_to_string(&mut through)
            .unwrap();
        assert_eq!(through, "same file");
    }

    /// A grab refused is answered with why — another program's, or the error itself — and
    /// the device refused is not held.
    #[test]
    fn a_refused_grab_is_answered_with_why() {
        let (manager, back, watching) = serving(vec![
            Answer::Busy,
            Answer::Failed(Errno::NODEV.raw_os_error()),
        ]);
        let (first, _) = io::pipe().unwrap();
        let (second, _) = io::pipe().unwrap();
        assert!(matches!(
            grab(&manager, first.as_fd()),
            Err(GrabRefused::Busy)
        ));
        match grab(&manager, second.as_fd()) {
            Err(GrabRefused::Failed(err)) => {
                assert_eq!(Errno::from_io_error(&err), Some(Errno::NODEV));
            }
            other => panic!("not the failure the supervisor answered: {other:?}"),
        }
        drop(manager);
        assert_eq!(watching.join().unwrap(), Ended::Gone);
        assert!(back.recv().unwrap().is_empty(), "nothing refused is held");
    }

    /// Every answer reads back as what was answered.
    #[test]
    fn answers_read_back_as_answered() {
        assert!(decode(Answer::Grabbed.bytes()).is_ok());
        assert!(matches!(
            decode(Answer::Busy.bytes()),
            Err(GrabRefused::Busy)
        ));
        match decode(Answer::Failed(Errno::IO.raw_os_error()).bytes()) {
            Err(GrabRefused::Failed(err)) => {
                assert_eq!(Errno::from_io_error(&err), Some(Errno::IO));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            decode([b'?', 0, 0, 0, 0]),
            Err(GrabRefused::Gone(_))
        ));
    }

    /// Beats keep the watch going past many quiet periods; a goodbye ends it, and lets go of
    /// every grab, at once.
    #[test]
    fn beats_keep_the_watch_and_a_goodbye_lets_every_grab_go() {
        let (manager, back, watching) = serving(Vec::new());
        let (reader, _writer) = io::pipe().unwrap();
        grab(&manager, reader.as_fd()).expect("grabbed");
        let deadline = Instant::now() + TEST_QUIET * 4;
        while Instant::now() < deadline {
            (&manager).write_all(&[BEAT]).unwrap();
            std::thread::sleep(TEST_QUIET / 5);
        }
        assert!(!watching.is_finished(), "beaten, it keeps watching");
        (&manager).write_all(&[GOODBYE]).unwrap();
        assert_eq!(watching.join().unwrap(), Ended::Goodbye);
        assert_eq!(back.recv().unwrap().len(), 1, "the grab let go");
    }

    /// A manager that goes without a goodbye — crashed, killed — has its grabs let go at
    /// once, not after the quiet period.
    #[test]
    fn a_manager_gone_without_goodbye_has_its_grabs_let_go_at_once() {
        let (manager, back, watching) = serving(Vec::new());
        let (reader, _writer) = io::pipe().unwrap();
        grab(&manager, reader.as_fd()).expect("grabbed");
        let gone = Instant::now();
        drop(manager);
        assert_eq!(watching.join().unwrap(), Ended::Gone);
        assert!(gone.elapsed() < TEST_QUIET, "at once");
        assert_eq!(back.recv().unwrap().len(), 1);
    }

    /// The grab in earnest, on a real device: the supervisor's grab holds it, and silence
    /// truly ends that grab — the release a recorder can't prove. Needs `/dev/uinput`, and
    /// read access to the device it makes, which the `input` group gives and a uaccess rule
    /// granting `/dev/uinput` alone does not; skips without either.
    #[test]
    fn a_real_grab_is_truly_released_when_the_manager_goes_silent() {
        if std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .is_err()
        {
            eprintln!("skipping: /dev/uinput is not writable here");
            return;
        }
        let mut sink = crate::linux::inject::UinputSink::open().expect("virtual device");
        let node = sink
            .dev_nodes()
            .expect("dev nodes")
            .into_iter()
            .next()
            .expect("the device has an event node");
        let device = match File::open(&node) {
            Ok(device) => device,
            Err(err) if err.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!(
                    "skipping: /dev/uinput is writable here but /dev/input is not, so the \
                     device can't be handed over"
                );
                return;
            }
            Err(err) => panic!("open the device we just made: {err}"),
        };
        let (manager, supervisor) = UnixStream::pair().unwrap();
        let watching =
            std::thread::spawn(move || serve_with(&supervisor, TEST_QUIET, &mut Grabs::default()));
        let mut ready = [0; 1];
        (&manager).read_exact(&mut ready).unwrap();
        grab(&manager, device.as_fd()).expect("the supervisor grabs a real device");
        assert!(
            crate::linux::grab::is_grabbed(&node).unwrap(),
            "the supervisor's grab holds the device"
        );
        assert_eq!(watching.join().unwrap(), Ended::Silent);
        assert!(
            !crate::linux::grab::is_grabbed(&node).unwrap(),
            "silence ended the grab — through the handle that took it"
        );
    }

    /// A supervisor that says it watches is taken; one that never does is refused — and
    /// killed, not left behind — since without it nothing may be grabbed.
    #[test]
    fn only_a_supervisor_that_says_it_watches_is_taken() {
        let mut stand_in = Command::new("sh");
        // Its stdin is the link: it answers on it, then waits for it to close.
        stand_in.args(["-c", "printf R >&0; exec cat >/dev/null"]);
        let supervisor =
            Supervisor::spawn_within(stand_in, Duration::from_secs(5)).expect("it said it watches");
        supervisor.beat().expect("it is there");
        supervisor.finish();

        let mut silent = Command::new("sh");
        silent.args(["-c", "exec sleep 30"]);
        let started = Instant::now();
        let refused = Supervisor::spawn_within(silent, Duration::from_millis(200)).unwrap_err();
        assert!(
            refused.to_string().contains("never said it was watching"),
            "{refused}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "killed, not waited out"
        );
    }

    /// A supervisor that has gone is a beat refused: the manager's cue to stop.
    #[test]
    fn a_beat_to_a_gone_supervisor_is_refused() {
        let mut short_lived = Command::new("sh");
        short_lived.args(["-c", "printf R >&0"]);
        let supervisor = Supervisor::spawn_within(short_lived, Duration::from_secs(5)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while supervisor.beat().is_ok() {
            assert!(Instant::now() < deadline, "the beat was never refused");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
