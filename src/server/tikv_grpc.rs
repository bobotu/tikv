// Copyright 2017 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::boxed::FnBox;
use std::fmt::Debug;
use std::sync::Mutex;
use grpc::futures_grpc::GrpcFutureSend;
use grpc::error::GrpcError;
use futures::Future;
use futures::sync::oneshot;
use protobuf::RepeatedField;
use kvproto::tikvpb_grpc;
use kvproto::kvrpcpb::*;
use kvproto::coprocessor::*;
use kvproto::errorpb::{Error as RegionError, ServerIsBusy};

use util::worker::Worker;
use storage::{self, Storage, Key, Options, Mutation};
use super::coprocessor::{RequestTask, EndPointTask, EndPointHost};
use super::metrics::*;
use super::Result;

#[allow(dead_code)]
const DEFAULT_COPROCESSOR_BATCH: usize = 50;

#[allow(dead_code)]
pub struct TiKVAsync {
    storage: Mutex<Storage>,
    end_point_worker: Mutex<Worker<EndPointTask>>,
    end_point_concurrency: usize,
}

#[allow(dead_code)]
impl TiKVAsync {
    pub fn new(storage: Storage, end_point_concurrency: usize) -> TiKVAsync {
        TiKVAsync {
            storage: Mutex::new(storage),
            end_point_worker: Mutex::new(Worker::new("end-point-worker")),
            end_point_concurrency: end_point_concurrency,
        }
    }

    pub fn run(&mut self) -> Result<()> {
        let end_point = EndPointHost::new(self.storage.lock().unwrap().get_engine(),
                                          self.end_point_worker.lock().unwrap().scheduler(),
                                          self.end_point_concurrency);
        try!(self.end_point_worker.lock().unwrap().start_batch(end_point, DEFAULT_COPROCESSOR_BATCH));
        Ok(())
    }

    pub fn stop(&mut self) {
        self.end_point_worker.lock().unwrap().stop();
    }
}

fn make_callback<T: Debug + Send + 'static>() -> (Box<FnBox(T) + Send>, GrpcFutureSend<T>) {
    let (tx, rx) = oneshot::channel();
    let callback = move |resp| {
        tx.send(resp).unwrap();
    };
    let future = rx.map_err(GrpcError::Canceled);
    (box callback, box future)
}

impl tikvpb_grpc::TiKVAsync for TiKVAsync {
    fn KvGet(&self, mut p: GetRequest) -> GrpcFutureSend<GetResponse> {
        RECV_MSG_COUNTER.with_label_values(&["kv"]).inc();
        let (cb, future) = make_callback();
        self.storage.lock().unwrap()
            .async_get(p.take_context(),
                       Key::from_raw(p.get_key()),
                       p.get_version(),
                       cb)
            .unwrap();
        future.map(|v| {
                let mut resp = GetResponse::new();
                if let Some(err) = extract_region_error(&v) {
                    resp.set_region_error(err);
                } else {
                    match v {
                        Ok(Some(val)) => resp.set_value(val),
                        Ok(None) => resp.set_value(vec![]),
                        Err(e) => resp.set_error(extract_key_error(&e)),
                    }
                }
                resp
            })
            .boxed()
    }

    fn KvScan(&self, mut p: ScanRequest) -> GrpcFutureSend<ScanResponse> {
        RECV_MSG_COUNTER.with_label_values(&["kv"]).inc();
        let mut options = Options::default();
        options.key_only = p.get_key_only();

        let (cb, future) = make_callback();
        self.storage.lock().unwrap()
            .async_scan(p.take_context(),
                        Key::from_raw(p.get_start_key()),
                        p.get_limit() as usize,
                        p.get_version(),
                        options,
                        cb)
            .unwrap();
        future.map(|v| {
                let mut resp = ScanResponse::new();
                if let Some(err) = extract_region_error(&v) {
                    resp.set_region_error(err);
                } else {
                    resp.set_pairs(RepeatedField::from_vec(extract_kv_pairs(v)));
                }
                resp
            })
            .boxed()
    }

    fn KvPrewrite(&self, mut p: PrewriteRequest) -> GrpcFutureSend<PrewriteResponse> {
        RECV_MSG_COUNTER.with_label_values(&["kv"]).inc();
        let mutations = p.take_mutations()
            .into_iter()
            .map(|mut x| {
                match x.get_op() {
                    Op::Put => Mutation::Put((Key::from_raw(x.get_key()), x.take_value())),
                    Op::Del => Mutation::Delete(Key::from_raw(x.get_key())),
                    Op::Lock => Mutation::Lock(Key::from_raw(x.get_key())),
                }
            })
            .collect();
        let mut options = Options::default();
        options.lock_ttl = p.get_lock_ttl();
        options.skip_constraint_check = p.get_skip_constraint_check();

        let (cb, future) = make_callback();
        self.storage.lock().unwrap()
            .async_prewrite(p.take_context(),
                            mutations,
                            p.take_primary_lock(),
                            p.get_start_version(),
                            options,
                            cb)
            .unwrap();
        future.map(|v| {
                let mut resp = PrewriteResponse::new();
                if let Some(err) = extract_region_error(&v) {
                    resp.set_region_error(err);
                } else {
                    resp.set_errors(RepeatedField::from_vec(extract_key_errors(v)));
                }
                resp
            })
            .boxed()
    }

    fn KvCommit(&self, mut p: CommitRequest) -> GrpcFutureSend<CommitResponse> {
        RECV_MSG_COUNTER.with_label_values(&["kv"]).inc();
        let keys = p.get_keys().iter().map(|x| Key::from_raw(x)).collect();

        let (cb, future) = make_callback();
        self.storage.lock().unwrap()
            .async_commit(p.take_context(),
                          keys,
                          p.get_start_version(),
                          p.get_commit_version(),
                          cb)
            .unwrap();
        future.map(|v| {
                let mut resp = CommitResponse::new();
                if let Some(err) = extract_region_error(&v) {
                    resp.set_region_error(err);
                } else if let Err(e) = v {
                    resp.set_error(extract_key_error(&e));
                }
                resp
            })
            .boxed()
    }

    fn KvCleanup(&self, mut p: CleanupRequest) -> GrpcFutureSend<CleanupResponse> {
        RECV_MSG_COUNTER.with_label_values(&["kv"]).inc();

        let (cb, future) = make_callback();
        self.storage.lock().unwrap()
            .async_cleanup(p.take_context(),
                           Key::from_raw(p.get_key()),
                           p.get_start_version(),
                           cb)
            .unwrap();
        future.map(|v| {
                let mut resp = CleanupResponse::new();
                if let Some(err) = extract_region_error(&v) {
                    resp.set_region_error(err);
                } else if let Err(e) = v {
                    if let Some(ts) = extract_committed(&e) {
                        resp.set_commit_version(ts);
                    } else {
                        resp.set_error(extract_key_error(&e));
                    }
                }
                resp
            })
            .boxed()
    }

    fn KvBatchGet(&self, mut p: BatchGetRequest) -> GrpcFutureSend<BatchGetResponse> {
        RECV_MSG_COUNTER.with_label_values(&["kv"]).inc();
        let keys = p.get_keys().into_iter().map(|x| Key::from_raw(x)).collect();

        let (cb, future) = make_callback();
        self.storage.lock().unwrap()
            .async_batch_get(p.take_context(), keys, p.get_version(), cb)
            .unwrap();
        future.map(|v| {
                let mut resp = BatchGetResponse::new();
                if let Some(err) = extract_region_error(&v) {
                    resp.set_region_error(err);
                } else {
                    resp.set_pairs(RepeatedField::from_vec(extract_kv_pairs(v)))
                }
                resp
            })
            .boxed()
    }

    fn KvBatchRollback(&self,
                       mut p: BatchRollbackRequest)
                       -> GrpcFutureSend<BatchRollbackResponse> {
        RECV_MSG_COUNTER.with_label_values(&["kv"]).inc();
        let keys = p.get_keys().into_iter().map(|x| Key::from_raw(x)).collect();

        let (cb, future) = make_callback();
        self.storage.lock().unwrap().async_rollback(p.take_context(), keys, p.get_start_version(), cb).unwrap();
        future.map(|v| {
                let mut resp = BatchRollbackResponse::new();
                if let Some(err) = extract_region_error(&v) {
                    resp.set_region_error(err);
                } else if let Err(e) = v {
                    resp.set_error(extract_key_error(&e));
                }
                resp
            })
            .boxed()
    }

    fn KvScanLock(&self, mut p: ScanLockRequest) -> GrpcFutureSend<ScanLockResponse> {
        RECV_MSG_COUNTER.with_label_values(&["kv"]).inc();

        let (cb, future) = make_callback();
        self.storage.lock().unwrap().async_scan_lock(p.take_context(), p.get_max_version(), cb).unwrap();
        future.map(|v| {
                let mut resp = ScanLockResponse::new();
                if let Some(err) = extract_region_error(&v) {
                    resp.set_region_error(err);
                } else {
                    match v {
                        Ok(locks) => resp.set_locks(RepeatedField::from_vec(locks)),
                        Err(e) => resp.set_error(extract_key_error(&e)),
                    }
                }
                resp
            })
            .boxed()
    }

    fn KvResolveLock(&self, mut p: ResolveLockRequest) -> GrpcFutureSend<ResolveLockResponse> {
        RECV_MSG_COUNTER.with_label_values(&["kv"]).inc();
        let commit_ts = match p.get_commit_version() {
            0 => None,
            x => Some(x),
        };

        let (cb, future) = make_callback();
        self.storage.lock().unwrap()
            .async_resolve_lock(p.take_context(), p.get_start_version(), commit_ts, cb)
            .unwrap();
        future.map(|v| {
                let mut resp = ResolveLockResponse::new();
                if let Some(err) = extract_region_error(&v) {
                    resp.set_region_error(err);
                } else if let Err(e) = v {
                    resp.set_error(extract_key_error(&e));
                }
                resp
            })
            .boxed()
    }

    fn KvGC(&self, mut p: GCRequest) -> GrpcFutureSend<GCResponse> {
        RECV_MSG_COUNTER.with_label_values(&["kv"]).inc();

        let (cb, future) = make_callback();
        self.storage.lock().unwrap().async_gc(p.take_context(), p.get_safe_point(), cb).unwrap();
        future.map(|v| {
                let mut resp = GCResponse::new();
                if let Some(err) = extract_region_error(&v) {
                    resp.set_region_error(err);
                } else if let Err(e) = v {
                    resp.set_error(extract_key_error(&e));
                }
                resp
            })
            .boxed()
    }

    fn RawGet(&self, mut p: RawGetRequest) -> GrpcFutureSend<RawGetResponse> {
        RECV_MSG_COUNTER.with_label_values(&["kv"]).inc();

        let (cb, future) = make_callback();
        self.storage.lock().unwrap().async_raw_get(p.take_context(), p.take_key(), cb).unwrap();
        future.map(|v| {
                let mut resp = RawGetResponse::new();
                if let Some(err) = extract_region_error(&v) {
                    resp.set_region_error(err);
                } else {
                    match v {
                        Ok(Some(val)) => resp.set_value(val),
                        Ok(None) => {}
                        Err(e) => resp.set_error(format!("{}", e)),
                    }
                }
                resp
            })
            .boxed()
    }

    fn RawPut(&self, mut p: RawPutRequest) -> GrpcFutureSend<RawPutResponse> {
        RECV_MSG_COUNTER.with_label_values(&["kv"]).inc();

        let (cb, future) = make_callback();
        self.storage.lock().unwrap().async_raw_put(p.take_context(), p.take_key(), p.take_value(), cb).unwrap();
        future.map(|v| {
                let mut resp = RawPutResponse::new();
                if let Some(err) = extract_region_error(&v) {
                    resp.set_region_error(err);
                } else if let Err(e) = v {
                    resp.set_error(format!("{}", e));
                }
                resp
            })
            .boxed()
    }

    fn RawDelete(&self, mut p: RawDeleteRequest) -> GrpcFutureSend<RawDeleteResponse> {
        RECV_MSG_COUNTER.with_label_values(&["kv"]).inc();

        let (cb, future) = make_callback();
        self.storage.lock().unwrap().async_raw_delete(p.take_context(), p.take_key(), cb).unwrap();
        future.map(|v| {
                let mut resp = RawDeleteResponse::new();
                if let Some(err) = extract_region_error(&v) {
                    resp.set_region_error(err);
                } else if let Err(e) = v {
                    resp.set_error(format!("{}", e));
                }
                resp
            })
            .boxed()
    }

    fn Coprocessor(&self, p: Request) -> GrpcFutureSend<Response> {
        RECV_MSG_COUNTER.with_label_values(&["coprocessor"]).inc();

        let (cb, future) = make_callback();
        self.end_point_worker.lock().unwrap().schedule(EndPointTask::Request(RequestTask::new(p, cb))).unwrap();
        future.boxed()
    }
}

use storage::txn::Error as TxnError;
use storage::mvcc::Error as MvccError;
use storage::engine::Error as EngineError;

fn extract_region_error<T>(res: &storage::Result<T>) -> Option<RegionError> {
    use storage::Error;
    match *res {
        // TODO: use `Error::cause` instead.
        Err(Error::Engine(EngineError::Request(ref e))) |
        Err(Error::Txn(TxnError::Engine(EngineError::Request(ref e)))) |
        Err(Error::Txn(TxnError::Mvcc(MvccError::Engine(EngineError::Request(ref e))))) => {
            Some(e.to_owned())
        }
        Err(Error::SchedTooBusy) => {
            let mut err = RegionError::new();
            err.set_server_is_busy(ServerIsBusy::new());
            Some(err)
        }
        _ => None,
    }
}

fn extract_committed(err: &storage::Error) -> Option<u64> {
    match *err {
        storage::Error::Txn(TxnError::Mvcc(MvccError::Committed { commit_ts })) => Some(commit_ts),
        _ => None,
    }
}

fn extract_key_error(err: &storage::Error) -> KeyError {
    let mut key_error = KeyError::new();
    match *err {
        storage::Error::Txn(TxnError::Mvcc(MvccError::KeyIsLocked { ref key,
                                                                    ref primary,
                                                                    ts,
                                                                    ttl })) => {
            let mut lock_info = LockInfo::new();
            lock_info.set_key(key.to_owned());
            lock_info.set_primary_lock(primary.to_owned());
            lock_info.set_lock_version(ts);
            lock_info.set_lock_ttl(ttl);
            key_error.set_locked(lock_info);
        }
        storage::Error::Txn(TxnError::Mvcc(MvccError::WriteConflict)) |
        storage::Error::Txn(TxnError::Mvcc(MvccError::TxnLockNotFound)) => {
            debug!("txn conflicts: {}", err);
            key_error.set_retryable(format!("{:?}", err));
        }
        _ => {
            error!("txn aborts: {}", err);
            key_error.set_abort(format!("{:?}", err));
        }
    }
    key_error
}

fn extract_kv_pairs(res: storage::Result<Vec<storage::Result<storage::KvPair>>>) -> Vec<KvPair> {
    let mut pairs = vec![];
    match res {
        Ok(res) => {
            for r in res {
                let mut pair = KvPair::new();
                match r {
                    Ok((key, value)) => {
                        pair.set_key(key);
                        pair.set_value(value);
                    }
                    Err(e) => {
                        pair.set_error(extract_key_error(&e));
                    }
                }
                pairs.push(pair);
            }
        }
        Err(e) => {
            let mut pair = KvPair::new();
            pair.set_error(extract_key_error(&e));
            pairs.push(pair);
        }
    }
    pairs
}

fn extract_key_errors(res: storage::Result<Vec<storage::Result<()>>>) -> Vec<KeyError> {
    let mut errs = vec![];
    match res {
        Ok(res) => {
            for r in res {
                if let Err(e) = r {
                    errs.push(extract_key_error(&e));
                }
            }
        }
        Err(e) => {
            errs.push(extract_key_error(&e));
        }
    }
    errs
}
