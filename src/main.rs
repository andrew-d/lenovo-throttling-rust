extern crate byteorder;
#[macro_use]
extern crate crossbeam_channel as channel;
extern crate dbus;
#[macro_use]
extern crate failure;
extern crate num_cpus;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate toml;

use std::fs::File;
use std::io;
use std::io::prelude::*;

use failure::Error;

mod msr;
mod power;
// mod util;


#[derive(Deserialize, Debug)]
struct Config {
    /// Configuration to apply when on battery.
    battery: ModeConfig,

    /// Configuration to apply when on AC power.
    ac: ModeConfig,
}

// Configuration for a specific power configuration
#[derive(Deserialize, Debug)]
struct ModeConfig {
    /// How often to reset configuration, in seconds.
    update_rate_sec: Option<usize>,

    /// Maximum package power for time window #1.
    pl1_tdp_w: Option<u64>,
    /// Time window #1 duration.
    pl1_duration: Option<u64>,

    /// Maximum package power for time window #2.
    pl2_tdp_w: Option<u64>,
    /// Time window #2 duration.
    pl2_duration: Option<u64>,

    /// Maximum CPU temperature before throttling.
    maximum_temp_c: Option<u64>,

    /// Whether to set HWP performance hints to 'performance' at high load.
    hwp_mode: Option<bool>,
}


fn main() {
    let config = match read_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error reading config: {}", e);
            return;
        },
    };
    println!("config = {:?}", config);

    let (initial, power_change) = power::notify_on_power_change().unwrap();
    println!("initial power state is: {:?}", initial);

    'outer: loop {
        let power_state = select_loop! {
            recv(power_change, state) => {
                println!("power state is: {:?}", state);
                state
            },

            disconnected() => break 'outer,
        };

        // TODO(andrew): the new state if we're using new crossbeam-channel
        //select! {
        //    recv(power_change, state) => {
        //        if let Some(state) = state {
        //            println!("power state is: {:?}", state);
        //        } else {
        //            println!("power handling failed");
        //            break 'outer;
        //        }
        //    },
        //}

        // Given the state, find our configuration and set variables.
        let conf = match power_state {
            power::PowerState::Battery => &config.battery,
            power::PowerState::AC      => &config.ac,
        };

        // Set the temperature trip target.
        if let Some(max_temp) = conf.maximum_temp_c {
            // Get the maximum temperature for the CPU (default to 100 if we can't since that's
            // ~almost always true.
            let abs_max = match msr::ReadMsrBuilder::new(0x1A2).mask((16, 23)).read_first() {
                Ok(v) => v,
                // TODO: print error
                Err(e) => 100,
            };

            // MSR layout:
            //
            //  Reserved    Maximum
            //    |           CPU
            //    |        Temperature
            //    |        (bits 23:16)
            //    |            |
            //    v            v
            //    00 000000 00000000 0000000000000000
            //         ^                    ^
            //         |                    |
            //      Temperature          Reserved
            //         Trip             (bits 0:15)
            //         Point
            //      (bits 29:24)
            //
            if let Ok(existing) = msr::ReadMsrBuilder::new(0x1A2).read_first() {
                // Mask out the bits with our target value.
                let mask = ((abs_max - max_temp) & 0b111111) << 24;
                let new = (existing & 0b11000000111111111111111111111111) | (mask as u64);

                println!("existing = {:032b}", existing);
                println!("new      = {:032b}", new);

                match msr::WriteMsrBuilder::new(0x1A2, new).write() {
                    Ok(_) => {},
                    Err(e) => eprintln!("error writing MSR: {}", e),
                }
            } else {
                eprintln!("could not get existing MSR value");
            }
        }
    }
}

fn read_config() -> Result<Config, Error> {
    let mut file = File::open("config.toml")?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;

    Ok(toml::from_str(&*contents)?)
}
