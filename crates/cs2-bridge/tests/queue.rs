//! `cs2_bridge::queue` against the C it ports.
//!
//! Every expectation here is a line of `bridge/mcp_bridge.c`: `q_push`/`q_pop`/`q_purge_gen`/
//! `q_remove_id` and `out_push`/`out_space`/`out_pop_blocking` (`:1406-1528`), the deadline
//! sweep (`:1587-1608`), `drain` (`:6289-6317`), `flush_outbound` (`:6323-6334`) and the
//! `build_error`/`build_error_ext` envelope (`:1563-1583`).
//!
//! The queue is a plain `std` structure, so every test drives it on its own [`Queues`] instance
//! (the C's file statics cannot be instantiated, which is why [`Queues::new`] exists); the
//! process-wide [`global`] is only checked for identity.

use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use cs2_bridge::queue::{
    error_reply, global, Queues, Request, BRIDGE_BUILD, INTERNAL_ERROR_CODE, OUT_CAP,
    QUEUE_TIMEOUT_CODE, QCAP,
};

/// A request with the C's default deadline (`DEFAULT_DEADLINE_MS = 30000`, `:129`); the queue
/// itself only sees `Option<Duration>` ("explicit 0 = no deadline", `:6654` maps to `None`).
fn request(id: &str, gen: u32) -> Request {
    Request {
        id: id.to_string(),
        method: "pet.text".to_string(),
        params: None,
        received: Instant::now(),
        deadline: Some(Duration::from_secs(30)),
        gen,
    }
}

/// A request whose deadline already passed: accepted 20 ms ago with a 10 ms deadline.
fn expired_request(id: &str, gen: u32) -> Request {
    Request {
        received: Instant::now()
            .checked_sub(Duration::from_millis(20))
            .expect("the host clock can name 20 ms ago"),
        deadline: Some(Duration::from_millis(10)),
        ..request(id, gen)
    }
}

/// Drains the outbound queue without waiting (the writer's call with a zero timeout).
fn pop_all(queues: &Queues) -> Vec<(String, u32)> {
    let mut lines = Vec::new();
    while let Some(line) = queues.out_pop_blocking(Duration::ZERO) {
        lines.push(line);
    }
    lines
}

/// The executor's line in the ordering tests: the request id, so the output order is the
/// execution order.
fn echo(request: &Request) -> String {
    format!("ran {}", request.id)
}

#[test]
fn capacities_are_the_c_defines() {
    // mcp_bridge.c:125-126.
    assert_eq!(QCAP, 64);
    assert_eq!(OUT_CAP, 64);
    // mcp_bridge.c:127; every error envelope below embeds it. The Rust bridge is m3.N, one N
    // per merged M4 group (the hosts accept any m3.x).
    let minor = BRIDGE_BUILD.strip_prefix("m3.").expect("an m3.x build");
    assert!(minor.parse::<u32>().is_ok(), "{BRIDGE_BUILD}");
}

#[test]
fn requests_run_in_fifo_order() {
    // q_push appends at `g_qTail`, q_pop takes `g_qHead` (`:1436-1456`).
    let queues = Queues::new();
    for id in ["a", "b", "c"] {
        queues.push(request(id, 1)).unwrap();
    }
    queues.drain(&mut echo);

    let lines = pop_all(&queues);
    assert_eq!(
        lines,
        vec![
            ("ran a".to_string(), 1),
            ("ran b".to_string(), 1),
            ("ran c".to_string(), 1),
        ]
    );
    assert_eq!(queues.depth(), 0);
}

#[test]
fn requests_execute_only_on_the_thread_calling_drain() {
    let queues = Queues::new();
    let main_thread = thread::current().id();

    // The IPC thread's `q_push` only enqueues (`:6662`); it never executes.
    thread::scope(|scope| {
        let queues = &queues;
        scope.spawn(move || {
            queues.push(request("from-ipc", 2)).unwrap();
            // Pushing must not have executed it on this thread.
            assert_eq!(queues.depth(), 1);
        });
    });
    queues.push(request("from-main", 2)).unwrap();

    let mut executed_on = Vec::new();
    let mut inflight_seen = Vec::new();
    queues.drain(&mut |request| {
        executed_on.push(thread::current().id());
        inflight_seen.push(queues.inflight_since().is_some());
        echo(request)
    });

    assert_eq!(executed_on, vec![main_thread, main_thread]);
    // g_inflightStart is set around exec_request (`:6309-6311`).
    assert_eq!(inflight_seen, vec![true, true]);
    assert_eq!(queues.inflight_since(), None);
}

#[test]
fn drain_executes_at_most_four_requests_per_call() {
    // `for (int n = 0; n < 4; n++)` (`:6294`).
    let queues = Queues::new();
    for id in ["a", "b", "c", "d", "e"] {
        queues.push(request(id, 3)).unwrap();
    }

    queues.drain(&mut echo);
    assert_eq!(
        pop_all(&queues)
            .into_iter()
            .map(|(line, _)| line)
            .collect::<Vec<_>>(),
        vec!["ran a", "ran b", "ran c", "ran d"]
    );
    assert_eq!(queues.depth(), 1);

    queues.drain(&mut echo);
    assert_eq!(
        pop_all(&queues)
            .into_iter()
            .map(|(line, _)| line)
            .collect::<Vec<_>>(),
        vec!["ran e"]
    );
}

#[test]
fn the_eight_millisecond_budget_ends_the_drain_after_a_slow_executor() {
    // `if (GetTickCount64() - t0 > 8) break;` (`:6314`).
    let queues = Queues::new();
    for id in ["slow", "after-1", "after-2"] {
        queues.push(request(id, 4)).unwrap();
    }

    let mut calls = 0;
    queues.drain(&mut |request| {
        calls += 1;
        thread::sleep(Duration::from_millis(20));
        echo(request)
    });

    assert_eq!(calls, 1, "the 8 ms budget must stop after the slow request");
    assert_eq!(queues.depth(), 2);
    assert_eq!(pop_all(&queues).len(), 1);
}

#[test]
fn a_reentrant_drain_runs_nothing() {
    // `static volatile int draining = 0; if (draining) return;` (`:6290-6292`) - a modal loop
    // inside the executor calls the tick again.
    let queues = Queues::new();
    queues.push(request("outer", 5)).unwrap();
    queues.push(request("inner", 5)).unwrap();

    // Recorded inside the executors, asserted after the outer drain returns: an assert inside
    // an executor is swallowed by drain's catch_unwind.
    let mut nested_calls = 0;
    let mut calls = Vec::new();
    let queues_ref = &queues;
    queues.drain(&mut |request| {
        calls.push(request.id.clone());
        if request.id == "outer" {
            queues_ref.drain(&mut |_nested| {
                nested_calls += 1;
                "nested must not run".to_string()
            });
        }
        echo(request)
    });

    assert_eq!(
        calls,
        vec!["outer", "inner"],
        "the nested drain executed nothing and the outer drain continued"
    );
    assert_eq!(nested_calls, 0);
    assert_eq!(queues.depth(), 0, "the nested drain consumed nothing the outer drain missed");
    assert_eq!(
        pop_all(&queues)
            .into_iter()
            .map(|(line, _)| line)
            .collect::<Vec<_>>(),
        vec!["ran outer", "ran inner"]
    );
}

#[test]
fn drain_runs_nothing_when_the_outbound_queue_is_full() {
    // `if (!out_space()) break;  /* P0-6: never execute without room to answer */` (`:6295`).
    let queues = Queues::new();
    for n in 0..OUT_CAP {
        queues.out_push(format!("pending {n}"), 6).unwrap();
    }
    queues.push(request("waiting", 6)).unwrap();

    let mut calls = 0;
    queues.drain(&mut |request| {
        calls += 1;
        echo(request)
    });
    assert_eq!(calls, 0);
    assert_eq!(queues.depth(), 1, "the request stays queued");

    // One slot frees up (the writer popped one) and the next drain runs it.
    assert!(queues.out_pop_blocking(Duration::ZERO).is_some());
    queues.drain(&mut echo);
    assert_eq!(queues.depth(), 0);
    assert_eq!(
        pop_all(&queues).last().map(|(line, gen)| (line.as_str(), *gen)),
        Some(("ran waiting", 6))
    );
}

#[test]
fn an_expired_request_is_answered_queue_timeout_without_running() {
    // `build_error(&b, 1, r.id, -32008, "QueueTimeout")` in drain (`:6299-6307`).
    let queues = Queues::new();
    queues.push(expired_request("old", 7)).unwrap();

    let mut calls = 0;
    queues.drain(&mut |request| {
        calls += 1;
        echo(request)
    });

    assert_eq!(calls, 0, "an expired request is never executed");
    assert_eq!(queues.depth(), 0);
    // The queue's own envelope, spelled out byte for byte (build_error, `:1563-1571`).
    assert_eq!(
        pop_all(&queues),
        vec![(
            "{\"jsonrpc\":\"2.0\",\"id\":\"old\",\"bridge\":{\"protocol\":1,\"build\":\"m3.0\"},\"error\":{\"code\":-32008,\"message\":\"QueueTimeout\"}}\n".replace("m3.0", BRIDGE_BUILD)
                .to_string(),
            7
        )]
    );
}

#[test]
fn purge_gen_drops_only_that_generation() {
    // q_purge_gen (`:1458-1475`): pop every head, keep the records whose gen differs.
    let queues = Queues::new();
    queues.push(request("a", 7)).unwrap();
    queues.push(request("b", 8)).unwrap();
    queues.push(request("c", 7)).unwrap();
    queues.push(request("d", 9)).unwrap();

    queues.purge_gen(7);

    assert_eq!(queues.depth(), 2);
    queues.drain(&mut echo);
    assert_eq!(
        pop_all(&queues),
        vec![("ran b".to_string(), 8), ("ran d".to_string(), 9)]
    );
}

#[test]
fn remove_id_removes_the_first_match_only() {
    // q_remove_id (`:1478-1497`): `if (!found && seq(r.id, id))`, i.e. the first match only.
    let queues = Queues::new();
    queues.push(request("a", 1)).unwrap();
    queues.push(request("b", 1)).unwrap();
    queues.push(request("a", 1)).unwrap();

    assert!(queues.remove_id("b"));
    assert_eq!(queues.depth(), 2);
    queues.drain(&mut echo);
    assert_eq!(
        pop_all(&queues)
            .into_iter()
            .map(|(line, _)| line)
            .collect::<Vec<_>>(),
        vec!["ran a", "ran a"]
    );
    assert!(!queues.remove_id("b"), "already removed");
}

#[test]
fn sweep_expired_answers_the_head_and_stops_at_a_live_request() {
    // q_sweep_expired (`:1587-1608`) only looks at the head and breaks on the first live one.
    let queues = Queues::new();
    queues.push(expired_request("expired-head", 10)).unwrap();
    queues.push(request("live", 10)).unwrap();
    queues.push(expired_request("expired-behind-live", 10)).unwrap();

    queues.sweep_expired();

    assert_eq!(
        pop_all(&queues),
        vec![(
            "{\"jsonrpc\":\"2.0\",\"id\":\"expired-head\",\"bridge\":{\"protocol\":1,\"build\":\"m3.0\"},\"error\":{\"code\":-32008,\"message\":\"QueueTimeout\"}}\n".replace("m3.0", BRIDGE_BUILD)
                .to_string(),
            10
        )]
    );
    assert_eq!(
        queues.depth(),
        2,
        "the sweep stops at the live request; the expired one behind it waits for the drain"
    );
}

#[test]
fn sweep_expired_answers_consecutive_expired_heads_in_order() {
    let queues = Queues::new();
    queues.push(expired_request("one", 11)).unwrap();
    queues.push(expired_request("two", 11)).unwrap();
    queues.push(request("live", 11)).unwrap();

    queues.sweep_expired();

    let lines = pop_all(&queues);
    assert_eq!(lines.len(), 2);
    assert_eq!(
        lines[0].0,
        error_reply(Some("one"), QUEUE_TIMEOUT_CODE, "QueueTimeout", None)
    );
    assert_eq!(
        lines[1].0,
        error_reply(Some("two"), QUEUE_TIMEOUT_CODE, "QueueTimeout", None)
    );
    assert_eq!(queues.depth(), 1);
}

#[test]
fn snapshot_reports_each_queued_request_head_first_under_the_lock() {
    // control.status's queue loop (`mcp_bridge.c:6533-6540`): head first, `id`/`method` as
    // JSON strings, `ageMs` from `receivedAt` against one `now` under `g_qcs`.
    let queues = Queues::new();
    queues.push(expired_request("old", 1)).unwrap();
    queues.push(request("fresh", 2)).unwrap();
    queues.push(request("gone", 3)).unwrap();
    assert!(queues.remove_id("gone"));

    let snapshot = queues.snapshot();
    assert_eq!(
        snapshot.iter().map(|queued| queued.id.as_str()).collect::<Vec<_>>(),
        ["old", "fresh"],
        "head-first order, and the removed id is gone"
    );
    assert_eq!(snapshot[0].method, "pet.text");
    // `old` was accepted 20 ms ago; its ageMs measures from `received`, not from the call.
    assert!(
        snapshot[0].age_ms >= 15,
        "ageMs must measure from receivedAt, got {}",
        snapshot[0].age_ms
    );
    assert!(
        snapshot[1].age_ms < 5000,
        "a just-queued request has a small ageMs, got {}",
        snapshot[1].age_ms
    );

    // Drained: the listing is empty again.
    queues.drain(&mut echo);
    assert!(queues.snapshot().is_empty());
}

#[test]
fn a_zero_deadline_is_no_deadline_like_the_c() {
    // `if (h->deadlineMs && ...)` (`mcp_bridge.c:1593`) and `if (r.deadlineMs)` (`:6299`):
    // deadlineMs == 0 means "no deadline", not "already expired".
    let zero_deadline = |id: &str, gen: u32| Request {
        received: Instant::now()
            .checked_sub(Duration::from_millis(20))
            .expect("the host clock can name 20 ms ago"),
        deadline: Some(Duration::ZERO),
        ..request(id, gen)
    };

    // The drain executes a zero-deadline request however long ago it was received...
    let queues = Queues::new();
    queues.push(zero_deadline("zero", 1)).unwrap();
    let mut calls = Vec::new();
    queues.drain(&mut |req| {
        calls.push(req.id.clone());
        echo(req)
    });
    assert_eq!(calls, vec!["zero"], "a zero deadline must not expire");
    assert_eq!(pop_all(&queues).len(), 1);

    // ...and the sweep leaves one queued too (`h->deadlineMs` is falsy, `:1593`).
    queues.push(zero_deadline("sweep-zero", 2)).unwrap();
    queues.sweep_expired();
    assert_eq!(
        queues.depth(),
        1,
        "the sweep must not take a zero-deadline request"
    );
    assert_eq!(pop_all(&queues).len(), 0);
}

#[test]
fn queue_full_is_an_error_at_exactly_its_capacity() {
    // q_push: `if (g_qCount >= QCAP) ... return 0;` (`:1438`).
    let queues = Queues::new();
    for n in 0..QCAP {
        queues.push(request(&format!("q{n}"), 1)).unwrap();
    }
    assert!(queues.push(request("one-too-many", 1)).is_err());
    assert_eq!(queues.depth(), QCAP);

    // out_push: `if (g_oCount >= OUT_CAP) ... return 0;` (`:1506`).
    let queues = Queues::new();
    for n in 0..OUT_CAP {
        queues.out_push(format!("line {n}"), 1).unwrap();
    }
    assert!(queues.out_push("one-too-many".to_string(), 1).is_err());
    assert!(!queues.out_space());
    assert!(queues.out_pop_blocking(Duration::ZERO).is_some());
    assert!(queues.out_space(), "one pop frees one slot");
}

#[test]
fn shutdown_wakes_a_thread_blocked_in_out_pop_blocking() {
    let queues = Arc::new(Queues::new());
    let (started, started_rx) = mpsc::channel();
    let (done, done_rx) = mpsc::channel();
    let worker = {
        let queues = Arc::clone(&queues);
        thread::spawn(move || {
            started.send(()).unwrap();
            done.send(queues.out_pop_blocking(Duration::from_secs(60)))
                .unwrap();
        })
    };

    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // The worker may still be between the signal and the wait; the 60 s deadline means a
    // return inside the 5 s below can only come from `shutdown`.
    thread::sleep(Duration::from_millis(100));
    assert!(
        done_rx.try_recv().is_err(),
        "out_pop_blocking returned before shutdown"
    );

    queues.shutdown();
    let result = done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("shutdown did not wake out_pop_blocking");
    // The recv_timeout bound above is the wake deadline; only `None` matters here.
    assert!(result.is_none(), "nothing was queued");
    worker.join().unwrap();
}

#[test]
fn shutdown_wakes_a_thread_blocked_in_flush_outbound_and_keeps_the_record() {
    let queues = Arc::new(Queues::new());
    queues.out_push("reply for a dead generation".to_string(), 12).unwrap();

    let (started, started_rx) = mpsc::channel();
    let (done, done_rx) = mpsc::channel();
    let worker = {
        let queues = Arc::clone(&queues);
        thread::spawn(move || {
            started.send(()).unwrap();
            queues.flush_outbound(12, Duration::from_secs(60));
            done.send(()).unwrap();
        })
    };

    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    thread::sleep(Duration::from_millis(100));
    assert!(
        done_rx.try_recv().is_err(),
        "flush_outbound returned before shutdown"
    );

    queues.shutdown();
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("shutdown did not wake flush_outbound");
    worker.join().unwrap();

    // The C only waits for the writer to deliver; it never consumes the record itself.
    assert_eq!(
        queues.out_pop_blocking(Duration::ZERO),
        Some(("reply for a dead generation".to_string(), 12))
    );
}

/// The WP2.B round-2 wake test: one `out_push` must reach the writer waiting in
/// `out_pop_blocking` however many waiter classes `out_changed` has - here a `flush_outbound`
/// caller is parked on the same condvar for most of the choreography, which is why the push
/// notifies **all** waiters (`notify_one` can pick one waiter, and a reply must never wait
/// behind a flush waiter).
///
/// The fully simultaneous state the review describes - the writer parked *and* a flusher
/// parked - is unreachable, and the choreography below shows why: a parked writer means the
/// outbound queue was empty at its last check, a parked flusher means a record of its
/// generation was pending at its last check, and every pop notifies all, so the flusher
/// returns as soon as the writer pops its generation's last record. `notify_all` on the push
/// is kept because it is the only choice that stays correct if a waiter class is ever added
/// whose park does not require a non-empty queue.
#[test]
fn out_push_wakes_the_blocked_writer_even_with_a_flush_outbound_waiter_class() {
    let queues = Arc::new(Queues::new());
    // Two records of the flusher's generation: the flusher stays parked through the writer's
    // first pop and returns only when the second one retires the generation.
    queues.out_push("flush record 1".to_string(), 16).unwrap();
    queues.out_push("flush record 2".to_string(), 16).unwrap();

    let (flush_started, flush_started_rx) = mpsc::channel();
    let (flushed, flushed_rx) = mpsc::channel();
    let flusher = {
        let queues = Arc::clone(&queues);
        thread::spawn(move || {
            flush_started.send(()).unwrap();
            queues.flush_outbound(16, Duration::from_secs(60));
            flushed.send(()).unwrap();
        })
    };
    flush_started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // The flusher may still be between the signal and the wait; with both records pending it
    // can only come back inside the bounds below via a pop of its generation.
    thread::sleep(Duration::from_millis(100));
    assert!(
        flushed_rx.try_recv().is_err(),
        "flush_outbound returned while records of its generation are pending"
    );

    // The writer: pop until the queue is empty, i.e. until it is the parked
    // `out_pop_blocking(60 s)` waiter; the `shutdown` below ends its loop.
    let (popped, popped_rx) = mpsc::channel();
    let writer = {
        let queues = Arc::clone(&queues);
        thread::spawn(move || {
            while let Some(record) = queues.out_pop_blocking(Duration::from_secs(60)) {
                popped.send(record).unwrap();
            }
        })
    };

    assert_eq!(
        popped_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the writer did not pop the flusher's first record"),
        ("flush record 1".to_string(), 16)
    );
    assert_eq!(
        popped_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the writer did not pop the flusher's second record"),
        ("flush record 2".to_string(), 16)
    );
    // The second pop retired the flusher's generation - the event it waits on.
    flushed_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("flush_outbound did not return when the writer popped its generation's last record");
    flusher.join().unwrap();

    // The writer is now the parked `out_pop_blocking(60 s)` waiter on the empty queue. The
    // one reply push must wake it inside the bound - not after its own 60 s timeout.
    queues.out_push("the reply".to_string(), 15).unwrap();
    assert_eq!(
        popped_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("out_push did not wake the parked writer"),
        ("the reply".to_string(), 15)
    );
    queues.shutdown();
    writer.join().unwrap();
}

/// An executor that panics must not escape `drain`, must answer an error object, and must not
/// stop the batch: the next request still runs. This is the WP2.B negative test.
#[test]
fn a_panicking_executor_is_answered_and_the_next_request_runs() {
    let queues = Queues::new();
    queues.push(request("boom", 13)).unwrap();
    queues.push(request("next", 13)).unwrap();

    let mut calls = 0;
    queues.drain(&mut |request| {
        calls += 1;
        if request.id == "boom" {
            panic!("the executor under test panics");
        }
        echo(request)
    });

    assert_eq!(calls, 2, "the drain continued after the panic");
    assert_eq!(queues.inflight_since(), None);
    let lines = pop_all(&queues);
    assert_eq!(lines.len(), 2);
    assert_eq!(
        lines[0].0,
        error_reply(
            Some("boom"),
            INTERNAL_ERROR_CODE,
            "InternalError",
            Some("{\"reason\":\"exec panicked\"}")
        )
    );
    assert_eq!(lines[0].0, "{\"jsonrpc\":\"2.0\",\"id\":\"boom\",\"bridge\":{\"protocol\":1,\"build\":\"m3.0\"},\"error\":{\"code\":-32603,\"message\":\"InternalError\",\"data\":{\"reason\":\"exec panicked\"}}}\n".replace("m3.0", BRIDGE_BUILD));
    assert_eq!(lines[1].0, "ran next");
}

#[test]
fn error_reply_is_byte_equal_to_build_error_and_build_error_ext() {
    // build_error (`:1563-1571`): hasId -> sb_addjson(id), else the literal `null`; then
    // `,"bridge":{"protocol":1,"build":"<BRIDGE_BUILD>"},"error":{"code":<int>,"message":<json>}}\n`.
    assert_eq!(
        error_reply(Some("req-1"), -32601, "MethodNotFound", None),
        "{\"jsonrpc\":\"2.0\",\"id\":\"req-1\",\"bridge\":{\"protocol\":1,\"build\":\"m3.0\"},\"error\":{\"code\":-32601,\"message\":\"MethodNotFound\"}}\n".replace("m3.0", BRIDGE_BUILD)
    );
    assert_eq!(
        error_reply(None, -32600, "invalid request: id required", None),
        "{\"jsonrpc\":\"2.0\",\"id\":null,\"bridge\":{\"protocol\":1,\"build\":\"m3.0\"},\"error\":{\"code\":-32600,\"message\":\"invalid request: id required\"}}\n".replace("m3.0", BRIDGE_BUILD)
    );
    // build_error_ext (`:1572-1581`): `if (dataJson) { ,"data": <raw> }` before the closing `}}`.
    assert_eq!(
        error_reply(Some("x"), -32011, "NotImplemented", Some("{\"blockedOn\":\"pet\"}")),
        "{\"jsonrpc\":\"2.0\",\"id\":\"x\",\"bridge\":{\"protocol\":1,\"build\":\"m3.0\"},\"error\":{\"code\":-32011,\"message\":\"NotImplemented\",\"data\":{\"blockedOn\":\"pet\"}}}\n".replace("m3.0", BRIDGE_BUILD)
    );
    // sb_addjson (`:289-319`) escapes the id and the message the same way.
    assert_eq!(
        error_reply(Some("a\"b\\c\nd"), -32008, "QueueTimeout", None),
        "{\"jsonrpc\":\"2.0\",\"id\":\"a\\\"b\\\\c\\nd\",\"bridge\":{\"protocol\":1,\"build\":\"m3.0\"},\"error\":{\"code\":-32008,\"message\":\"QueueTimeout\"}}\n".replace("m3.0", BRIDGE_BUILD)
    );
}

#[test]
fn the_process_wide_queue_is_the_one_instance_the_lanes_share() {
    // The C's `g_q`/`g_out` are file statics (`:1411-1421`): one instance per process, and
    // `global()` hands out that same live state every call - not a fresh pair. Only this test
    // touches `global()`, so the round-trip below is deterministic even under the parallel
    // harness.
    let fresh = Queues::new();
    assert!(
        !std::ptr::eq(global(), &fresh),
        "global() must not hand out a fresh pair"
    );
    global().push(request("shared", 1)).unwrap();
    assert_eq!(global().depth(), 1, "the process-wide instance carries state");
    assert!(global().remove_id("shared"));
    assert_eq!(global().depth(), 0);
    assert!(!global().is_shutting_down());
}

#[test]
fn out_push_wakes_a_thread_blocked_in_out_pop_blocking() {
    // The writer's steady state (`:1545`): parked in out_pop_blocking, one push, one record.
    let queues = Arc::new(Queues::new());
    let (started, started_rx) = mpsc::channel();
    let (done, done_rx) = mpsc::channel();
    let worker = {
        let queues = Arc::clone(&queues);
        thread::spawn(move || {
            started.send(()).unwrap();
            done.send(queues.out_pop_blocking(Duration::from_secs(60)))
                .unwrap();
        })
    };

    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    // The worker may still be between the signal and the wait; the 60 s deadline means a
    // return inside the 5 s bound below can only come from the push.
    thread::sleep(Duration::from_millis(100));
    assert!(
        done_rx.try_recv().is_err(),
        "out_pop_blocking returned before the push"
    );

    queues.out_push("steady-state reply".to_string(), 14).unwrap();
    let popped = done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("out_push did not wake the parked writer");
    assert_eq!(popped, Some(("steady-state reply".to_string(), 14)));
    worker.join().unwrap();
}

#[test]
fn flush_outbound_returns_when_the_writer_pops_the_generations_last_record() {
    // flush_outbound's purpose (`mcp_bridge.c:6323-6334`): return once no record of the gen
    // is pending. Another generation's record must not hold it, and it must not consume one.
    let queues = Arc::new(Queues::new());
    queues.out_push("gen 12 first".to_string(), 12).unwrap();
    queues.out_push("gen 12 second".to_string(), 12).unwrap();
    queues.out_push("gen 13 stays".to_string(), 13).unwrap();

    let (done, done_rx) = mpsc::channel();
    let flusher = {
        let queues = Arc::clone(&queues);
        thread::spawn(move || {
            queues.flush_outbound(12, Duration::from_secs(60));
            done.send(()).unwrap();
        })
    };
    // Two gen-12 records are pending; a return inside the bounds below can only be a
    // retirement of that generation.
    thread::sleep(Duration::from_millis(100));
    assert!(
        done_rx.try_recv().is_err(),
        "flush_outbound returned while gen-12 records are pending"
    );

    // The writer pops one at a time...
    assert!(queues.out_pop_blocking(Duration::ZERO).is_some());
    thread::sleep(Duration::from_millis(100));
    assert!(
        done_rx.try_recv().is_err(),
        "one gen-12 record is still pending"
    );

    // ...and the second pop retires gen 12; the gen-13 record is not the flusher's.
    assert!(queues.out_pop_blocking(Duration::ZERO).is_some());
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("flush_outbound did not return after its generation's last record was popped");
    assert_eq!(
        queues.out_pop_blocking(Duration::ZERO),
        Some(("gen 13 stays".to_string(), 13)),
        "flush_outbound must not have consumed another generation's record"
    );
    flusher.join().unwrap();
}

#[test]
fn out_pop_blocking_times_out_after_the_given_timeout() {
    // The C's SleepConditionVariableCS with a timeout (`:1522-1526`); deviation (1) in the
    // module docs: one timeout window, then None - not a re-armed poll.
    let queues = Queues::new();
    let started = Instant::now();
    assert_eq!(queues.out_pop_blocking(Duration::from_millis(50)), None);
    assert!(
        started.elapsed() >= Duration::from_millis(50),
        "out_pop_blocking returned before its timeout: {:?}",
        started.elapsed()
    );
}

#[test]
fn another_threads_drain_runs_nothing_while_a_drain_is_executing() {
    // The re-entrancy guard is per queue pair, not per thread (`static volatile int draining`,
    // `:6290-6292`): the modal-loop case is a nested call on one thread, but a second thread's
    // drain must see the same guard while the first is inside its executor.
    let queues = Arc::new(Queues::new());
    queues.push(request("outer", 1)).unwrap();
    queues.push(request("inner", 1)).unwrap();

    let (in_exec, in_exec_rx) = mpsc::channel();
    let (b_attempted, b_attempted_rx) = mpsc::channel();
    let thread_b = {
        let queues = Arc::clone(&queues);
        thread::spawn(move || {
            in_exec_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            // Thread B's drain attempt lands while thread A is inside exec.
            queues.drain(&mut |request| format!("thread B ran {}", request.id));
            b_attempted.send(()).unwrap();
        })
    };

    // Recorded inside the executor, asserted after the drain returns (an assert inside an
    // executor is swallowed by drain's catch_unwind).
    let mut calls = Vec::new();
    queues.drain(&mut |request| {
        calls.push((request.id.clone(), thread::current().id()));
        if request.id == "outer" {
            in_exec.send(()).unwrap();
            // Return only after thread B's drain attempt has landed.
            b_attempted_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        echo(request)
    });
    thread_b.join().unwrap();

    let drain_thread = calls[0].1;
    assert_eq!(
        calls.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        ["outer", "inner"],
        "thread B's drain must have executed nothing"
    );
    assert!(
        calls.iter().all(|(_, tid)| *tid == drain_thread),
        "every request runs on the thread that called drain"
    );
    assert_eq!(queues.depth(), 0);
    assert_eq!(
        pop_all(&queues),
        vec![
            ("ran outer".to_string(), 1),
            ("ran inner".to_string(), 1)
        ]
    );
}
