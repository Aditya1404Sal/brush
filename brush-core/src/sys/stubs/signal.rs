//! Signal processing utilities

use crate::{error, sys, traps};

/// Signals as a Linux system numbers them.
///
/// On WASM they are synthetic: `kill` delivers them to the shell's logical processes (see
/// `execution::process`), and names and numbers follow bash on Linux with musl, whose C library
/// reserves 32 to 34 (they have numbers but no names).
#[cfg(target_arch = "wasm32")]
#[allow(
    unnameable_types,
    missing_docs,
    non_camel_case_types,
    reason = "each variant is named as the signal is"
)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Signal {
    SIGHUP = 1,
    SIGINT = 2,
    SIGQUIT = 3,
    SIGILL = 4,
    SIGTRAP = 5,
    SIGABRT = 6,
    SIGBUS = 7,
    SIGFPE = 8,
    SIGKILL = 9,
    SIGUSR1 = 10,
    SIGSEGV = 11,
    SIGUSR2 = 12,
    SIGPIPE = 13,
    SIGALRM = 14,
    SIGTERM = 15,
    SIGSTKFLT = 16,
    SIGCHLD = 17,
    SIGCONT = 18,
    SIGSTOP = 19,
    SIGTSTP = 20,
    SIGTTIN = 21,
    SIGTTOU = 22,
    SIGURG = 23,
    SIGXCPU = 24,
    SIGXFSZ = 25,
    SIGVTALRM = 26,
    SIGPROF = 27,
    SIGWINCH = 28,
    SIGIO = 29,
    SIGPWR = 30,
    SIGSYS = 31,
    SIG32 = 32,
    SIG33 = 33,
    SIG34 = 34,
    SIGRTMIN = 35,
    SIGRTMIN_1 = 36,
    SIGRTMIN_2 = 37,
    SIGRTMIN_3 = 38,
    SIGRTMIN_4 = 39,
    SIGRTMIN_5 = 40,
    SIGRTMIN_6 = 41,
    SIGRTMIN_7 = 42,
    SIGRTMIN_8 = 43,
    SIGRTMIN_9 = 44,
    SIGRTMIN_10 = 45,
    SIGRTMIN_11 = 46,
    SIGRTMIN_12 = 47,
    SIGRTMIN_13 = 48,
    SIGRTMIN_14 = 49,
    SIGRTMAX_14 = 50,
    SIGRTMAX_13 = 51,
    SIGRTMAX_12 = 52,
    SIGRTMAX_11 = 53,
    SIGRTMAX_10 = 54,
    SIGRTMAX_9 = 55,
    SIGRTMAX_8 = 56,
    SIGRTMAX_7 = 57,
    SIGRTMAX_6 = 58,
    SIGRTMAX_5 = 59,
    SIGRTMAX_4 = 60,
    SIGRTMAX_3 = 61,
    SIGRTMAX_2 = 62,
    SIGRTMAX_1 = 63,
    SIGRTMAX = 64,
}

/// A stub enum representing system signals on unsupported platforms.
#[cfg(not(target_arch = "wasm32"))]
#[allow(unnameable_types)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Signal {}

#[cfg(target_arch = "wasm32")]
const SIGNALS: [(Signal, &str); 64] = [
    (Signal::SIGHUP, "SIGHUP"),
    (Signal::SIGINT, "SIGINT"),
    (Signal::SIGQUIT, "SIGQUIT"),
    (Signal::SIGILL, "SIGILL"),
    (Signal::SIGTRAP, "SIGTRAP"),
    (Signal::SIGABRT, "SIGABRT"),
    (Signal::SIGBUS, "SIGBUS"),
    (Signal::SIGFPE, "SIGFPE"),
    (Signal::SIGKILL, "SIGKILL"),
    (Signal::SIGUSR1, "SIGUSR1"),
    (Signal::SIGSEGV, "SIGSEGV"),
    (Signal::SIGUSR2, "SIGUSR2"),
    (Signal::SIGPIPE, "SIGPIPE"),
    (Signal::SIGALRM, "SIGALRM"),
    (Signal::SIGTERM, "SIGTERM"),
    (Signal::SIGSTKFLT, "SIGSTKFLT"),
    (Signal::SIGCHLD, "SIGCHLD"),
    (Signal::SIGCONT, "SIGCONT"),
    (Signal::SIGSTOP, "SIGSTOP"),
    (Signal::SIGTSTP, "SIGTSTP"),
    (Signal::SIGTTIN, "SIGTTIN"),
    (Signal::SIGTTOU, "SIGTTOU"),
    (Signal::SIGURG, "SIGURG"),
    (Signal::SIGXCPU, "SIGXCPU"),
    (Signal::SIGXFSZ, "SIGXFSZ"),
    (Signal::SIGVTALRM, "SIGVTALRM"),
    (Signal::SIGPROF, "SIGPROF"),
    (Signal::SIGWINCH, "SIGWINCH"),
    (Signal::SIGIO, "SIGIO"),
    (Signal::SIGPWR, "SIGPWR"),
    (Signal::SIGSYS, "SIGSYS"),
    (Signal::SIG32, "32"),
    (Signal::SIG33, "33"),
    (Signal::SIG34, "34"),
    (Signal::SIGRTMIN, "SIGRTMIN"),
    (Signal::SIGRTMIN_1, "SIGRTMIN+1"),
    (Signal::SIGRTMIN_2, "SIGRTMIN+2"),
    (Signal::SIGRTMIN_3, "SIGRTMIN+3"),
    (Signal::SIGRTMIN_4, "SIGRTMIN+4"),
    (Signal::SIGRTMIN_5, "SIGRTMIN+5"),
    (Signal::SIGRTMIN_6, "SIGRTMIN+6"),
    (Signal::SIGRTMIN_7, "SIGRTMIN+7"),
    (Signal::SIGRTMIN_8, "SIGRTMIN+8"),
    (Signal::SIGRTMIN_9, "SIGRTMIN+9"),
    (Signal::SIGRTMIN_10, "SIGRTMIN+10"),
    (Signal::SIGRTMIN_11, "SIGRTMIN+11"),
    (Signal::SIGRTMIN_12, "SIGRTMIN+12"),
    (Signal::SIGRTMIN_13, "SIGRTMIN+13"),
    (Signal::SIGRTMIN_14, "SIGRTMIN+14"),
    (Signal::SIGRTMAX_14, "SIGRTMAX-14"),
    (Signal::SIGRTMAX_13, "SIGRTMAX-13"),
    (Signal::SIGRTMAX_12, "SIGRTMAX-12"),
    (Signal::SIGRTMAX_11, "SIGRTMAX-11"),
    (Signal::SIGRTMAX_10, "SIGRTMAX-10"),
    (Signal::SIGRTMAX_9, "SIGRTMAX-9"),
    (Signal::SIGRTMAX_8, "SIGRTMAX-8"),
    (Signal::SIGRTMAX_7, "SIGRTMAX-7"),
    (Signal::SIGRTMAX_6, "SIGRTMAX-6"),
    (Signal::SIGRTMAX_5, "SIGRTMAX-5"),
    (Signal::SIGRTMAX_4, "SIGRTMAX-4"),
    (Signal::SIGRTMAX_3, "SIGRTMAX-3"),
    (Signal::SIGRTMAX_2, "SIGRTMAX-2"),
    (Signal::SIGRTMAX_1, "SIGRTMAX-1"),
    (Signal::SIGRTMAX, "SIGRTMAX"),
];

impl Signal {
    /// Returns an iterator over the signals that have names, in number order, as `kill -l`
    /// lists them.
    pub fn iterator() -> impl Iterator<Item = Self> {
        #[cfg(target_arch = "wasm32")]
        return SIGNALS
            .into_iter()
            .filter(|(signal, _)| !matches!(*signal as i32, 32..=34))
            .map(|(signal, _)| signal);
        #[cfg(not(target_arch = "wasm32"))]
        std::iter::empty()
    }

    /// Converts the signal into its corresponding name as a `&'static str`; the reserved numbers
    /// 32 to 34 have no name and show as their number.
    pub const fn as_str(self) -> &'static str {
        #[cfg(target_arch = "wasm32")]
        return SIGNALS[self as usize - 1].1;
        #[cfg(not(target_arch = "wasm32"))]
        ""
    }

    /// Creates a `Signal` from a name with its `SIG` prefix, such as `SIGTERM`, `SIGRTMIN+3` or
    /// `SIGRTMAX-2`.
    #[allow(
        clippy::should_implement_trait,
        reason = "matches the target-specific Signal API"
    )]
    pub fn from_str(s: &str) -> Result<Self, error::Error> {
        #[cfg(target_arch = "wasm32")]
        {
            if let Some((signal, _)) = SIGNALS
                .into_iter()
                .find(|(signal, name)| *name == s && !matches!(*signal as i32, 32..=34))
            {
                return Ok(signal);
            }
            // As bash does, any offset from either end of the real-time range names a signal.
            let offset = |rest: &str| rest.parse::<i32>().ok();
            let number = if let Some(rest) = s.strip_prefix("SIGRTMIN+") {
                offset(rest).map(|n| 35 + n)
            } else if let Some(rest) = s.strip_prefix("SIGRTMAX-") {
                offset(rest).map(|n| 64 - n)
            } else {
                None
            };
            if let Some(signal) = number
                .filter(|n| (35..=64).contains(n))
                .and_then(|n| Self::try_from(n).ok())
            {
                return Ok(signal);
            }
        }
        Err(error::ErrorKind::InvalidSignal(s.into()).into())
    }
}

impl TryFrom<i32> for Signal {
    type Error = error::Error;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        #[cfg(target_arch = "wasm32")]
        if let Some((signal, _)) = usize::try_from(value)
            .ok()
            .and_then(|index| index.checked_sub(1))
            .and_then(|index| SIGNALS.get(index))
        {
            return Ok(*signal);
        }
        Err(error::ErrorKind::InvalidSignal(std::format!("{value}")).into())
    }
}

pub(crate) fn continue_process(_pid: sys::process::ProcessId) -> Result<(), error::Error> {
    Err(error::ErrorKind::NotSupportedOnThisPlatform("continuing process").into())
}

/// Checks whether a specific process exists and can be signaled.
///
/// This is a stub implementation that returns an error.
pub fn check_signalable(_pid: sys::process::ProcessId) -> Result<(), error::Error> {
    Err(error::ErrorKind::NotSupportedOnThisPlatform("checking process").into())
}

/// Sends a signal to a specific process.
///
/// This is a stub implementation that returns an error.
pub fn kill_process(
    _pid: sys::process::ProcessId,
    _signal: traps::TrapSignal,
) -> Result<(), error::Error> {
    Err(error::ErrorKind::NotSupportedOnThisPlatform("killing process").into())
}

pub(crate) fn lead_new_process_group() -> Result<(), error::Error> {
    Ok(())
}

pub(crate) struct FakeSignal {}

impl FakeSignal {
    fn new() -> Self {
        Self {}
    }

    pub async fn recv(&self) {
        futures::future::pending::<()>().await;
    }
}

pub(crate) fn tstp_signal_listener() -> Result<FakeSignal, error::Error> {
    Ok(FakeSignal::new())
}

pub(crate) fn chld_signal_listener() -> Result<FakeSignal, error::Error> {
    Ok(FakeSignal::new())
}

pub(crate) async fn await_ctrl_c() -> std::io::Result<()> {
    FakeSignal::new().recv().await;
    Ok(())
}

pub(crate) fn mask_sigttou() -> Result<(), error::Error> {
    Ok(())
}

pub(crate) fn poll_for_stopped_children() -> Result<bool, error::Error> {
    Ok(false)
}
