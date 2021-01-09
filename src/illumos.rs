//! Watcher implementation for the illumos Event Port API

use super::event::*;
use super::{Config, Error, Event, EventFn, EventKind, RecursiveMode, Result,
    Watcher};
use libc::{c_int, c_void, stat};
use std::sync::{Arc, Mutex};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::os::unix::ffi::OsStrExt;
use event_port_sys::*;
use std::ffi::CString;
use std::collections::HashMap;


enum Obj {
    Box(Box<FileObj>),
    Pointer(*mut FileObj),
}

struct WatchPath {
    path: CString,
    obj: Obj,
}

struct Watch {
    root: PathBuf,
    recursive: bool,
    paths: Vec<WatchPath>,
}


/*
 * XXX
 * A watchpath should have...
 *
 * a Box<FileObj> that we can turn into a raw pointer when associating, and
 * return to a Box when we get it back from port_get().
 *
 * It should have a way to look it up even when it's on raw pointer holiday so
 * that we can port_dissociate() it if need be.
 */




struct Watches {
    all: Vec<Option<PathBuf>>,
}

impl Watches {
    fn new() -> Watches {
        Watches {
            all: Vec::new(),
        }
    }

    fn insert(&mut self, p: PathBuf) -> Option<usize> {
        let x = Some(p);

        if self.all.contains(&x) {
            return None;
        }

        let id = self.all.len();
        self.all.push(x);
        Some(id)
    }

    fn remove(&mut self, id: usize) -> Option<PathBuf> {
        self.all.push(None);
        let out = self.all.swap_remove(id);
        out
    }
}

struct InnerInner {
}

struct Inner {
    port: c_int,
    inner: Mutex<InnerInner>,
    tx: Mutex<SyncSender<Command>>,
    rx: Mutex<Receiver<Command>>,
    watches: Mutex<Watches>,
    event_fn: Mutex<Box<dyn EventFn>>,
}

impl Inner {
    fn new<F: EventFn>(event_fn: F) -> Result<Self> {
        let (tx, rx) = sync_channel::<Command>(0);
        let port = unsafe { port_create() };
        if port < 0 {
            return Err(Error::io(std::io::Error::last_os_error()));
        }

        let ii = InnerInner {
        };

        Ok(Inner {
            tx: Mutex::new(tx),
            rx: Mutex::new(rx),
            event_fn: Mutex::new(Box::new(event_fn)),
            inner: Mutex::new(ii),
            watches: Mutex::new(Watches::new()),
            port,
        })
    }

    fn send(&self, c: Command) {
        self.tx.lock().unwrap().send(c).unwrap();
    }

    fn run(&self) {
        loop {
            let mut pe: PortEvent = unsafe { std::mem::zeroed() };

            /*
             * Listen on the port.
             */
            let (r, e) = unsafe {
                let r = port_get(self.port, &mut pe, std::ptr::null());
                let e = errno();
                (r, e)
            };

            if r != 0 {
                panic!("port_get failed: {}", e);
            }

            if pe.portev_source == PORT_SOURCE_FILE as u16 {
                let id: usize = pe.portev_user as usize;
                println!("PORT_SOURCE_FILE! id {}", id);

                let p = self.watches.lock().unwrap().remove(id);
                println!(" ---> {:?}", p);

                let e = pe.portev_events;
                if e & FILE_ACCESS != 0 {
                    /*
                     * atime changed
                     */
                    println!("   ~~ ACCESS");
                    let ev = Event::new(
                        EventKind::Access(AccessKind::Any))
                        .add_path(p.unwrap());

                    let efn = self.event_fn.lock().unwrap();
                    (efn)(Ok(ev));
                }
                if e & FILE_MODIFIED != 0 {
                    /*
                     * mtime changed
                     */
                    println!("   ~~ MODIFIED");
                }
                if e & FILE_ATTRIB != 0 {
                    /*
                     * ctime changed
                     */
                    println!("   ~~ ATTRIB");
                }
                if e & FILE_TRUNC != 0 {
                    println!("   ~~ TRUNC");
                }
                if e & FILE_DELETE != 0 {
                    println!("   ~~ DELETE");
                }
                if e & FILE_RENAME_TO != 0 {
                    println!("   ~~ RENAME_TO");
                }
                if e & FILE_RENAME_FROM != 0 {
                    println!("   ~~ RENAME_FROM");
                }
                if e & UNMOUNTED != 0 {
                    println!("   ~~ UNMOUNTED");
                }
                if e & MOUNTEDOVER != 0 {
                    println!("   ~~ MOUNTEDOVER");
                }

            } else if pe.portev_source != PORT_SOURCE_USER as u16 {
                /* XXX */
                panic!("unexpected state! {}, source {}, obj {:?}, user {:?}",
                    e, pe.portev_source, pe.portev_object,
                    pe.portev_user);
            }

            let c = self.rx.lock().unwrap().recv().unwrap();

            match c {
                Command::Shutdown => return,
                Command::AddWatch(p, rm, tx) => {
                    println!("WATCH: {}, {:?}", p.display(), rm);

                    let id = self.watches.lock().unwrap().insert(p.clone()).unwrap();
                    println!("   id -> {}", id);

                    let lpsz = CString::new(p.as_os_str().as_bytes()).unwrap();

                    /*
                     * Stat the file so that we know what to put in the
                     * association.
                     * XXX lstat()? FILE_NOFOLLOW?  inotify behaviour unclear...
                     */
                    let mut st: stat = unsafe { std::mem::zeroed() };
                    if unsafe { stat(lpsz.as_ptr(), &mut st) } != 0 {
                        tx.send(
                            Err(Error::io(std::io::Error::last_os_error())))
                            .unwrap();
                        continue;
                    }

                    println!("stat({}) ok", p.display());

                    let mut fo: FileObj = unsafe { std::mem::zeroed() };
                    fo.fo_atime.tv_sec = st.st_atime;
                    fo.fo_atime.tv_nsec = st.st_atime_nsec;
                    fo.fo_mtime.tv_sec = st.st_mtime;
                    fo.fo_mtime.tv_nsec = st.st_mtime_nsec;
                    fo.fo_ctime.tv_sec = st.st_ctime;
                    fo.fo_ctime.tv_nsec = st.st_ctime_nsec;
                    fo.fo_name = lpsz.as_ptr();

                    let idp: *mut c_void = id as *mut c_void;
                    let fop: *mut c_void = &mut fo as *mut _ as *mut c_void;
                    println!("fop = {:?}", fop);

                    let mask = FILE_ACCESS | FILE_MODIFIED | FILE_ATTRIB |
                        FILE_TRUNC;
                    if unsafe { port_associate(self.port, PORT_SOURCE_FILE,
                        fop, mask, idp) } != 0
                    {
                        tx.send(
                            Err(Error::io(std::io::Error::last_os_error())))
                            .unwrap();
                        continue;
                    }

                    println!("port_associate({}) ok", p.display());

                    tx.send(Ok(())).unwrap();
                }
            };
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        assert_eq!(unsafe { libc::close(self.port) }, 0);
    }
}

/// Watcher implementation for the illumos Event Port API
pub struct EventPortWatcher {
    inner: Arc<Inner>,
}

impl EventPortWatcher {
    fn spawn(&self) {
        let inner = Arc::clone(&self.inner);
        std::thread::spawn(move || inner.run());
    }
}

enum Command {
    AddWatch(PathBuf, RecursiveMode, SyncSender<Result<()>>),
    // RemoveWatch(PathBuf, SyncSender<Result<()>>),
    Shutdown,
}

impl Watcher for EventPortWatcher {
    fn new_immediate<F: EventFn>(event_fn: F) -> Result<EventPortWatcher> {
        let i = Inner::new(event_fn)?;

        let epw = EventPortWatcher {
            inner: Arc::new(i),
        };
        epw.spawn();
        Ok(epw)
    }

    fn watch<P: AsRef<Path>>(&mut self, path: P, recursive_mode: RecursiveMode)
        -> Result<()>
    {
        let p = path.as_ref();
        let p = if p.is_absolute() {
            p.to_owned()
        } else {
            let dir = std::env::current_dir().map_err(Error::io)?;
            dir.join(p)
        };

        let (tx, rx) = sync_channel(0);

        // Wake the port thread so that they can receive our channel message:
        if unsafe { port_send(self.inner.port, 1, std::ptr::null_mut()) } != 0 {
            return Err(Error::io(std::io::Error::last_os_error()));
        }

        self.inner.send(Command::AddWatch(p, recursive_mode, tx));
        Ok(rx.recv().unwrap()?)
    }

    fn unwatch<P: AsRef<Path>>(&mut self, path: P) -> Result<()> {
        unimplemented!("unwatch");
    }

    fn configure(&mut self, config: Config) -> Result<bool> {
        unimplemented!("configure");
    }
}

impl Drop for EventPortWatcher {
    fn drop(&mut self) {
        // self.0.lock().unwrap().send(Command::Shutdown).unwrap();
    }
}
