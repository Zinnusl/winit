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

/// Scalar metadata for the next matching server-core button edge.
/// XKB grab notifications can intervene; other dispatch and empty polling
/// expire it. No Xlib cookie pointer survives its owning dispatch.
#[derive(Clone, Copy)]
pub(super) struct ButtonOrigin {
    pub(super) time: ffi::Time,
    pub(super) serial: std::os::raw::c_ulong,
    pub(super) event_type: i32,
    pub(super) device_id: xinput::DeviceId,
    pub(super) emulated: bool,
}

/// XI2 raw edges precede core edges and carry both the master identity and
/// emulation flag. Steam ignores their raw event types. Selecting window XI2
/// slave buttons instead would make Steam see that click twice.
/// Raw details are physical numbers; core details are mapped logical numbers.
/// Queue adjacency, timestamp, serial and edge type identify the counterpart.
pub(super) fn button_origin(input: &ffi::XIRawEvent) -> ButtonOrigin {
    ButtonOrigin {
        time: input.time,
        serial: input.serial,
        event_type: if input.evtype == ffi::XI_RawButtonPress {
            ffi::ButtonPress
        } else {
            ffi::ButtonRelease
        },
        device_id: input.deviceid as _,
        emulated: input.flags & ffi::XIPointerEmulated != 0,
    }
}
