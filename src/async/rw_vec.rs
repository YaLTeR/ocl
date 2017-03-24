//! A mutex-like lock which can be shared between threads and can interact
//! with OpenCL events.
//!
//! 
//! 

extern crate qutex;

use std::ops::{Deref, DerefMut};
use futures::{Future, Poll, Async};
use futures::sync::oneshot;
use core::ClContextPtr;
use ::{Event, EventList};
use async::{Error as AsyncError, Result as AsyncResult};
pub use self::qutex::{Request, Guard, FutureGuard, Qutex};

const PRINT_DEBUG: bool = false;

/// Allows access to the data contained within a lock just like a mutex guard.
pub struct RwGuard<T> {
    rw_vec: RwVec<T>,
    unlock_event: Option<Event>,
}

impl<T> RwGuard<T> {
    /// Returns a new `RwGuard`.
    fn new(rw_vec: RwVec<T>, unlock_event: Option<Event>) -> RwGuard<T> {
        RwGuard {
            rw_vec: rw_vec,
            unlock_event: unlock_event,
        }
    }

    /// Triggers the unlock event and releases the lock held by this `RwGuard`
    /// before returning the original `RwVec`.
    pub fn unlock(self) -> RwVec<T> {
        self.rw_vec.clone()
    }

    /// Returns a reference to the event previously set using
    /// `create_unlock_event` on the `FutureRwGuard` which preceeded this
    /// `RwGuard`. The event can be manually 'triggered' by calling
    /// `...set_complete()...` or used normally (as a wait event) by
    /// subsequent commands. If the event is not manually completed it will be
    /// automatically set complete when this `RwGuard` is dropped.
    pub fn unlock_event(&self) -> Option<&Event> {
        self.unlock_event.as_ref()
    }

    /// Triggers the unlock event by setting it complete.
    fn complete_unlock_event(&self) {
        if let Some(ref e) = self.unlock_event {
            if !e.is_complete().expect("ReadCompletion::drop") {
                e.set_complete().expect("ReadCompletion::drop");
            }
        }
    }
}

impl<T> Deref for RwGuard<T> {
    type Target = Vec<T>;

    fn deref(&self) -> &Vec<T> {
        unsafe { &*self.rw_vec.qutex.as_ptr() }
    }
}

impl<T> DerefMut for RwGuard<T> {
    fn deref_mut(&mut self) -> &mut Vec<T> {
        unsafe { &mut *self.rw_vec.qutex.as_mut_ptr() }
    }
}

impl<T> Drop for RwGuard<T> {
    fn drop(&mut self) {
        // println!("Dropping and unlocking RwGuard.");
        unsafe { self.rw_vec.qutex.direct_unlock() };
        self.complete_unlock_event();
    }
}


/// The polling stage of a `FutureRwGuard`.
#[derive(PartialEq)]
enum Stage {
    Marker,
    Qutex,
    Command,
}


/// A future that resolves to an `RwGuard` after ensuring that the data being
/// guarded is appropriately locked during the execution of an OpenCL command.
///
/// 1. Waits until both an exclusive data lock can be obtained **and** all
///    prerequisite OpenCL commands have completed.
/// 2. Triggers an OpenCL command, remaining locked until the command finishes
///    execution.
/// 3. Returns an `RwGuard` which provides exclusive access to the locked
///    data.
/// 
#[must_use = "futures do nothing unless polled"]
pub struct FutureRwGuard<T> {
    rw_vec: Option<RwVec<T>>,
    rx: oneshot::Receiver<()>,
    /// Bring this back if we decide to switch back to an event marker, which
    /// is probably more efficient than the list: 
    // wait_event: Option<Event>
    wait_list: Option<EventList>,
    lock_event: Option<Event>,
    command_completion: Option<Event>,
    unlock_event: Option<Event>,
    stage: Stage,
}

impl<T> FutureRwGuard<T> {
    pub fn new(rw_vec: RwVec<T>, rx: oneshot::Receiver<()>) 
            -> FutureRwGuard<T>
    {
        FutureRwGuard {
            rw_vec: Some(rw_vec),
            rx: rx,
            // wait_event: None,
            wait_list: None,
            lock_event: None,
            command_completion: None,
            unlock_event: None,
            stage: Stage::Marker,
        }
    }

    //////// DO NOT REMOVE (evaluate bringing back):
        // /// Sets a wait event.
        // ///
        // /// Setting a wait event will cause this `FutureRwGuard` to wait until
        // /// that event has its status set to complete (by polling it like any
        // /// other future) before obtaining a lock on the guarded internal `Vec`.
        // ///
        // /// If multiple wait events need waiting on, add them to an `EventList`
        // /// and enqueue a marker or create an array and use the `IntoMarker`
        // /// trait to produce a marker which can be passed here.
        // pub fn set_wait_event(&mut self, wait_event: Event) {
        //     self.wait_event = Some(wait_event)
        // }
    /////////

    /// Sets an event wait list.
    ///
    /// Setting a wait list will cause this `FutureRwGuard` to wait until
    /// contained events have their status set to complete before obtaining a
    /// lock on the guarded internal `Vec`.
    pub fn set_wait_list<L: Into<EventList>>(&mut self, wait_list: L) {
        self.wait_list = Some(wait_list.into());
    }

    /// Sets a command completion event.
    ///
    /// If a command completion event corresponding to the read or write
    /// command being executed in association with this `FutureRwGuard` is
    /// specified before this `FutureRwGuard` is polled it will cause this
    /// `FutureRwGuard` to suffix itself with an additional future that will
    /// wait until the command completion event completes before resolving
    /// into an `RwGuard`.
    ///
    /// Not specifying a command completion event will cause this
    /// `FutureRwGuard` to resolve into an `RwGuard` immediately after the
    /// lock is obtained (indicated by the optionally created lock event).
    pub fn set_command_completion_event(&mut self, command_completion: Event) {
        self.command_completion = Some(command_completion);
    }

    /// Creates an event which will be triggered when a lock is obtained on
    /// the guarded internal `Vec`.
    ///
    /// The returned event can be added to the wait list of subsequent OpenCL
    /// commands with the expectation that when all preceeding futures are
    /// complete, the event will automatically be 'triggered' by having its
    /// status set to complete, causing those commands to execute. This can be
    /// used to inject host side code in amongst OpenCL commands without
    /// thread blocking or extra delays of any kind.
    pub fn create_lock_event<C: ClContextPtr>(&mut self, context: C) -> AsyncResult<&Event> {
        let lock_event = Event::user(context)?;
        self.lock_event = Some(lock_event);
        Ok(self.lock_event.as_mut().unwrap())
    }

    /// Creates an event which will be triggered after this future resolves
    /// **and** the ensuing `RwGuard` is dropped or manually unlocked.
    ///    
    /// The returned event can be added to the wait list of subsequent OpenCL
    /// commands with the expectation that when all preceeding futures are
    /// complete, the event will automatically be 'triggered' by having its
    /// status set to complete, causing those commands to execute. This can be
    /// used to inject host side code in amongst OpenCL commands without
    /// thread blocking or extra delays of any kind.
    pub fn create_unlock_event<C: ClContextPtr>(&mut self, context: C) -> AsyncResult<&Event> {
        let uev = Event::user(context)?;
        self.unlock_event = Some(uev);
        Ok(self.unlock_event.as_ref().unwrap())
    }

    /// Returns a reference to the event previously created with
    /// `::create_lock_event` which will trigger (be completed) when the wait
    /// events are complete and the qutex is locked.
    pub fn lock_event(&self) -> Option<&Event> {
        self.lock_event.as_ref()
    }

    /// Returns a reference to the event previously created with
    /// `::create_unlock_event` which will trigger (be completed) when a lock
    /// is obtained on the guarded internal `Vec`.
    pub fn unlock_event(&self) -> Option<&Event> {
        self.unlock_event.as_ref()
    }

    /// Blocks the current thread until the OpenCL command is complete and a 
    pub fn wait(self) -> AsyncResult<RwGuard<T>> {
        <Self as Future>::wait(self)
    }

    /// Returns a mutable pointer to the data contained within the internal
    /// `Vec`, bypassing all locks and protections.
    pub unsafe fn as_mut_ptr(&self) -> Option<*mut T> {
        self.rw_vec.as_ref().map(|rw_vec| (*rw_vec.qutex.as_mut_ptr()).as_mut_ptr())
    }

    /// Returns a mutable slice to the data contained within the internal
    /// `Vec`, bypassing all locks and protections.
    pub unsafe fn as_mut_slice<'a, 'b>(&'a self) -> Option<&'b mut [T]> {
        self.as_mut_ptr().map(|ptr| {
            ::std::slice::from_raw_parts_mut(ptr, self.len())
        })
    }

    /// Returns the length of the internal `Vec`.
    pub fn len(&self) -> usize {
        unsafe { (*self.rw_vec.as_ref().expect("FutureRwGuard::len: No RwVec found.")
            .qutex.as_ptr()).len() }
    }

    /// Polls the wait events until all requisite commands have completed then
    /// polls the qutex queue.
    fn poll_wait_events(&mut self) -> AsyncResult<Async<RwGuard<T>>> {
        debug_assert!(self.stage == Stage::Marker);
        if PRINT_DEBUG { println!("###### FutureRwGuard::poll_wait_events (thread: {})...",
            ::std::thread::current().name().unwrap_or("<unnamed>")); }

        // Check completion of wait list, if it exists:
        if let Some(ref mut wait_list) = self.wait_list {
            if PRINT_DEBUG { println!("###### FutureRwGuard::poll_wait_events: Polling wait_events (thread: {})...",
                ::std::thread::current().name().unwrap_or("<unnamed>")); }

            if let Async::NotReady = wait_list.poll()? {
                return Ok(Async::NotReady);
            }
            
        }

        self.stage = Stage::Qutex;
        self.poll_qutex()
    }

    /// Polls the qutex until we have obtained a lock then polls the command
    /// event.
    #[cfg(not(feature = "async_block"))]
    fn poll_qutex(&mut self) -> AsyncResult<Async<RwGuard<T>>> {        
        debug_assert!(self.stage == Stage::Qutex);
        // println!("###### FutureRwGuard::poll_qutex: called.");

        // Move the queue along:
        unsafe { self.rw_vec.as_ref().unwrap().qutex.process_queue(); }

        // Check for completion of the rx:
        match self.rx.poll() {
            // If the poll returns `Async::Ready`, we have been popped from
            // the front of the qutex queue and we now have exclusive access.
            // Otherwise, return the `NotReady`. The rx (oneshot channel) will
            // arrange for this task to be awakened when it's ready.
            Ok(status) => {
                if PRINT_DEBUG { println!("###### FutureRwGuard::poll_qutex: status: {:?}, (thread: {}).", 
                    status, ::std::thread::current().name().unwrap_or("<unnamed>")); }
                match status {
                    Async::Ready(_) => {
                        if let Some(ref lock_event) = self.lock_event {
                            // // Sleeping before locking synchronizes something
                            // // on certain hardware which can prevent weird
                            // // behavior (such as out-of-date data from a
                            // // load). Unknown if this is OpenCL-specific or
                            // // Intel-specific or what. This mimics what a
                            // // thread-mutex generally does in practice so I'm
                            // // sure there's a good explanation for this.
                            // ::std::thread::sleep(::std::time::Duration::new(0, 1));
                            lock_event.set_complete()?
                        }
                        self.stage = Stage::Command;
                        self.poll_command()
                    },
                    Async::NotReady => Ok(Async::NotReady),
                }
            },
            Err(e) => return Err(e.into()),
        }
    }


    /// Polls the qutex until we have obtained a lock then polls the command
    /// event.
    #[cfg(feature = "async_block")]
    fn poll_qutex(&mut self) -> AsyncResult<Async<RwGuard<T>>> {        
        debug_assert!(self.stage == Stage::Qutex);
        // println!("###### FutureRwGuard::poll_qutex: called.");

        // Move the queue along:
        unsafe { self.rw_vec.as_ref().unwrap().qutex.process_queue()
            .expect("###### Error polling FutureRwGuard"); }

        // Loop until completion of the rx:
        loop {
            match self.rx.poll() {
                // If the poll returns `Async::Ready`, we have been popped from
                // the front of the qutex queue and we now have exclusive access.
                // Otherwise, return the `NotReady`. The rx (oneshot channel) will
                // arrange for this task to be awakened when it's ready.
                Ok(status) => {
                    if PRINT_DEBUG { println!("###### FutureRwGuard::poll_qutex: status: {:?}, (thread: {}).", status,
                        ::std::thread::current().name().unwrap_or("<unnamed>")); }

                    match status {
                        Async::Ready(_) => {
                            if let Some(ref lock_event) = self.lock_event {
                                lock_event.set_complete()?
                            }
                            self.stage = Stage::Command;
                            if PRINT_DEBUG { println!("###### FutureRwGuard::poll_qutex: Moving to command stage."); }
                            return self.poll_command();
                        },
                        Async::NotReady => {
                            if PRINT_DEBUG { println!("###### FutureRwGuard::poll_qutex: Parking thread."); }
                            ::std::thread::sleep(::std::time::Duration::from_millis(10));
                        },
                    }
                },
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Polls the command event until it is complete then returns an `RwGuard`
    /// which can be safely accessed immediately.
    fn poll_command(&mut self) -> AsyncResult<Async<RwGuard<T>>> {
        debug_assert!(self.stage == Stage::Command);
        if PRINT_DEBUG { println!("###### FutureRwGuard::poll_command (thread: {})...",
            ::std::thread::current().name().unwrap_or("<unnamed>")); }

        if let Some(ref mut command_completion) = self.command_completion {
            if PRINT_DEBUG { println!("###### FutureRwGuard::poll_command: Polling command completion event (thread: {}).",
                ::std::thread::current().name().unwrap_or("<unnamed>")); }

            if let Async::NotReady = command_completion.poll()? {
                return Ok(Async::NotReady);
            }
        }

        if PRINT_DEBUG { println!("###### FutureRwGuard::poll_command: All polling complete (thread: {}).", 
            ::std::thread::current().name().unwrap_or("<unnamed>")); }

        Ok(Async::Ready(RwGuard::new(self.rw_vec.take().unwrap(), self.unlock_event.take())))
    }
}

impl<T> Future for FutureRwGuard<T> {
    type Item = RwGuard<T>;
    type Error = AsyncError;

    #[inline]
    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        if self.rw_vec.is_some() {
            match self.stage {
                Stage::Marker => self.poll_wait_events(),
                Stage::Qutex => self.poll_qutex(),
                Stage::Command => self.poll_command(),
            }            
        } else {
            Err("FutureRwGuard::poll: Task already completed.".into())
        }
    }
}


/// A locking `Vec` which interoperates with OpenCL events and Rust futures to
/// provide exclusive access to data.
///
/// Calling `::lock` or `::request_lock` returns a future which will
/// resolve into a `RwGuard`.
///
/// ## Platform Compatibility
///
/// Some CPU device/platform combinations have synchronization problems when
/// accessing an `RwVec` from multiple threads. Known platforms with problems
/// are 2nd and 4th gen Intel Core processors (Sandy Bridge and Haswell) with
/// Intel OpenCL CPU drivers. Others may be likewise affected. Run the
/// `device_check.rs` example to determine if your device/platform is
/// affected. AMD platform drivers are known to work properly on the
/// aforementioned CPUs so use those instead if possible.
///
pub struct RwVec<T> {
    qutex: Qutex<Vec<T>>,
}

impl<T> RwVec<T> {
    /// Creates and returns a new `RwVec`.
    #[inline]
    pub fn new() -> RwVec<T> {
        RwVec {
            qutex: Qutex::new(Vec::new())
        }
    }

    /// Returns a new `FutureRwGuard` which will resolve into a a `RwGuard`.
    pub fn request_lock(self) -> FutureRwGuard<T> {
        if PRINT_DEBUG { println!("RwVec::request_lock: Lock requested."); }
        let (tx, rx) = oneshot::channel();
        unsafe { self.qutex.push_request(Request::new(tx)); }
        FutureRwGuard::new(self.into(), rx)
    }

    pub unsafe fn as_mut_slice(&self) -> &mut [T] {
        let ptr = (*self.qutex.as_mut_ptr()).as_mut_ptr();
        let len = (*self.qutex.as_ptr()).len();
        ::std::slice::from_raw_parts_mut(ptr, len)
    }
}

impl<T> From<Qutex<Vec<T>>> for RwVec<T> {
    fn from(q: Qutex<Vec<T>>) -> RwVec<T> {
        RwVec { qutex: q }
    }
}

impl<T> From<Vec<T>> for RwVec<T> {
    fn from(vec: Vec<T>) -> RwVec<T> {
        RwVec { qutex: Qutex::new(vec) }
    }
}

impl<T> Clone for RwVec<T> {
    #[inline]
    fn clone(&self) -> RwVec<T> {
        RwVec {
            qutex: self.qutex.clone(),
        }
    }
}