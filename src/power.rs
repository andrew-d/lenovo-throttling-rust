use std::collections::HashMap;
use std::fs::File;
use std::io::prelude::*;
use std::io::ErrorKind;
use std::{thread, time};

use ::channel;
use dbus::{Connection, BusType};
use dbus::arg::{RefArg, Variant};
use failure::Error;


/// Current power state
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PowerState {
    AC,
    Battery,
}

/// Returns the current power status, and a channel that emits power change events.
pub fn notify_on_power_change() -> Result<(PowerState, channel::Receiver<PowerState>), Error> {
    // Get current state first (so we can print diffs)
    let initial_state = is_on_battery()?;

    let (send, recv) = channel::bounded(0);
    thread::spawn(move || {
        // Track current state so we can only emit events when it's changed.
        let mut current_state = initial_state;

        // Start off by polling with D-Bus. This will only return if something goes wrong.
        match poll_dbus(&send, &mut current_state) {
            Ok(_) => {},
            Err(e) => {
                // TODO: logging?
                eprintln!("error in D-Bus polling: {}", e);
            },
        };

        // If we get here, something wonky happened and we got an unexpected message; switch to a
        // simpler poll-based method.
        //
        // TODO(andrew): Allow exiting this somehow.
        let sleep = time::Duration::from_millis(5000);
        loop {
            thread::sleep(sleep);

            match is_on_battery() {
                Ok(new_state) => {
                    if new_state != current_state {
                        send.send(new_state);
                        current_state = new_state;
                    }
                },
                Err(e) => {
                    // TODO: logging?
                    eprintln!("error in sysfs polling: {}", e);
                },
            }
        }
    });

    Ok((initial_state, recv))
}

fn poll_dbus(
    sender: &channel::Sender<PowerState>,
    current_state: &mut PowerState,
) -> Result<(), Error> {

    // Create the D-Bus connection.
    let conn = Connection::get_private(BusType::System)?;
    conn.add_match("interface='org.freedesktop.DBus.Properties',path='/org/freedesktop/UPower/devices/line_power_AC',member='PropertiesChanged'")?;

    // Repeat our dbus loop ~forever
    'outer: loop {
        for msg in conn.incoming(10000) {
            // Look for 'PropertiesChanged' events.
            if let Ok((_name, changed)) = msg.read2::<
                &str,                               // Message name
                HashMap<&str, Variant<Box<RefArg>>> // Changed properties
                // Not used: Vec<&str>              // Invalidated properties
            >() {
                // We only care if there's an argument named 'Online' that's an integer.
                if let Some(val) = changed.get("Online") {
                    if let Some(i) = val.as_i64() {
                        let new_state = if i == 0 {
                            PowerState::Battery
                        } else {
                            PowerState::AC
                        };

                        if new_state != *current_state {
                            sender.send(new_state);
                            *current_state = new_state;
                        }
                        continue;
                    }
                }

                // We're not expecting any other messages, so if we get here, something went wrong;
                // break out and switch to polling.
                bail!("unknown message received");
            }
        }
    }
}

// Returns the current power state of the system.
fn is_on_battery() -> Result<PowerState, Error> {
    let mut f = match File::open("/sys/class/power_supply/AC/online") {
        Ok(f) => f,
        Err(e) => {
            // Assume that we're on battery if we don't find an AC supply
            if e.kind() == ErrorKind::NotFound {
                return Ok(PowerState::Battery);
            }

            return Err(e.into());
        },
    };

    let mut contents = String::new();
    f.read_to_string(&mut contents)?;

    Ok(if contents == "1\n" {
        PowerState::AC
    } else {
        PowerState::Battery
    })
}
