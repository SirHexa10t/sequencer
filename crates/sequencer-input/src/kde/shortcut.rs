//! KWin's global shortcuts, run from outside: what pressing one does, without its keys.
//!
//! KDE's global-shortcut service (`org.kde.kglobalaccel`) keeps an object for each
//! component that registered shortcuts, and `invokeShortcut` on it runs one by name. Plasma
//! 6's KWin hosts that service itself and registers its own actions under the component
//! `kwin` — "Increase Opacity", "Window Close", … — each acting on the focused window, as
//! its keys would.

use super::KWinError;
use super::script::CALL_TIMEOUT;
use crate::dbus::{Connection, Error, Value};

/// KDE's global-shortcut service.
const KGLOBALACCEL: &str = "org.kde.kglobalaccel";
/// KWin's component: the object its own shortcuts are invoked on.
const KWIN_COMPONENT: &str = "/component/kwin";
const COMPONENT_INTERFACE: &str = "org.kde.kglobalaccel.Component";

/// Runs `action`, one of KWin's global shortcuts, as pressing its keys would. `action` is
/// the shortcut's own, untranslated name: "Increase Opacity", not what System Settings
/// shows in another language.
///
/// # Errors
///
/// [`KWinError`]: no session bus, no global-shortcut service on it
/// ([`KWinError::NotRunning`]), or a service that refused the call
/// ([`KWinError::Refused`]).
pub fn invoke_kwin_shortcut(action: &str) -> Result<(), KWinError> {
    invoke_on(Connection::session()?, action)
}

/// Asks the service's owner by name first, as the window calls ask KWin's: a call to a name
/// nobody owns is answered at once by some buses and left to time out by others, and might
/// start a service that was not running.
fn invoke_on(mut bus: Connection, action: &str) -> Result<(), KWinError> {
    let owner = bus.name_owner(KGLOBALACCEL)?.ok_or(KWinError::NotRunning)?;
    let invoked = bus.call(
        &owner,
        KWIN_COMPONENT,
        COMPONENT_INTERFACE,
        "invokeShortcut",
        vec![Value::str(action)],
        CALL_TIMEOUT,
    );
    match invoked {
        Ok(_) => Ok(()),
        Err(Error::Remote { name, message }) => {
            Err(KWinError::Refused(format!("{name}: {message}")))
        }
        Err(other) => Err(other.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::dbus::test_bus::TestBus;
    use crate::dbus::{Kind, Message};

    /// Plays KDE's global-shortcut service while `ask` runs: owns its name, answers each
    /// call with `answer`, and returns what `ask` got and every call that came in.
    fn serving<T: Send>(
        bus: &TestBus,
        answer: impl Fn(&mut Connection, &Message),
        ask: impl FnOnce() -> T + Send,
    ) -> (T, Vec<Message>) {
        let mut service = bus.connect();
        service.request_name(KGLOBALACCEL).unwrap();
        std::thread::scope(|scope| {
            let asking = scope.spawn(ask);
            let mut calls = Vec::new();
            while !asking.is_finished() {
                if let Ok(Some(message)) = service.next_message(Some(Duration::from_millis(50)))
                    && message.kind == Kind::MethodCall
                {
                    answer(&mut service, &message);
                    calls.push(message);
                }
            }
            (asking.join().unwrap(), calls)
        })
    }

    /// The action is invoked by its name on KWin's component, and the service's empty
    /// answer is success.
    #[test]
    fn an_action_is_invoked_by_name_on_kwins_component() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let (invoked, calls) = serving(
            &bus,
            |service, call| service.reply(call, Vec::new()).unwrap(),
            || invoke_on(bus.connect(), "Increase Opacity"),
        );
        invoked.expect("the service answered");
        let [call] = calls.as_slice() else {
            panic!("one call, got {}", calls.len());
        };
        assert_eq!(call.path.as_deref(), Some("/component/kwin"));
        assert_eq!(
            call.interface.as_deref(),
            Some("org.kde.kglobalaccel.Component")
        );
        assert_eq!(call.member.as_deref(), Some("invokeShortcut"));
        assert_eq!(call.body, vec![Value::str("Increase Opacity")]);
    }

    /// A refusal comes back as what the service said.
    #[test]
    fn a_refusal_says_what_the_service_said() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let (invoked, _) = serving(
            &bus,
            |service, call| service.decline(call).unwrap(),
            || invoke_on(bus.connect(), "No Such Action"),
        );
        let err = invoked.unwrap_err();
        assert!(
            matches!(&err, KWinError::Refused(said) if said.contains("UnknownMethod")),
            "{err:?}"
        );
    }

    /// No global-shortcut service on the bus: no KWin to run the shortcut.
    #[test]
    fn without_the_service_kwin_is_not_running() {
        let Some(bus) = TestBus::start() else {
            return;
        };
        let err = invoke_on(bus.connect(), "Increase Opacity").unwrap_err();
        assert!(matches!(err, KWinError::NotRunning), "{err:?}");
    }
}
