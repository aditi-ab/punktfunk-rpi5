#![cfg(windows)]
//! Characterization tests for the windows-reactor re-render semantics this client's
//! architecture depends on. These are not tests of the client — they encode the CONTRACT
//! the client's state placement assumes (see the re-render discipline note in `app/mod.rs`),
//! against the same pinned rev, using reactor's own headless harness (`RecordingBackend`).
//! Run them after every SHA bump: a red test here means the discipline note is stale again,
//! in one direction or the other.
//!
//! History that motivates each case:
//! * The June 2026 rev dropped a background `AsyncSetState` write on the floor unless the
//!   owning component's props changed — that bug forced ALL thread-driven state into the
//!   root component. Fixed July 2026 (`dirty: Arc<AtomicBool>` in the engine).
//! * The July 2026 pre-bump rev still pruned a dirty component sitting under an
//!   element-equal non-component wrapper (`border`, `scroll_viewer`) — the reconciler's
//!   dirty lookup was one level deep. Fixed by the 2026-07-29 pin (upstream regression
//!   test: `nested_state_rerender.rs`).
//! * Event handlers wired in the WinUI backend (`add_Click` etc.) were believed to bypass
//!   the render flush, so a sync `use_state` write from one never repainted. The harness
//!   fires events through the same backend attach path; the third test pins the engine
//!   side of that claim.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::thread;

use test_reactor::{Op, RecordingBackend};
use windows_reactor::{
    border, component, vstack, AsyncSetState, ChannelDispatcher, Component, ControlKind,
    Dispatcher, DispatcherQueuePriority, Element, Event, Prop, PropValue, RenderCx, RenderHost,
    SetState, TextBlock, ToggleSwitch,
};

type Job = Box<dyn FnOnce()>;

/// The upstream tests' single-threaded "UI thread": jobs queue until drained.
#[derive(Clone, Default)]
struct TestDispatcher {
    queue: Rc<RefCell<Vec<Job>>>,
}

impl TestDispatcher {
    fn drain(&self) {
        loop {
            let item = {
                let mut q = self.queue.borrow_mut();
                if q.is_empty() {
                    None
                } else {
                    Some(q.remove(0))
                }
            };
            match item {
                Some(f) => f(),
                None => break,
            }
        }
    }
}

impl Dispatcher for TestDispatcher {
    fn enqueue(&self, _p: DispatcherQueuePriority, f: Box<dyn FnOnce()>) -> bool {
        self.queue.borrow_mut().push(f);
        true
    }
}

fn last_text(ops: &[Op]) -> Option<String> {
    ops.iter().rev().find_map(|op| match op {
        Op::SetProp {
            prop: Prop::Text,
            value: PropValue::Str(s),
            ..
        } => Some(s.clone()),
        _ => None,
    })
}

// --- Case 1: sync `use_state` in a child under an element-equal `border` wrapper ---------
// The client wraps every screen in an animated `border` that compares equal once the
// entrance tween settles; `pair.rs` documents a live workaround for this exact topology.

struct SyncInner {
    renders: Rc<Cell<u32>>,
    setter_out: Rc<RefCell<Option<SetState<u64>>>>,
}

impl Component for SyncInner {
    fn render(&self, _props: &(), cx: &mut RenderCx) -> Element {
        let (n, set) = cx.use_state(0_u64);
        *self.setter_out.borrow_mut() = Some(set);
        self.renders.set(self.renders.get() + 1);
        Element::TextBlock(TextBlock::new(format!("sync-{n}")))
    }
}

struct SyncRoot {
    renders: Rc<Cell<u32>>,
    setter_out: Rc<RefCell<Option<SetState<u64>>>>,
}

impl Component for SyncRoot {
    fn render(&self, _props: &(), _cx: &mut RenderCx) -> Element {
        border(component(
            SyncInner {
                renders: Rc::clone(&self.renders),
                setter_out: Rc::clone(&self.setter_out),
            },
            (),
        ))
        .into()
    }
}

#[test]
fn sync_state_child_under_element_equal_border_rerenders() {
    let dispatcher = TestDispatcher::default();
    let setter_out = Rc::new(RefCell::new(None));
    let renders = Rc::new(Cell::new(0));
    let host = RenderHost::new(
        RecordingBackend::new(),
        Box::new(SyncRoot {
            renders: Rc::clone(&renders),
            setter_out: Rc::clone(&setter_out),
        }) as Box<dyn Component>,
        dispatcher.clone(),
    );
    host.kick();
    dispatcher.drain();
    assert_eq!(renders.get(), 1, "inner mounts once");

    setter_out.borrow().as_ref().unwrap().call(1);
    dispatcher.drain();

    assert_eq!(
        renders.get(),
        2,
        "a dirty child under an element-equal border must re-render \
         (the pre-2026-07-29 reconciler pruned at the wrapper)"
    );
    assert_eq!(
        host.with_reconciler(|r| last_text(&r.backend.ops)),
        Some("sync-1".to_string())
    );
}

// --- Case 2: background `AsyncSetState` write into a child with unchanged props ----------
// THE case the root-hoisting discipline was built around. MEASURED BROKEN at the 2026-07-29
// pin, for a new reason: every component's `RenderCx` draws a fresh `HostId`
// (`RenderCx::new` → `HostId::next()`), but rerender callbacks are only registered for the
// ROOT's id (`RenderHost::set_marshaller` / the app bootstrap). A child-owned async write
// stores the value and sets the dirty flag, then calls
// `request_ui_rerender_on_ui_thread(child_host_id)` — a registry miss, silently dropped.
// The new value appears whenever anything ELSE triggers a pass. (Upstream's
// `async_setter_marks_owning_component_dirty_for_rerender` installs the guard for the very
// cx it tests, so it never sees this.) The test asserts the BROKEN behaviour on purpose:
// when a future bump fixes it upstream, this fails loudly and the root-hoisting rule for
// thread-driven state in `app/mod.rs` can finally be relaxed.

struct AsyncInner {
    renders: Rc<Cell<u32>>,
    setter_out: Rc<RefCell<Option<AsyncSetState<u64>>>>,
}

impl Component for AsyncInner {
    fn render(&self, _props: &(), cx: &mut RenderCx) -> Element {
        let (n, set) = cx.use_async_state(0_u64);
        *self.setter_out.borrow_mut() = Some(set);
        self.renders.set(self.renders.get() + 1);
        Element::TextBlock(TextBlock::new(format!("async-{n}")))
    }
}

struct AsyncRoot {
    renders: Rc<Cell<u32>>,
    root_renders: Rc<Cell<u32>>,
    setter_out: Rc<RefCell<Option<AsyncSetState<u64>>>>,
}

impl Component for AsyncRoot {
    fn render(&self, _props: &(), _cx: &mut RenderCx) -> Element {
        self.root_renders.set(self.root_renders.get() + 1);
        border(component(
            AsyncInner {
                renders: Rc::clone(&self.renders),
                setter_out: Rc::clone(&self.setter_out),
            },
            (),
        ))
        .into()
    }
}

#[test]
fn async_state_child_under_element_equal_border_rerenders() {
    let dispatcher = TestDispatcher::default();
    let channel = ChannelDispatcher::new();
    let setter_out: Rc<RefCell<Option<AsyncSetState<u64>>>> = Rc::new(RefCell::new(None));
    let renders = Rc::new(Cell::new(0));
    let root_renders = Rc::new(Cell::new(0));
    let host = RenderHost::new(
        RecordingBackend::new(),
        Box::new(AsyncRoot {
            renders: Rc::clone(&renders),
            root_renders: Rc::clone(&root_renders),
            setter_out: Rc::clone(&setter_out),
        }) as Box<dyn Component>,
        dispatcher.clone(),
    );
    host.set_marshaller(Some(channel.marshaller()));
    host.kick();
    dispatcher.drain();
    assert_eq!(renders.get(), 1, "inner mounts once");

    // The write happens OFF the UI thread, exactly like discovery/HUD/speed-test threads.
    let set = setter_out.borrow().as_ref().unwrap().clone();
    thread::spawn(move || set.call(1)).join().unwrap();

    // Marshal back onto the "UI thread", then run the render it requested.
    channel.drain();
    dispatcher.drain();

    assert_eq!(
        root_renders.get(),
        1,
        "CONTRACT CHANGED: a child-owned async write now triggers a render pass — the \
         host-id registry miss was fixed upstream. Thread-driven state may move out of \
         root(); update the discipline note in app/mod.rs and this test"
    );
    assert_eq!(
        renders.get(),
        1,
        "CONTRACT CHANGED: the child re-rendered from its own async write — see above"
    );

    // The write itself was NOT lost: it lands on the next pass someone else triggers.
    // (This is exactly the 'appears after unrelated navigation' failure mode the client
    // keeps paying for, and why thread-driven state lives in root().)
    host.kick();
    dispatcher.drain();
    assert_eq!(renders.get(), 2);
    assert_eq!(
        host.with_reconciler(|r| last_text(&r.backend.ops)),
        Some("async-1".to_string()),
        "the buffered write must surface on the next externally-triggered pass"
    );
}

// --- Case 3: sync `use_state` written from a backend-fired event handler -----------------
// The client hoists `forget`/`rename`/`hover` to root because WinUI-wired handlers
// (MenuFlyout `add_Click`, ComboBox selection, pointer enter/exit) mark dirty without ever
// pumping the reconciler. This test PASSES — the ENGINE honours a handler-driven sync
// write — but the claim it probes is about the REAL WinUI backend, and there it was
// re-confirmed live on 2026-07-29: a componentized settings page with local sync scope
// state did not repaint on a real ComboBox pick (see the discipline note in app/mod.rs;
// the de-hoist was reverted). Keep this green as the ENGINE contract; do NOT read it as
// permission to de-hoist event-driven state until a live UIA check passes too.

struct ToggleInner {
    renders: Rc<Cell<u32>>,
}

impl Component for ToggleInner {
    fn render(&self, _props: &(), cx: &mut RenderCx) -> Element {
        let (n, set) = cx.use_state(0_u64);
        self.renders.set(self.renders.get() + 1);
        vstack((
            TextBlock::new(format!("evt-{n}")),
            ToggleSwitch::new(false).on_toggled(move |_| set.call(n + 1)),
        ))
        .into()
    }
}

struct ToggleRoot {
    renders: Rc<Cell<u32>>,
}

impl Component for ToggleRoot {
    fn render(&self, _props: &(), _cx: &mut RenderCx) -> Element {
        border(component(
            ToggleInner {
                renders: Rc::clone(&self.renders),
            },
            (),
        ))
        .into()
    }
}

#[test]
fn sync_state_from_backend_fired_event_rerenders() {
    let dispatcher = TestDispatcher::default();
    let renders = Rc::new(Cell::new(0));
    let host = RenderHost::new(
        RecordingBackend::new(),
        Box::new(ToggleRoot {
            renders: Rc::clone(&renders),
        }) as Box<dyn Component>,
        dispatcher.clone(),
    );
    host.kick();
    dispatcher.drain();
    assert_eq!(renders.get(), 1);

    let toggle_id = host.with_reconciler(|r| {
        r.backend
            .ops
            .iter()
            .find_map(|op| match op {
                Op::Create {
                    id,
                    kind: ControlKind::ToggleSwitch,
                } => Some(*id),
                _ => None,
            })
            .expect("the toggle was created")
    });
    host.with_reconciler(|r| r.backend.fire_bool(toggle_id, Event::Toggled, true));
    dispatcher.drain();

    assert_eq!(
        renders.get(),
        2,
        "a sync use_state write from a backend-fired handler must schedule and run a \
         re-render — if this fails, the engine really does drop handler-driven dirt, and \
         the root-hoisting of forget/rename/hover stays justified at the engine level"
    );
    assert_eq!(
        host.with_reconciler(|r| last_text(&r.backend.ops)),
        Some("evt-1".to_string())
    );
}
