//! Driver for 25-series SPI Flash and EEPROM chips.

use crate::{utils::HexSlice, BlockDevice, Error, Read, FastBlockRead};
use bitflags::bitflags;
use core::convert::TryInto;
use core::fmt;
use embedded_hal::blocking::spi::Transfer;
use embedded_hal::digital::v2::OutputPin;

/// 3-Byte JEDEC manufacturer and device identification.
pub struct Identification {
    /// Data collected
    /// - First byte is the manufacturer's ID code from eg JEDEC Publication No. 106AJ
    /// - The trailing bytes are a manufacturer-specific device ID.
    bytes: [u8; 3],

    /// The number of continuations that precede the main manufacturer ID
    continuations: u8,
}

impl Identification {
    /// Build an Identification from JEDEC ID bytes.
    pub fn from_jedec_id(buf: &[u8]) -> Identification {
        // Example response for Cypress part FM25V02A:
        // 7F 7F 7F 7F 7F 7F C2 22 08  (9 bytes)
        // 0x7F is a "continuation code", not part of the core manufacturer ID
        // 0xC2 is the company identifier for Cypress (Ramtron)

        // Find the end of the continuation bytes (0x7F)
        let mut start_idx = 0;
        for i in 0..(buf.len() - 2) {
            if buf[i] != 0x7F {
                start_idx = i;
                break;
            }
        }

        Self {
            bytes: [buf[start_idx], buf[start_idx + 1], buf[start_idx + 2]],
            continuations: start_idx as u8,
        }
    }

    /// The JEDEC manufacturer code for this chip.
    pub fn mfr_code(&self) -> u8 {
        self.bytes[0]
    }

    /// The manufacturer-specific device ID for this chip.
    pub fn device_id(&self) -> &[u8] {
        self.bytes[1..].as_ref()
    }

    /// Number of continuation codes in this chip ID.
    ///
    /// For example the ARM Ltd identifier is `7F 7F 7F 7F 3B` (5 bytes), so
    /// the continuation count is 4.
    pub fn continuation_count(&self) -> u8 {
        self.continuations
    }
}

impl fmt::Debug for Identification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Identification")
            .field(&HexSlice(self.bytes))
            .finish()
    }
}

#[allow(unused)] // TODO support more features
enum Opcode {
    /// Read the 8-bit legacy device ID.
    ReadDeviceId = 0xAB,
    /// Read the 8-bit manufacturer and device IDs.
    ReadMfDId = 0x90,
    /// Read the 8-bit manufacturer and device IDs for Dual I/O chips
    ReadMfDIdDual = 0x92,
    /// Read the 8-bit manufacturer and device IDs for Quad I/O chips
    ReadMfDIdQuad = 0x94,
    /// Read 16-bit manufacturer ID and 8-bit device ID.
    ReadJedecId = 0x9F,
    /// Set the write enable latch.
    WriteEnable = 0x06,
    /// Clear the write enable latch.
    WriteDisable = 0x04,
    /// Read the 8-bit status register.
    ReadStatus = 0x05,
    /// Write the 8-bit status register. Not all bits are writeable.
    WriteStatus = 0x01,
    Read = 0x03,
    /// fast bulk read mode: MOSI ignored
    FastRead = 0x0B,
    PageProg = 0x02, // directly writes to EEPROMs too
    SectorErase = 0x20,
    BlockErase = 0xD8,
    ChipErase = 0xC7,
}

bitflags! {
    /// Status register bits.
    pub struct Status: u8 {
        /// Erase or write in progress.
        const BUSY = 1 << 0;
        /// Status of the **W**rite **E**nable **L**atch.
        const WEL = 1 << 1;
        /// The 3 protection region bits.
        const PROT = 0b00011100;
        /// **S**tatus **R**egister **W**rite **D**isable bit.
        const SRWD = 1 << 7;
    }
}

/// Driver for 25-series SPI Flash chips.
///
/// # Type Parameters
///
/// * **`SPI`**: The SPI master to which the flash chip is attached.
/// * **`CS`**: The **C**hip-**S**elect line attached to the `\CS`/`\CE` pin of
///   the flash chip.
#[derive(Debug)]
pub struct Flash<SPI: Transfer<u8>, CS: OutputPin> {
    spi: SPI,
    cs: CS,
    /// the number of bytes used for addressing: typically 2 or 3
    address_len: u8,
}

impl<SPI: Transfer<u8>, CS: OutputPin> Flash<SPI, CS> {
    /// Creates a new 25-series flash driver.
    ///
    /// # Parameters
    ///
    /// * **`spi`**: An SPI master. Must be configured to operate in the correct
    ///   mode for the device.
    /// * **`cs`**: The **C**hip-**S**elect Pin connected to the `\CS`/`\CE` pin
    ///   of the flash chip. Will be driven low when accessing the device.
    pub fn init(spi: SPI, cs: CS) -> Result<Self, Error<SPI, CS>> {
        Self::init_full(spi,cs,3)
    }

    /// Creates a new 25-series flash driver.
    ///
    /// # Parameters
    ///
    /// * **`spi`**: An SPI master. Must be configured to operate in the correct
    ///   mode for the device.
    /// * **`cs`**: The **C**hip-**S**elect Pin connected to the `\CS`/`\CE` pin
    ///   of the flash chip. Will be driven low when accessing the device.
    /// * **`address_len`**: The number of bytes used for addressing this device: 2 or 3 supported
    pub fn init_full(spi: SPI, cs: CS, address_len: u8) -> Result<Self, Error<SPI, CS>> {
        let mut this = Self { spi, cs, address_len };
        let status = this.read_status()?;
        info!("Flash::init: status = {:?}", status);

        // Here we don't expect any writes to be in progress, and the latch must
        // also be deasserted.
        if !(status & (Status::BUSY | Status::WEL)).is_empty() {
            return Err(Error::UnexpectedStatus);
        }

        Ok(this)
    }

    fn command(&mut self, bytes: &mut [u8]) -> Result<(), Error<SPI, CS>> {
        // If the SPI transfer fails, make sure to disable CS anyways
        self.cs.set_low().map_err(Error::Gpio)?;
        let spi_result = self.spi.transfer(bytes).map_err(Error::Spi);
        self.cs.set_high().map_err(Error::Gpio)?;
        spi_result?;
        Ok(())
    }

    fn read_command(&mut self, cmd_buf: &mut [u8], read_buf: &mut [u8]) -> Result<(), Error<SPI, CS>> {
        self.cs.set_low().map_err(Error::Gpio)?;
        let mut spi_result = self.spi.transfer( cmd_buf);
        if spi_result.is_ok() {
            spi_result = self.spi.transfer(read_buf);
        }
        self.cs.set_high().map_err(Error::Gpio)?;
        spi_result.map(|_| ()).map_err(Error::Spi)?;
        Ok(())
    }

    /// Reads the JEDEC manufacturer/device identification.
    pub fn read_jedec_id(&mut self) -> Result<Identification, Error<SPI, CS>> {
        // Optimistically read 12 bytes, even though some identifiers will be shorter
        let mut buf: [u8; 12] = [0; 12];
        buf[0] = Opcode::ReadJedecId as u8;
        self.command(&mut buf)?;

        // Skip buf[0] (SPI read response byte)
        Ok(Identification::from_jedec_id(&buf[1..]))
    }
	
	pub fn read_mfd_id(&mut self) -> Result<Identification, Error<SPI, CS>> {
		// Read three bytes from ReadMfDId address
        let mut buf: [u8; 4] = [Opcode::ReadMfDId as u8, 0, 0, 0];
        self.command(&mut buf)?;

        // Skip buf[0] (SPI read response byte)
        Ok(Identification::from_jedec_id(&buf[1..]))
	}
	
	/// Try multiple methods to find a valid device identifier.
	pub fn find_device_identifier(&mut self) -> Result<Identification, Error<SPI, CS>> {
		let test1 = self.read_jedec_id()?;
		if test1.mfr_code() == 0  {
			//No such manufacturer code: Try another method
			let test2 = self.read_mfd_id()?;
			return Ok(test2);
		}
		
		return Ok(test1);
	}

    /// Reads the status register.
    pub fn read_status(&mut self) -> Result<Status, Error<SPI, CS>> {
        let mut buf = [Opcode::ReadStatus as u8, 0];
        self.command(&mut buf)?;

        Ok(Status::from_bits_truncate(buf[1]))
    }

    fn write_enable(&mut self) -> Result<(), Error<SPI, CS>> {
        let mut cmd_buf = [Opcode::WriteEnable as u8];
        self.command(&mut cmd_buf)?;
        Ok(())
    }

    fn wait_done(&mut self) -> Result<(), Error<SPI, CS>> {
        // TODO: Consider changing this to a delay based pattern
        while self.read_status()?.contains(Status::BUSY) {}
        Ok(())
    }

}

impl<SPI: Transfer<u8>, CS: OutputPin> Read<u32, SPI, CS> for Flash<SPI, CS> {
    /// Reads flash contents into `buf`, starting at `addr`.
    ///
    /// Note that `addr` is not fully decoded: Flash chips will typically only
    /// look at the lowest `N` bits needed to encode their size, which means
    /// that the contents are "mirrored" to addresses that are a multiple of the
    /// flash size. Only 24 bits of `addr` are transferred to the device in any
    /// case, limiting the maximum size of 25-series SPI flash chips to 16 MiB.
    ///
    /// # Parameters
    ///
    /// * `addr`: 24-bit address to start reading at.
    /// * `buf`: Destination buffer to fill.
    fn read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Error<SPI, CS>> {
        // TODO what happens if `buf` is empty?
        match self.address_len {
            3 => {
                self.read_command(&mut [
                    Opcode::Read as u8,
                    (addr >> 16) as u8,
                    (addr >> 8) as u8,
                    addr as u8,
                ], buf )
            }
            _ => {
                self.read_command(&mut [
                    Opcode::Read as u8,
                    (addr >> 8) as u8,
                    addr as u8,
                ], buf)
            }
        }
    }
}

impl<SPI: Transfer<u8>, CS: OutputPin> FastBlockRead<u32, SPI, CS> for Flash<SPI, CS> {
    /// Reads flash contents into `buf`, starting at `addr`
    /// using the fast block read mode, where the device will
    /// continue writing to MISO as long as it receives a clock signal
    /// (and MOSI is ignored).
    ///
    /// # Parameters
    ///
    /// * `addr`: 24-bit address to start reading at. (Supported addresses vary by device.)
    /// * `buf`: Destination buffer to fill.
    fn fast_block_read(&mut self, addr: u32, buf: &mut [u8]) -> Result<(), Error<SPI, CS>> {

        // This mode is invoked by sending { opcode, device address, dummy byte }

        match self.address_len {
            3 => {
                self.read_command(&mut [
                    Opcode::FastRead as u8,
                    (addr >> 16) as u8,
                    (addr >> 8) as u8,
                    addr as u8,
                    0 as u8, //dummy byte
                ], buf)
            }
            _ => {
                self.read_command(&mut [
                    Opcode::FastRead as u8,
                    (addr >> 8) as u8,
                    addr as u8,
                    0 as u8, //dummy byte
                ], buf)
            }
        }

    }
}

impl<SPI: Transfer<u8>, CS: OutputPin> BlockDevice<u32, SPI, CS> for Flash<SPI, CS> {
    fn erase_sectors(&mut self, addr: u32, amount: usize) -> Result<(), Error<SPI, CS>> {
        for c in 0..amount {
            self.write_enable()?;

            let current_addr: u32 = (addr as usize + c * 256).try_into().unwrap();
            let mut cmd_buf = [
                Opcode::SectorErase as u8,
                (current_addr >> 16) as u8,
                (current_addr >> 8) as u8,
                current_addr as u8,
            ];
            self.command(&mut cmd_buf)?;
            self.wait_done()?;
        }

        Ok(())
    }

    fn write_bytes(&mut self, addr: u32, data: &mut [u8]) -> Result<(), Error<SPI, CS>> {
        for (c, chunk) in data.chunks_mut(256).enumerate() {
            self.write_enable()?;

            let current_addr: u32 = (addr as usize + c * 256).try_into().unwrap();
            let mut cmd_buf = [
                Opcode::PageProg as u8,
                (current_addr >> 16) as u8,
                (current_addr >> 8) as u8,
                current_addr as u8,
            ];

            self.cs.set_low().map_err(Error::Gpio)?;
            let mut spi_result = self.spi.transfer(&mut cmd_buf);
            if spi_result.is_ok() {
                spi_result = self.spi.transfer(chunk);
            }
            self.cs.set_high().map_err(Error::Gpio)?;
            spi_result.map(|_| ()).map_err(Error::Spi)?;
            self.wait_done()?;
        }
        Ok(())
    }

    fn erase_all(&mut self) -> Result<(), Error<SPI, CS>> {
        self.write_enable()?;
        let mut cmd_buf = [Opcode::ChipErase as u8];
        self.command(&mut cmd_buf)?;
        self.wait_done()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_jedec_id() {
        let cypress_id_bytes = [0x7F, 0x7F, 0x7F, 0x7F, 0x7F, 0x7F, 0xC2, 0x22, 0x08];
        let ident = Identification::from_jedec_id(&cypress_id_bytes);
        assert_eq!(0xC2, ident.mfr_code());
        assert_eq!(6, ident.continuation_count());
        let device_id = ident.device_id();
        assert_eq!(device_id[0], 0x22);
        assert_eq!(device_id[1], 0x08);
		
		// ID bytes from actual read_mfd_id
		let winbond_id_bytes =  [0x17, 0x40, 0x18];
        let ident = Identification::from_jedec_id(&cypress_id_bytes);
        assert_eq!(0x17, ident.mfr_code());
        assert_eq!(0, ident.continuation_count());
        let device_id = ident.device_id();
        assert_eq!(device_id[0], 0x40);
        assert_eq!(device_id[1], 0x18);
    }
	

	
}
