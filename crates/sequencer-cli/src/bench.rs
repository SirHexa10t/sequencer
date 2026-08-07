//! The bench command: measure what the machine actually delivers.

// Only the live progress line asks whether stderr is a terminal.
#[cfg(all(feature = "evdev", target_os = "linux"))]
use std::io::IsTerminal as _;

#[cfg(not(all(feature = "evdev", target_os = "linux")))]
use crate::Error;
use crate::args::BenchArgs;
#[cfg(all(feature = "evdev", target_os = "linux"))]
use crate::exit;
use crate::{Deps, Result};

/// `sequencer bench`.
///
/// # Errors
///
/// If there is no backend for this platform, or the devices cannot be opened.
#[cfg(all(feature = "evdev", target_os = "linux"))]
pub fn bench(args: &BenchArgs, deps: &mut Deps<'_>) -> Result<u8> {
    match args.cps {
        Some(rate) => writeln!(deps.out, "Measuring {rate}/s for {:.1}s...", args.seconds)?,
        None => writeln!(
            deps.out,
            "Measuring the ceiling for {:.1}s (no target rate)...",
            args.seconds
        )?,
    }
    deps.out.flush()?;

    let mut observer = BenchProgress {
        live: std::io::stderr().is_terminal(),
    };
    let result = sequencer_input::linux::bench::run(args.cps, args.seconds, &mut observer)?;
    if observer.live {
        // Wipe the progress line so the summary starts on clean ground.
        eprint!("\r\u{1b}[K");
    }

    writeln!(deps.out)?;
    if let Some(requested) = args.cps {
        writeln!(deps.out, "  requested   {requested:>10.0}/s")?;
    }
    writeln!(deps.out, "  emitted     {:>10.0}/s", result.emitted_rate())?;
    writeln!(
        deps.out,
        "  delivered   {:>10.0}/s",
        result.delivered_rate()
    )?;
    writeln!(
        deps.out,
        "\n{} presses written over {:.3}s; the kernel delivered {}.",
        result.emitted,
        result.elapsed.as_secs_f64(),
        result.delivered
    )?;

    // Emitted is what this process wrote; delivered is what a reader actually saw. A gap
    // means events were coalesced or dropped below us, which is the number that matters
    // and the one a rate computed purely from our own loop would never show.
    if result.delivered < result.emitted {
        let lost = result.emitted - result.delivered;
        writeln!(
            deps.out,
            "{lost} did not arrive: at this rate the kernel or the reader is the \
             bottleneck, not the loop."
        )?;
    }
    Ok(exit::OK)
}

/// Renders the benchmark's live progress, and sheds root once the devices are open.
///
/// Progress goes to **stderr**, not `deps.out`: it is a carriage-return-overwritten line
/// meant for a watching human, and mixing it into the stream a caller may be capturing
/// would leave a pile of `\r`-joined junk in their file. It is suppressed entirely when
/// stderr is not a terminal, so a redirected run just prints its summary.
#[cfg(all(feature = "evdev", target_os = "linux"))]
struct BenchProgress {
    live: bool,
}

#[cfg(all(feature = "evdev", target_os = "linux"))]
impl sequencer_input::linux::BenchObserver for BenchProgress {
    fn devices_open(
        &mut self,
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        crate::elevate::drop_root_after_open().map_err(Into::into)
    }

    fn sample(&mut self, sample: sequencer_input::linux::BenchSample) {
        if !self.live {
            return;
        }
        // `\r` + erase-to-end-of-line: one line, rewritten, no scrollback spam.
        eprint!(
            "\r\u{1b}[K  {:>5.1}s   emitting {:>8.0}/s   delivered {:>8.0}/s",
            sample.elapsed.as_secs_f64(),
            sample.emitted_rate(),
            sample.delivered_rate(),
        );
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }
}

/// `sequencer bench`, without the device backend.
///
/// # Errors
///
/// Always. Measuring delivery means reading the emitted events back off a device node,
/// which is the one thing the X11 backend cannot do: XTEST hands events to the server and
/// there is nothing underneath to read them from.
#[cfg(not(all(feature = "evdev", target_os = "linux")))]
pub fn bench(_args: &BenchArgs, _deps: &mut Deps<'_>) -> Result<u8> {
    Err(Error::NotImplemented(
        "bench needs the device backend: it measures delivery by reading its own events \
         back, and only /dev/input can be read back."
            .to_owned(),
    ))
}
