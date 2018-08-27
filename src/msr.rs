use byteorder::{ReadBytesExt, NativeEndian, WriteBytesExt};
use num_cpus;

use std::fs::{File, OpenOptions};
use std::io::{self, SeekFrom};
use std::io::prelude::*;


/// Builder structure for reading from a MSR (Model-Specific Register).
pub struct ReadMsrBuilder {
    msr: u64,
    mask: Option<(u32, u32)>,
}


impl ReadMsrBuilder {
    /// Creates a new ReadMsrBuilder for the given MSR.
    pub fn new(msr: u64) -> ReadMsrBuilder {
        ReadMsrBuilder {
            msr,
            mask: None,
        }
    }

    /// Sets the bits to read from
    pub fn mask(&mut self, mask: (u32, u32)) -> &mut ReadMsrBuilder {
        assert!(mask.0 < mask.1);
        self.mask = Some(mask);
        self
    }

    fn extract_bits(&self, val: u64) -> u64 {
        let (from_bit, to_bit) = match self.mask {
            Some(m) => m,
            None => return val,
        };

        // We want bits [from, to], so build a bitmask for those bits (inclusive).
        let mask: u64 = (from_bit..to_bit)
            .map(|b| u64::pow(2, b))
            .sum();

        (val & mask) >> from_bit
    }

    /// Read the value from every CPU in the system as an array.
    pub fn read(&self) -> io::Result<Vec<u64>> {
        let mut res = vec![];
        for i in 0..num_cpus::get() {
            let val = read_one_msr(i, self.msr)?;
            res.push(self.extract_bits(val));
        }

        Ok(res)
    }

    /// Read the value from the first CPU in the system.
    pub fn read_first(&self) -> io::Result<u64> {
        Ok(self.extract_bits(read_one_msr(0, self.msr)?))
    }
}

/// Builder structure for writing to a MSR (Model-Specific Register).
pub struct WriteMsrBuilder {
    msr: u64,
    val: u64,
}

impl WriteMsrBuilder {
    /// Creates a new WriteMsrBuilder with the given MSR/value pair.
    ///
    /// By default, values will be written to every CPU in the system.
    pub fn new(msr: u64, val: u64) -> WriteMsrBuilder {
        WriteMsrBuilder {
            msr,
            val,
        }
    }

    /// Writes the value to all CPUs in the system.
    pub fn write(&self) -> io::Result<()> {
        for cpu in 0..num_cpus::get() {
            if let Err(e) = self.write_one(cpu) {
                eprintln!("error updating cpu {}: {}", cpu, e);
                return Err(e);
            }
        }

        Ok(())
    }

    /// Writes the value to a single CPU in the system.
    pub fn write_one(&self, cpu: usize) -> io::Result<()> {
        write_one_msr(cpu, self.msr, self.val)
    }
}

fn read_one_msr(cpu: usize, msr: u64) -> io::Result<u64> {
    let mut file = File::open(format!("/dev/cpu/{}/msr", cpu))?;
    file.seek(SeekFrom::Start(msr))?;
    file.read_u64::<NativeEndian>()
}

fn write_one_msr(cpu: usize, msr: u64, val: u64) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(false)
        .open(format!("/dev/cpu/{}/msr", cpu))?;
    file.seek(SeekFrom::Start(msr))?;
    file.write_u64::<NativeEndian>(val)?;
    Ok(())
}
