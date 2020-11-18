// Copyright 2017 TiKV Project Authors. Licensed under Apache-2.0.

use std::sync::{atomic::Ordering, mpsc::channel, mpsc::RecvTimeoutError, Arc};
use std::thread;
use std::time::Duration;

use grpcio::*;
use kvproto::kvrpcpb::{self, Context, Op, PrewriteRequest, RawPutRequest};
use kvproto::tikvpb::TikvClient;

use errors::{extract_key_error, extract_region_error};
use futures::executor::block_on;
use test_raftstore::{must_get_equal, must_get_none, new_peer, new_server_cluster};
use tikv::storage::kv::{Error as KvError, ErrorInner as KvErrorInner, SnapContext};
use tikv::storage::lock_manager::DummyLockManager;
use tikv::storage::txn::{commands, Error as TxnError, ErrorInner as TxnErrorInner};
use tikv::storage::{self, test_util::*, *};
use tikv_util::{collections::HashMap, HandyRwLock};
use txn_types::Key;
use txn_types::{Mutation, TimeStamp};

#[test]
fn test_scheduler_leader_change_twice() {
    let snapshot_fp = "scheduler_async_snapshot_finish";
    let mut cluster = new_server_cluster(0, 2);
    cluster.run();
    let region0 = cluster.get_region(b"");
    let peers = region0.get_peers();
    cluster.must_transfer_leader(region0.get_id(), peers[0].clone());
    let engine0 = cluster.sim.rl().storages[&peers[0].get_id()].clone();
    let storage0 = TestStorageBuilder::<_, DummyLockManager>::from_engine_and_lock_mgr(
        engine0,
        DummyLockManager {},
    )
    .build()
    .unwrap();

    let mut ctx0 = Context::default();
    ctx0.set_region_id(region0.get_id());
    ctx0.set_region_epoch(region0.get_region_epoch().clone());
    ctx0.set_peer(peers[0].clone());
    let (prewrite_tx, prewrite_rx) = channel();
    fail::cfg(snapshot_fp, "pause").unwrap();
    storage0
        .sched_txn_command(
            commands::Prewrite::new(
                vec![Mutation::Put((Key::from_raw(b"k"), b"v".to_vec()))],
                b"k".to_vec(),
                10.into(),
                0,
                false,
                0,
                TimeStamp::default(),
                TimeStamp::default(),
                None,
                false,
                ctx0,
            ),
            Box::new(move |res: storage::Result<_>| {
                prewrite_tx.send(res).unwrap();
            }),
        )
        .unwrap();
    // Sleep to make sure the failpoint is triggered.
    thread::sleep(Duration::from_millis(2000));
    // Transfer leader twice, then unblock snapshot.
    cluster.must_transfer_leader(region0.get_id(), peers[1].clone());
    cluster.must_transfer_leader(region0.get_id(), peers[0].clone());
    fail::remove(snapshot_fp);

    match prewrite_rx.recv_timeout(Duration::from_secs(5)).unwrap() {
        Err(Error(box ErrorInner::Txn(TxnError(box TxnErrorInner::Engine(KvError(
            box KvErrorInner::Request(ref e),
        ))))))
        | Err(Error(box ErrorInner::Engine(KvError(box KvErrorInner::Request(ref e))))) => {
            assert!(e.has_stale_command(), "{:?}", e);
        }
        res => {
            panic!("expect stale command, but got {:?}", res);
        }
    }
}

#[test]
fn test_server_catching_api_error() {
    let raftkv_fp = "raftkv_early_error_report";
    let mut cluster = new_server_cluster(0, 1);
    cluster.run();
    let region = cluster.get_region(b"");
    let leader = region.get_peers()[0].clone();

    fail::cfg(raftkv_fp, "return").unwrap();

    let env = Arc::new(Environment::new(1));
    let channel =
        ChannelBuilder::new(env).connect(&cluster.sim.rl().get_addr(leader.get_store_id()));
    let client = TikvClient::new(channel);

    let mut ctx = Context::default();
    ctx.set_region_id(region.get_id());
    ctx.set_region_epoch(region.get_region_epoch().clone());
    ctx.set_peer(leader);

    let mut prewrite_req = PrewriteRequest::default();
    prewrite_req.set_context(ctx.clone());
    let mut mutation = kvrpcpb::Mutation::default();
    mutation.op = Op::Put;
    mutation.key = b"k3".to_vec();
    mutation.value = b"v3".to_vec();
    prewrite_req.set_mutations(vec![mutation].into_iter().collect());
    prewrite_req.primary_lock = b"k3".to_vec();
    prewrite_req.start_version = 1;
    prewrite_req.lock_ttl = prewrite_req.start_version + 1;
    let prewrite_resp = client.kv_prewrite(&prewrite_req).unwrap();
    assert!(prewrite_resp.has_region_error(), "{:?}", prewrite_resp);
    assert!(
        prewrite_resp.get_region_error().has_region_not_found(),
        "{:?}",
        prewrite_resp
    );
    must_get_none(&cluster.get_engine(1), b"k3");

    let mut put_req = RawPutRequest::default();
    put_req.set_context(ctx);
    put_req.key = b"k3".to_vec();
    put_req.value = b"v3".to_vec();
    let put_resp = client.raw_put(&put_req).unwrap();
    assert!(put_resp.has_region_error(), "{:?}", put_resp);
    assert!(
        put_resp.get_region_error().has_region_not_found(),
        "{:?}",
        put_resp
    );
    must_get_none(&cluster.get_engine(1), b"k3");

    fail::remove(raftkv_fp);
    let put_resp = client.raw_put(&put_req).unwrap();
    assert!(!put_resp.has_region_error(), "{:?}", put_resp);
    must_get_equal(&cluster.get_engine(1), b"k3", b"v3");
}

#[test]
fn test_raftkv_early_error_report() {
    let raftkv_fp = "raftkv_early_error_report";
    let mut cluster = new_server_cluster(0, 1);
    cluster.run();
    cluster.must_split(&cluster.get_region(b"k0"), b"k1");

    let env = Arc::new(Environment::new(1));
    let mut clients: HashMap<&[u8], (Context, TikvClient)> = HashMap::default();
    for &k in &[b"k0", b"k1"] {
        let region = cluster.get_region(k);
        let leader = region.get_peers()[0].clone();
        let mut ctx = Context::default();
        let channel = ChannelBuilder::new(env.clone())
            .connect(&cluster.sim.rl().get_addr(leader.get_store_id()));
        let client = TikvClient::new(channel);
        ctx.set_region_id(region.get_id());
        ctx.set_region_epoch(region.get_region_epoch().clone());
        ctx.set_peer(leader);
        clients.insert(k, (ctx, client));
    }

    // Inject error to all regions.
    fail::cfg(raftkv_fp, "return").unwrap();
    for (k, (ctx, client)) in &clients {
        let mut put_req = RawPutRequest::default();
        put_req.set_context(ctx.clone());
        put_req.key = k.to_vec();
        put_req.value = b"v".to_vec();
        let put_resp = client.raw_put(&put_req).unwrap();
        assert!(put_resp.has_region_error(), "{:?}", put_resp);
        assert!(
            put_resp.get_region_error().has_region_not_found(),
            "{:?}",
            put_resp
        );
        must_get_none(&cluster.get_engine(1), k);
    }
    fail::remove(raftkv_fp);

    // Inject only one region
    let injected_region_id = clients[b"k0".as_ref()].0.get_region_id();
    fail::cfg(raftkv_fp, &format!("return({})", injected_region_id)).unwrap();
    for (k, (ctx, client)) in &clients {
        let mut put_req = RawPutRequest::default();
        put_req.set_context(ctx.clone());
        put_req.key = k.to_vec();
        put_req.value = b"v".to_vec();
        let put_resp = client.raw_put(&put_req).unwrap();
        if ctx.get_region_id() == injected_region_id {
            assert!(put_resp.has_region_error(), "{:?}", put_resp);
            assert!(
                put_resp.get_region_error().has_region_not_found(),
                "{:?}",
                put_resp
            );
            must_get_none(&cluster.get_engine(1), k);
        } else {
            assert!(!put_resp.has_region_error(), "{:?}", put_resp);
            must_get_equal(&cluster.get_engine(1), k, b"v");
        }
    }
    fail::remove(raftkv_fp);
}

#[test]
fn test_pipelined_pessimistic_lock() {
    let rockskv_async_write_fp = "rockskv_async_write";
    let rockskv_write_modifies_fp = "rockskv_write_modifies";
    let scheduler_async_write_finish_fp = "scheduler_async_write_finish";
    let before_pipelined_write_finish_fp = "before_pipelined_write_finish";

    let storage = TestStorageBuilder::new(DummyLockManager {})
        .set_pipelined_pessimistic_lock(true)
        .build()
        .unwrap();

    let (tx, rx) = channel();
    let (key, val) = (Key::from_raw(b"key"), b"val".to_vec());

    // Even if storage fails to write the lock to engine, client should
    // receive the successful response.
    fail::cfg(rockskv_write_modifies_fp, "return()").unwrap();
    fail::cfg(scheduler_async_write_finish_fp, "pause").unwrap();
    storage
        .sched_txn_command(
            new_acquire_pessimistic_lock_command(vec![(key.clone(), false)], 10, 10, true),
            expect_pessimistic_lock_res_callback(
                tx.clone(),
                PessimisticLockRes::Values(vec![None]),
            ),
        )
        .unwrap();
    rx.recv().unwrap();
    fail::remove(rockskv_write_modifies_fp);
    fail::remove(scheduler_async_write_finish_fp);
    storage
        .sched_txn_command(
            commands::PrewritePessimistic::new(
                vec![(Mutation::Put((key.clone(), val.clone())), true)],
                key.to_raw().unwrap(),
                10.into(),
                3000,
                10.into(),
                1,
                11.into(),
                TimeStamp::default(),
                None,
                false,
                Context::default(),
            ),
            expect_ok_callback(tx.clone(), 0),
        )
        .unwrap();
    rx.recv().unwrap();
    storage
        .sched_txn_command(
            commands::Commit::new(vec![key.clone()], 10.into(), 20.into(), Context::default()),
            expect_ok_callback(tx.clone(), 0),
        )
        .unwrap();
    rx.recv().unwrap();

    // Should report failure if storage fails to schedule write request to engine.
    fail::cfg(rockskv_async_write_fp, "return()").unwrap();
    storage
        .sched_txn_command(
            new_acquire_pessimistic_lock_command(vec![(key.clone(), false)], 30, 30, true),
            expect_fail_callback(tx.clone(), 0, |_| ()),
        )
        .unwrap();
    rx.recv().unwrap();
    fail::remove(rockskv_async_write_fp);

    // Shouldn't release latches until async write finished.
    fail::cfg(scheduler_async_write_finish_fp, "pause").unwrap();
    for blocked in &[false, true] {
        storage
            .sched_txn_command(
                new_acquire_pessimistic_lock_command(vec![(key.clone(), false)], 40, 40, true),
                expect_pessimistic_lock_res_callback(
                    tx.clone(),
                    PessimisticLockRes::Values(vec![Some(val.clone())]),
                ),
            )
            .unwrap();

        if !*blocked {
            rx.recv().unwrap();
        } else {
            // Blocked by latches.
            rx.recv_timeout(Duration::from_millis(500)).unwrap_err();
        }
    }
    fail::remove(scheduler_async_write_finish_fp);
    rx.recv().unwrap();
    delete_pessimistic_lock(&storage, key.clone(), 40, 40);

    // Pipelined write is finished before async write.
    fail::cfg(scheduler_async_write_finish_fp, "pause").unwrap();
    storage
        .sched_txn_command(
            new_acquire_pessimistic_lock_command(vec![(key.clone(), false)], 50, 50, true),
            expect_pessimistic_lock_res_callback(
                tx.clone(),
                PessimisticLockRes::Values(vec![Some(val.clone())]),
            ),
        )
        .unwrap();
    rx.recv().unwrap();
    fail::remove(scheduler_async_write_finish_fp);
    delete_pessimistic_lock(&storage, key.clone(), 50, 50);

    // The proposed callback, which is responsible for returning response, is not guaranteed to be
    // invoked. In this case it should still be continued properly.
    fail::cfg(before_pipelined_write_finish_fp, "return()").unwrap();
    storage
        .sched_txn_command(
            new_acquire_pessimistic_lock_command(
                vec![(key.clone(), false), (Key::from_raw(b"nonexist"), false)],
                60,
                60,
                true,
            ),
            expect_pessimistic_lock_res_callback(
                tx,
                PessimisticLockRes::Values(vec![Some(val), None]),
            ),
        )
        .unwrap();
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    fail::remove(before_pipelined_write_finish_fp);
    delete_pessimistic_lock(&storage, key, 60, 60);
}

#[test]
fn test_async_commit_prewrite_with_stale_max_ts() {
    let mut cluster = new_server_cluster(0, 2);
    cluster.run();

    let engine = cluster
        .sim
        .read()
        .unwrap()
        .storages
        .get(&1)
        .unwrap()
        .clone();
    let storage = TestStorageBuilder::<_, DummyLockManager>::from_engine_and_lock_mgr(
        engine.clone(),
        DummyLockManager {},
    )
    .build()
    .unwrap();

    // Fail to get timestamp from PD at first
    fail::cfg("test_raftstore_get_tso", "pause").unwrap();
    cluster.must_transfer_leader(1, new_peer(2, 2));
    cluster.must_transfer_leader(1, new_peer(1, 1));

    let mut ctx = Context::default();
    ctx.set_region_id(1);
    ctx.set_region_epoch(cluster.get_region_epoch(1));
    ctx.set_peer(cluster.leader_of_region(1).unwrap());

    let check_max_timestamp_not_synced = |expected: bool| {
        // prewrite
        let (prewrite_tx, prewrite_rx) = channel();
        storage
            .sched_txn_command(
                commands::Prewrite::new(
                    vec![Mutation::Put((Key::from_raw(b"k1"), b"v".to_vec()))],
                    b"k1".to_vec(),
                    10.into(),
                    100,
                    false,
                    2,
                    TimeStamp::default(),
                    TimeStamp::default(),
                    Some(vec![b"k2".to_vec()]),
                    false,
                    ctx.clone(),
                ),
                Box::new(move |res: storage::Result<_>| {
                    prewrite_tx.send(res).unwrap();
                }),
            )
            .unwrap();
        let res = prewrite_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let region_error = extract_region_error(&res);
        assert_eq!(
            region_error
                .map(|e| e.has_max_timestamp_not_synced())
                .unwrap_or(false),
            expected
        );

        // pessimistic prewrite
        let (prewrite_tx, prewrite_rx) = channel();
        storage
            .sched_txn_command(
                commands::PrewritePessimistic::new(
                    vec![(Mutation::Put((Key::from_raw(b"k1"), b"v".to_vec())), true)],
                    b"k1".to_vec(),
                    10.into(),
                    100,
                    20.into(),
                    2,
                    TimeStamp::default(),
                    TimeStamp::default(),
                    Some(vec![b"k2".to_vec()]),
                    false,
                    ctx.clone(),
                ),
                Box::new(move |res: storage::Result<_>| {
                    prewrite_tx.send(res).unwrap();
                }),
            )
            .unwrap();
        let res = prewrite_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let region_error = extract_region_error(&res);
        assert_eq!(
            region_error
                .map(|e| e.has_max_timestamp_not_synced())
                .unwrap_or(false),
            expected
        );
    };

    // should get max timestamp not synced error
    check_max_timestamp_not_synced(true);

    // can get timestamp from PD
    fail::remove("test_raftstore_get_tso");

    // wait for timestamp synced
    let snap_ctx = SnapContext {
        pb_ctx: &ctx,
        ..Default::default()
    };
    let snapshot = engine.snapshot(snap_ctx).unwrap();
    let max_ts_sync_status = snapshot.max_ts_sync_status.clone().unwrap();
    for retry in 0..10 {
        if max_ts_sync_status.load(Ordering::SeqCst) & 1 == 1 {
            break;
        }
        thread::sleep(Duration::from_millis(1 << retry));
    }
    assert!(snapshot.is_max_ts_synced());

    // should NOT get max timestamp not synced error
    check_max_timestamp_not_synced(false);
}

fn expect_locked(err: tikv::storage::Error, key: &[u8], lock_ts: TimeStamp) {
    let lock_info = extract_key_error(&err).take_locked();
    assert_eq!(lock_info.get_key(), key);
    assert_eq!(lock_info.get_lock_version(), lock_ts.into_inner());
}

fn test_async_apply_prewrite_impl<E: Engine>(
    storage: &Storage<E, DummyLockManager>,
    ctx: Context,
    key: &[u8],
    value: &[u8],
    start_ts: u64,
    commit_ts: Option<u64>,
    is_pessimistic: bool,
    need_lock: bool,
    use_async_commit: bool,
    expect_async_apply: bool,
) {
    let on_handle_apply = "on_handle_apply";

    let start_ts = TimeStamp::from(start_ts);

    // Acquire the pessimistic lock if needed
    if need_lock {
        let (tx, rx) = channel();
        storage
            .sched_txn_command(
                commands::AcquirePessimisticLock::new(
                    vec![(Key::from_raw(key), false)],
                    key.to_vec(),
                    start_ts,
                    0,
                    true,
                    start_ts,
                    None,
                    false,
                    0.into(),
                    ctx.clone(),
                ),
                Box::new(move |r| tx.send(r).unwrap()),
            )
            .unwrap();
        rx.recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap()
            .unwrap();
    }

    // Prewrite and block it at apply phase.
    fail::cfg(on_handle_apply, "pause").unwrap();
    let (tx, rx) = channel();
    let secondaries = if use_async_commit { Some(vec![]) } else { None };
    if !is_pessimistic {
        storage
            .sched_txn_command(
                commands::Prewrite::new(
                    vec![Mutation::Put((Key::from_raw(key), value.to_vec()))],
                    key.to_vec(),
                    start_ts,
                    0,
                    false,
                    1,
                    0.into(),
                    0.into(),
                    secondaries,
                    false,
                    ctx.clone(),
                ),
                Box::new(move |r| tx.send(r).unwrap()),
            )
            .unwrap();
    } else {
        storage
            .sched_txn_command(
                commands::PrewritePessimistic::new(
                    vec![(
                        Mutation::Put((Key::from_raw(key), value.to_vec())),
                        need_lock,
                    )],
                    key.to_vec(),
                    start_ts,
                    0,
                    start_ts,
                    1,
                    0.into(),
                    0.into(),
                    secondaries,
                    false,
                    ctx.clone(),
                ),
                Box::new(move |r| tx.send(r).unwrap()),
            )
            .unwrap();
    }

    if expect_async_apply {
        // The result should be able to be returned.
        let res = rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(res.locks.len(), 0);
        assert!(use_async_commit);
        assert!(commit_ts.is_none());
        let min_commit_ts = res.min_commit_ts;
        assert!(
            min_commit_ts > start_ts,
            "min_commit_ts({}) not greater than start_ts({})",
            min_commit_ts,
            start_ts
        );

        // The memory lock is not released so reading will encounter the lock.
        thread::sleep(Duration::from_millis(300));
        let err = block_on(storage.get(ctx.clone(), Key::from_raw(key), min_commit_ts.next()))
            .unwrap_err();
        expect_locked(err, key, start_ts);

        // Commit command will be blocked.
        let (tx, rx) = channel();
        storage
            .sched_txn_command(
                commands::Commit::new(
                    vec![Key::from_raw(key)],
                    start_ts,
                    min_commit_ts,
                    ctx.clone(),
                ),
                Box::new(move |r| tx.send(r).unwrap()),
            )
            .unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(300)).unwrap_err(),
            RecvTimeoutError::Timeout
        );

        // Continue applying and then the commit command can continue.
        fail::remove(on_handle_apply);
        rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();

        let got_value = block_on(storage.get(ctx, Key::from_raw(key), min_commit_ts.next()))
            .unwrap()
            .0;
        assert_eq!(got_value.unwrap().as_slice(), value);
    } else {
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(300)).unwrap_err(),
            RecvTimeoutError::Timeout
        );

        fail::remove(on_handle_apply);
        let res = rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        assert_eq!(res.locks.len(), 0);
        assert_eq!(res.min_commit_ts, 0.into());

        // Commit it.
        let commit_ts = commit_ts.unwrap().into();
        let (tx, rx) = channel();
        storage
            .sched_txn_command(
                commands::Commit::new(vec![Key::from_raw(key)], start_ts, commit_ts, ctx.clone()),
                Box::new(move |r| tx.send(r).unwrap()),
            )
            .unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();

        let got_value = block_on(storage.get(ctx, Key::from_raw(key), commit_ts.next()))
            .unwrap()
            .0;
        assert_eq!(got_value.unwrap().as_slice(), value);
    }
}

#[test]
fn test_async_apply_prewrite() {
    let mut cluster = new_server_cluster(0, 1);
    cluster.run();

    let engine = cluster
        .sim
        .read()
        .unwrap()
        .storages
        .get(&1)
        .unwrap()
        .clone();
    let storage = TestStorageBuilder::<_, DummyLockManager>::from_engine_and_lock_mgr(
        engine,
        DummyLockManager {},
    )
    .set_async_apply_prewrite(true)
    .build()
    .unwrap();

    let mut ctx = Context::default();
    ctx.set_region_id(1);
    ctx.set_region_epoch(cluster.get_region_epoch(1));
    ctx.set_peer(cluster.leader_of_region(1).unwrap());

    test_async_apply_prewrite_impl(
        &storage,
        ctx.clone(),
        b"key",
        b"value1",
        10,
        None,
        false,
        false,
        true,
        true,
    );
    test_async_apply_prewrite_impl(
        &storage,
        ctx.clone(),
        b"key",
        b"value2",
        20,
        None,
        true,
        false,
        true,
        true,
    );
    test_async_apply_prewrite_impl(
        &storage,
        ctx.clone(),
        b"key",
        b"value3",
        30,
        None,
        true,
        true,
        true,
        true,
    );

    test_async_apply_prewrite_impl(
        &storage,
        ctx.clone(),
        b"key",
        b"value1",
        40,
        Some(45),
        false,
        false,
        false,
        false,
    );
    test_async_apply_prewrite_impl(
        &storage,
        ctx.clone(),
        b"key",
        b"value2",
        50,
        Some(55),
        true,
        false,
        false,
        false,
    );
    test_async_apply_prewrite_impl(
        &storage,
        ctx,
        b"key",
        b"value3",
        60,
        Some(65),
        true,
        true,
        false,
        false,
    );
}

#[test]
fn test_async_apply_prewrite_fallback() {
    let mut cluster = new_server_cluster(0, 1);
    cluster.run();

    let engine = cluster
        .sim
        .read()
        .unwrap()
        .storages
        .get(&1)
        .unwrap()
        .clone();
    let storage = TestStorageBuilder::<_, DummyLockManager>::from_engine_and_lock_mgr(
        engine,
        DummyLockManager {},
    )
    .set_async_apply_prewrite(true)
    .build()
    .unwrap();

    let mut ctx = Context::default();
    ctx.set_region_id(1);
    ctx.set_region_epoch(cluster.get_region_epoch(1));
    ctx.set_peer(cluster.leader_of_region(1).unwrap());

    let before_async_apply_prewrite_finish = "before_async_apply_prewrite_finish";
    let on_handle_apply = "on_handle_apply";

    fail::cfg(before_async_apply_prewrite_finish, "return()").unwrap();
    fail::cfg(on_handle_apply, "pause").unwrap();

    let (key, value) = (b"k1", b"v1");
    let (tx, rx) = channel();
    storage
        .sched_txn_command(
            commands::Prewrite::new(
                vec![Mutation::Put((Key::from_raw(key), value.to_vec()))],
                key.to_vec(),
                10.into(),
                0,
                false,
                1,
                0.into(),
                0.into(),
                Some(vec![]),
                false,
                ctx.clone(),
            ),
            Box::new(move |r| tx.send(r).unwrap()),
        )
        .unwrap();

    assert_eq!(
        rx.recv_timeout(Duration::from_millis(200)).unwrap_err(),
        RecvTimeoutError::Timeout
    );

    fail::remove(on_handle_apply);

    let res = rx.recv().unwrap().unwrap();
    assert!(res.min_commit_ts > 10.into());

    fail::remove(before_async_apply_prewrite_finish);

    let (tx, rx) = channel();
    storage
        .sched_txn_command(
            commands::Commit::new(vec![Key::from_raw(key)], 10.into(), res.min_commit_ts, ctx),
            Box::new(move |r| tx.send(r).unwrap()),
        )
        .unwrap();

    rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
}

fn test_async_apply_prewrite_1pc_impl<E: Engine>(
    storage: &Storage<E, DummyLockManager>,
    ctx: Context,
    key: &[u8],
    value: &[u8],
    start_ts: u64,
    is_pessimistic: bool,
) {
    let on_handle_apply = "on_handle_apply";

    let start_ts = TimeStamp::from(start_ts);

    if is_pessimistic {
        let (tx, rx) = channel();
        storage
            .sched_txn_command(
                commands::AcquirePessimisticLock::new(
                    vec![(Key::from_raw(key), false)],
                    key.to_vec(),
                    start_ts,
                    0,
                    true,
                    start_ts,
                    None,
                    false,
                    0.into(),
                    ctx.clone(),
                ),
                Box::new(move |r| tx.send(r).unwrap()),
            )
            .unwrap();
        rx.recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap()
            .unwrap();
    }

    // Prewrite and block it at apply phase.
    fail::cfg(on_handle_apply, "pause").unwrap();
    let (tx, rx) = channel();
    if !is_pessimistic {
        storage
            .sched_txn_command(
                commands::Prewrite::new(
                    vec![Mutation::Put((Key::from_raw(key), value.to_vec()))],
                    key.to_vec(),
                    start_ts,
                    0,
                    false,
                    1,
                    0.into(),
                    0.into(),
                    None,
                    true,
                    ctx.clone(),
                ),
                Box::new(move |r| tx.send(r).unwrap()),
            )
            .unwrap();
    } else {
        storage
            .sched_txn_command(
                commands::PrewritePessimistic::new(
                    vec![(Mutation::Put((Key::from_raw(key), value.to_vec())), true)],
                    key.to_vec(),
                    start_ts,
                    0,
                    start_ts,
                    1,
                    0.into(),
                    0.into(),
                    None,
                    true,
                    ctx.clone(),
                ),
                Box::new(move |r| tx.send(r).unwrap()),
            )
            .unwrap();
    }

    let res = rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
    assert_eq!(res.locks.len(), 0);
    assert!(res.one_pc_commit_ts > start_ts);
    let commit_ts = res.one_pc_commit_ts;

    let err = block_on(storage.get(ctx.clone(), Key::from_raw(key), commit_ts.next())).unwrap_err();
    expect_locked(err, key, start_ts);

    fail::remove(on_handle_apply);
    // The key may need some time to be applied.
    for retry in 0.. {
        let res = block_on(storage.get(ctx.clone(), Key::from_raw(key), commit_ts.next()));
        match res {
            Ok(v) => {
                assert_eq!(v.0.unwrap().as_slice(), value);
                break;
            }
            Err(e) => expect_locked(e, key, start_ts),
        }

        if retry > 20 {
            panic!("the key is not applied for too long time");
        }
        thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn test_async_apply_prewrite_1pc() {
    let mut cluster = new_server_cluster(0, 1);
    cluster.run();

    let engine = cluster
        .sim
        .read()
        .unwrap()
        .storages
        .get(&1)
        .unwrap()
        .clone();
    let storage = TestStorageBuilder::<_, DummyLockManager>::from_engine_and_lock_mgr(
        engine,
        DummyLockManager {},
    )
    .set_async_apply_prewrite(true)
    .build()
    .unwrap();

    let mut ctx = Context::default();
    ctx.set_region_id(1);
    ctx.set_region_epoch(cluster.get_region_epoch(1));
    ctx.set_peer(cluster.leader_of_region(1).unwrap());

    test_async_apply_prewrite_1pc_impl(&storage, ctx.clone(), b"key", b"value1", 10, false);
    test_async_apply_prewrite_1pc_impl(&storage, ctx.clone(), b"key", b"value2", 20, true);
}
