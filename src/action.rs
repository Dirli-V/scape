use std::process::Command;

use mlua::Function as LuaFunction;
use tracing::{error, info};

use crate::State;

#[derive(Debug)]
pub enum Action {
    /// Quit the compositor
    Quit,
    /// Trigger a vt-switch
    VtSwitch(i32),
    /// Spawn a command
    Spawn { command: String },
    /// Focus or spawn a command
    FocusOrSpawn { app_id: String, command: String },
    /// Scales output up/down
    ChangeScale { percentage_points: isize },
    /// Sets output scale
    SetScale { percentage: usize },
    /// Rotate output
    RotateOutput { output: usize, rotation: usize },
    /// Move window to zone
    MoveWindow { window: Option<usize>, zone: String },
    /// Run Lua callback
    Callback(LuaFunction<'static>),
    /// Tab through windows
    Tab { index: usize },
    /// Do nothing more
    None,
}

impl State {
    pub fn execute(&mut self, action: Action) {
        info!(?action, "Executing action");
        match action {
            Action::Quit => self.stop_loop(),
            Action::VtSwitch(vt) => {
                info!(to = vt, "Trying to switch vt");
                if let Err(err) = self.backend_data.switch_vt(vt) {
                    error!(vt, "Error switching vt: {}", err);
                }
            }
            Action::Spawn { command } => self.spawn(&command),
            Action::ChangeScale {
                percentage_points: _,
            } => todo!(),
            Action::SetScale { percentage: _ } => todo!(),
            Action::RotateOutput {
                output: _,
                rotation: _,
            } => todo!(),
            Action::MoveWindow { window: _, zone } => {
                let (space_name, space) = self.spaces.iter().next().unwrap();
                if let Some(window) = space.elements().last().cloned() {
                    self.place_window(&space_name.to_owned(), &window, false, Some(&zone), true);
                }
            }
            Action::Tab { index } => {
                let (space_name, space) = self.spaces.iter().next().unwrap();
                let maybe_window = space.elements().rev().nth(index).cloned();
                if let Some(window) = maybe_window {
                    self.focus_window(window, &space_name.to_owned());
                }
            }
            Action::Callback(callback) => callback.call(()).unwrap(),
            Action::FocusOrSpawn { app_id, command } => {
                if !self.focus_window_by_app_id(app_id) {
                    self.execute(Action::Spawn { command });
                }
            }
            Action::None => {}
        }
    }

    fn spawn(&self, command: &str) {
        info!(command, "Starting program");

        if let Err(e) = Command::new(command)
            .envs(
                self.socket_name
                    .clone()
                    .map(|v| ("WAYLAND_DISPLAY", v))
                    .into_iter()
                    .chain(
                        self.xwayland_state
                            .as_ref()
                            .and_then(|v| v.display)
                            .map(|v| ("DISPLAY", format!(":{}", v))),
                    ),
            )
            .spawn()
        {
            error!(command, err = %e, "Failed to start program");
        }
    }
}
