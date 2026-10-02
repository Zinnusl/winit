//! Keep window creation and pointer events observable by the Steam overlay.
//!
//! Xlib calls and event translation belong to the owning event thread. They are
//! small, connection-ordered operations, not background-safe CPU work.

use std::ptr;

use x11rb::{
    connection::Connection as _,
    protocol::{xinput, xproto},
};

use super::{ffi, util::memory::XSmartPointer, X11Error, XConnection};

/// Preserve the selected visual, colormap, event mask and parent while making
/// creation observable at the public Xlib boundary intercepted by Steam.
pub(super) fn create_window(
    xconn: &XConnection,
    parent: xproto::Window,
    position: (i32, i32),
    dimensions: (u32, u32),
    depth: u8,
    visual_id: xproto::Visualid,
    attributes: &xproto::CreateWindowAux,
) -> Result<xproto::Window, X11Error> {
    // Complete earlier XCB requests (notably colormap creation) before Xlib
    // issues a request on the same underlying connection.
    xconn.xcb_connection().flush()?;
    xconn.sync_with_server()?;

    let visual_info = if visual_id == x11rb::COPY_FROM_PARENT {
        None
    } else {
        // SAFETY: these are POD Xlib structures. XGetVisualInfo returns an
        // XFree-owned array; the Visual itself stays owned by the display.
        let mut template: ffi::XVisualInfo = unsafe { std::mem::zeroed() };
        template.visualid = visual_id.into();
        let mut count = 0;
        let info = unsafe {
            (xconn.xlib.XGetVisualInfo)(xconn.display, ffi::VisualIDMask, &mut template, &mut count)
        };
        let info = XSmartPointer::new(xconn, info).ok_or(X11Error::NoSuchVisual(visual_id))?;
        if count < 1 {
            return Err(X11Error::NoSuchVisual(visual_id));
        }
        Some(info)
    };
    let visual = visual_info
        .as_ref()
        .map_or(ptr::null_mut(), |info| info.visual);
    // SAFETY: XSetWindowAttributes is POD; only fields named by mask are read.
    let mut native: ffi::XSetWindowAttributes = unsafe { std::mem::zeroed() };
    native.border_pixel = attributes.border_pixel.unwrap_or(0).into();
    native.event_mask = attributes
        .event_mask
        .map_or(0, |mask| u32::from(mask).into());
    native.colormap = attributes.colormap.unwrap_or(0).into();
    native.override_redirect = attributes.override_redirect.unwrap_or(0) as _;
    let mask = ffi::CWBorderPixel | ffi::CWEventMask | ffi::CWColormap | ffi::CWOverrideRedirect;
    // SAFETY: display is live, visual belongs to it (or is CopyFromParent),
    // and native is initialized for every selected attribute.
    let window = unsafe {
        (xconn.xlib.XCreateWindow)(
            xconn.display,
            parent.into(),
            position.0,
            position.1,
            dimensions.0,
            dimensions.1,
            0,
            depth.into(),
            ffi::InputOutput as _,
            visual,
            mask,
            &mut native,
        )
    };
    xconn.sync_with_server()?;
    if window == 0 {
        return Err(X11Error::UnexpectedNull("XCreateWindow"));
    }
    Ok(window as xproto::Window)
}

/// Origin metadata for only the next event returned from the Xlib queue.
/// No cookie pointer survives publication. A consumed event's metadata expires
/// on an empty queue or the next native dispatch, so it cannot label later input.
#[derive(Clone, Copy)]
pub(super) struct ButtonOrigin {
    pub(super) window: ffi::Window,
    pub(super) time: ffi::Time,
    pub(super) serial: std::os::raw::c_ulong,
    pub(super) button: u32,
    pub(super) event_type: i32,
    pub(super) device_id: xinput::DeviceId,
}

/// XI2 selection suppresses native core pointer delivery to this client.
/// Requeue an equivalent core event through the same Xlib queue Steam reads.
/// The original XI2 motion retains precise positions and scroll valuators;
/// queued motion is overlay-only, while queued ordinary clicks reach winit
/// only if Steam leaves them in the queue.
pub(super) fn expose_pointer_event(
    xconn: &XConnection,
    input: &ffi::XIDeviceEvent,
) -> Option<ButtonOrigin> {
    let button = input.evtype == ffi::XI_ButtonPress || input.evtype == ffi::XI_ButtonRelease;
    if button && !(4..=7).contains(&input.detail) && input.flags & ffi::XIPointerEmulated != 0 {
        // Preserve upstream's touch-versus-emulated-mouse distinction.
        return None;
    }
    let state = (input.mods.effective as u32 & 0xff)
        | ((input.group.effective as u32 & 3) << 13)
        | (1..=5).fold(0, |state, button| {
            let byte = button / 8;
            let pressed = byte < input.buttons.mask_len
                // SAFETY: the live XI2 cookie owns mask_len bytes.
                && unsafe { *input.buttons.mask.add(byte as usize) } & (1 << (button % 8)) != 0;
            state | if pressed { 1 << (7 + button) } else { 0 }
        });
    // SAFETY: XEvent is POD. Its selected pointer member is fully initialized
    // before XPutBackEvent copies it into this display's owned event queue.
    let mut event: ffi::XEvent = unsafe { std::mem::zeroed() };
    if button {
        event.button = ffi::XButtonEvent {
            type_: if input.evtype == ffi::XI_ButtonPress {
                ffi::ButtonPress
            } else {
                ffi::ButtonRelease
            },
            serial: input.serial,
            send_event: input.send_event,
            display: xconn.display,
            window: input.event,
            root: input.root,
            subwindow: input.child,
            time: input.time,
            x: input.event_x as _,
            y: input.event_y as _,
            x_root: input.root_x as _,
            y_root: input.root_y as _,
            state,
            button: input.detail as _,
            same_screen: 1,
        };
    } else {
        event.motion = ffi::XMotionEvent {
            type_: ffi::MotionNotify,
            serial: input.serial,
            send_event: input.send_event,
            display: xconn.display,
            window: input.event,
            root: input.root,
            subwindow: input.child,
            time: input.time,
            x: input.event_x as _,
            y: input.event_y as _,
            x_root: input.root_x as _,
            y_root: input.root_y as _,
            state,
            is_hint: 0,
            same_screen: 1,
        };
    }
    // SAFETY: this connection and event are live and owned by the event thread.
    unsafe { (xconn.xlib.XPutBackEvent)(xconn.display, &mut event) };
    button.then(|| ButtonOrigin {
        window: input.event,
        time: input.time,
        serial: input.serial,
        button: input.detail as _,
        event_type: if input.evtype == ffi::XI_ButtonPress {
            ffi::ButtonPress
        } else {
            ffi::ButtonRelease
        },
        device_id: input.deviceid as _,
    })
}
