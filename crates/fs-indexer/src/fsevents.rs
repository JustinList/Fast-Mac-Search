use std::path::PathBuf;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded};
use serde::{Deserialize, Serialize};

/// Monotonically increasing macOS `FSEventStreamEventId` (`kFSEventStreamEventIdSinceNow` = `u64::MAX`).
pub const FSEVENT_SINCE_NOW: u64 = 0xFFFF_FFFF_FFFF_FFFF;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FsEventAction {
    Upsert,
    Remove,
    RescanSubtree,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsDeltaEvent {
    pub event_id: u64,
    pub path: PathBuf,
    pub action: FsEventAction,
    pub is_dir: bool,
}

/// Active handle to a background macOS `FSEventStream` running on a serial Grand Central Dispatch queue.
pub struct FsEventsWatcher {
    rx: Receiver<FsDeltaEvent>,
    #[cfg(target_os = "macos")]
    #[expect(dead_code, reason = "RAII guard keeping the GCD FSEventStream alive")]
    inner: macos_fsevents::StreamGuard,
}

impl FsEventsWatcher {
    /// Starts watching `roots` via macOS `FSEvents`, replaying historical kernel events since `since_event_id`
    /// (pass [`FSEVENT_SINCE_NOW`] for live events only).
    pub fn start(
        roots: &[PathBuf],
        since_event_id: u64,
        coalesce_latency: Duration,
    ) -> std::io::Result<Self> {
        let (tx, rx) = bounded(16_384);

        #[cfg(target_os = "macos")]
        {
            let inner =
                macos_fsevents::StreamGuard::start(roots, since_event_id, coalesce_latency, tx)?;
            Ok(Self { rx, inner })
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = (roots, since_event_id, coalesce_latency, tx);
            Ok(Self { rx })
        }
    }

    pub fn receiver(&self) -> &Receiver<FsDeltaEvent> {
        &self.rx
    }

    /// Queries the current system-wide `FSEventStreamEventId` checkpoint from macOS `CoreServices`.
    pub fn current_system_event_id() -> u64 {
        #[cfg(target_os = "macos")]
        {
            macos_fsevents::current_event_id()
        }
        #[cfg(not(target_os = "macos"))]
        {
            0
        }
    }
}

#[cfg(target_os = "macos")]
mod macos_fsevents {
    use super::*;
    use std::ffi::{CStr, c_char, c_void};
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::ptr;

    type CFAllocatorRef = *const c_void;
    type CFArrayRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFTypeRef = *const c_void;
    type FSEventStreamRef = *mut c_void;
    type ConstFSEventStreamRef = *const c_void;
    type DispatchQueueT = *mut c_void;

    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;

    const K_FSEVENT_CREATE_FLAG_NO_DEFER: u32 = 0x0000_0002;
    const K_FSEVENT_CREATE_FLAG_FILE_EVENTS: u32 = 0x0000_0010;

    const FLAG_MUST_SCAN_SUBDIRS: u32 = 0x0000_0001;
    const FLAG_HISTORY_DONE: u32 = 0x0000_0010;
    const FLAG_ITEM_CREATED: u32 = 0x0000_0100;
    const FLAG_ITEM_REMOVED: u32 = 0x0000_0200;
    const FLAG_ITEM_RENAMED: u32 = 0x0000_0800;
    const FLAG_ITEM_MODIFIED: u32 = 0x0000_1000;
    const FLAG_ITEM_IS_DIR: u32 = 0x0002_0000;

    #[repr(C)]
    struct FSEventStreamContext {
        version: isize,
        info: *mut c_void,
        retain: Option<unsafe extern "C" fn(*const c_void) -> *const c_void>,
        release: Option<unsafe extern "C" fn(*const c_void)>,
        copy_description: Option<unsafe extern "C" fn(*const c_void) -> CFStringRef>,
    }

    type FSEventStreamCallback = unsafe extern "C" fn(
        stream_ref: ConstFSEventStreamRef,
        client_call_back_info: *mut c_void,
        num_events: usize,
        event_paths: *mut c_void,
        event_flags: *const u32,
        event_ids: *const u64,
    );

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFTypeArrayCallBacks: c_void;

        fn CFStringCreateWithBytes(
            alloc: CFAllocatorRef,
            bytes: *const u8,
            num_bytes: isize,
            encoding: u32,
            is_external: u8,
        ) -> CFStringRef;
        fn CFArrayCreate(
            allocator: CFAllocatorRef,
            values: *const *const c_void,
            num_values: isize,
            call_backs: *const c_void,
        ) -> CFArrayRef;
        fn CFRelease(cf: CFTypeRef);
    }

    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        fn FSEventStreamCreate(
            allocator: CFAllocatorRef,
            callback: FSEventStreamCallback,
            context: *mut FSEventStreamContext,
            paths_to_watch: CFArrayRef,
            since_when: u64,
            latency: f64,
            flags: u32,
        ) -> FSEventStreamRef;
        fn FSEventStreamSetDispatchQueue(stream_ref: FSEventStreamRef, q: DispatchQueueT);
        fn FSEventStreamStart(stream_ref: FSEventStreamRef) -> u8;
        fn FSEventStreamStop(stream_ref: FSEventStreamRef);
        fn FSEventStreamInvalidate(stream_ref: FSEventStreamRef);
        fn FSEventStreamRelease(stream_ref: FSEventStreamRef);
        fn FSEventsGetCurrentEventId() -> u64;
    }

    unsafe extern "C" {
        fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> DispatchQueueT;
        fn dispatch_release(object: *mut c_void);
    }

    struct CallbackCtx {
        tx: Sender<FsDeltaEvent>,
    }

    pub struct StreamGuard {
        stream: FSEventStreamRef,
        queue: DispatchQueueT,
        ctx_ptr: *mut CallbackCtx,
    }

    // FSEventStreamRef + serial GCD queue are thread-safe to manage across threads
    unsafe impl Send for StreamGuard {}
    unsafe impl Sync for StreamGuard {}

    pub fn current_event_id() -> u64 {
        unsafe { FSEventsGetCurrentEventId() }
    }

    unsafe extern "C" fn fsevents_callback(
        _stream_ref: ConstFSEventStreamRef,
        client_info: *mut c_void,
        num_events: usize,
        event_paths: *mut c_void,
        event_flags: *const u32,
        event_ids: *const u64,
    ) {
        if client_info.is_null() || event_paths.is_null() {
            return;
        }
        let ctx = unsafe { &*(client_info as *const CallbackCtx) };
        let c_paths = event_paths as *const *const c_char;

        for i in 0..num_events {
            let flags = unsafe { *event_flags.add(i) };
            if (flags & FLAG_HISTORY_DONE) != 0 {
                continue;
            }
            let event_id = unsafe { *event_ids.add(i) };
            let path_ptr = unsafe { *c_paths.add(i) };
            if path_ptr.is_null() {
                continue;
            }
            let c_str = unsafe { CStr::from_ptr(path_ptr) };
            let path = PathBuf::from(std::ffi::OsStr::from_bytes(c_str.to_bytes()));
            let is_dir = (flags & FLAG_ITEM_IS_DIR) != 0;

            let action = if (flags & FLAG_MUST_SCAN_SUBDIRS) != 0 {
                FsEventAction::RescanSubtree
            } else if (flags & FLAG_ITEM_REMOVED) != 0 && !path.exists() {
                FsEventAction::Remove
            } else if (flags & FLAG_ITEM_RENAMED) != 0 {
                if path.exists() {
                    FsEventAction::Upsert
                } else {
                    FsEventAction::Remove
                }
            } else if (flags & (FLAG_ITEM_CREATED | FLAG_ITEM_MODIFIED)) != 0 || path.exists() {
                FsEventAction::Upsert
            } else {
                FsEventAction::Remove
            };

            let _ = ctx.tx.try_send(FsDeltaEvent {
                event_id,
                path,
                action,
                is_dir,
            });
        }
    }

    impl StreamGuard {
        pub fn start(
            roots: &[PathBuf],
            since_event_id: u64,
            coalesce_latency: Duration,
            tx: Sender<FsDeltaEvent>,
        ) -> io::Result<Self> {
            if roots.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "at least one watch root path is required",
                ));
            }

            unsafe {
                let mut cf_strings = Vec::with_capacity(roots.len());
                for root in roots {
                    let bytes = root.as_os_str().as_bytes();
                    let s = CFStringCreateWithBytes(
                        ptr::null(),
                        bytes.as_ptr(),
                        bytes.len() as isize,
                        K_CF_STRING_ENCODING_UTF8,
                        0,
                    );
                    if s.is_null() {
                        for prev in cf_strings {
                            CFRelease(prev);
                        }
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "failed to create CFString for watch root",
                        ));
                    }
                    cf_strings.push(s);
                }

                let cf_array = CFArrayCreate(
                    ptr::null(),
                    cf_strings.as_ptr(),
                    cf_strings.len() as isize,
                    &kCFTypeArrayCallBacks,
                );
                for s in cf_strings {
                    CFRelease(s);
                }
                if cf_array.is_null() {
                    return Err(io::Error::other("CFArrayCreate failed"));
                }

                let ctx_box = Box::new(CallbackCtx { tx });
                let ctx_ptr = Box::into_raw(ctx_box);

                let mut stream_ctx = FSEventStreamContext {
                    version: 0,
                    info: ctx_ptr.cast(),
                    retain: None,
                    release: None,
                    copy_description: None,
                };

                let stream = FSEventStreamCreate(
                    ptr::null(),
                    fsevents_callback,
                    &mut stream_ctx,
                    cf_array,
                    since_event_id,
                    coalesce_latency.as_secs_f64(),
                    K_FSEVENT_CREATE_FLAG_FILE_EVENTS | K_FSEVENT_CREATE_FLAG_NO_DEFER,
                );
                CFRelease(cf_array);

                if stream.is_null() {
                    drop(Box::from_raw(ctx_ptr));
                    return Err(io::Error::other("FSEventStreamCreate returned NULL"));
                }

                let queue = dispatch_queue_create(
                    c"dev.fastmacsearch.fsevents".as_ptr(),
                    ptr::null(),
                );
                FSEventStreamSetDispatchQueue(stream, queue);

                if FSEventStreamStart(stream) == 0 {
                    FSEventStreamSetDispatchQueue(stream, ptr::null_mut());
                    FSEventStreamInvalidate(stream);
                    FSEventStreamRelease(stream);
                    dispatch_release(queue);
                    drop(Box::from_raw(ctx_ptr));
                    return Err(io::Error::other("FSEventStreamStart failed"));
                }

                Ok(Self {
                    stream,
                    queue,
                    ctx_ptr,
                })
            }
        }
    }

    impl Drop for StreamGuard {
        fn drop(&mut self) {
            unsafe {
                FSEventStreamStop(self.stream);
                FSEventStreamSetDispatchQueue(self.stream, ptr::null_mut());
                FSEventStreamInvalidate(self.stream);
                FSEventStreamRelease(self.stream);
                dispatch_release(self.queue);
                drop(Box::from_raw(self.ctx_ptr));
            }
        }
    }
}
