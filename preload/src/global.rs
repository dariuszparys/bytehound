use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

use crate::arc_lite::ArcLite;
use crate::event::{InternalAllocationId, InternalEvent, send_event};
use crate::spin_lock::{SpinLock, SpinLockGuard};
use crate::syscall;
use crate::unwind::{ThreadUnwindState, prepare_to_start_unwinding};
use crate::timestamp::Timestamp;

pub type RawThreadHandle = ArcLite< ThreadData >;

struct ThreadRegistry {
    enabled_for_new_threads: bool,
    threads: Option< HashMap< u32, RawThreadHandle > >,
    dead_thread_queue: Vec< (Timestamp, RawThreadHandle) >,
    thread_counter: u64
}

unsafe impl Send for ThreadRegistry {}

impl ThreadRegistry {
    fn threads( &mut self ) -> &mut HashMap< u32, RawThreadHandle > {
        self.threads.get_or_insert_with( HashMap::new )
    }
}

const STATE_UNINITIALIZED: usize = 0;
const STATE_DISABLED: usize = 1;
const STATE_STARTING: usize = 2;
const STATE_ENABLED: usize = 3;
const STATE_STOPPING: usize = 4;
const STATE_PERMANENTLY_DISABLED: usize = 5;
static STATE: AtomicUsize = AtomicUsize::new( STATE_UNINITIALIZED );

static THREAD_RUNNING: AtomicBool = AtomicBool::new( false );

const DESIRED_STATE_DISABLED: usize = 0;
const DESIRED_STATE_SUSPENDED: usize = 1;
const DESIRED_STATE_ENABLED: usize = 2;
static DESIRED_STATE: AtomicUsize = AtomicUsize::new( DESIRED_STATE_DISABLED );

static THREAD_REGISTRY: SpinLock< ThreadRegistry > = SpinLock::new( ThreadRegistry {
    enabled_for_new_threads: false,
    threads: None,
    dead_thread_queue: Vec::new(),
    thread_counter: 1
});

static PROCESSING_THREAD_HANDLE: SpinLock< Option< std::thread::JoinHandle< () > > > = SpinLock::new( None );

pub static mut SYM_REGISTER_FRAME: Option< unsafe extern "C" fn( fde: *const u8 ) > = None;
pub static mut SYM_DEREGISTER_FRAME: Option< unsafe extern "C" fn( fde: *const u8 ) > = None;

pub static MMAP_LOCK: SpinLock< () > = SpinLock::new(());

pub fn toggle() {
    if STATE.load( Ordering::SeqCst ) == STATE_PERMANENTLY_DISABLED {
        return;
    }

    let value = DESIRED_STATE.load( Ordering::SeqCst );
    let new_value = match value {
        DESIRED_STATE_DISABLED => {
            info!( "Tracing will be toggled ON (for the first time)" );
            DESIRED_STATE_ENABLED
        },
        DESIRED_STATE_SUSPENDED => {
            info!( "Tracing will be toggled ON" );
            DESIRED_STATE_ENABLED
        },
        DESIRED_STATE_ENABLED => {
            info!( "Tracing will be toggled OFF" );
            DESIRED_STATE_SUSPENDED
        },
        _ => unreachable!()
    };

    DESIRED_STATE.store( new_value, Ordering::SeqCst );
}

pub fn enable() -> bool {
    if STATE.load( Ordering::SeqCst ) == STATE_PERMANENTLY_DISABLED {
        return false;
    }

    DESIRED_STATE.swap( DESIRED_STATE_ENABLED, Ordering::SeqCst ) != DESIRED_STATE_ENABLED
}

pub fn disable() -> bool {
    if STATE.load( Ordering::SeqCst ) == STATE_PERMANENTLY_DISABLED {
        return false;
    }

    DESIRED_STATE.swap( DESIRED_STATE_SUSPENDED, Ordering::SeqCst ) == DESIRED_STATE_ENABLED
}

fn is_busy() -> bool {
    let state = STATE.load( Ordering::SeqCst );
    if state == STATE_STARTING || state == STATE_STOPPING {
        return true;
    }

    let requested_state = DESIRED_STATE.load( Ordering::SeqCst );
    let is_thread_running = THREAD_RUNNING.load( Ordering::SeqCst );
    if requested_state == DESIRED_STATE_DISABLED && is_thread_running && state == STATE_ENABLED {
        return true;
    }

    false
}

fn try_sync_processing_thread_destruction() {
    let mut handle = PROCESSING_THREAD_HANDLE.lock();
    let state = STATE.load( Ordering::SeqCst );
    if state == STATE_STOPPING || state == STATE_DISABLED {
        if let Some( handle ) = handle.take() {
            let _ = handle.join();
        }
    }
}

pub fn sync() {
    try_sync_processing_thread_destruction();

    while is_busy() {
        thread::sleep( std::time::Duration::from_millis( 1 ) );
    }

    try_sync_processing_thread_destruction();
}

pub extern fn on_exit() {
    if STATE.load( Ordering::SeqCst ) == STATE_PERMANENTLY_DISABLED {
        return;
    }

    info!( "Exit hook called" );

    DESIRED_STATE.store( DESIRED_STATE_DISABLED, Ordering::SeqCst );
    send_event( InternalEvent::Exit );

    let mut count = 0;
    while THREAD_RUNNING.load( Ordering::SeqCst ) == true && count < 2000 {
        unsafe {
            libc::usleep( 25 * 1000 );
            count += 1;
        }
    }

    info!( "Exit hook finished" );
}

pub unsafe extern fn on_fork() {
    STATE.store( STATE_PERMANENTLY_DISABLED, Ordering::SeqCst );
    DESIRED_STATE.store( DESIRED_STATE_DISABLED, Ordering::SeqCst );
    THREAD_RUNNING.store( false, Ordering::SeqCst );
    THREAD_REGISTRY.force_unlock(); // In case we were forked when the lock was held.
    {
        let tid = syscall::gettid();
        let mut registry = THREAD_REGISTRY.lock();
        registry.enabled_for_new_threads = false;
        registry.threads().retain( |&thread_id, _| {
            thread_id == tid
        });
    }

    TLS.with( |tls| tls.set_enabled( false ) );
}

fn spawn_processing_thread() {
    info!( "Spawning event processing thread..." );

    let mut thread_handle = PROCESSING_THREAD_HANDLE.lock();
    assert!( !THREAD_RUNNING.load( Ordering::SeqCst ) );

    let new_handle = thread::Builder::new().name( "mem-prof".into() ).spawn( move || {
        TLS.with( |tls| {
            unsafe {
                *tls.is_internal.get() = true;
            }
            assert!( !tls.is_enabled() );
        });

        THREAD_RUNNING.store( true, Ordering::SeqCst );

        let result = std::panic::catch_unwind( || {
            crate::processing_thread::thread_main();
        });

        if result.is_err() {
            DESIRED_STATE.store( DESIRED_STATE_DISABLED, Ordering::SeqCst );
        }

        let mut thread_registry = THREAD_REGISTRY.lock();
        thread_registry.enabled_for_new_threads = false;
        for tls in thread_registry.threads().values() {
            if tls.is_internal() {
                continue;
            }

            debug!( "Disabling thread {:04x}...", tls.thread_id );
            tls.set_enabled( false );
            tls.unwind_cache.clear();
        }

        STATE.store( STATE_DISABLED, Ordering::SeqCst );
        info!( "Tracing was disabled" );

        THREAD_RUNNING.store( false, Ordering::SeqCst );

        if let Err( err ) = result {
            std::panic::resume_unwind( err );
        }
    }).expect( "failed to start the main memory profiler thread" );

    while THREAD_RUNNING.load( Ordering::SeqCst ) == false {
        thread::yield_now();
    }

    *thread_handle = Some( new_handle );
}

#[cfg(target_arch = "x86_64")]
fn find_internal_syms< const N: usize >( names: &[&str; N] ) -> [usize; N] {
    let mut addresses = [0; N];

    unsafe {
        use goblin::elf64::header::Header;
        use goblin::elf64::section_header::SectionHeader;
        use goblin::elf::section_header::SHT_SYMTAB;
        use goblin::elf::sym::sym64::Sym;

        let mut path = libc::getauxval( libc::AT_EXECFN ) as *const libc::c_char;
        let mut path_buffer: [libc::c_char; libc::PATH_MAX as usize] = [0; libc::PATH_MAX as usize];
        if path.is_null() {
            if libc::realpath( b"/proc/self/exe\0".as_ptr() as _, path_buffer.as_mut_ptr() ).is_null() {
                panic!( "couldn't find path to itself: {}", std::io::Error::last_os_error() );
            } else {
                path = path_buffer.as_ptr();
            }
        }

        let fd = libc::open( path, libc::O_RDONLY );
        if fd < 0 {
            panic!( "failed to open {:?}: {}", std::ffi::CStr::from_ptr( path ), std::io::Error::last_os_error() );
        }

        let mut buf: libc::stat64 = std::mem::zeroed();
        if libc::fstat64( fd as _, &mut buf ) != 0 {
            panic!( "couldn't fstat the executable: {}", std::io::Error::last_os_error() );
        }

        let size = buf.st_size as usize;
        let executable = syscall::mmap( std::ptr::null_mut(), size, libc::PROT_READ, libc::MAP_PRIVATE, fd, 0 );
        assert_ne!( executable, libc::MAP_FAILED );

        let elf_header = *(executable as *const Header);
        let address_offset = libc::getauxval( libc::AT_PHDR ) as usize - elf_header.e_phoff as usize;

        assert_eq!( elf_header.e_shentsize as usize, std::mem::size_of::< SectionHeader >() );
        let section_headers = std::slice::from_raw_parts(
            ((executable as *const u8).add( elf_header.e_shoff as usize )) as *const SectionHeader,
            elf_header.e_shnum as usize
        );

        for section_header in section_headers {
            if section_header.sh_type != SHT_SYMTAB {
                continue;
            }
            let strtab_key = section_header.sh_link as usize;
            let strtab_section_header = section_headers[ strtab_key ];
            let strtab_bytes = std::slice::from_raw_parts( (executable as *const u8).add( strtab_section_header.sh_offset as usize ), strtab_section_header.sh_size as usize );

            let syms = std::slice::from_raw_parts(
                (executable as *const u8).add( section_header.sh_offset as usize ) as *const Sym,
                section_header.sh_size as usize / std::mem::size_of::< Sym >()
            );

            for sym in syms {
                let bytes = &strtab_bytes[ sym.st_name as usize.. ];
                let name = &bytes[ ..bytes.iter().position( |&byte| byte == 0 ).unwrap_or( bytes.len() ) ];
                for (target_name, output_address) in names.iter().zip( addresses.iter_mut() ) {
                    if name == target_name.as_bytes() {
                        if let Some( address ) = address_offset.checked_add( sym.st_value as usize ) {
                            info!( "Found '{}' at: 0x{:016X}", target_name, address );
                            *output_address = address;
                            break;
                        }
                    }
                }
            }

            break;
        }

        if syscall::munmap( executable, size ) != 0 {
            warn!( "munmap failed: {}", std::io::Error::last_os_error() );
        }
    }

    addresses
}

#[cfg(target_arch = "x86_64")]
fn hook_jemalloc() {
    let names = [
        "_rjem_malloc",
        "_rjem_mallocx",
        "_rjem_calloc",
        "_rjem_sdallocx",
        "_rjem_realloc",
        "_rjem_rallocx",
        "_rjem_nallocx",
        "_rjem_xallocx",
        "_rjem_malloc_usable_size",
        "_rjem_mallctl",
        "_rjem_posix_memalign",
        "_rjem_aligned_alloc",
        "_rjem_free",
        "_rjem_sallocx",
        "_rjem_dallocx",
        "_rjem_mallctlnametomib",
        "_rjem_mallctlbymib",
        "_rjem_malloc_stats_print",
    ];

    let replacements = [
        crate::api::_rjem_malloc as usize,
        crate::api::_rjem_mallocx as usize,
        crate::api::_rjem_calloc as usize,
        crate::api::_rjem_sdallocx as usize,
        crate::api::_rjem_realloc as usize,
        crate::api::_rjem_rallocx as usize,
        crate::api::_rjem_nallocx as usize,
        crate::api::_rjem_xallocx as usize,
        crate::api::_rjem_malloc_usable_size as usize,
        crate::api::_rjem_mallctl as usize,
        crate::api::_rjem_posix_memalign as usize,
        crate::api::_rjem_aligned_alloc as usize,
        crate::api::_rjem_free as usize,
        crate::api::_rjem_sallocx as usize,
        crate::api::_rjem_dallocx as usize,
        crate::api::_rjem_mallctlnametomib as usize,
        crate::api::_rjem_mallctlbymib as usize,
        crate::api::_rjem_malloc_stats_print as usize,
    ];

    let addresses = find_internal_syms( &names );
    if addresses.iter().all( |&address| address == 0 ) {
        info!( "Couldn't find jemalloc in the executable's address space" );
        return;
    }

    assert_eq!( names.len(), replacements.len() );
    assert_eq!( names.len(), addresses.len() );

    for ((name, replacement), address) in names.iter().zip( replacements ).zip( addresses ) {
        if address == 0 {
            info!( "Symbol not found: \"{}\"", name );
            continue;
        }

        let page = (address as usize & !(4096 - 1)) as *mut libc::c_void;
        unsafe {
            if libc::mprotect( page, 4096, libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC ) < 0 {
                panic!( "mprotect failed: {}", std::io::Error::last_os_error() );
            }

            // Write a `jmp` instruction with a RIP-relative addressing mode, with a zero displacement.
            let mut p = address as *mut u8;
            std::ptr::write_unaligned( p, 0xFF ); p = p.add( 1 );
            std::ptr::write_unaligned( p, 0x25 ); p = p.add( 1 );
            std::ptr::write_unaligned( p, 0x00 ); p = p.add( 1 );
            std::ptr::write_unaligned( p, 0x00 ); p = p.add( 1 );
            std::ptr::write_unaligned( p, 0x00 ); p = p.add( 1 );
            std::ptr::write_unaligned( p, 0x00 ); p = p.add( 1 );
            std::ptr::write_unaligned( p as *mut usize, replacement );

            if libc::mprotect( page, 4096, libc::PROT_READ | libc::PROT_EXEC ) < 0 {
                warn!( "mprotect failed: {}", std::io::Error::last_os_error() );
            }
        }
    }
}

fn resolve_original_syms() {
    unsafe {
        let register_frame = libc::dlsym( libc::RTLD_NEXT, b"__register_frame\0".as_ptr() as *const libc::c_char );
        let deregister_frame = libc::dlsym( libc::RTLD_NEXT, b"__deregister_frame\0".as_ptr() as *const libc::c_char );
        if register_frame.is_null() || deregister_frame.is_null() {
            if register_frame.is_null() {
                warn!( "Failed to find `__register_frame` symbol" );
            }
            if deregister_frame.is_null() {
                warn!( "Failed to find `__deregister_frame` symbol" );
            }
            return;
        }

        crate::global::SYM_REGISTER_FRAME = Some( std::mem::transmute( register_frame ) );
        crate::global::SYM_DEREGISTER_FRAME = Some( std::mem::transmute( deregister_frame ) );
    }
}

#[cold]
#[inline(never)]
fn try_enable( state: usize ) -> bool {
    if state == STATE_UNINITIALIZED {
        STATE.store( STATE_DISABLED, Ordering::SeqCst );
        crate::init::startup();
    }

    if DESIRED_STATE.load( Ordering::SeqCst ) == DESIRED_STATE_DISABLED {
        return false;
    }

    if STATE.compare_exchange( STATE_DISABLED, STATE_STARTING, Ordering::SeqCst, Ordering::SeqCst ).is_err() {
        return false;
    }

    static LOCK: SpinLock< () > = SpinLock::new(());
    let mut _lock = match LOCK.try_lock() {
        Some( guard ) => guard,
        None => {
            return false;
        }
    };

    {
        let thread_registry = THREAD_REGISTRY.lock();
        assert!( !thread_registry.enabled_for_new_threads );
    }

    prepare_to_start_unwinding();
    spawn_processing_thread();

    {
        let mut thread_registry = THREAD_REGISTRY.lock();
        thread_registry.enabled_for_new_threads = true;
        for tls in thread_registry.threads().values() {
            if tls.is_internal() {
                continue;
            }

            debug!( "Enabling thread {:04x}...", tls.thread_id );
            tls.set_enabled( true );
        }
    }

    resolve_original_syms();

    #[cfg(target_arch = "x86_64")]
    hook_jemalloc();

    STATE.store( STATE_ENABLED, Ordering::SeqCst );
    info!( "Tracing was enabled" );

    true
}

pub fn try_disable_if_requested() {
    if DESIRED_STATE.load( Ordering::SeqCst ) != DESIRED_STATE_DISABLED {
        return;
    }

    if STATE.compare_exchange( STATE_ENABLED, STATE_STOPPING, Ordering::SeqCst, Ordering::SeqCst ).is_err() {
        return;
    }

    send_event( InternalEvent::Exit );
}

const THROTTLE_LIMIT: usize = 8192;

#[cold]
#[inline(never)]
fn throttle( tls: &RawThreadHandle ) {
    while ArcLite::get_refcount_relaxed( tls ) >= THROTTLE_LIMIT {
        thread::yield_now();
    }
}

pub fn is_actively_running() -> bool {
    DESIRED_STATE.load( Ordering::Relaxed ) == DESIRED_STATE_ENABLED
}

/// A handle to per-thread storage; you can't do anything with it.
///
/// Can be sent to other threads.
pub struct WeakThreadHandle( RawThreadHandle );
unsafe impl Send for WeakThreadHandle {}
unsafe impl Sync for WeakThreadHandle {}

impl WeakThreadHandle {
    pub fn system_tid( &self ) -> u32 {
        self.0.thread_id
    }

    pub fn unique_tid( &self ) -> u64 {
        self.0.internal_thread_id
    }
}

/// A handle to per-thread storage.
///
/// Can only be aquired for the current thread, and cannot be sent to other threads.
pub struct StrongThreadHandle( Option< RawThreadHandle > );

impl StrongThreadHandle {
    #[cold]
    #[inline(never)]
    fn acquire_slow() -> Option< Self > {
        let current_thread_id = syscall::gettid();
        let mut registry = THREAD_REGISTRY.lock();
        if let Some( thread ) = registry.threads().get( &current_thread_id ) {
            debug!( "Acquired a dead thread: {:04X}", current_thread_id );
            Some( StrongThreadHandle( Some( thread.clone() ) ) )
        } else {
            warn!( "Failed to acquire a handle for thread: {:04X}", current_thread_id );
            None
        }
    }

    #[inline(always)]
    pub fn acquire() -> Option< Self > {
        let state = STATE.load( Ordering::Relaxed );
        if state != STATE_ENABLED {
            if !try_enable( state ) {
                return None;
            }
        }

        let tls = TLS.with( |tls| {
            if ArcLite::get_refcount_relaxed( tls ) >= THROTTLE_LIMIT {
                throttle( tls );
            }

            if !tls.is_enabled() {
                None
            } else {
                tls.set_enabled( false );
                Some( tls.0.clone() )
            }
        });

        match tls {
            Some( Some( tls ) ) => {
                Some( StrongThreadHandle( Some( tls ) ) )
            },
            Some( None ) => {
                None
            },
            None => {
                Self::acquire_slow()
            }
        }
    }

    pub fn decay( mut self ) -> WeakThreadHandle {
        let tls = match self.0.take() {
            Some( tls ) => tls,
            None => unsafe { std::hint::unreachable_unchecked() }
        };

        tls.set_enabled( true );
        WeakThreadHandle( tls )
    }

    pub fn unwind_state( &mut self ) -> &mut ThreadUnwindState {
        let tls = match self.0.as_ref() {
            Some( tls ) => tls,
            None => unsafe { std::hint::unreachable_unchecked() }
        };

        unsafe {
            &mut *tls.unwind_state.get()
        }
    }

    pub fn unwind_cache( &self ) -> &Arc< crate::unwind::Cache > {
        let tls = match self.0.as_ref() {
            Some( tls ) => tls,
            None => unsafe { std::hint::unreachable_unchecked() }
        };

        &tls.unwind_cache
    }

    pub fn on_new_allocation( &mut self ) -> InternalAllocationId {
        let tls = match self.0.as_ref() {
            Some( tls ) => tls,
            None => unsafe { std::hint::unreachable_unchecked() }
        };

        let counter = tls.allocation_counter.get();
        let allocation;
        unsafe {
            allocation = *counter;
            *counter += 1;
        }

        InternalAllocationId::new( tls.internal_thread_id, allocation )
    }
}

impl Drop for StrongThreadHandle {
    fn drop( &mut self ) {
        if let Some( tls ) = self.0.take() {
            tls.set_enabled( true );
        }
    }
}

pub struct AllocationLock {
    current_thread_id: u32,
    registry_lock: SpinLockGuard< 'static, ThreadRegistry >
}

impl AllocationLock {
    pub fn new() -> Self {
        let mut registry_lock = THREAD_REGISTRY.lock();
        let current_thread_id = syscall::gettid();
        let threads = registry_lock.threads();
        for (&thread_id, tls) in threads.iter_mut() {
            if thread_id == current_thread_id {
                continue;
            }

            if tls.is_internal() {
                continue;
            }
            unsafe {
                ArcLite::add( tls, THROTTLE_LIMIT );
            }
        }

        std::sync::atomic::fence( Ordering::SeqCst );

        for (&thread_id, tls) in threads.iter_mut() {
            if thread_id == current_thread_id {
                continue;
            }

            if tls.is_internal() {
                continue;
            }
            while ArcLite::get_refcount_relaxed( tls ) != THROTTLE_LIMIT {
                thread::yield_now();
            }
        }

        std::sync::atomic::fence( Ordering::SeqCst );

        AllocationLock {
            current_thread_id,
            registry_lock
        }
    }
}

impl Drop for AllocationLock {
    fn drop( &mut self ) {
        for (&thread_id, tls) in self.registry_lock.threads().iter_mut() {
            if thread_id == self.current_thread_id {
                continue;
            }

            unsafe {
                ArcLite::sub( tls, THROTTLE_LIMIT );
            }
        }
    }
}

pub struct ThreadData {
    thread_id: u32,
    internal_thread_id: u64,
    is_internal: UnsafeCell< bool >,
    enabled: AtomicBool,
    unwind_cache: Arc< crate::unwind::Cache >,
    unwind_state: UnsafeCell< ThreadUnwindState >,
    allocation_counter: UnsafeCell< u64 >
}

impl ThreadData {
    #[inline(always)]
    pub fn is_enabled( &self ) -> bool {
        self.enabled.load( Ordering::Relaxed )
    }

    #[inline(always)]
    pub fn is_internal( &self ) -> bool {
        unsafe {
            *self.is_internal.get()
        }
    }

    fn set_enabled( &self, value: bool ) {
        self.enabled.store( value, Ordering::Relaxed )
    }
}

struct ThreadSentinel( RawThreadHandle );

impl Deref for ThreadSentinel {
    type Target = RawThreadHandle;
    fn deref( &self ) -> &Self::Target {
        &self.0
    }
}

impl Drop for ThreadSentinel {
    fn drop( &mut self ) {
        let mut registry = THREAD_REGISTRY.lock();
        if let Some( thread ) = registry.threads().get( &self.thread_id ) {
            let thread = thread.clone();
            registry.dead_thread_queue.push( (crate::timestamp::get_timestamp(), thread) );
        }

        debug!( "Thread dropped: {:04X}", self.thread_id );
    }
}

thread_local_reentrant! {
    static TLS: ThreadSentinel = |callback| {
        let thread_id = syscall::gettid();
        let mut registry = THREAD_REGISTRY.lock();
        let internal_thread_id = registry.thread_counter;
        registry.thread_counter += 1;

        let tls = ThreadData {
            thread_id,
            internal_thread_id,
            is_internal: UnsafeCell::new( false ),
            enabled: AtomicBool::new( registry.enabled_for_new_threads ),
            unwind_cache: Arc::new( crate::unwind::Cache::new() ),
            unwind_state: UnsafeCell::new( ThreadUnwindState::new() ),
            allocation_counter: UnsafeCell::new( 1 )
        };

        let tls = ArcLite::new( tls );
        registry.threads().insert( thread_id, tls.clone() );

        callback( ThreadSentinel( tls ) )
    };
}

pub fn garbage_collect_dead_threads( now: Timestamp ) {
    use std::collections::hash_map::Entry;

    let mut registry = THREAD_REGISTRY.lock();
    let registry = &mut *registry;

    if registry.dead_thread_queue.is_empty() {
        return;
    }

    let count = registry.dead_thread_queue.iter()
        .take_while( |&(time_of_death, _)| time_of_death.as_secs() + 3 < now.as_secs() )
        .count();

    if count == 0 {
        return;
    }

    let threads = registry.threads.get_or_insert_with( HashMap::new );
    for (_, thread) in registry.dead_thread_queue.drain( ..count ) {
        if let Entry::Occupied( entry ) = threads.entry( thread.thread_id ) {
            if RawThreadHandle::ptr_eq( entry.get(), &thread ) {
                entry.remove_entry();
            }
        }
    }
}
