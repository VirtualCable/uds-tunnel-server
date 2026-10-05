// BSD 3-Clause License
// Copyright (c) 2026, Virtual Cable S.L.
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are met:
//
// 1. Redistributions of source code must retain the above copyright notice,
//    this list of conditions and the following disclaimer.
//
// 2. Redistributions in binary form must reproduce the above copyright notice,
//    this list of conditions and the following disclaimer in the documentation
//    and/or other materials provided with the distribution.
//
// 3. Neither the name of the copyright holder nor the names of its contributors
//    may be used to endorse or promote products derived from this software
//    without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
// AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
// IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
// DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
// FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
// DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
// SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
// CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
// OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
// OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

// Authors: Adolfo Gómez, dkmaster at dkmon dot com
use std::sync::{Arc, Condvar, Mutex};

use anyhow::Result;
use tokio::sync::Notify;

#[derive(Clone, Debug)]
pub struct Trigger {
    state: Arc<(Mutex<bool>, Condvar, Notify)>,
}

// unsafe impl Send for Trigger {}
// unsafe impl Sync for Trigger {}

impl Trigger {
    pub fn new() -> Self {
        Trigger {
            state: Arc::new((Mutex::new(false), Condvar::new(), Notify::new())),
        }
    }

    pub fn trigger(&self) {
        let (lock, cvar, notify) = &*self.state;
        let mut guard = lock.lock().unwrap();
        *guard = true;
        cvar.notify_all();
        notify.notify_waiters();
    }

    pub fn trigger_after(&self, delay: std::time::Duration) {
        let trigger = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            trigger.trigger();
        });
    }

    pub fn is_triggered(&self) -> bool {
        let (lock, _, _) = &*self.state;
        *lock.lock().unwrap()
    }

    pub fn wait(&self) {
        let (lock, cvar, _) = &*self.state;
        let mut guard = lock.lock().unwrap();
        while !*guard {
            guard = cvar.wait(guard).unwrap();
        }
    }

    pub fn wait_timeout(&self, timeout: std::time::Duration) -> Result<()> {
        let (lock, cvar, _) = &*self.state;
        let triggered = lock.lock().unwrap();
        let (guard, _result) = cvar
            .wait_timeout_while(triggered, timeout, |t| !*t)
            .unwrap();
        if *guard {
            Ok(())
        } else {
            Err(anyhow::anyhow!("Timeout"))
        }
    }

    pub async fn is_triggered_async(&self) -> bool {
        let (lock, _, _) = &*self.state;
        *lock.lock().unwrap()
    }

    pub async fn wait_async(&self) {
        let (lock, _, notify) = &*self.state;
        // Arm the waiter BEFORE reading the flag. `notify_waiters()` stores
        // no permit, and `notified()` snapshots its broadcast counter at
        // creation, so a `trigger()` landing between the flag check and the
        // creation of the `Notified` future is invisible to the waiter and
        // this wait would hang forever. Creating the future first closes
        // that window; `enable()` additionally registers it on the notify
        // list before the check (belt and braces across tokio versions),
        // and the flag check still short-circuits the common case.
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if *lock.lock().unwrap() {
            return;
        }
        notified.await;
    }

    pub async fn wait_timeout_async(&self, timeout: std::time::Duration) -> Result<()> {
        let (lock, _, notify) = &*self.state;
        // Same arm-before-check ordering as `wait_async`.
        let notified = notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if *lock.lock().unwrap() {
            return Ok(());
        }
        tokio::select! {
            _ = notified => Ok(()),
            _ = tokio::time::sleep(timeout) => Err(anyhow::anyhow!("Timeout")),
        }
    }

    pub async fn trigger_after_async(&self, delay: std::time::Duration) {
        tokio::task::spawn({
            let trigger = self.clone();
            async move {
                tokio::time::sleep(delay).await;
                trigger.trigger();
            }
        });
    }
}

impl Default for Trigger {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn trigger_wait_blocks_until_set() {
        let trigger = Trigger::new();
        let trigger_clone = trigger.clone();
        let handle = thread::spawn(move || {
            // Wait 100ms and then set the trigger
            thread::sleep(Duration::from_millis(100));
            trigger_clone.trigger();
        });
        handle.join().unwrap();
        trigger.wait();
    }

    #[test]
    fn trigger_wait_timeout() {
        let trigger = Trigger::new();
        let result = trigger.wait_timeout(Duration::from_millis(100));
        assert!(result.is_err());
    }

    /// `Trigger::wait_async` checks the flag, then awaits `Notify::notified()`. A
    /// `trigger()` landing in that gap must still wake the waiter, otherwise a
    /// stop signal is silently lost (hung task / leaked session).
    #[test]
    fn trigger_async_wakeup_never_lost() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            let mut lost = 0usize;
            for i in 0..20000u64 {
                let t = Trigger::new();
                let waiter = {
                    let t = t.clone();
                    tokio::spawn(async move { t.wait_async().await })
                };
                // Vary the interleaving: sometimes trigger immediately, sometimes
                // after a yield, sometimes from a separate task.
                match i % 3 {
                    0 => t.trigger(),
                    1 => {
                        tokio::task::yield_now().await;
                        t.trigger();
                    }
                    _ => {
                        let t2 = t.clone();
                        tokio::spawn(async move { t2.trigger() });
                    }
                }
                if tokio::time::timeout(std::time::Duration::from_secs(3), waiter)
                    .await
                    .is_err()
                {
                    lost += 1;
                }
            }
            assert_eq!(
                lost, 0,
                "{lost} wait_async() calls never observed the trigger"
            );
        });
    }

    /// The fix's ordering, pinned deterministically: arm the waiter
    /// (create + `enable()`), THEN read the flag, and let the trigger land
    /// between the check and the final await. An armed waiter sits in
    /// `Notify`'s wait list, so `notify_waiters()` wakes it and the wait
    /// completes.
    #[tokio::test]
    async fn armed_waiter_observes_a_trigger_landing_after_the_flag_check() {
        use std::sync::{Arc, Mutex};
        use tokio::sync::Notify;

        let notify = Arc::new(Notify::new());
        let flag = Arc::new(Mutex::new(false));

        let mut notified = Box::pin(notify.notified());
        notified.as_mut().enable(); // arm BEFORE the check
        assert!(!*flag.lock().unwrap(), "not triggered yet -> would await");
        // trigger() runs while the waiter is between check and await:
        *flag.lock().unwrap() = true;
        notify.notify_waiters();
        let woke = tokio::time::timeout(std::time::Duration::from_millis(200), notified)
            .await
            .is_ok();
        assert!(woke, "an armed waiter must observe the notification");
    }

    /// Deterministic reproduction of the defect: the old ordering read the
    /// flag first and created the `Notified` afterwards. A `trigger()` that
    /// lands between the flag check and the future creation is invisible to
    /// the waiter — `notify_waiters()` stores no permit, and the future's
    /// broadcast counter is snapshotted at creation, after the notification
    /// already happened — so the await hangs forever. This pins the window
    /// the arm-before-check ordering in `wait_async` closes, and stays
    /// version-independent (it does not rely on the runtime detecting a
    /// broadcast between creation and the first poll).
    #[tokio::test]
    async fn check_before_create_ordering_loses_a_trigger_landing_in_the_gap() {
        use std::sync::{Arc, Mutex};
        use tokio::sync::Notify;

        let notify = Arc::new(Notify::new());
        let flag = Arc::new(Mutex::new(false));

        // 1. old buggy ordering: check the flag first...
        let triggered = { *flag.lock().unwrap() };
        assert!(!triggered, "not triggered yet -> would await");
        // 2. trigger() lands in the gap (flag set, broadcast fired)
        //    before the waiter creates its future...
        *flag.lock().unwrap() = true;
        notify.notify_waiters();
        // 3. ...and only now the waiter creates the Notified and awaits it.
        //    The creation snapshot misses the broadcast: lost wakeup.
        let notified = notify.notified();
        let woke = tokio::time::timeout(std::time::Duration::from_millis(200), notified)
            .await
            .is_ok();
        assert!(
            !woke,
            "a waiter created after the broadcast must miss it (defect control)"
        );
    }

    /// A pre-set flag must still short-circuit `wait_async` immediately even
    /// though the waiter arms itself first.
    #[tokio::test]
    async fn wait_async_returns_immediately_when_already_triggered() {
        let t = Trigger::new();
        t.trigger();
        tokio::time::timeout(std::time::Duration::from_millis(200), t.wait_async())
            .await
            .expect("pre-triggered flag must short-circuit the armed wait");
    }

    #[test]
    fn trigger_is_set() {
        let trigger = Trigger::new();
        assert!(!trigger.is_triggered());
        trigger.trigger();
        assert!(trigger.is_triggered());
        trigger.wait(); // Should return immediately
        assert!(trigger.is_triggered());
    }
}
