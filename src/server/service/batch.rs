// Copyright 2017 TiKV Project Authors. Licensed under Apache-2.0.

use itertools::izip;

use crate::server::metrics::GRPC_MSG_HISTOGRAM_STATIC;
use crate::server::service::kv::batch_commands_response;
use crate::storage::{
    errors::{extract_key_error, extract_region_error},
    kv::Engine,
    lock_manager::LockManager,
    Storage,
};
use kvproto::kvrpcpb::*;
use tikv_util::future::poll_future_notify;
use tikv_util::mpsc::batch::Sender;
use tikv_util::time::{duration_to_sec, Instant};
use tikv_util::trace::Scope;

pub struct ReqBatcher {
    gets: Vec<GetRequest>,
    get_ids: Vec<u64>,
    get_scopes: Vec<Scope>,
    get_report_routines: Vec<Box<dyn FnOnce(&mut GetResponse) + Send>>,

    raw_gets: Vec<RawGetRequest>,
    raw_get_ids: Vec<u64>,
    raw_get_scopes: Vec<Scope>,
    raw_get_report_routines: Vec<Box<dyn FnOnce(&mut RawGetResponse) + Send>>,
}

impl ReqBatcher {
    pub fn new() -> ReqBatcher {
        ReqBatcher {
            gets: vec![],
            get_ids: vec![],
            get_scopes: vec![],
            get_report_routines: vec![],

            raw_gets: vec![],
            raw_get_ids: vec![],
            raw_get_scopes: vec![],
            raw_get_report_routines: vec![],
        }
    }

    pub fn can_batch_get(&self, req: &GetRequest) -> bool {
        req.get_context().get_priority() == CommandPri::Normal
    }

    pub fn can_batch_raw_get(&self, req: &RawGetRequest) -> bool {
        req.get_context().get_priority() == CommandPri::Normal
    }

    pub fn add_get_request(
        &mut self,
        req: GetRequest,
        id: u64,
        scope: Scope,
        report_routine: impl FnOnce(&mut GetResponse) + Send + 'static,
    ) {
        self.gets.push(req);
        self.get_ids.push(id);
        self.get_scopes.push(scope);
        self.get_report_routines.push(Box::new(report_routine));
    }

    pub fn add_raw_get_request(
        &mut self,
        req: RawGetRequest,
        id: u64,
        scope: Scope,
        report_routine: impl FnOnce(&mut RawGetResponse) + Send + 'static,
    ) {
        self.raw_gets.push(req);
        self.raw_get_ids.push(id);
        self.raw_get_scopes.push(scope);
        self.raw_get_report_routines.push(Box::new(report_routine));
    }

    pub fn maybe_commit<E: Engine, L: LockManager>(
        &mut self,
        storage: &Storage<E, L>,
        tx: &Sender<(u64, batch_commands_response::Response)>,
    ) {
        if self.gets.len() > 10 {
            let gets = std::mem::replace(&mut self.gets, vec![]);
            let ids = std::mem::replace(&mut self.get_ids, vec![]);
            let scopes = std::mem::replace(&mut self.get_scopes, vec![]);
            let report_routines = std::mem::replace(&mut self.get_report_routines, vec![]);

            let scope = Scope::from_parents("BatchGetCommand", scopes.iter());
            let _g = scope.enter();
            future_batch_get_command(storage, ids, gets, scopes, report_routines, tx.clone());
        }
        if self.raw_gets.len() > 16 {
            let gets = std::mem::replace(&mut self.raw_gets, vec![]);
            let ids = std::mem::replace(&mut self.raw_get_ids, vec![]);
            let scopes = std::mem::replace(&mut self.raw_get_scopes, vec![]);
            let report_routines = std::mem::replace(&mut self.raw_get_report_routines, vec![]);

            let scope = Scope::from_parents("BatchRawGetCommand", scopes.iter());
            let _g = scope.enter();
            future_batch_raw_get_command(storage, ids, gets, scopes, report_routines, tx.clone());
        }
    }

    pub fn commit<E: Engine, L: LockManager>(
        &mut self,
        storage: &Storage<E, L>,
        tx: &Sender<(u64, batch_commands_response::Response)>,
    ) {
        if !self.gets.is_empty() {
            let gets = std::mem::replace(&mut self.gets, vec![]);
            let ids = std::mem::replace(&mut self.get_ids, vec![]);
            let scopes = std::mem::replace(&mut self.get_scopes, vec![]);
            let report_routines = std::mem::replace(&mut self.get_report_routines, vec![]);

            let scope = Scope::from_parents("BatchGetCommand", scopes.iter());
            let _g = scope.enter();
            future_batch_get_command(storage, ids, gets, scopes, report_routines, tx.clone());
        }
        if !self.raw_gets.is_empty() {
            let gets = std::mem::replace(&mut self.raw_gets, vec![]);
            let ids = std::mem::replace(&mut self.raw_get_ids, vec![]);
            let scopes = std::mem::replace(&mut self.get_scopes, vec![]);
            let report_routines = std::mem::replace(&mut self.raw_get_report_routines, vec![]);

            let scope = Scope::from_parents("BatchRawGetCommand", scopes.iter());
            let _g = scope.enter();
            future_batch_raw_get_command(storage, ids, gets, scopes, report_routines, tx.clone());
        }
    }
}

fn future_batch_get_command<E: Engine, L: LockManager>(
    storage: &Storage<E, L>,
    requests: Vec<u64>,
    gets: Vec<GetRequest>,
    scopes: Vec<Scope>,
    report_routines: Vec<Box<dyn FnOnce(&mut GetResponse) + Send>>,
    tx: Sender<(u64, batch_commands_response::Response)>,
) {
    let begin_instant = Instant::now_coarse();
    let ret = storage.batch_get_command(gets);
    let f = async move {
        let ret = ret.await;
        drop(scopes);
        match ret {
            Ok(ret) => {
                for (v, req, report_routine) in izip!(ret, requests, report_routines) {
                    let mut resp = GetResponse::default();
                    if let Some(err) = extract_region_error(&v) {
                        resp.set_region_error(err);
                    } else {
                        match v {
                            Ok((val, statistics, perf_statistics_delta)) => {
                                let scan_detail_v2 =
                                    resp.mut_exec_details_v2().mut_scan_detail_v2();
                                statistics.write_scan_detail(scan_detail_v2);
                                perf_statistics_delta.write_scan_detail(scan_detail_v2);
                                match val {
                                    Some(val) => resp.set_value(val),
                                    None => resp.set_not_found(true),
                                }
                            }
                            Err(e) => resp.set_error(extract_key_error(&e)),
                        }
                    }
                    report_routine(&mut resp);
                    let mut res = batch_commands_response::Response::default();
                    res.cmd = Some(batch_commands_response::response::Cmd::Get(resp));
                    if tx.send_and_notify((req, res)).is_err() {
                        error!("KvService response batch commands fail");
                    }
                }
            }
            e => {
                let mut resp = GetResponse::default();
                if let Some(err) = extract_region_error(&e) {
                    resp.set_region_error(err);
                } else if let Err(e) = e {
                    resp.set_error(extract_key_error(&e));
                }
                let mut res = batch_commands_response::Response::default();
                res.cmd = Some(batch_commands_response::response::Cmd::Get(resp));
                for req in requests {
                    if tx.send_and_notify((req, res.clone())).is_err() {
                        error!("KvService response batch commands fail");
                    }
                }
            }
        }
        GRPC_MSG_HISTOGRAM_STATIC
            .kv_batch_get_command
            .observe(begin_instant.elapsed_secs());
    };
    poll_future_notify(f);
}

fn future_batch_raw_get_command<E: Engine, L: LockManager>(
    storage: &Storage<E, L>,
    requests: Vec<u64>,
    gets: Vec<RawGetRequest>,
    scopes: Vec<Scope>,
    report_routines: Vec<Box<dyn FnOnce(&mut RawGetResponse) + Send>>,
    tx: Sender<(u64, batch_commands_response::Response)>,
) {
    let begin_instant = Instant::now_coarse();
    let ret = storage.raw_batch_get_command(gets);
    let f = async move {
        let ret = ret.await;
        drop(scopes);
        match ret {
            Ok(v) => {
                if requests.len() != v.len() {
                    error!("KvService batch response size mismatch");
                }
                for (req, v, report_routine) in izip!(requests, v, report_routines) {
                    let mut resp = RawGetResponse::default();
                    if let Some(err) = extract_region_error(&v) {
                        resp.set_region_error(err);
                    } else {
                        match v {
                            Ok(Some(val)) => resp.set_value(val),
                            Ok(None) => resp.set_not_found(true),
                            Err(e) => resp.set_error(format!("{}", e)),
                        }
                    }
                    report_routine(&mut resp);
                    let mut res = batch_commands_response::Response::default();
                    res.cmd = Some(batch_commands_response::response::Cmd::RawGet(resp));
                    if tx.send_and_notify((req, res)).is_err() {
                        error!("KvService response batch commands fail");
                    }
                }
            }
            e => {
                let mut resp = RawGetResponse::default();
                if let Some(err) = extract_region_error(&e) {
                    resp.set_region_error(err);
                } else if let Err(e) = e {
                    resp.set_error(format!("{}", e));
                }
                let mut res = batch_commands_response::Response::default();
                res.cmd = Some(batch_commands_response::response::Cmd::RawGet(resp));
                for req in requests {
                    if tx.send_and_notify((req, res.clone())).is_err() {
                        error!("KvService response batch commands fail");
                    }
                }
            }
        }
        GRPC_MSG_HISTOGRAM_STATIC
            .raw_batch_get_command
            .observe(duration_to_sec(begin_instant.elapsed()));
    };
    poll_future_notify(f);
}
