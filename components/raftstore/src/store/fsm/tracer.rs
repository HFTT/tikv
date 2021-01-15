// Copyright 2021 TiKV Project Authors. Licensed under Apache-2.0.

use std::cell::RefCell;
use std::sync::Arc;

use tikv_util::trace::*;

thread_local! {
    static RAFT_TRACER: RefCell<RaftTracer> = RefCell::new(RaftTracer {
        local_collector: None,
        scopes: vec![],
    });
}

pub struct RaftTracer {
    local_collector: Option<LocalCollector>,
    scopes: Vec<Scope>,
}

impl RaftTracer {
    pub fn begin() {
        RAFT_TRACER.with(|tracer| {
            let mut tracer = tracer.borrow_mut();
            tracer.local_collector = Some(LocalCollector::start());
        });
    }

    pub fn add_scope(scope: Scope) {
        RAFT_TRACER.with(|tracer| {
            let mut tracer = tracer.borrow_mut();
            tracer.scopes.push(scope);
        });
    }

    pub fn end() {
        RAFT_TRACER.with(|tracer| {
            let mut tracer = tracer.borrow_mut();
            let raw_spans = tracer
                .local_collector
                .take()
                .expect("empty local_collector")
                .collect();
            if !tracer.scopes.is_empty() {
                let raw_spans = Arc::new(raw_spans);
                for scope in tracer.scopes.split_off(0) {
                    scope.extend_raw_spans(raw_spans.clone());
                }
            }
        });
    }
}

thread_local! {
    static APPLY_TRACER: RefCell<ApplyTracer> = RefCell::new(ApplyTracer { local_collector: None, spans: vec![] });
}

pub struct ApplyTracer {
    local_collector: Option<LocalCollector>,
    spans: Vec<Arc<RawSpans>>,
}

impl ApplyTracer {
    pub fn begin() {
        APPLY_TRACER.with(|tracer| {
            let mut tracer = tracer.borrow_mut();
            tracer.local_collector = Some(LocalCollector::start());
        });
    }

    pub fn truncate() {
        APPLY_TRACER.with(|tracer| {
            let mut tracer = tracer.borrow_mut();

            let raw_spans = tracer
                .local_collector
                .take()
                .expect("empty local_collector")
                .collect();
            if !raw_spans.spans.is_empty() {
                tracer.spans.push(Arc::new(raw_spans));
            }

            tracer.local_collector = Some(LocalCollector::start());
        });
    }

    pub fn partial_submit(f: impl FnOnce(&[Arc<RawSpans>])) {
        APPLY_TRACER.with(|tracer| {
            let tracer = tracer.borrow();

            if !tracer.spans.is_empty() {
                f(tracer.spans.as_slice());
            }
        });
    }

    pub fn end() {
        APPLY_TRACER.with(|tracer| {
            let mut tracer = tracer.borrow_mut();
            let _ = tracer
                .local_collector
                .take()
                .expect("empty local_collector")
                .collect();
            tracer.spans.clear();
        })
    }
}
