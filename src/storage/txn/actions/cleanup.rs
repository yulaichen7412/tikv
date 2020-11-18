use crate::storage::mvcc::txn::MissingLockAction;
use crate::storage::mvcc::{
    metrics::{MVCC_CONFLICT_COUNTER, MVCC_DUPLICATE_CMD_COUNTER_VEC},
    ErrorInner, Key, MvccTxn, ReleasedLock, Result as MvccResult, TimeStamp,
};
use crate::storage::{Snapshot, TxnStatus};

use super::check_txn_status::check_txn_status_missing_lock;

/// Cleanup the lock if it's TTL has expired, comparing with `current_ts`. If `current_ts` is 0,
/// cleanup the lock without checking TTL. If the lock is the primary lock of a pessimistic
/// transaction, the rollback record is protected from being collapsed.
///
/// Returns the released lock. Returns error if the key is locked or has already been
/// committed.
pub fn cleanup<S: Snapshot>(
    txn: &mut MvccTxn<S>,
    key: Key,
    current_ts: TimeStamp,
    protect_rollback: bool,
) -> MvccResult<Option<ReleasedLock>> {
    fail_point!("cleanup", |err| Err(
        crate::storage::mvcc::txn::make_txn_error(err, &key, txn.start_ts,).into()
    ));

    match txn.reader.load_lock(&key)? {
        Some(ref lock) if lock.ts == txn.start_ts => {
            // If current_ts is not 0, check the Lock's TTL.
            // If the lock is not expired, do not rollback it but report key is locked.
            if !current_ts.is_zero() && lock.ts.physical() + lock.ttl >= current_ts.physical() {
                return Err(
                    ErrorInner::KeyIsLocked(lock.clone().into_lock_info(key.into_raw()?)).into(),
                );
            }

            let is_pessimistic_txn = !lock.for_update_ts.is_zero();
            txn.check_write_and_rollback_lock(key, lock, is_pessimistic_txn)
        }
        l => match check_txn_status_missing_lock(
            txn,
            key,
            l,
            MissingLockAction::rollback_protect(protect_rollback),
        )? {
            TxnStatus::Committed { commit_ts } => {
                MVCC_CONFLICT_COUNTER.rollback_committed.inc();
                Err(ErrorInner::Committed { commit_ts }.into())
            }
            TxnStatus::RolledBack => {
                // Return Ok on Rollback already exist.
                MVCC_DUPLICATE_CMD_COUNTER_VEC.rollback.inc();
                Ok(None)
            }
            TxnStatus::LockNotExist => Ok(None),
            _ => unreachable!(),
        },
    }
}

pub mod tests {
    use super::*;
    use crate::storage::mvcc::{Error as MvccError, MvccTxn};
    use crate::storage::Engine;
    use concurrency_manager::ConcurrencyManager;
    use kvproto::kvrpcpb::Context;
    use txn_types::TimeStamp;

    use crate::storage::mvcc::tests::write;
    #[cfg(test)]
    use crate::storage::{
        mvcc::tests::{
            must_get_rollback_protected, must_get_rollback_ts, must_locked, must_unlocked,
        },
        txn::commands::txn_heart_beat,
        txn::tests::{
            must_acquire_pessimistic_lock, must_pessimistic_prewrite_put, must_prewrite_put,
        },
        TestEngineBuilder,
    };

    pub fn must_succeed<E: Engine>(
        engine: &E,
        key: &[u8],
        start_ts: impl Into<TimeStamp>,
        current_ts: impl Into<TimeStamp>,
    ) {
        let ctx = Context::default();
        let snapshot = engine.snapshot(Default::default()).unwrap();
        let current_ts = current_ts.into();
        let cm = ConcurrencyManager::new(current_ts);
        let mut txn = MvccTxn::new(snapshot, start_ts.into(), true, cm);
        cleanup(&mut txn, Key::from_raw(key), current_ts, true).unwrap();
        write(engine, &ctx, txn.into_modifies());
    }

    pub fn must_err<E: Engine>(
        engine: &E,
        key: &[u8],
        start_ts: impl Into<TimeStamp>,
        current_ts: impl Into<TimeStamp>,
    ) -> MvccError {
        let snapshot = engine.snapshot(Default::default()).unwrap();
        let current_ts = current_ts.into();
        let cm = ConcurrencyManager::new(current_ts);
        let mut txn = MvccTxn::new(snapshot, start_ts.into(), true, cm);
        cleanup(&mut txn, Key::from_raw(key), current_ts, true).unwrap_err()
    }

    #[test]
    fn test_cleanup() {
        // Cleanup's logic is mostly similar to rollback, except the TTL check. Tests that not
        // related to TTL check should be covered by other test cases.
        let engine = TestEngineBuilder::new().build().unwrap();

        // Shorthand for composing ts.
        let ts = TimeStamp::compose;

        let (k, v) = (b"k", b"v");

        must_prewrite_put(&engine, k, v, k, ts(10, 0));
        must_locked(&engine, k, ts(10, 0));
        txn_heart_beat::tests::must_success(&engine, k, ts(10, 0), 100, 100);
        // Check the last txn_heart_beat has set the lock's TTL to 100.
        txn_heart_beat::tests::must_success(&engine, k, ts(10, 0), 90, 100);

        // TTL not expired. Do nothing but returns an error.
        must_err(&engine, k, ts(10, 0), ts(20, 0));
        must_locked(&engine, k, ts(10, 0));

        // Try to cleanup another transaction's lock. Does nothing.
        must_succeed(&engine, k, ts(10, 1), ts(120, 0));
        // If there is no exisiting lock when cleanup, it may be a pessimistic transaction,
        // so the rollback should be protected.
        must_get_rollback_protected(&engine, k, ts(10, 1), true);
        must_locked(&engine, k, ts(10, 0));

        // TTL expired. The lock should be removed.
        must_succeed(&engine, k, ts(10, 0), ts(120, 0));
        must_unlocked(&engine, k);
        // Rollbacks of optimistic transactions needn't be protected
        must_get_rollback_protected(&engine, k, ts(10, 0), false);
        must_get_rollback_ts(&engine, k, ts(10, 0));

        // Rollbacks of primary keys in pessimistic transactions should be protected
        must_acquire_pessimistic_lock(&engine, k, k, ts(11, 1), ts(12, 1));
        must_succeed(&engine, k, ts(11, 1), ts(120, 0));
        must_get_rollback_protected(&engine, k, ts(11, 1), true);

        must_acquire_pessimistic_lock(&engine, k, k, ts(13, 1), ts(14, 1));
        must_pessimistic_prewrite_put(&engine, k, v, k, ts(13, 1), ts(14, 1), true);
        must_succeed(&engine, k, ts(13, 1), ts(120, 0));
        must_get_rollback_protected(&engine, k, ts(13, 1), true);
    }
}
