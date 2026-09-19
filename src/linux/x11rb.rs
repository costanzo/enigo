use std::collections::VecDeque;
use std::convert::TryInto;

use log::{debug, error, trace, warn};
use x11rb::{
    connection::Connection,
    protocol::{
        randr::ConnectionExt as _,
        xinput::DeviceUse,
        xproto::{
            ConnectionExt as _, GetKeyboardMappingReply, GetModifierMappingReply, ImageFormat,
            ImageOrder, Screen,
        },
        xtest::ConnectionExt as _,
    },
    rust_connection::{ConnectError, ConnectionError, DefaultStream, ReplyError, RustConnection},
    wrapper::ConnectionExt as _,
};

use super::keymap::{Bind, KeyMap, Keysym};
use crate::screen::{decode_pixel_value, normalize_masked_channel};
use crate::{
    Axis, Button, CaptureError, CaptureFrame, CaptureResult, Coordinate, Direction, Display,
    DisplayId, InputBounds, InputError, InputResult, Key, Keyboard, Mouse, NewConError,
    Screen as CaptureScreen,
};

type CompositorConnection = RustConnection<DefaultStream>;

pub type Keycode = u8;

pub struct Con {
    connection: CompositorConnection,
    screen: Screen,
    keymap: KeyMap<Keycode>,
    modifiers: [Vec<Keycode>; 8],
}

struct CaptureGeometry {
    id: String,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
}

impl From<ConnectionError> for NewConError {
    fn from(error: ConnectionError) -> Self {
        // This should only be possible when trying to get the modifier map
        error!("{error:?}");
        Self::EstablishCon("failed to get the modifier map")
    }
}
impl From<ConnectError> for NewConError {
    fn from(error: ConnectError) -> Self {
        error!("{error:?}");
        Self::EstablishCon("failed to establish the connection")
    }
}
impl From<ReplyError> for NewConError {
    fn from(error: ReplyError) -> Self {
        error!("{error:?}");
        Self::Reply
    }
}
impl Con {
    /// Tries to establish a new X11 connection using the specified parameters
    ///
    /// # Arguments
    ///
    /// * `dpy_name` - If no `dpy_name` is provided, the value from $DISPLAY is
    ///   used
    ///
    /// # Errors
    /// TODO
    pub fn new(dpy_name: Option<&str>) -> Result<Con, NewConError> {
        debug!("using x11rb");
        let (connection, screen_idx) = x11rb::connect(dpy_name)?;
        let setup = connection.setup();
        let screen = setup.roots[screen_idx].clone();
        let min_keycode = setup.min_keycode;
        let max_keycode = setup.max_keycode;
        let (keysyms_per_keycode, keysyms) =
            Self::get_keyboard_mapping(&connection, min_keycode, max_keycode)?; // Check if a mapping is possible
        let unused_keycodes =
            Self::unused_keycodes(min_keycode, max_keycode, keysyms_per_keycode, &keysyms); // Check if a mapping is possible

        if unused_keycodes.is_empty() {
            return Err(NewConError::NoEmptyKeycodes);
        }
        let keymap = KeyMap::new(
            min_keycode,
            max_keycode,
            unused_keycodes,
            keysyms_per_keycode,
            keysyms,
        );

        // Get the keycodes of the modifiers
        let modifiers = Self::find_modifier_keycodes(&connection)?;

        Ok(Con {
            connection,
            screen,
            keymap,
            modifiers,
        })
    }

    /// Find keycodes that have not yet been mapped any keysyms
    fn get_keyboard_mapping(
        connection: &CompositorConnection,
        keycode_min: Keycode,
        keycode_max: Keycode,
    ) -> Result<(u8, Vec<u32>), ReplyError> {
        let GetKeyboardMappingReply {
            keysyms_per_keycode,
            keysyms,
            ..
        } = connection
            .get_keyboard_mapping(keycode_min, keycode_max - keycode_min + 1)?
            .reply()?;

        //let keysyms = keysyms.into_iter().map(|s| Keysym::from(s)).collect();
        Ok((keysyms_per_keycode, keysyms))
    }

    fn unused_keycodes(
        keycode_min: Keycode,
        keycode_max: Keycode,
        keysyms_per_keycode: u8,
        keysyms: &[u32],
    ) -> VecDeque<Keycode> {
        let mut unused_keycodes: VecDeque<Keycode> =
            VecDeque::with_capacity((keycode_max - keycode_min) as usize);

        // Split the mapping into the chunks of keysyms that are mapped to each keycode
        trace!("initial keymap:");
        let keysyms = keysyms.chunks(keysyms_per_keycode as usize);
        for (syms, kc) in keysyms.zip(keycode_min..=keycode_max) {
            // Check if the keycode is unused
            if log::log_enabled!(log::Level::Trace) {
                let syms_name: Vec<Keysym> = syms.iter().map(|&s| Keysym::from(s)).collect();
                trace!("{kc}:  {syms_name:?}");
            }

            // Never use keycode 8
            // Keycode 8 is special: when converted to evdev keycodes,
            // 8 is subtracted, resulting in 0. This typically leads to no effect
            // when simulating input because keycode 0 corresponds to NoSymbol,
            // meaning it has no assigned key mapping.
            if syms.iter().all(|&s| s == Keysym::NoSymbol.raw()) && kc != 8 {
                unused_keycodes.push_back(kc);
            }
        }
        debug!("unused keycodes: {unused_keycodes:?}");
        unused_keycodes
    }

    /// Find the keycodes that must be used for the modifiers
    fn find_modifier_keycodes(
        connection: &CompositorConnection,
    ) -> Result<[Vec<Keycode>; 8], ReplyError> {
        let modifier_reply = connection.get_modifier_mapping()?.reply()?;
        let keycodes_per_modifier = modifier_reply.keycodes_per_modifier() as usize;
        let GetModifierMappingReply {
            keycodes: modifiers,
            ..
        } = modifier_reply;

        let mut modifiers_array: [Vec<Keycode>; 8] = Default::default(); // Initialize with empty vectors
        let modifier_mapping = modifiers.chunks(keycodes_per_modifier);
        if modifier_mapping.len() > 8 {
            error!(
                "the associated keycodes of {} modifiers were returned! Only 8 were expected",
                modifier_mapping.len()
            );
            return Err(ReplyError::ConnectionError(ConnectionError::UnknownError));
        }
        for (mod_no, mod_keycodes) in modifier_mapping.enumerate() {
            let keycodes: Vec<_> = mod_keycodes.iter().copied().filter(|&kc| kc != 0).collect();
            if keycodes.is_empty() {
                warn!("modifier_no: {mod_no} is unmapped");
            }
            modifiers_array[mod_no] = keycodes;
        }
        debug!("the keycodes associated with the modifiers are:\n{modifiers_array:?}");

        Ok(modifiers_array)
    }

    // Get the device id of the first device that is found which has the same usage
    // as the input parameter
    fn device_id(&self, usage: DeviceUse) -> InputResult<u8> {
        x11rb::protocol::xinput::list_input_devices(&self.connection)
            .map_err(|e| {
                error!("{e}");
                InputError::Simulate("error when listing input devices with x11rb")
            })?
            .reply()
            .map_err(|e| {
                error!("{e}");
                InputError::Simulate("error with the reply from listing input devices with x11rb")
            })?
            .devices
            .iter()
            .find(|d| d.device_use == usage)
            .map_or_else(
                || {
                    Err(InputError::Simulate(
                        "error with the reply from listing input devices with x11rb",
                    ))
                },
                |d| Ok(d.device_id),
            )
    }

    fn capture_geometry(&self) -> CaptureGeometry {
        match self.randr_primary_geometry() {
            Ok(Some(geometry)) => geometry,
            Ok(None) => self.root_geometry(),
            Err(error) => {
                warn!("unable to resolve the RandR primary output: {error}");
                self.root_geometry()
            }
        }
    }

    fn root_geometry(&self) -> CaptureGeometry {
        CaptureGeometry {
            id: format!("x11-root-{}", self.screen.root),
            x: 0,
            y: 0,
            width: self.screen.width_in_pixels,
            height: self.screen.height_in_pixels,
        }
    }

    fn randr_primary_geometry(&self) -> Result<Option<CaptureGeometry>, ReplyError> {
        let output = self
            .connection
            .randr_get_output_primary(self.screen.root)?
            .reply()?
            .output;
        if output == x11rb::NONE {
            return Ok(None);
        }
        let resources = self
            .connection
            .randr_get_screen_resources_current(self.screen.root)?
            .reply()?;
        let output_info = self
            .connection
            .randr_get_output_info(output, resources.config_timestamp)?
            .reply()?;
        if output_info.crtc == x11rb::NONE {
            return Ok(None);
        }
        let crtc = self
            .connection
            .randr_get_crtc_info(output_info.crtc, resources.config_timestamp)?
            .reply()?;
        if crtc.width == 0 || crtc.height == 0 {
            return Ok(None);
        }
        Ok(Some(CaptureGeometry {
            id: format!("x11-output-{output}"),
            x: crtc.x,
            y: crtc.y,
            width: crtc.width,
            height: crtc.height,
        }))
    }
}

impl Drop for Con {
    fn drop(&mut self) {
        // Map all previously mapped keycodes to the NoSymbol keysym to revert all
        // changes
        debug!("x11rb connection was dropped");
        for &keycode in self.keymap.keymap_mapping.additionally_mapped.values() {
            match self.connection.bind_key(keycode, Keysym::NoSymbol) {
                Ok(()) => debug!("unmapped keycode {keycode:?}"),
                Err(e) => error!("unable to unmap keycode {keycode:?}. {e:?}"),
            }
        }
    }
}

impl Bind<Keycode> for CompositorConnection {
    fn bind_key(&self, keycode: Keycode, keysym: Keysym) -> Result<(), ()> {
        // A list of two keycodes has to be mapped, otherwise the map is not what would
        // be expected If we would try to map only one keysym, we would get a
        // map that is tolower(keysym), toupper(keysym), tolower(keysym),
        // toupper(keysym), tolower(keysym), toupper(keysym), 0, 0, 0, 0, ...
        // https://stackoverflow.com/a/44334103
        self.change_keyboard_mapping(1, keycode, 2, &[keysym.raw(), keysym.raw()])
            .map_err(|e| error!("error when changing the keyboard mapping with x11rb: {e:?}"))?;
        self.sync().map_err(|e| error!("error when syncing with X server using x11rb after the keyboard mapping was changed: {e:?}"))
    }
}

impl Keyboard for Con {
    fn fast_text(&mut self, _text: &str) -> InputResult<Option<()>> {
        Ok(None)
    }

    fn key(&mut self, key: Key, direction: Direction) -> InputResult<()> {
        let keycode = self.keymap.key_to_keycode(&self.connection, key)?;

        if log::log_enabled!(log::Level::Debug) {
            for (mod_idx, mod_keycodes) in self.modifiers.iter().enumerate() {
                if mod_keycodes.contains(&keycode) {
                    debug!("the key is modifier no: {mod_idx}");
                }
            }
        }

        self.raw(keycode.into(), direction)
    }

    fn raw(&mut self, keycode: u16, direction: Direction) -> InputResult<()> {
        let Ok(keycode) = keycode.try_into() else {
            return Err(InputError::InvalidInput(
                "Keycode was too large. It has to fit in u8 on X11",
            ));
        };
        let time = x11rb::CURRENT_TIME; // CURRENT_TIME == 0
        let root = self.screen.root;
        let root_x = 0;
        let root_y = 0;
        let deviceid = self.device_id(DeviceUse::IS_X_KEYBOARD)?;

        debug!("xtest_fake_input with keycode {keycode}, deviceid {deviceid}, time {time}");
        if direction == Direction::Press || direction == Direction::Click {
            self.connection
                .xtest_fake_input(
                    x11rb::protocol::xproto::KEY_PRESS_EVENT,
                    keycode,
                    time,
                    root,
                    root_x,
                    root_y,
                    deviceid,
                )
                .map_err(|e| {
                    error!("{e}");
                    InputError::Simulate("error when using xtest_fake_input with x11rb")
                })?;
            trace!("press");

            self.connection.sync() .map_err(|e| {
                error!("{e}");
                InputError::Simulate("error when syncing with X server using x11rb after xtest_fake_input was called")
            })?;
        }

        if direction == Direction::Release || direction == Direction::Click {
            self.connection
                .xtest_fake_input(
                    x11rb::protocol::xproto::KEY_RELEASE_EVENT,
                    keycode,
                    time,
                    root,
                    root_x,
                    root_y,
                    deviceid,
                )
                .map_err(|e| {
                    error!("{e}");
                    InputError::Simulate("error when using xtest_fake_input with x11rb")
                })?;
            trace!("released");

            self.connection.sync() .map_err(|e| {
                error!("{e}");
                InputError::Simulate("error when syncing with X server using x11rb after xtest_fake_input was called")
            })?;
        }

        // Let the keymap know that the key was held/no longer held
        // This is important to avoid unmapping held keys
        self.keymap.key(keycode, direction);

        Ok(())
    }
}

impl Mouse for Con {
    fn button(&mut self, button: Button, direction: Direction) -> InputResult<()> {
        let detail = match button {
            Button::Left => 1,
            Button::Middle => 2,
            Button::Right => 3,
            Button::ScrollUp => 4,
            Button::ScrollDown => 5,
            Button::ScrollLeft => 6,
            Button::ScrollRight => 7,
            Button::Back => 8,
            Button::Forward => 9,
        };
        let time = x11rb::CURRENT_TIME; // CURRENT_TIME == 0
        let root = self.screen.root;
        let root_x = 0;
        let root_y = 0;
        let deviceid = self.device_id(DeviceUse::IS_X_POINTER)?;

        debug!("xtest_fake_input with button {detail}, deviceid {deviceid}, time {time}");
        if direction == Direction::Press || direction == Direction::Click {
            self.connection
                .xtest_fake_input(
                    x11rb::protocol::xproto::BUTTON_PRESS_EVENT,
                    detail,
                    time,
                    root,
                    root_x,
                    root_y,
                    deviceid,
                )
                .map_err(|e| {
                    error!("{e}");
                    InputError::Simulate("error when using xtest_fake_input with x11rb")
                })?;

            self.connection.sync()
            .map_err(|e| {
                error!("{e}");
                InputError::Simulate("error when syncing with X server using x11rb after xtest_fake_input was called")
            })?;
        }
        if direction == Direction::Release || direction == Direction::Click {
            self.connection
                .xtest_fake_input(
                    x11rb::protocol::xproto::BUTTON_RELEASE_EVENT,
                    detail,
                    time,
                    root,
                    root_x,
                    root_y,
                    deviceid,
                )
                .map_err(|e| {
                    error!("{e}");
                    InputError::Simulate("error when using xtest_fake_input with x11rb")
                })?;

            self.connection.sync()
            .map_err(|e| {
                error!("{e}");
                InputError::Simulate("error when syncing with X server using x11rb after xtest_fake_input was called")
            })?;
        }
        Ok(())
    }

    fn move_mouse(&mut self, x: i32, y: i32, coordinate: Coordinate) -> InputResult<()> {
        let type_ = x11rb::protocol::xproto::MOTION_NOTIFY_EVENT;
        let detail = match coordinate {
            Coordinate::Rel => 1,
            Coordinate::Abs => 0,
        };
        let time = x11rb::CURRENT_TIME; // CURRENT_TIME == 0
        let root = x11rb::NONE; //  the root window of the screen the pointer is currently on

        let Ok(root_x) = x.try_into() else {
            return Err(InputError::InvalidInput(
                "the coordinates cannot be negative and must fit in i16",
            ));
        };
        let Ok(root_y) = y.try_into() else {
            return Err(InputError::InvalidInput(
                "the coordinates cannot be negative and must fit in i16",
            ));
        };
        let deviceid = self.device_id(DeviceUse::IS_X_POINTER)?;

        debug!(
            "xtest_fake_input with coordinate {detail}, deviceid {deviceid}, x {root_x}, y {root_y}, time {time}"
        );

        self.connection
            .xtest_fake_input(type_, detail, time, root, root_x, root_y, deviceid) // TODO: Check if using x11rb::protocol::xproto::warp_pointer would be better
            .map_err(|e| {
                error!("{e}");
                InputError::Simulate("error when using xtest_fake_input with x11rb")
            })?;
        self.connection.sync()
            .map_err(|e| {
                error!("{e}");
                InputError::Simulate("error when syncing with X server using x11rb after the keyboard mapping was changed")
            })?;
        Ok(())
    }

    fn scroll(&mut self, length: i32, axis: Axis) -> InputResult<()> {
        let button = match (length.is_positive(), axis) {
            (true, Axis::Vertical) => Button::ScrollDown,
            (false, Axis::Vertical) => Button::ScrollUp,
            (true, Axis::Horizontal) => Button::ScrollRight,
            (false, Axis::Horizontal) => Button::ScrollLeft,
        };

        for _ in 0..length.abs() {
            self.button(button, Direction::Click)?;
        }

        Ok(())
    }

    fn main_display(&self) -> InputResult<(i32, i32)> {
        let geometry = self.capture_geometry();
        Ok((geometry.width.into(), geometry.height.into()))
    }

    fn location(&self) -> InputResult<(i32, i32)> {
        let reply = self
            .connection
            .query_pointer(self.screen.root)
            .map_err(|e| {
                error!("{e}");
                InputError::Simulate("error when requesting query_pointer with x11rb")
            })?
            .reply()
            .map_err(|e| {
                error!("{e}");
                InputError::Simulate("error with the reply of query_pointer with x11rb")
            })?;
        Ok((reply.root_x as i32, reply.root_y as i32))
    }
}

impl CaptureScreen for Con {
    fn displays(&mut self) -> CaptureResult<Vec<Display>> {
        let geometry = self.capture_geometry();
        let width = u32::from(geometry.width);
        let height = u32::from(geometry.height);
        Ok(vec![Display {
            id: DisplayId(geometry.id),
            name: Some("X11 primary display".into()),
            primary: true,
            input_bounds: InputBounds {
                x: i32::from(geometry.x),
                y: i32::from(geometry.y),
                width,
                height,
            },
            pixel_width: width,
            pixel_height: height,
        }])
    }

    fn capture(&mut self, display_id: &DisplayId) -> CaptureResult<CaptureFrame> {
        let geometry = self.capture_geometry();
        if display_id.0 != geometry.id {
            return Err(CaptureError::DisplayNotFound);
        }
        let width = geometry.width;
        let height = geometry.height;
        let reply = self
            .connection
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.screen.root,
                geometry.x,
                geometry.y,
                width,
                height,
                u32::MAX,
            )
            .map_err(capture_error)?
            .reply()
            .map_err(capture_error)?;
        let setup = self.connection.setup();
        let format = setup
            .pixmap_formats
            .iter()
            .find(|format| format.depth == reply.depth)
            .ok_or(CaptureError::Unavailable(
                "X11 pixmap format is unavailable",
            ))?;
        let visual = self
            .screen
            .allowed_depths
            .iter()
            .flat_map(|depth| depth.visuals.iter())
            .find(|visual| visual.visual_id == reply.visual)
            .ok_or(CaptureError::Unavailable("X11 visual is unavailable"))?;
        let bits_per_pixel = usize::from(format.bits_per_pixel);
        let bytes_per_pixel = bits_per_pixel.div_ceil(8);
        if !(2..=4).contains(&bytes_per_pixel) {
            return Err(CaptureError::Unavailable("X11 pixel format is unsupported"));
        }
        let scanline_pad = usize::from(format.scanline_pad);
        let row_bits = usize::from(width)
            .checked_mul(bits_per_pixel)
            .ok_or(CaptureError::InvalidFrame("X11 row is too large"))?;
        let row_bytes = row_bits
            .div_ceil(scanline_pad)
            .checked_mul(scanline_pad)
            .and_then(|bits| bits.checked_div(8))
            .ok_or(CaptureError::InvalidFrame("X11 row is too large"))?;
        let expected_len = row_bytes
            .checked_mul(usize::from(height))
            .ok_or(CaptureError::InvalidFrame("X11 frame is too large"))?;
        if reply.data.len() < expected_len {
            return Err(CaptureError::InvalidFrame(
                "X11 image payload is shorter than its geometry",
            ));
        }
        let mut rgba = Vec::with_capacity(
            usize::from(width)
                .checked_mul(usize::from(height))
                .and_then(|pixels| pixels.checked_mul(4))
                .ok_or(CaptureError::InvalidFrame("X11 frame is too large"))?,
        );
        for row in reply.data.chunks_exact(row_bytes).take(usize::from(height)) {
            for pixel in row.chunks_exact(bytes_per_pixel).take(usize::from(width)) {
                let value =
                    decode_pixel_value(pixel, setup.image_byte_order == ImageOrder::LSB_FIRST);
                rgba.extend_from_slice(&[
                    normalize_masked_channel(value, visual.red_mask),
                    normalize_masked_channel(value, visual.green_mask),
                    normalize_masked_channel(value, visual.blue_mask),
                    255,
                ]);
            }
        }
        CaptureFrame::new(
            display_id.clone(),
            u32::from(width),
            u32::from(height),
            InputBounds {
                x: i32::from(geometry.x),
                y: i32::from(geometry.y),
                width: u32::from(width),
                height: u32::from(height),
            },
            rgba,
        )
    }
}

fn capture_error(error: impl std::fmt::Display) -> CaptureError {
    CaptureError::Platform(error.to_string())
}
