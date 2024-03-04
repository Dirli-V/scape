use crate::State;
use smithay::{
    delegate_xdg_activation,
    input::Seat,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    wayland::xdg_activation::{
        XdgActivationHandler, XdgActivationState, XdgActivationToken, XdgActivationTokenData,
    },
};

impl XdgActivationHandler for State {
    fn activation_state(&mut self) -> &mut XdgActivationState {
        &mut self.xdg_activation_state
    }

    fn token_created(&mut self, _token: XdgActivationToken, data: XdgActivationTokenData) -> bool {
        if let Some((serial, seat)) = data.serial {
            let keyboard = self.seat.get_keyboard().unwrap();
            Seat::from_resource(&seat) == Some(self.seat.clone())
                && keyboard
                    .last_enter()
                    .map(|last_enter| serial.is_no_older_than(&last_enter))
                    .unwrap_or(false)
        } else {
            false
        }
    }

    fn request_activation(
        &mut self,
        _token: XdgActivationToken,
        token_data: XdgActivationTokenData,
        surface: WlSurface,
    ) {
        if token_data.timestamp.elapsed().as_secs() < 10 {
            // Just grant the wish
            let w = self
                .space
                .elements()
                .find(|window| window.wl_surface().map(|s| s == surface).unwrap_or(false))
                .cloned();
            if let Some(window) = w {
                self.space.raise_element(&window, true);
            }
        }
    }
}

delegate_xdg_activation!(State);
