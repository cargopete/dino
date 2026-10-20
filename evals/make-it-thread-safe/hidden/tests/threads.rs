use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use fixture::{Event, EventKind, Job, JobId, JobState, Registry, Scheduler, Stats, SubscriptionId, Tally};

fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn the_named_types_are_send_and_sync() {
    assert_send_sync::<Registry>();
    assert_send_sync::<Scheduler>();
    assert_send_sync::<Tally>();
    assert_send_sync::<Job>();
    assert_send_sync::<Event>();
    assert_send_sync::<Stats>();
    assert_send_sync::<JobId>();
    assert_send_sync::<SubscriptionId>();
}

/// Runs `body` on its own thread and fails, rather than hanging, if it does
/// not finish in time.
fn within_deadline(body: impl FnOnce() + Send + 'static) {
    let (done, finished) = mpsc::channel();
    thread::spawn(move || {
        body();
        let _ = done.send(());
    });
    match finished.recv_timeout(Duration::from_secs(10)) {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => panic!("deadlocked: did not finish within 10s"),
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the body panicked"),
    }
}

#[derive(Default)]
struct Counts {
    submitted: AtomicU64,
    progressed: AtomicU64,
    completed: AtomicU64,
    cancelled: AtomicU64,
}

impl Counts {
    fn record(&self, event: &Event) {
        let counter = match event.kind() {
            EventKind::Submitted => &self.submitted,
            EventKind::Progressed => &self.progressed,
            EventKind::Completed => &self.completed,
            EventKind::Cancelled => &self.cancelled,
        };
        counter.fetch_add(1, Ordering::SeqCst);
    }

    fn total(&self) -> u64 {
        self.submitted.load(Ordering::SeqCst)
            + self.progressed.load(Ordering::SeqCst)
            + self.completed.load(Ordering::SeqCst)
            + self.cancelled.load(Ordering::SeqCst)
    }
}

const SUBMITTERS: usize = 8;
const PER_SUBMITTER: usize = 60;
const JOBS: usize = SUBMITTERS * PER_SUBMITTER;

fn steps_for(n: usize) -> u32 {
    (n % 5) as u32 + 1
}

#[test]
fn concurrent_ticks_advance_every_step_exactly_once() {
    within_deadline(|| {
        let registry = Registry::new();
        let counts = Arc::new(Counts::default());
        let sink = Arc::clone(&counts);
        registry.subscribe(move |event| sink.record(event));
        let tally = Tally::attach(&registry);
        let scheduler = Scheduler::new(registry.clone());

        let submitting = AtomicUsize::new(SUBMITTERS);
        let advanced = AtomicUsize::new(0);
        let ids = Mutex::new(Vec::new());
        thread::scope(|scope| {
            for submitter in 0..SUBMITTERS {
                let (registry, submitting, ids) = (&registry, &submitting, &ids);
                scope.spawn(move || {
                    let mut mine = Vec::new();
                    for n in 0..PER_SUBMITTER {
                        let name = format!("job-{submitter}-{n}");
                        mine.push(registry.submit(&name, steps_for(n)));
                    }
                    ids.lock().unwrap().extend(mine);
                    submitting.fetch_sub(1, Ordering::SeqCst);
                });
            }
            for _ in 0..4 {
                let (scheduler, submitting, advanced) = (&scheduler, &submitting, &advanced);
                scope.spawn(move || loop {
                    let still_submitting = submitting.load(Ordering::SeqCst) > 0;
                    let moved = scheduler.tick();
                    advanced.fetch_add(moved, Ordering::SeqCst);
                    if !still_submitting && scheduler.registry().running().is_empty() {
                        break;
                    }
                    thread::yield_now();
                });
            }
        });

        let ids = ids.into_inner().unwrap();
        let distinct: BTreeSet<JobId> = ids.iter().copied().collect();
        assert_eq!(distinct.len(), JOBS, "job ids were reused");
        assert_eq!(registry.len(), JOBS);

        let total_steps: u64 = (0..PER_SUBMITTER).map(|n| u64::from(steps_for(n))).sum::<u64>()
            * SUBMITTERS as u64;
        assert_eq!(advanced.load(Ordering::SeqCst) as u64, total_steps);
        for id in &ids {
            let job = registry.get(*id).expect("job");
            assert_eq!(job.state(), JobState::Completed);
            assert_eq!(job.done(), job.total());
        }

        let stats = registry.stats();
        assert_eq!(stats.submitted, JOBS as u64);
        assert_eq!(stats.completed, JOBS as u64);
        assert_eq!(stats.cancelled, 0);
        assert_eq!(counts.submitted.load(Ordering::SeqCst), JOBS as u64);
        assert_eq!(counts.progressed.load(Ordering::SeqCst), total_steps);
        assert_eq!(counts.completed.load(Ordering::SeqCst), JOBS as u64);
        assert_eq!(tally.count(EventKind::Progressed), total_steps);
        assert_eq!(tally.total(), counts.total());
        assert_eq!(stats.delivered, 2 * counts.total());
    });
}

#[test]
fn cancels_and_advances_race_without_losing_count() {
    within_deadline(|| {
        let registry = Registry::new();
        let counts = Arc::new(Counts::default());
        let sink = Arc::clone(&counts);
        registry.subscribe(move |event| sink.record(event));
        let ids: Vec<JobId> = (0..JOBS).map(|n| registry.submit("race", steps_for(n) + 2)).collect();

        let cancelled = AtomicU64::new(0);
        thread::scope(|scope| {
            for worker in 0..6 {
                let (registry, ids, cancelled) = (&registry, &ids, &cancelled);
                scope.spawn(move || {
                    for round in 0..8 {
                        for (n, id) in ids.iter().enumerate() {
                            if (n + worker + round) % 11 == 0 {
                                if registry.cancel(*id) {
                                    cancelled.fetch_add(1, Ordering::SeqCst);
                                }
                            } else {
                                registry.advance(*id);
                            }
                        }
                    }
                });
            }
        });

        let jobs: Vec<Job> = ids.iter().map(|id| registry.get(*id).expect("job")).collect();
        let completed = jobs.iter().filter(|job| job.state() == JobState::Completed).count() as u64;
        let cancelled_jobs = jobs.iter().filter(|job| job.state() == JobState::Cancelled).count() as u64;
        let done: u64 = jobs.iter().map(|job| u64::from(job.done())).sum();
        for job in &jobs {
            assert!(job.done() <= job.total(), "advanced past its total");
            if job.state() == JobState::Completed {
                assert_eq!(job.done(), job.total());
            }
        }

        let stats = registry.stats();
        assert_eq!(stats.submitted, JOBS as u64);
        assert_eq!(stats.completed, completed);
        assert_eq!(stats.cancelled, cancelled_jobs);
        assert_eq!(cancelled.load(Ordering::SeqCst), cancelled_jobs);
        assert_eq!(counts.cancelled.load(Ordering::SeqCst), cancelled_jobs);
        assert_eq!(counts.completed.load(Ordering::SeqCst), completed);
        assert_eq!(counts.progressed.load(Ordering::SeqCst), done);
        assert_eq!(stats.delivered, counts.total());
        assert!(cancelled_jobs > 0 && completed > 0);
    });
}

#[test]
fn a_callback_can_call_back_into_the_registry() {
    within_deadline(|| {
        let registry = Registry::new();
        let follow_ups = Arc::new(Mutex::new(Vec::new()));
        let (handle, sink) = (registry.clone(), Arc::clone(&follow_ups));
        registry.subscribe(move |event| {
            if let Event::Completed { id } = event {
                let job = handle.get(*id).expect("the completed job");
                assert_eq!(job.state(), JobState::Completed);
                assert!(handle.running().iter().all(|running| running != id));
                let _ = handle.stats();
                let _ = handle.len();
                if job.name() == "first" {
                    let next = handle.submit("second", 1);
                    sink.lock().unwrap().push(next);
                }
            }
        });

        let first = registry.submit("first", 2);
        let scheduler = Scheduler::new(registry.clone());
        assert_eq!(scheduler.run_until_idle(), 3);
        let follow_ups = follow_ups.lock().unwrap().clone();
        assert_eq!(follow_ups.len(), 1);
        assert_eq!(registry.get(follow_ups[0]).expect("second").state(), JobState::Completed);
        assert_eq!(registry.get(first).expect("first").state(), JobState::Completed);
        assert_eq!(registry.stats().completed, 2);
    });
}

#[test]
fn a_callback_can_unsubscribe_itself_and_subscribe_others() {
    within_deadline(|| {
        let registry = Registry::new();
        let once_calls = Arc::new(AtomicU64::new(0));
        let late_calls = Arc::new(Mutex::new(Vec::new()));
        let own: Arc<Mutex<Option<SubscriptionId>>> = Arc::new(Mutex::new(None));
        let (handle, calls, slot, late) = (
            registry.clone(),
            Arc::clone(&once_calls),
            Arc::clone(&own),
            Arc::clone(&late_calls),
        );
        let id = registry.subscribe(move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            let mine = slot.lock().unwrap().take();
            if let Some(mine) = mine {
                assert!(handle.unsubscribe(mine));
                assert!(!handle.unsubscribe(mine));
                let late = Arc::clone(&late);
                handle.subscribe(move |event| late.lock().unwrap().push(event.clone()));
            }
        });
        *own.lock().unwrap() = Some(id);

        registry.submit("one", 1);
        let two = registry.submit("two", 1);
        registry.advance(two);
        assert_eq!(once_calls.load(Ordering::SeqCst), 1);
        // Whether a subscriber added mid-dispatch sees that same event is left open.
        let late: Vec<Event> = late_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.job() == two)
            .cloned()
            .collect();
        assert_eq!(
            late,
            vec![
                Event::Submitted { id: two },
                Event::Progressed { id: two, done: 1, total: 1 },
                Event::Completed { id: two },
            ]
        );
        assert!(!registry.unsubscribe(id));
    });
}

#[test]
fn callbacks_may_publish_from_many_threads_at_once() {
    within_deadline(|| {
        let registry = Registry::new();
        let chained = Arc::new(AtomicU64::new(0));
        let (handle, counter) = (registry.clone(), Arc::clone(&chained));
        registry.subscribe(move |event| {
            if let Event::Submitted { id } = event {
                if handle.get(*id).map(|job| job.name() == "parent").unwrap_or(false) {
                    let child = handle.submit("child", 1);
                    handle.advance(child);
                    counter.fetch_add(1, Ordering::SeqCst);
                }
            }
        });
        let running = AtomicBool::new(true);
        thread::scope(|scope| {
            for _ in 0..4 {
                let registry = &registry;
                scope.spawn(move || {
                    for _ in 0..50 {
                        let parent = registry.submit("parent", 1);
                        registry.advance(parent);
                    }
                });
            }
            let (registry, running) = (&registry, &running);
            scope.spawn(move || {
                while running.load(Ordering::SeqCst) {
                    let watcher = registry.subscribe(|_| {});
                    registry.unsubscribe(watcher);
                    if registry.len() >= 400 {
                        running.store(false, Ordering::SeqCst);
                    }
                }
            });
        });
        assert_eq!(chained.load(Ordering::SeqCst), 200);
        assert_eq!(registry.len(), 400);
        assert_eq!(registry.stats().completed, 400);
        assert!(registry.running().is_empty());
    });
}
