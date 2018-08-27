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
use std::cmp;
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
    pl1_duration: Option<f64>,

    /// Maximum package power for time window #2.
    pl2_tdp_w: Option<u64>,
    /// Time window #2 duration.
    pl2_duration: Option<f64>,

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

    let msr_updates_battery = build_msr_updates(&config.battery).unwrap();
    let msr_updates_ac      = build_msr_updates(&config.ac).unwrap();

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

        // Given the state, select the right set of MSR updates.
        let msr_updates = match power_state {
            power::PowerState::Battery => &msr_updates_battery,
            power::PowerState::AC      => &msr_updates_ac,
        };

        // Write our MSRs.
        for &(msr, value) in msr_updates.iter() {
            match msr::WriteMsrBuilder::new(msr, value).write() {
                Err(e) => eprintln!("error writing MSR {:x}: {}", msr, e),
                Ok(_) => eprintln!("set MSR {:x} successfully", msr),
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

fn build_msr_updates(conf: &ModeConfig) -> Result<Vec<(u64, u64)>, Error> {
    // Build MSR update values.
    let mut msr_updates: Vec<(u64, u64)> = vec![];

    // MSR_TEMPERATURE_TARGET: Maximum temperature for the CPU.
    if let Some(max_temp) = conf.maximum_temp_c {
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

        // Read register.
        let msr_value = msr::ReadMsrBuilder::new(0x1A2).read_first()?;

        // Get the critical temperature for the CPU.
        let critical_temp = (msr_value >> 16) & 0b11111111;

        // Ensure we don't go within 3 degrees of the critical target.
        let max_temp = cmp::min(max_temp, critical_temp - 3);

        // Calculate the value we're going to write back by masking out the bits with our target
        // value.
        let mask = ((critical_temp - max_temp) & 0b111111) << 24;
        let new_value = (msr_value & 0b11000000111111111111111111111111) | (mask as u64);

        println!("MSR_TEMPERATURE_TARGET: old = {:032b}", msr_value);
        println!("MSR_TEMPERATURE_TARGET: new = {:032b}", new_value);

        msr_updates.push((0x1A2, new_value));
    }

    // MSR_RAPL_POWER_UNIT brief documentation:
    //
    //      Reserved      Reserved   Reserved
    //          |            |         |
    //          v            v         v
    //    000000000000 0000 000 00000 0000 0000
    //                  ^         ^          ^
    //                  |         |          |
    //                Time      Energy     Power
    //                Units     Status     Units
    //                          Units
    //
    // Per the Intel SDM Volume 3:
    //
    //   Time Units (bits 19:16): Time related information (in Seconds) is based on the multiplier,
    //   1/ 2^TU; where TU is an unsigned integer represented by bits 19:16.
    //   Default value is 1010b, indicating time unit is in 976 micro-seconds increment.
    //
    //   Energy Status Units (bits 12:8): Energy related information (in Joules) is based on the
    //   multiplier, 1/2^ESU; where ESU is an unsigned integer represented by bits 12:8.
    //   Default value is 10000b, indicating energy status unit is in 15.3 micro-Joules increment
    //
    //   Power Units (bits 3:0): Power related information (in Watts) is based on the multiplier,
    //   1/ 2^PU; where PU is an unsigned integer represented by bits 3:0.
    //   Default value is 0011b, indicating power unit is in 1/8 Watts increment.
    //
    //

    let rapl_power_unit = msr::ReadMsrBuilder::new(0x606).read_first()?;

    // Calculate the power and time units by following the formulas above.
    let power_unit = rapl_power_unit & 0b1111;
    let power_unit = 1.0f64 / u64::pow(2, power_unit as u32) as f64;

    let time_unit = (rapl_power_unit >> 16) & 0b1111;
    let time_unit = 1.0f64 / u64::pow(2, time_unit as u32) as f64;

    println!("power unit = {}", power_unit);
    println!("time unit  = {}", time_unit);

    // MSR_PKG_POWER_LIMIT brief documentation:
    //
    //   Lock     Time
    //    |       Window
    //    |        For      Enable                           Package      Package
    //    |       Power     Power                            Clamping      Power
    //    |      Limit #2   Limit #2            Reserved    Limitation    Limit #1
    //    |         |         |                    |             |           |
    //    v         v         v                    v             v           v
    //    0 0000000 0000000 0 0 000000000000000 00000000 0000000 0 0 000000000000000
    //         ^            ^          ^                  ^        ^
    //         |            |          |                  |        |
    //       Reserved   Package     Package             Time      Enable
    //                  Clamping     Power              Window    Power
    //                 Limitation   Limit #2             For      Limit #1
    //                                                  Power
    //                                                 Limit #1
    //
    //   Package Power Limit #1 (bits 14:0): Sets the average power usage limit of the package
    //   domain corresponding to time window # 1. The unit of this field is specified by the
    //   "Power Units" field of MSR_RAPL_POWER_UNIT.
    //
    //   Enable Power Limit #1 (bit 15): 0 = disabled; 1 = enabled.
    //
    //   Package Clamping Limitation #1 (bit 16): Allow going below OS-requested P/T state setting
    //   during time window specified by bits 23:17.
    //
    //   Time Window for Power Limit #1 (bits 23:17): Indicates the time window for power limit #1
    //     Time limit = 2^Y * (1.0 + Z/4.0) * Time_Unit
    //   Here "Y" is the unsigned integer value represented by bits 21:17, "Z" is an unsigned
    //   integer represented by bits 23:22. "Time_Unit" is specified by the "Time Units" field of
    //   MSR_RAPL_POWER_UNIT. This field may have a hard-coded value in hardware and ignores values
    //   written by software.
    //
    //   Package Power Limit #2 (bits 46:32): Sets the average power usage limit of the package
    //   domain corresponding to time window # 2. The unit of this field is specified by the
    //   "Power Units" field of MSR_RAPL_POWER_UNIT.
    //
    //   Enable Power Limit #2 (bit 47): 0 = disabled; 1 = enabled.
    //
    //   Package Clamping Limitation #2 (bit 48): Allow going below OS-requested P/T state setting
    //   during time window specified by bits 23:17.
    //
    //   Time Window for Power Limit #2 (bits 55:49): Indicates the time window for power limit #2
    //     Time limit = 2^Y * (1.0 + Z/4.0) * Time_Unit
    //   Here "Y" is the unsigned integer value represented by bits 53:49, "Z" is an unsigned
    //   integer represented by bits 55:54. "Time_Unit" is specified by the "Time Units" field of
    //   MSR_RAPL_POWER_UNIT. This field may have a hard-coded value in hardware and ignores values
    //   written by software.
    //
    //   Lock (bit 63): If set, all write attempts to this MSR are ignored until next RESET.
    //

    // Get the initial value for the power limit (MSR_PKG_POWER_LIMIT)
    let initial_power_limit = msr::ReadMsrBuilder::new(0x610).read_first()?;

    // TODO: check lock bit

    // Build all possible time limit values, which we use below in order to find the closest one to
    // the input value.
    //
    // Note that Y is 5 bits, so the max value is 31, and Z is 2, so the max value is 3
    let time_limits: Vec<(f64, u32, u32)> = {
        let mut arr = (0..(31+1)).flat_map(|y| {
            (0..(3+1)).map(|z| {
                let lim = u64::pow(2, y) as f64 * (1.0f64 + (z as f64) / 4.0) * time_unit;

                (lim, y, z)
            }).collect::<Vec<_>>()
        }).collect::<Vec<_>>();

        // Sort by the limit itself.
        arr.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
        arr
    };

    println!("time limits = {:?}", time_limits);

    // This is the value we'll set, if config flags are given.
    let mut new_power_limit = initial_power_limit;

    {
        // Helper function to take a TDP & duration and mask the new_power_limit variable.
        let mut do_mask = |tdp: u64, duration: f64, offset: u64| {
            // Iterate through the time_limits array until we find the first duration that's
            // smaller than the given duration.
            // This is inefficient, but... probably fine.
            let (y, z) = time_limits.iter()
                .find(|&&(lim, _, _)| duration <= lim)
                .map(|&(_, y, z)| (y, z))
                .unwrap();

            println!("PL#: y = {}, z = {}", y, z);

            // Make the time window.
            let tw = (y | (z << 5)) as u64;

            // The actual power limit is just the number given, in terms of the unit.
            // TODO: detect when larger than 15 bits
            let pl = (tdp as f64 / power_unit).round() as u64;

            // The bitmask that we're clearing; these are the Time Window and Package Power Limit fields
            // for PL1, with an optional offset, then binary negated so that we're keeping everything
            // *except* these values;
            let clear: u64 = !(0b111111100111111111111111 << offset);

            // The bitmask that we're setting; as above, the correct values, then shifted.
            // Note that we also set the "enable" bit.
            let set: u64 = (pl | (1 << 15) | tw << 17) << offset;

            // Perform the mask.
            new_power_limit = new_power_limit & clear;
            new_power_limit = new_power_limit | set;
        };

        // Set PL 1 and 2 if given.
        match (conf.pl1_tdp_w, conf.pl1_duration) {
            (Some(tdp), Some(duration)) => {
                do_mask(tdp, duration, 0);
            },
            _ => {},
        }
        match (conf.pl2_tdp_w, conf.pl2_duration) {
            (Some(tdp), Some(duration)) => {
                do_mask(tdp, duration, 32);
            },
            _ => {},
        }
    }

    // Set the MSR update if we've changed anything.
    if new_power_limit != initial_power_limit {
        msr_updates.push((0x610, new_power_limit));
    }

    // TODO: add support for cTDP

    Ok(msr_updates)
}
