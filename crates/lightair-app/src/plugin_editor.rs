//! A plugin's own editor, in a window of its own: a VST3's view, an AUv3's view controller, an
//! Audio Unit v2's Cocoa view, or (for units without one) Apple's generic view of its parameters.
//! Main thread only, except where noted.

use std::ffi::c_void;
use std::sync::{Arc, mpsc};

use block2::RcBlock;
use lightair_obs::plugins::AudioUnitPlugin;
use lightair_obs::plugins::vst3::Vst3Plugin;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, Sel};
use objc2::{msg_send, sel};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioUnitSetProperty(unit: *mut c_void, id: u32, scope: u32, el: u32, data: *const c_void, size: u32) -> i32;
    fn AudioUnitGetProperty(unit: *mut c_void, id: u32, scope: u32, el: u32, data: *mut c_void, size: *mut u32) -> i32;
}

unsafe extern "C" {
    fn objc_msgSend();
    fn CFRelease(cf: *const c_void);
}

const PROP_COCOA_UI: u32 = 31;
const PROP_REQUEST_VIEW_CONTROLLER: u32 = 56;

/// What the unit gave for its editor (retained object pointers, to cross threads).
pub enum Made {
    /// An AUv3's view controller.
    Controller(usize),
    /// A view (v2 Cocoa UI or the generic one).
    View(usize),
    /// Nothing yet: make the generic view (on the main thread).
    Generic,
}

/// Asks the unit for its editor. An AUv3 answers later, on another thread; a v2 unit at once.
pub fn request(unit: &AudioUnitPlugin) -> mpsc::Receiver<Made> {
    let (tx, rx) = mpsc::channel();
    let raw = unit.raw();
    let reply = tx.clone();
    let block = RcBlock::new(move |vc: *mut AnyObject| {
        let made = match unsafe { Retained::retain(vc) } {
            Some(vc) => Made::Controller(Retained::into_raw(vc) as usize),
            None => Made::Generic,
        };
        let _ = reply.send(made);
    });
    let ptr: *const block2::DynBlock<dyn Fn(*mut AnyObject)> = &*block;
    let st = unsafe {
        AudioUnitSetProperty(
            raw,
            PROP_REQUEST_VIEW_CONTROLLER,
            0,
            0,
            &ptr as *const _ as *const c_void,
            std::mem::size_of::<*const c_void>() as u32,
        )
    };
    if st == 0 {
        // the unit keeps (copies) the block until it answers; hold one reference regardless
        std::mem::forget(block);
    } else {
        // a v2 unit: its Cocoa view, made right here
        let made = match unsafe { cocoa_view(raw) } {
            Some(v) => Made::View(Retained::into_raw(v) as usize),
            None => Made::Generic,
        };
        let _ = tx.send(made);
    }
    rx
}

/// An Audio Unit v2's own Cocoa view (kAudioUnitProperty_CocoaUI).
unsafe fn cocoa_view(unit: *mut c_void) -> Option<Retained<AnyObject>> {
    #[repr(C)]
    struct Info {
        bundle: *const c_void,
        classes: [*const c_void; 8],
    }
    let mut info = Info { bundle: std::ptr::null(), classes: [std::ptr::null(); 8] };
    let mut size = std::mem::size_of::<Info>() as u32;
    let st = unsafe { AudioUnitGetProperty(unit, PROP_COCOA_UI, 0, 0, &mut info as *mut _ as *mut c_void, &mut size) };
    if st != 0 || info.bundle.is_null() || info.classes[0].is_null() {
        return None;
    }
    let classes = ((size as usize).saturating_sub(8) / 8).clamp(1, 8);
    let view = unsafe {
        let bundle: Option<Retained<AnyObject>> =
            msg_send![AnyClass::get(c"NSBundle")?, bundleWithURL: info.bundle as *mut AnyObject];
        let class: Option<&AnyClass> =
            bundle.as_deref().and_then(|b| msg_send![b, classNamed: info.classes[0] as *mut AnyObject]);
        class.and_then(|class| {
            let factory: Retained<AnyObject> = msg_send![class, new];
            // uiViewForAudioUnit:withSize: (its AudioUnit argument's type isn't one objc2 knows)
            let f: unsafe extern "C" fn(*mut AnyObject, Sel, *mut c_void, NSSize) -> *mut AnyObject =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let v = f(Retained::as_ptr(&factory) as *mut AnyObject, sel!(uiViewForAudioUnit:withSize:), unit, NSSize::ZERO);
            Retained::retain(v)
        })
    };
    unsafe {
        CFRelease(info.bundle);
        for c in &info.classes[..classes] {
            if !c.is_null() {
                CFRelease(*c);
            }
        }
    }
    view
}

/// Apple's generic editor: every parameter as a control (CoreAudioKit's AUGenericView).
unsafe fn generic_view(unit: *mut c_void) -> Option<Retained<AnyObject>> {
    static LOADED: std::sync::Once = std::sync::Once::new();
    LOADED.call_once(|| unsafe {
        libc::dlopen(c"/System/Library/Frameworks/CoreAudioKit.framework/CoreAudioKit".as_ptr(), libc::RTLD_LAZY);
    });
    unsafe {
        let class = AnyClass::get(c"AUGenericView")?;
        let alloc: *mut AnyObject = msg_send![class, alloc];
        let f: unsafe extern "C" fn(*mut AnyObject, Sel, *mut c_void) -> *mut AnyObject =
            std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let view = Retained::from_raw(f(alloc, sel!(initWithAudioUnit:), unit))?;
        let _: () = msg_send![&*view, setShowsExpertParameters: true];
        Some(view)
    }
}

/// What an editor window shows (dropped before the window: a VST3 view detaches first).
enum Content {
    Au(#[allow(dead_code)] Arc<AudioUnitPlugin>),
    Vst3(#[allow(dead_code)] lightair_obs::plugins::vst3::Vst3View, #[allow(dead_code)] Arc<Vst3Plugin>),
}

/// An open editor window.
pub struct Editor {
    /// Keeps the plugin (and a VST3's attached view) alive while it shows.
    _content: Content,
    window: Retained<AnyObject>,
}

/// A titled window with a content area of `size`.
unsafe fn window(title: &str, size: NSSize, resizable: bool) -> Option<Retained<AnyObject>> {
    unsafe {
        // titled, closable, miniaturizable (+ resizable)
        let style: usize = 1 | 2 | 4 | if resizable { 8 } else { 0 };
        let alloc: Allocated<AnyObject> = msg_send![AnyClass::get(c"NSWindow")?, alloc];
        let window: Retained<AnyObject> = msg_send![
            alloc,
            initWithContentRect: NSRect::new(NSPoint::new(0.0, 0.0), size),
            styleMask: style,
            backing: 2usize,
            defer: false
        ];
        let _: () = msg_send![&*window, setReleasedWhenClosed: false];
        let _: () = msg_send![&*window, setTitle: &*NSString::from_str(title)];
        Some(window)
    }
}

unsafe fn show(window: &AnyObject) {
    unsafe {
        let _: () = msg_send![window, center];
        // a background copy (tests, screenshots) shows it without taking the keyboard
        if std::env::var_os("LIGHTAIR_OPEN").is_some() {
            let _: () = msg_send![window, orderFront: std::ptr::null::<AnyObject>()];
        } else {
            let _: () = msg_send![window, makeKeyAndOrderFront: std::ptr::null::<AnyObject>()];
        }
    }
}

impl Editor {
    /// Puts what an Audio Unit made in a window titled `title`, and shows it.
    pub fn open_au(unit: Arc<AudioUnitPlugin>, made: Made, title: &str) -> Option<Editor> {
        unsafe {
            let (controller, view): (Option<Retained<AnyObject>>, Option<Retained<AnyObject>>) = match made {
                Made::Controller(p) => (Retained::from_raw(p as *mut AnyObject), None),
                Made::View(p) => (None, Retained::from_raw(p as *mut AnyObject)),
                Made::Generic => (None, generic_view(unit.raw())),
            };
            if controller.is_none() && view.is_none() {
                return None;
            }
            let mut size = NSSize::new(0.0, 0.0);
            if let Some(vc) = &controller {
                size = msg_send![&**vc, preferredContentSize];
                if size.width < 1.0 || size.height < 1.0 {
                    let v: Retained<AnyObject> = msg_send![&**vc, view];
                    let frame: NSRect = msg_send![&*v, frame];
                    size = frame.size;
                }
            } else if let Some(v) = &view {
                let frame: NSRect = msg_send![&**v, frame];
                size = frame.size;
            }
            if size.width < 1.0 || size.height < 1.0 {
                size = NSSize::new(640.0, 400.0);
            }
            // AUv3 views follow the window
            let window = window(title, size, controller.is_some())?;
            if let Some(vc) = &controller {
                let _: () = msg_send![&*window, setContentViewController: &**vc];
                let _: () = msg_send![&*window, setContentSize: size];
            } else if let Some(v) = &view {
                let _: () = msg_send![&*window, setContentView: &**v];
            }
            show(&window);
            Some(Editor { _content: Content::Au(unit), window })
        }
    }

    /// A VST3's own editor in a window titled `title`, shown.
    pub fn open_vst3(plugin: Arc<Vst3Plugin>, title: &str) -> Option<Editor> {
        let (view, (w, h)) = plugin.create_view()?;
        let size = NSSize::new(w.max(200) as f64, h.max(100) as f64);
        unsafe {
            let window = window(title, size, false)?;
            let content: Retained<AnyObject> = msg_send![&*window, contentView];
            // the plugin asks for new sizes; the window lives as long as the view (see Content)
            let win = Retained::as_ptr(&window) as usize;
            let resize = move |w: i32, h: i32| {
                let win = win as *const AnyObject;
                let _: () = msg_send![&*win, setContentSize: NSSize::new(w as f64, h as f64)];
            };
            // the content view lives as long as the window, which outlives the view (Content)
            let attached =
                lightair_obs::plugins::vst3::Vst3View::attach(view, Retained::as_ptr(&content) as *mut c_void, resize)?;
            show(&window);
            Some(Editor { _content: Content::Vst3(attached, plugin), window })
        }
    }

    /// Still open (showing or in the Dock): the user hasn't closed it.
    pub fn is_open(&self) -> bool {
        unsafe {
            let visible: bool = msg_send![&*self.window, isVisible];
            let docked: bool = msg_send![&*self.window, isMiniaturized];
            visible || docked
        }
    }

    pub fn front(&self) {
        unsafe {
            let _: () = msg_send![&*self.window, makeKeyAndOrderFront: std::ptr::null::<AnyObject>()];
        }
    }

    pub fn close(&self) {
        unsafe {
            let _: () = msg_send![&*self.window, close];
        }
    }
}
