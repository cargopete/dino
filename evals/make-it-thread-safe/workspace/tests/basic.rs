use std::sync::{Arc, Mutex};

use fixture::{Event, EventKind, JobState, Registry, Scheduler, Tally};

#[test]
fn a_job_runs_to_completion() {
    let registry = Registry::new();
    let id = registry.submit("build", 2);
    assert_eq!(registry.advance(id), Some(JobState::Running));
    assert_eq!(registry.advance(id), Some(JobState::Completed));
    assert_eq!(registry.advance(id), Some(JobState::Completed));
    let job = registry.get(id).expect("job");
    assert_eq!((job.name(), job.done(), job.total()), ("build", 2, 2));
    assert_eq!(registry.stats().completed, 1);
}

#[test]
fn subscribers_see_events_in_order() {
    let registry = Registry::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    registry.subscribe(move |event| sink.lock().unwrap().push(event.clone()));
    let id = registry.submit("lint", 1);
    registry.advance(id);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            Event::Submitted { id },
            Event::Progressed { id, done: 1, total: 1 },
            Event::Completed { id },
        ]
    );
    assert_eq!(registry.stats().delivered, 3);
}

#[test]
fn cancelling_stops_a_job() {
    let registry = Registry::new();
    let id = registry.submit("deploy", 3);
    assert!(registry.cancel(id));
    assert!(!registry.cancel(id));
    assert_eq!(registry.advance(id), Some(JobState::Cancelled));
    assert!(registry.running().is_empty());
}

#[test]
fn the_scheduler_drains_the_registry() {
    let registry = Registry::new();
    registry.submit("a", 1);
    registry.submit("b", 3);
    let tally = Tally::attach(&registry);
    let scheduler = Scheduler::new(registry.clone());
    assert_eq!(scheduler.run_until_idle(), 4);
    assert_eq!(tally.count(EventKind::Completed), 2);
    assert_eq!(tally.count(EventKind::Progressed), 4);
    let counts = tally.detach();
    assert_eq!(counts.get(&EventKind::Submitted), None);
    registry.submit("c", 1);
    assert_eq!(registry.len(), 3);
}
