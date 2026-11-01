//! The query behind a polled capture watcher: the default input and the
//! list of inputs, asked of a host that is kept between polls.
//!
//! The default input alone is not enough. When the capture falls back to the
//! last physical input (the default input is the virtual device) and that
//! input goes away, the default does not change, but the sound server moves
//! the capture stream to the default source without an error, which would
//! feed the call mix back into itself. A change to the input list is
//! reported as [`DeviceEvent::DeviceListChanged`], so the supervisor checks
//! the identity a build would pick now and refuses the excluded default.
//!
//! The host is kept while it answers. When its connection fails, a fresh one
//! is asked in the same poll, so a reconnect to the same default input reads
//! as no change rather than as a change there and back.

use super::DeviceEvent;
use crate::sink::DeviceIdentity;

/// What a poll saw: the default input and every input, sorted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct InputsSnapshot {
    pub(crate) default: Option<DeviceIdentity>,
    pub(crate) inputs: Vec<DeviceIdentity>,
}

impl InputsSnapshot {
    /// A snapshot of `default` and `inputs` (in any order).
    pub(crate) fn new(default: Option<DeviceIdentity>, mut inputs: Vec<DeviceIdentity>) -> Self {
        inputs.sort();
        InputsSnapshot { default, inputs }
    }
}

/// The event for a change from `previous` to `current`: a new default input
/// (or a first answer that came late, with no previous one) always rebuilds
/// the capture; a list change only makes the supervisor check its identity.
pub(crate) fn change_event(
    previous: Option<&InputsSnapshot>,
    current: &InputsSnapshot,
) -> DeviceEvent {
    match previous {
        Some(previous) if previous.default == current.default => DeviceEvent::DeviceListChanged,
        _ => DeviceEvent::DefaultInputChanged,
    }
}

/// A host the query can ask.
pub(crate) trait InputHost {
    /// Whether to keep this host between polls (the shared host, not a
    /// fallback that should be retried).
    fn keep(&self) -> bool;

    /// The inputs now, or `None` if the host no longer answers.
    fn inputs(&self) -> Option<InputsSnapshot>;
}

/// Asks a kept host for its inputs, getting a new one from `connect` when
/// there is none or its connection failed.
pub(crate) struct InputsQuery<H, F> {
    kept: Option<H>,
    connect: F,
}

impl<H, F> InputsQuery<H, F>
where
    H: InputHost,
    F: FnMut() -> H,
{
    pub(crate) fn new(connect: F) -> Self {
        InputsQuery {
            kept: None,
            connect,
        }
    }

    /// The inputs now. Empty if no host answers.
    pub(crate) fn poll(&mut self) -> InputsSnapshot {
        if let Some(host) = self.kept.take()
            && let Some(snapshot) = host.inputs()
        {
            self.kept = Some(host);
            return snapshot;
        }
        // No kept host, or its connection failed: ask a fresh one now
        // (`connect` reconnects the shared one).
        let host = (self.connect)();
        let snapshot = host.inputs();
        if snapshot.is_some() && host.keep() {
            self.kept = Some(host);
        }
        snapshot.unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watcher::tests::table_event;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn id(name: &str) -> DeviceIdentity {
        DeviceIdentity::new(name.to_string())
    }

    fn snapshot(default: Option<&str>, inputs: &[&str]) -> InputsSnapshot {
        InputsSnapshot::new(
            default.map(id),
            inputs.iter().map(|name| id(name)).collect(),
        )
    }

    /// A host whose answers are shared with the test, so it can be changed
    /// (or made to fail) after the query kept it.
    #[derive(Clone)]
    struct FakeHost {
        keep: bool,
        answer: Rc<RefCell<Option<InputsSnapshot>>>,
    }

    impl InputHost for FakeHost {
        fn keep(&self) -> bool {
            self.keep
        }

        fn inputs(&self) -> Option<InputsSnapshot> {
            self.answer.borrow().clone()
        }
    }

    /// A query whose `connect` hands out `hosts` in order and counts calls.
    fn query(
        hosts: Vec<FakeHost>,
    ) -> (
        InputsQuery<FakeHost, impl FnMut() -> FakeHost>,
        Rc<RefCell<usize>>,
    ) {
        let connects = Rc::new(RefCell::new(0));
        let counter = Rc::clone(&connects);
        let mut hosts = hosts.into_iter();
        let query = InputsQuery::new(move || {
            *counter.borrow_mut() += 1;
            hosts.next().expect("no more fake hosts")
        });
        (query, connects)
    }

    fn host(keep: bool, answer: Option<InputsSnapshot>) -> FakeHost {
        FakeHost {
            keep,
            answer: Rc::new(RefCell::new(answer)),
        }
    }

    #[test]
    fn snapshots_ignore_the_order_of_inputs() {
        assert_eq!(
            snapshot(Some("a"), &["b", "a"]),
            snapshot(Some("a"), &["a", "b"])
        );
    }

    #[test]
    fn a_new_default_rebuilds_and_a_list_change_checks_the_identity() {
        let before = snapshot(Some("mic"), &["mic", "usb"]);
        let default_changed = table_event("linux:default input changed");
        assert_eq!(
            change_event(Some(&before), &snapshot(Some("usb"), &["mic", "usb"])),
            default_changed
        );
        assert_eq!(
            change_event(Some(&before), &snapshot(None, &["mic", "usb"])),
            default_changed
        );
        assert_eq!(
            change_event(Some(&before), &snapshot(Some("mic"), &["mic"])),
            table_event("linux:input list changed")
        );
        // A late first answer has nothing to compare with.
        assert_eq!(change_event(None, &before), default_changed);
    }

    #[test]
    fn a_healthy_shared_host_is_kept_between_polls() {
        let shared = host(true, Some(snapshot(Some("mic"), &["mic"])));
        let (mut query, connects) = query(vec![shared.clone()]);
        assert_eq!(query.poll(), snapshot(Some("mic"), &["mic"]));
        *shared.answer.borrow_mut() = Some(snapshot(Some("usb"), &["mic", "usb"]));
        assert_eq!(query.poll(), snapshot(Some("usb"), &["mic", "usb"]));
        assert_eq!(*connects.borrow(), 1);
    }

    #[test]
    fn a_failed_connection_is_replaced_in_the_same_poll() {
        let first = host(true, Some(snapshot(Some("mic"), &["mic"])));
        let reconnected = host(true, Some(snapshot(Some("mic"), &["mic"])));
        let (mut query, connects) = query(vec![first.clone(), reconnected]);
        assert_eq!(query.poll(), snapshot(Some("mic"), &["mic"]));
        *first.answer.borrow_mut() = None;
        // The same answer as before, not an empty one: no change to report.
        assert_eq!(query.poll(), snapshot(Some("mic"), &["mic"]));
        assert_eq!(*connects.borrow(), 2);
        // The new host is kept.
        assert_eq!(query.poll(), snapshot(Some("mic"), &["mic"]));
        assert_eq!(*connects.borrow(), 2);
    }

    #[test]
    fn a_fallback_host_is_asked_but_not_kept() {
        let fallback = host(false, Some(snapshot(Some("hw"), &["hw"])));
        let shared = host(true, Some(snapshot(Some("mic"), &["mic"])));
        let (mut query, connects) = query(vec![fallback, shared]);
        assert_eq!(query.poll(), snapshot(Some("hw"), &["hw"]));
        assert_eq!(query.poll(), snapshot(Some("mic"), &["mic"]));
        assert_eq!(*connects.borrow(), 2);
    }

    #[test]
    fn no_answering_host_reads_as_no_inputs_and_is_not_kept() {
        let dead = host(true, None);
        let back = host(true, Some(snapshot(Some("mic"), &["mic"])));
        let (mut query, connects) = query(vec![dead, back]);
        assert_eq!(query.poll(), InputsSnapshot::default());
        assert_eq!(query.poll(), snapshot(Some("mic"), &["mic"]));
        assert_eq!(*connects.borrow(), 2);
    }
}
