//! Register types and the core interface for armv7-a

use crate::architecture::arm::core::armv7a_debug_regs::*;
use crate::architecture::arm::core::register;
use crate::architecture::arm::sequences::ArmDebugSequence;
use crate::core::{RegisterFile, RegisterValue};
use crate::error::Error;
use crate::memory::{valid_32_address, Memory};
use crate::CoreInterface;
use crate::CoreStatus;
use crate::DebugProbeError;
use crate::MemoryInterface;
use crate::RegisterId;
use crate::{Architecture, CoreInformation, CoreType, InstructionSet};
use anyhow::Result;

use super::instructions::aarch32::{
    build_bx, build_ldc, build_mcr, build_mov, build_mrc, build_mrs, build_stc,
};
use super::CortexAState;
use super::ARM_REGISTER_FILE;

use std::mem::size_of;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

/// Errors for the ARMv7-A state machine
#[derive(thiserror::Error, Debug)]
pub enum Armv7aError {
    /// Invalid register number
    #[error("Register number {0} is not valid for ARMv7-A")]
    InvalidRegisterNumber(u16),

    /// Not halted
    #[error("Core is running but operation requires it to be halted")]
    NotHalted,

    /// Data Abort occurred
    #[error("A data abort occurred")]
    DataAbort,
}

/// Interface for interacting with an ARMv7-A core
pub struct Armv7a<'probe> {
    memory: Memory<'probe>,

    state: &'probe mut CortexAState,

    base_address: u64,

    sequence: Arc<dyn ArmDebugSequence>,

    num_breakpoints: Option<u32>,

    itr_enabled: bool,
}

impl<'probe> Armv7a<'probe> {
    pub(crate) fn new(
        mut memory: Memory<'probe>,
        state: &'probe mut CortexAState,
        base_address: u64,
        sequence: Arc<dyn ArmDebugSequence>,
    ) -> Result<Self, Error> {
        if !state.initialized() {
            // determine current state
            let address = Dbgdscr::get_mmio_address(base_address);
            let dbgdscr = Dbgdscr(memory.read_word_32(address)?);

            log::debug!("State when connecting: {:x?}", dbgdscr);

            let core_state = if dbgdscr.halted() {
                let reason = dbgdscr.halt_reason();

                log::debug!("Core was halted when connecting, reason: {:?}", reason);

                CoreStatus::Halted(reason)
            } else {
                CoreStatus::Running
            };

            state.current_state = core_state;
            state.register_cache = vec![None; 17];
            state.initialize();
        }

        Ok(Self {
            memory,
            state,
            base_address,
            sequence,
            num_breakpoints: None,
            itr_enabled: false,
        })
    }

    /// Execute an instruction
    fn execute_instruction(&mut self, instruction: u32) -> Result<Dbgdscr, Error> {
        if !self.state.current_state.is_halted() {
            return Err(Error::architecture_specific(Armv7aError::NotHalted));
        }

        // Enable ITR if needed
        if !self.itr_enabled {
            let address = Dbgdscr::get_mmio_address(self.base_address);
            let mut dbgdscr = Dbgdscr(self.memory.read_word_32(address)?);
            dbgdscr.set_itren(true);

            self.memory.write_word_32(address, dbgdscr.into())?;

            self.itr_enabled = true;
        }

        // Run instruction
        let address = Dbgitr::get_mmio_address(self.base_address);
        self.memory.write_word_32(address, instruction)?;

        // Wait for completion
        let address = Dbgdscr::get_mmio_address(self.base_address);
        let mut dbgdscr = Dbgdscr(self.memory.read_word_32(address)?);

        while !dbgdscr.instrcoml_l() {
            dbgdscr = Dbgdscr(self.memory.read_word_32(address)?);
        }

        // Check if we had any aborts, if so clear them and fail
        if dbgdscr.adabort_l() || dbgdscr.sdabort_l() {
            let address = Dbgdrcr::get_mmio_address(self.base_address);
            let mut dbgdrcr = Dbgdrcr(0);
            dbgdrcr.set_cse(true);

            self.memory.write_word_32(address, dbgdrcr.into())?;

            return Err(Error::architecture_specific(Armv7aError::DataAbort));
        }

        Ok(dbgdscr)
    }

    /// Execute an instruction on the CPU and return the result
    fn execute_instruction_with_result(&mut self, instruction: u32) -> Result<u32, Error> {
        // Run instruction
        let mut dbgdscr = self.execute_instruction(instruction)?;

        // Wait for TXfull
        while !dbgdscr.txfull_l() {
            let address = Dbgdscr::get_mmio_address(self.base_address);
            dbgdscr = Dbgdscr(self.memory.read_word_32(address)?);
        }

        // Read result
        let address = Dbgdtrtx::get_mmio_address(self.base_address);
        let result = self.memory.read_word_32(address)?;

        Ok(result)
    }

    fn execute_instruction_with_input(
        &mut self,
        instruction: u32,
        value: u32,
    ) -> Result<(), Error> {
        // Move value
        let address = Dbgdtrrx::get_mmio_address(self.base_address);
        self.memory.write_word_32(address, value)?;

        // Wait for RXfull
        let address = Dbgdscr::get_mmio_address(self.base_address);
        let mut dbgdscr = Dbgdscr(self.memory.read_word_32(address)?);

        while !dbgdscr.rxfull_l() {
            dbgdscr = Dbgdscr(self.memory.read_word_32(address)?);
        }

        // Run instruction
        self.execute_instruction(instruction)?;

        Ok(())
    }

    fn reset_register_cache(&mut self) {
        self.state.register_cache = vec![None; 17];
    }

    /// Sync any updated registers back to the core
    fn writeback_registers(&mut self) -> Result<(), Error> {
        for i in 0..self.state.register_cache.len() {
            if let Some((val, writeback)) = self.state.register_cache[i] {
                if writeback {
                    match i {
                        0..=14 => {
                            let instruction = build_mrc(14, 0, i as u16, 0, 5, 0);

                            self.execute_instruction_with_input(instruction, val.try_into()?)?;
                        }
                        15 => {
                            // Move val to r0
                            let instruction = build_mrc(14, 0, 0, 0, 5, 0);

                            self.execute_instruction_with_input(instruction, val.try_into()?)?;

                            // BX r0
                            let instruction = build_bx(0);
                            self.execute_instruction(instruction)?;
                        }
                        _ => {
                            panic!("Logic missing for writeback of register {}", i);
                        }
                    }
                }
            }
        }

        self.reset_register_cache();

        Ok(())
    }

    /// Save r0 if needed before it gets clobbered by instruction execution
    fn prepare_r0_for_clobber(&mut self) -> Result<(), Error> {
        if self.state.register_cache[0].is_none() {
            // cache r0 since we're going to clobber it
            let r0_val: u32 = self.read_core_reg(RegisterId(0))?.try_into()?;

            // Mark r0 as needing writeback
            self.state.register_cache[0] = Some((r0_val.into(), true));
        }

        Ok(())
    }

    fn set_r0(&mut self, value: u32) -> Result<(), Error> {
        let instruction = build_mrc(14, 0, 0, 0, 5, 0);

        self.execute_instruction_with_input(instruction, value)
    }
}

impl<'probe> CoreInterface for Armv7a<'probe> {
    fn wait_for_core_halted(&mut self, timeout: Duration) -> Result<(), Error> {
        // Wait until halted state is active again.
        let start = Instant::now();

        let address = Dbgdscr::get_mmio_address(self.base_address);

        while start.elapsed() < timeout {
            let dbgdscr = Dbgdscr(self.memory.read_word_32(address)?);
            if dbgdscr.halted() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Err(Error::Probe(DebugProbeError::Timeout))
    }

    fn core_halted(&mut self) -> Result<bool, Error> {
        let address = Dbgdscr::get_mmio_address(self.base_address);
        let dbgdscr = Dbgdscr(self.memory.read_word_32(address)?);

        Ok(dbgdscr.halted())
    }

    fn halt(&mut self, timeout: Duration) -> Result<CoreInformation, Error> {
        if !matches!(self.state.current_state, CoreStatus::Halted(_)) {
            let address = Dbgdrcr::get_mmio_address(self.base_address);
            let mut value = Dbgdrcr(0);
            value.set_hrq(true);

            self.memory.write_word_32(address, value.into())?;

            self.wait_for_core_halted(timeout)?;

            // Reset our cached values
            self.reset_register_cache();
        }
        // Update core status
        let _ = self.status()?;

        // try to read the program counter
        let pc_value = self.read_core_reg(register::PC.id)?;

        // get pc
        Ok(CoreInformation {
            pc: pc_value.try_into()?,
        })
    }

    fn run(&mut self) -> Result<(), Error> {
        if matches!(self.state.current_state, CoreStatus::Running) {
            return Ok(());
        }

        // set writeback values
        self.writeback_registers()?;

        let address = Dbgdrcr::get_mmio_address(self.base_address);
        let mut value = Dbgdrcr(0);
        value.set_rrq(true);

        self.memory.write_word_32(address, value.into())?;

        // Wait for ack
        let address = Dbgdscr::get_mmio_address(self.base_address);

        loop {
            let dbgdscr = Dbgdscr(self.memory.read_word_32(address)?);
            if dbgdscr.restarted() {
                break;
            }
        }

        // Recompute / verify current state
        self.state.current_state = CoreStatus::Running;
        let _ = self.status()?;

        Ok(())
    }

    fn reset(&mut self) -> Result<(), Error> {
        self.sequence.reset_system(
            &mut self.memory,
            crate::CoreType::Armv7a,
            Some(self.base_address),
        )?;

        // Reset our cached values
        self.reset_register_cache();

        Ok(())
    }

    fn reset_and_halt(&mut self, timeout: Duration) -> Result<CoreInformation, Error> {
        self.sequence.reset_catch_set(
            &mut self.memory,
            crate::CoreType::Armv7a,
            Some(self.base_address),
        )?;
        self.sequence.reset_system(
            &mut self.memory,
            crate::CoreType::Armv7a,
            Some(self.base_address),
        )?;

        // Request halt
        let address = Dbgdrcr::get_mmio_address(self.base_address);
        let mut value = Dbgdrcr(0);
        value.set_hrq(true);

        self.memory.write_word_32(address, value.into())?;

        // Release from reset
        self.sequence.reset_catch_clear(
            &mut self.memory,
            crate::CoreType::Armv7a,
            Some(self.base_address),
        )?;

        self.wait_for_core_halted(timeout)?;

        // Update core status
        let _ = self.status()?;

        // Reset our cached values
        self.reset_register_cache();

        // try to read the program counter
        let pc_value = self.read_core_reg(register::PC.id)?;

        // get pc
        Ok(CoreInformation {
            pc: pc_value.try_into()?,
        })
    }

    fn step(&mut self) -> Result<CoreInformation, Error> {
        // Save current breakpoint
        let bp_unit_index = (self.available_breakpoint_units()? - 1) as usize;
        let bp_value_addr =
            Dbgbvr::get_mmio_address(self.base_address) + (bp_unit_index * size_of::<u32>()) as u64;
        let saved_bp_value = self.memory.read_word_32(bp_value_addr)?;

        let bp_control_addr =
            Dbgbcr::get_mmio_address(self.base_address) + (bp_unit_index * size_of::<u32>()) as u64;
        let saved_bp_control = self.memory.read_word_32(bp_control_addr)?;

        // Set breakpoint for any change
        let current_pc: u32 = self.read_core_reg(register::PC.id)?.try_into()?;
        let mut bp_control = Dbgbcr(0);

        // Breakpoint type - address mismatch
        bp_control.set_bt(0b0100);
        // Match on all modes
        bp_control.set_hmc(true);
        bp_control.set_pmc(0b11);
        // Match on all bytes
        bp_control.set_bas(0b1111);
        // Enable
        bp_control.set_e(true);

        self.memory.write_word_32(bp_value_addr, current_pc)?;
        self.memory
            .write_word_32(bp_control_addr, bp_control.into())?;

        // Resume
        self.run()?;

        // Wait for halt
        self.wait_for_core_halted(Duration::from_millis(100))?;

        // Reset breakpoint
        self.memory.write_word_32(bp_value_addr, saved_bp_value)?;
        self.memory
            .write_word_32(bp_control_addr, saved_bp_control)?;

        // try to read the program counter
        let pc_value = self.read_core_reg(register::PC.id)?;

        // get pc
        Ok(CoreInformation {
            pc: pc_value.try_into()?,
        })
    }

    fn read_core_reg(&mut self, address: RegisterId) -> Result<RegisterValue, Error> {
        let reg_num = address.0;

        // check cache
        if (reg_num as usize) < self.state.register_cache.len() {
            if let Some(cached_result) = self.state.register_cache[reg_num as usize] {
                return Ok(cached_result.0);
            }
        }

        // Generate instruction to extract register
        let result = match reg_num {
            0..=14 => {
                // r0-r14, valid
                // MCR p14, 0, <Rd>, c0, c5, 0 ; Write DBGDTRTXint Register
                let instruction = build_mcr(14, 0, reg_num, 0, 5, 0);

                self.execute_instruction_with_result(instruction)
            }
            15 => {
                // PC, must access via r0
                self.prepare_r0_for_clobber()?;

                // MOV r0, PC
                let instruction = build_mov(0, 15);
                self.execute_instruction(instruction)?;

                // Read from r0
                let instruction = build_mcr(14, 0, 0, 0, 5, 0);
                let pra_plus_offset = self.execute_instruction_with_result(instruction)?;

                // PC returned is PC + 8
                Ok(pra_plus_offset - 8)
            }
            16 => {
                // CPSR, must access via r0
                self.prepare_r0_for_clobber()?;

                // MRS r0, CPSR
                let instruction = build_mrs(0);
                self.execute_instruction(instruction)?;

                // Read from r0
                let instruction = build_mcr(14, 0, 0, 0, 5, 0);
                let cpsr = self.execute_instruction_with_result(instruction)?;

                Ok(cpsr)
            }
            _ => Err(Error::architecture_specific(
                Armv7aError::InvalidRegisterNumber(reg_num),
            )),
        };

        if let Ok(value) = result {
            self.state.register_cache[reg_num as usize] = Some((value.into(), false));

            Ok(value.into())
        } else {
            Err(result.err().unwrap())
        }
    }

    fn write_core_reg(&mut self, address: RegisterId, value: RegisterValue) -> Result<()> {
        let value: u32 = value.try_into()?;
        let reg_num = address.0;

        if (reg_num as usize) >= self.state.register_cache.len() {
            return Err(
                Error::architecture_specific(Armv7aError::InvalidRegisterNumber(reg_num)).into(),
            );
        }
        self.state.register_cache[reg_num as usize] = Some((value.into(), true));

        Ok(())
    }

    fn available_breakpoint_units(&mut self) -> Result<u32, Error> {
        if self.num_breakpoints.is_none() {
            let address = Dbgdidr::get_mmio_address(self.base_address);
            let dbgdidr = Dbgdidr(self.memory.read_word_32(address)?);

            self.num_breakpoints = Some(dbgdidr.brps() + 1);
        }
        Ok(self.num_breakpoints.unwrap())
    }

    fn enable_breakpoints(&mut self, _state: bool) -> Result<(), Error> {
        // Breakpoints are always on with v7-A
        Ok(())
    }

    fn set_hw_breakpoint(&mut self, bp_unit_index: usize, addr: u64) -> Result<(), Error> {
        let addr = valid_32_address(addr)?;

        let bp_value_addr =
            Dbgbvr::get_mmio_address(self.base_address) + (bp_unit_index * size_of::<u32>()) as u64;
        let bp_control_addr =
            Dbgbcr::get_mmio_address(self.base_address) + (bp_unit_index * size_of::<u32>()) as u64;
        let mut bp_control = Dbgbcr(0);

        // Breakpoint type - address match
        bp_control.set_bt(0b0000);
        // Match on all modes
        bp_control.set_hmc(true);
        bp_control.set_pmc(0b11);
        // Match on all bytes
        bp_control.set_bas(0b1111);
        // Enable
        bp_control.set_e(true);

        self.memory.write_word_32(bp_value_addr, addr)?;
        self.memory
            .write_word_32(bp_control_addr, bp_control.into())?;

        Ok(())
    }

    fn registers(&self) -> &'static RegisterFile {
        &ARM_REGISTER_FILE
    }

    fn clear_hw_breakpoint(&mut self, bp_unit_index: usize) -> Result<(), Error> {
        let bp_value_addr =
            Dbgbvr::get_mmio_address(self.base_address) + (bp_unit_index * size_of::<u32>()) as u64;
        let bp_control_addr =
            Dbgbcr::get_mmio_address(self.base_address) + (bp_unit_index * size_of::<u32>()) as u64;

        self.memory.write_word_32(bp_value_addr, 0)?;
        self.memory.write_word_32(bp_control_addr, 0)?;

        Ok(())
    }

    fn hw_breakpoints_enabled(&self) -> bool {
        true
    }

    fn architecture(&self) -> Architecture {
        Architecture::Arm
    }

    fn core_type(&self) -> CoreType {
        CoreType::Armv7a
    }

    fn instruction_set(&mut self) -> Result<InstructionSet, Error> {
        let cpsr: u32 = self.read_core_reg(RegisterId(16))?.try_into()?;

        // CPSR bit 5 - T - Thumb mode
        match (cpsr >> 5) & 1 {
            1 => Ok(InstructionSet::Thumb2),
            _ => Ok(InstructionSet::A32),
        }
    }

    fn status(&mut self) -> Result<crate::core::CoreStatus, Error> {
        // determine current state
        let address = Dbgdscr::get_mmio_address(self.base_address);
        let dbgdscr = Dbgdscr(self.memory.read_word_32(address)?);

        if dbgdscr.halted() {
            let reason = dbgdscr.halt_reason();

            self.state.current_state = CoreStatus::Halted(reason);

            return Ok(CoreStatus::Halted(reason));
        }
        // Core is neither halted nor sleeping, so we assume it is running.
        if self.state.current_state.is_halted() {
            log::warn!("Core is running, but we expected it to be halted");
        }

        self.state.current_state = CoreStatus::Running;

        Ok(CoreStatus::Running)
    }

    /// See docs on the [`CoreInterface::hw_breakpoints`] trait
    fn hw_breakpoints(&mut self) -> Result<Vec<Option<u64>>, Error> {
        let mut breakpoints = vec![];
        let num_hw_breakpoints = self.available_breakpoint_units()? as usize;

        for bp_unit_index in 0..num_hw_breakpoints {
            let bp_value_addr = Dbgbvr::get_mmio_address(self.base_address)
                + (bp_unit_index * size_of::<u32>()) as u64;
            let bp_value = self.memory.read_word_32(bp_value_addr)?;

            let bp_control_addr = Dbgbcr::get_mmio_address(self.base_address)
                + (bp_unit_index * size_of::<u32>()) as u64;
            let bp_control = Dbgbcr(self.memory.read_word_32(bp_control_addr)?);

            if bp_control.e() {
                breakpoints.push(Some(bp_value as u64));
            } else {
                breakpoints.push(None);
            }
        }
        Ok(breakpoints)
    }

    fn fpu_support(&mut self) -> Result<bool, crate::error::Error> {
        Err(crate::error::Error::Other(anyhow::anyhow!(
            "Fpu detection not yet implemented"
        )))
    }

    fn on_session_stop(&mut self) -> Result<(), Error> {
        if matches!(self.state.current_state, CoreStatus::Halted(_)) {
            // We may have clobbered registers we wrote during debugging
            // Best effort attempt to put them back before we exit
            self.writeback_registers()
        } else {
            Ok(())
        }
    }
}

impl<'probe> MemoryInterface for Armv7a<'probe> {
    fn supports_native_64bit_access(&mut self) -> bool {
        false
    }
    fn read_word_64(&mut self, address: u64) -> Result<u64, crate::error::Error> {
        let mut ret: u64 = self.read_word_32(address)? as u64;
        ret |= (self.read_word_32(address + 4)? as u64) << 32;

        Ok(ret)
    }
    fn read_word_32(&mut self, address: u64) -> Result<u32, Error> {
        let address = valid_32_address(address)?;

        // LDC p14, c5, [r0], #4
        let instr = build_ldc(14, 5, 0, 4);

        // Save r0
        self.prepare_r0_for_clobber()?;

        // Load r0 with the address to read from
        self.set_r0(address)?;

        // Read memory from [r0]
        self.execute_instruction_with_result(instr)
    }
    fn read_word_8(&mut self, address: u64) -> Result<u8, Error> {
        // Find the word this is in and its byte offset
        let byte_offset = address % 4;
        let word_start = address - byte_offset;

        // Read the word
        let data = self.read_word_32(word_start)?;

        // Return the byte
        Ok(data.to_le_bytes()[byte_offset as usize])
    }
    fn read_64(&mut self, address: u64, data: &mut [u64]) -> Result<(), crate::error::Error> {
        for (i, word) in data.iter_mut().enumerate() {
            *word = self.read_word_64(address + ((i as u64) * 8))?;
        }

        Ok(())
    }
    fn read_32(&mut self, address: u64, data: &mut [u32]) -> Result<(), Error> {
        for (i, word) in data.iter_mut().enumerate() {
            *word = self.read_word_32(address + ((i as u64) * 4))?;
        }

        Ok(())
    }
    fn read_8(&mut self, address: u64, data: &mut [u8]) -> Result<(), Error> {
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = self.read_word_8(address + (i as u64))?;
        }

        Ok(())
    }
    fn write_word_64(&mut self, address: u64, data: u64) -> Result<(), crate::error::Error> {
        let data_low = data as u32;
        let data_high = (data >> 32) as u32;

        self.write_word_32(address, data_low)?;
        self.write_word_32(address + 4, data_high)
    }
    fn write_word_32(&mut self, address: u64, data: u32) -> Result<(), Error> {
        let address = valid_32_address(address)?;

        // STC p14, c5, [r0], #4
        let instr = build_stc(14, 5, 0, 4);

        // Save r0
        self.prepare_r0_for_clobber()?;

        // Load r0 with the address to write to
        self.set_r0(address)?;

        // Write to [r0]
        self.execute_instruction_with_input(instr, data)
    }
    fn write_word_8(&mut self, address: u64, data: u8) -> Result<(), Error> {
        // Find the word this is in and its byte offset
        let byte_offset = address % 4;
        let word_start = address - byte_offset;

        // Get the current word value
        let current_word = self.read_word_32(word_start)?;
        let mut word_bytes = current_word.to_le_bytes();
        word_bytes[byte_offset as usize] = data;

        self.write_word_32(word_start, u32::from_le_bytes(word_bytes))
    }
    fn write_64(&mut self, address: u64, data: &[u64]) -> Result<(), crate::error::Error> {
        for (i, word) in data.iter().enumerate() {
            self.write_word_64(address + ((i as u64) * 8), *word)?;
        }

        Ok(())
    }
    fn write_32(&mut self, address: u64, data: &[u32]) -> Result<(), Error> {
        for (i, word) in data.iter().enumerate() {
            self.write_word_32(address + ((i as u64) * 4), *word)?;
        }

        Ok(())
    }
    fn write_8(&mut self, address: u64, data: &[u8]) -> Result<(), Error> {
        for (i, byte) in data.iter().enumerate() {
            self.write_word_8(address + ((i as u64) * 4), *byte)?;
        }

        Ok(())
    }
    fn flush(&mut self) -> Result<(), Error> {
        // Nothing to do - this runs through the CPU which automatically handles any caching
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use crate::architecture::arm::{
        ap::MemoryAp, communication_interface::SwdSequence,
        memory::adi_v5_memory_interface::ArmProbe, sequences::DefaultArmSequence, ApAddress,
        DpAddress,
    };

    use super::*;

    const TEST_BASE_ADDRESS: u64 = 0x8000_1000;

    fn address_to_reg_num(address: u64) -> u32 {
        ((address - TEST_BASE_ADDRESS) / 4) as u32
    }

    pub struct ExpectedMemoryOp {
        read: bool,
        address: u64,
        value: u32,
    }

    pub struct MockProbe {
        expected_ops: Vec<ExpectedMemoryOp>,
    }

    impl MockProbe {
        pub fn new() -> Self {
            MockProbe {
                expected_ops: vec![],
            }
        }

        pub fn expected_read(&mut self, addr: u64, value: u32) {
            self.expected_ops.push(ExpectedMemoryOp {
                read: true,
                address: addr,
                value: value,
            });
        }

        pub fn expected_write(&mut self, addr: u64, value: u32) {
            self.expected_ops.push(ExpectedMemoryOp {
                read: false,
                address: addr,
                value: value,
            });
        }
    }

    impl ArmProbe for MockProbe {
        fn read_8(&mut self, _ap: MemoryAp, _address: u64, _data: &mut [u8]) -> Result<(), Error> {
            todo!()
        }

        fn read_32(&mut self, _ap: MemoryAp, address: u64, data: &mut [u32]) -> Result<(), Error> {
            if self.expected_ops.len() == 0 {
                panic!(
                    "Received unexpected read_32 op: register {:#}",
                    address_to_reg_num(address)
                );
            }

            assert_eq!(data.len(), 1);

            let expected_op = self.expected_ops.remove(0);

            assert_eq!(
                expected_op.read,
                true,
                "R/W mismatch for register: Expected {:#} Actual: {:#}",
                address_to_reg_num(expected_op.address),
                address_to_reg_num(address)
            );
            assert_eq!(
                expected_op.address,
                address,
                "Read from unexpected register: Expected {:#} Actual: {:#}",
                address_to_reg_num(expected_op.address),
                address_to_reg_num(address)
            );

            data[0] = expected_op.value;

            Ok(())
        }

        fn write_8(&mut self, _ap: MemoryAp, _address: u64, _data: &[u8]) -> Result<(), Error> {
            todo!()
        }

        fn write_32(&mut self, _ap: MemoryAp, address: u64, data: &[u32]) -> Result<(), Error> {
            if self.expected_ops.len() == 0 {
                panic!(
                    "Received unexpected write_32 op: register {:#}",
                    address_to_reg_num(address)
                );
            }

            assert_eq!(data.len(), 1);

            let expected_op = self.expected_ops.remove(0);

            assert_eq!(expected_op.read, false);
            assert_eq!(
                expected_op.address,
                address,
                "Write to unexpected register: Expected {:#} Actual: {:#}",
                address_to_reg_num(expected_op.address),
                address_to_reg_num(address)
            );

            assert_eq!(
                expected_op.value, data[0],
                "Write value mismatch Expected {:#X} Actual: {:#X}",
                expected_op.value, data[0]
            );

            Ok(())
        }

        fn flush(&mut self) -> Result<(), Error> {
            todo!()
        }

        fn get_arm_communication_interface(
            &mut self,
        ) -> Result<
            &mut crate::architecture::arm::ArmCommunicationInterface<
                crate::architecture::arm::communication_interface::Initialized,
            >,
            Error,
        > {
            todo!()
        }

        fn read_64(
            &mut self,
            _ap: MemoryAp,
            _address: u64,
            _data: &mut [u64],
        ) -> Result<(), Error> {
            todo!()
        }

        fn write_64(&mut self, _ap: MemoryAp, _address: u64, _data: &[u64]) -> Result<(), Error> {
            todo!()
        }

        fn supports_native_64bit_access(&mut self) -> bool {
            false
        }
    }

    impl SwdSequence for MockProbe {
        fn swj_sequence(&mut self, _bit_len: u8, _bits: u64) -> Result<(), Error> {
            todo!()
        }

        fn swj_pins(
            &mut self,
            _pin_out: u32,
            _pin_select: u32,
            _pin_wait: u32,
        ) -> Result<u32, Error> {
            todo!()
        }
    }

    fn add_status_expectations(probe: &mut MockProbe, halted: bool) {
        let mut dbgdscr = Dbgdscr(0);
        dbgdscr.set_halted(halted);
        dbgdscr.set_restarted(true);
        probe.expected_read(Dbgdscr::get_mmio_address(TEST_BASE_ADDRESS), dbgdscr.into());
    }

    fn add_enable_itr_expectations(probe: &mut MockProbe) {
        let mut dbgdscr = Dbgdscr(0);
        dbgdscr.set_halted(true);
        probe.expected_read(Dbgdscr::get_mmio_address(TEST_BASE_ADDRESS), dbgdscr.into());
        dbgdscr.set_itren(true);
        probe.expected_write(Dbgdscr::get_mmio_address(TEST_BASE_ADDRESS), dbgdscr.into());
    }

    fn add_read_reg_expectations(probe: &mut MockProbe, reg: u16, value: u32) {
        probe.expected_write(
            Dbgitr::get_mmio_address(TEST_BASE_ADDRESS),
            build_mcr(14, 0, reg, 0, 5, 0),
        );
        let mut dbgdscr = Dbgdscr(0);
        dbgdscr.set_instrcoml_l(true);
        dbgdscr.set_txfull_l(true);

        probe.expected_read(Dbgdscr::get_mmio_address(TEST_BASE_ADDRESS), dbgdscr.into());
        probe.expected_read(Dbgdtrtx::get_mmio_address(TEST_BASE_ADDRESS), value);
    }

    fn add_read_pc_expectations(probe: &mut MockProbe, value: u32) {
        let mut dbgdscr = Dbgdscr(0);
        dbgdscr.set_instrcoml_l(true);
        dbgdscr.set_txfull_l(true);

        probe.expected_write(
            Dbgitr::get_mmio_address(TEST_BASE_ADDRESS),
            build_mov(0, 15),
        );
        probe.expected_read(Dbgdscr::get_mmio_address(TEST_BASE_ADDRESS), dbgdscr.into());
        // + 8 to add expected offset on halt
        add_read_reg_expectations(probe, 0, value + 8);
    }

    fn add_read_cpsr_expectations(probe: &mut MockProbe, value: u32) {
        let mut dbgdscr = Dbgdscr(0);
        dbgdscr.set_instrcoml_l(true);
        dbgdscr.set_txfull_l(true);

        probe.expected_write(Dbgitr::get_mmio_address(TEST_BASE_ADDRESS), build_mrs(0));
        probe.expected_read(Dbgdscr::get_mmio_address(TEST_BASE_ADDRESS), dbgdscr.into());
        add_read_reg_expectations(probe, 0, value);
    }

    fn add_idr_expectations(probe: &mut MockProbe, bp_count: u32) {
        let mut dbgdidr = Dbgdidr(0);
        dbgdidr.set_brps(bp_count - 1);
        probe.expected_read(Dbgdidr::get_mmio_address(TEST_BASE_ADDRESS), dbgdidr.into());
    }

    fn add_set_r0_expectation(probe: &mut MockProbe, value: u32) {
        let mut dbgdscr = Dbgdscr(0);
        dbgdscr.set_instrcoml_l(true);
        dbgdscr.set_rxfull_l(true);

        probe.expected_write(Dbgdtrrx::get_mmio_address(TEST_BASE_ADDRESS), value);
        probe.expected_read(Dbgdscr::get_mmio_address(TEST_BASE_ADDRESS), dbgdscr.into());

        probe.expected_write(
            Dbgitr::get_mmio_address(TEST_BASE_ADDRESS),
            build_mrc(14, 0, 0, 0, 5, 0),
        );
        probe.expected_read(Dbgdscr::get_mmio_address(TEST_BASE_ADDRESS), dbgdscr.into());
    }

    fn add_read_memory_expectations(probe: &mut MockProbe, address: u64, value: u32) {
        add_set_r0_expectation(probe, address as u32);

        let mut dbgdscr = Dbgdscr(0);
        dbgdscr.set_instrcoml_l(true);
        dbgdscr.set_txfull_l(true);

        probe.expected_write(
            Dbgitr::get_mmio_address(TEST_BASE_ADDRESS),
            build_ldc(14, 5, 0, 4),
        );
        probe.expected_read(Dbgdscr::get_mmio_address(TEST_BASE_ADDRESS), dbgdscr.into());
        probe.expected_read(Dbgdtrtx::get_mmio_address(TEST_BASE_ADDRESS), value);
    }

    #[test]
    fn armv7a_new() {
        let mut probe = MockProbe::new();

        // Add expectations
        add_status_expectations(&mut probe, true);

        let mock_mem = Memory::new(
            probe,
            MemoryAp::new(ApAddress {
                ap: 0,
                dp: DpAddress::Default,
            }),
        );

        let _ = Armv7a::new(
            mock_mem,
            &mut CortexAState::new(),
            TEST_BASE_ADDRESS,
            DefaultArmSequence::create(),
        )
        .unwrap();
    }

    #[test]
    fn armv7a_core_halted() {
        let mut probe = MockProbe::new();
        let mut state = CortexAState::new();

        // Add expectations
        add_status_expectations(&mut probe, true);

        let mut dbgdscr = Dbgdscr(0);
        dbgdscr.set_halted(false);
        probe.expected_read(Dbgdscr::get_mmio_address(TEST_BASE_ADDRESS), dbgdscr.into());

        dbgdscr.set_halted(true);
        probe.expected_read(Dbgdscr::get_mmio_address(TEST_BASE_ADDRESS), dbgdscr.into());

        let mock_mem = Memory::new(
            probe,
            MemoryAp::new(ApAddress {
                ap: 0,
                dp: DpAddress::Default,
            }),
        );

        let mut armv7a = Armv7a::new(
            mock_mem,
            &mut state,
            TEST_BASE_ADDRESS,
            DefaultArmSequence::create(),
        )
        .unwrap();

        // First read false, second read true
        assert_eq!(false, armv7a.core_halted().unwrap());
        assert_eq!(true, armv7a.core_halted().unwrap());
    }

    #[test]
    fn armv7a_wait_for_core_halted() {
        let mut probe = MockProbe::new();
        let mut state = CortexAState::new();

        // Add expectations
        add_status_expectations(&mut probe, true);

        let mut dbgdscr = Dbgdscr(0);
        dbgdscr.set_halted(false);
        probe.expected_read(Dbgdscr::get_mmio_address(TEST_BASE_ADDRESS), dbgdscr.into());

        dbgdscr.set_halted(true);
        probe.expected_read(Dbgdscr::get_mmio_address(TEST_BASE_ADDRESS), dbgdscr.into());

        let mock_mem = Memory::new(
            probe,
            MemoryAp::new(ApAddress {
                ap: 0,
                dp: DpAddress::Default,
            }),
        );

        let mut armv7a = Armv7a::new(
            mock_mem,
            &mut state,
            TEST_BASE_ADDRESS,
            DefaultArmSequence::create(),
        )
        .unwrap();

        // Should halt on second read
        armv7a
            .wait_for_core_halted(Duration::from_millis(100))
            .unwrap();
    }

    #[test]
    fn armv7a_status_running() {
        let mut probe = MockProbe::new();
        let mut state = CortexAState::new();

        // Add expectations
        add_status_expectations(&mut probe, true);

        let mut dbgdscr = Dbgdscr(0);
        dbgdscr.set_halted(false);
        probe.expected_read(Dbgdscr::get_mmio_address(TEST_BASE_ADDRESS), dbgdscr.into());

        let mock_mem = Memory::new(
            probe,
            MemoryAp::new(ApAddress {
                ap: 0,
                dp: DpAddress::Default,
            }),
        );

        let mut armv7a = Armv7a::new(
            mock_mem,
            &mut state,
            TEST_BASE_ADDRESS,
            DefaultArmSequence::create(),
        )
        .unwrap();

        // Should halt on second read
        assert_eq!(CoreStatus::Running, armv7a.status().unwrap());
    }

    #[test]
    fn armv7a_status_halted() {
        let mut probe = MockProbe::new();
        let mut state = CortexAState::new();

        // Add expectations
        add_status_expectations(&mut probe, true);

        let mut dbgdscr = Dbgdscr(0);
        dbgdscr.set_halted(true);
        probe.expected_read(Dbgdscr::get_mmio_address(TEST_BASE_ADDRESS), dbgdscr.into());

        let mock_mem = Memory::new(
            probe,
            MemoryAp::new(ApAddress {
                ap: 0,
                dp: DpAddress::Default,
            }),
        );

        let mut armv7a = Armv7a::new(
            mock_mem,
            &mut state,
            TEST_BASE_ADDRESS,
            DefaultArmSequence::create(),
        )
        .unwrap();

        // Should halt on second read
        assert_eq!(
            CoreStatus::Halted(crate::HaltReason::Request),
            armv7a.status().unwrap()
        );
    }

    #[test]
    fn armv7a_read_core_reg_common() {
        const REG_VALUE: u32 = 0xABCD;

        let mut probe = MockProbe::new();
        let mut state = CortexAState::new();

        // Add expectations
        add_status_expectations(&mut probe, true);

        // Read status, update ITR
        add_enable_itr_expectations(&mut probe);

        // Read register
        add_read_reg_expectations(&mut probe, 2, REG_VALUE);

        let mock_mem = Memory::new(
            probe,
            MemoryAp::new(ApAddress {
                ap: 0,
                dp: DpAddress::Default,
            }),
        );

        let mut armv7a = Armv7a::new(
            mock_mem,
            &mut state,
            TEST_BASE_ADDRESS,
            DefaultArmSequence::create(),
        )
        .unwrap();

        // First read will hit expectations
        assert_eq!(
            RegisterValue::from(REG_VALUE),
            armv7a.read_core_reg(RegisterId(2)).unwrap()
        );

        // Second read will cache, no new expectations
        assert_eq!(
            RegisterValue::from(REG_VALUE),
            armv7a.read_core_reg(RegisterId(2)).unwrap()
        );
    }

    #[test]
    fn armv7a_read_core_reg_pc() {
        const REG_VALUE: u32 = 0xABCD;

        let mut probe = MockProbe::new();
        let mut state = CortexAState::new();

        // Add expectations
        add_status_expectations(&mut probe, true);

        // Read status, update ITR
        add_enable_itr_expectations(&mut probe);

        // Read PC
        add_read_reg_expectations(&mut probe, 0, 0);
        add_read_pc_expectations(&mut probe, REG_VALUE);

        let mock_mem = Memory::new(
            probe,
            MemoryAp::new(ApAddress {
                ap: 0,
                dp: DpAddress::Default,
            }),
        );

        let mut armv7a = Armv7a::new(
            mock_mem,
            &mut state,
            TEST_BASE_ADDRESS,
            DefaultArmSequence::create(),
        )
        .unwrap();

        // First read will hit expectations
        assert_eq!(
            RegisterValue::from(REG_VALUE),
            armv7a.read_core_reg(RegisterId(15)).unwrap()
        );

        // Second read will cache, no new expectations
        assert_eq!(
            RegisterValue::from(REG_VALUE),
            armv7a.read_core_reg(RegisterId(15)).unwrap()
        );
    }

    #[test]
    fn armv7a_read_core_reg_cpsr() {
        const REG_VALUE: u32 = 0xABCD;

        let mut probe = MockProbe::new();
        let mut state = CortexAState::new();

        // Add expectations
        add_status_expectations(&mut probe, true);

        // Read status, update ITR
        add_enable_itr_expectations(&mut probe);

        // Read CPSR
        add_read_reg_expectations(&mut probe, 0, 0);
        add_read_cpsr_expectations(&mut probe, REG_VALUE);

        let mock_mem = Memory::new(
            probe,
            MemoryAp::new(ApAddress {
                ap: 0,
                dp: DpAddress::Default,
            }),
        );

        let mut armv7a = Armv7a::new(
            mock_mem,
            &mut state,
            TEST_BASE_ADDRESS,
            DefaultArmSequence::create(),
        )
        .unwrap();

        // First read will hit expectations
        assert_eq!(
            RegisterValue::from(REG_VALUE),
            armv7a.read_core_reg(RegisterId(16)).unwrap()
        );

        // Second read will cache, no new expectations
        assert_eq!(
            RegisterValue::from(REG_VALUE),
            armv7a.read_core_reg(RegisterId(16)).unwrap()
        );
    }

    #[test]
    fn armv7a_halt() {
        const REG_VALUE: u32 = 0xABCD;

        let mut probe = MockProbe::new();
        let mut state = CortexAState::new();

        // Add expectations
        add_status_expectations(&mut probe, false);

        // Write halt request
        let mut dbgdrcr = Dbgdrcr(0);
        dbgdrcr.set_hrq(true);
        probe.expected_write(Dbgdrcr::get_mmio_address(TEST_BASE_ADDRESS), dbgdrcr.into());

        // Wait for halted
        add_status_expectations(&mut probe, true);

        // Read status
        add_status_expectations(&mut probe, true);

        // Read status, update ITR
        add_enable_itr_expectations(&mut probe);

        // Read PC
        add_read_reg_expectations(&mut probe, 0, 0);
        add_read_pc_expectations(&mut probe, REG_VALUE);

        let mock_mem = Memory::new(
            probe,
            MemoryAp::new(ApAddress {
                ap: 0,
                dp: DpAddress::Default,
            }),
        );

        let mut armv7a = Armv7a::new(
            mock_mem,
            &mut state,
            TEST_BASE_ADDRESS,
            DefaultArmSequence::create(),
        )
        .unwrap();

        // Verify PC
        assert_eq!(
            REG_VALUE as u64,
            armv7a.halt(Duration::from_millis(100)).unwrap().pc
        );
    }

    #[test]
    fn armv7a_run() {
        let mut probe = MockProbe::new();
        let mut state = CortexAState::new();

        // Add expectations
        add_status_expectations(&mut probe, true);

        // Write resume request
        let mut dbgdrcr = Dbgdrcr(0);
        dbgdrcr.set_rrq(true);
        probe.expected_write(Dbgdrcr::get_mmio_address(TEST_BASE_ADDRESS), dbgdrcr.into());

        // Wait for running
        add_status_expectations(&mut probe, false);

        // Read status
        add_status_expectations(&mut probe, false);

        let mock_mem = Memory::new(
            probe,
            MemoryAp::new(ApAddress {
                ap: 0,
                dp: DpAddress::Default,
            }),
        );

        let mut armv7a = Armv7a::new(
            mock_mem,
            &mut state,
            TEST_BASE_ADDRESS,
            DefaultArmSequence::create(),
        )
        .unwrap();

        armv7a.run().unwrap();
    }

    #[test]
    fn armv7a_available_breakpoint_units() {
        const BP_COUNT: u32 = 4;
        let mut probe = MockProbe::new();
        let mut state = CortexAState::new();

        // Add expectations
        add_status_expectations(&mut probe, true);

        // Read breakpoint count
        add_idr_expectations(&mut probe, BP_COUNT);

        let mock_mem = Memory::new(
            probe,
            MemoryAp::new(ApAddress {
                ap: 0,
                dp: DpAddress::Default,
            }),
        );

        let mut armv7a = Armv7a::new(
            mock_mem,
            &mut state,
            TEST_BASE_ADDRESS,
            DefaultArmSequence::create(),
        )
        .unwrap();

        assert_eq!(BP_COUNT, armv7a.available_breakpoint_units().unwrap());
    }

    #[test]
    fn armv7a_hw_breakpoints() {
        const BP_COUNT: u32 = 4;
        const BP1: u64 = 0x2345;
        const BP2: u64 = 0x8000_0000;
        let mut probe = MockProbe::new();
        let mut state = CortexAState::new();

        // Add expectations
        add_status_expectations(&mut probe, true);

        // Read breakpoint count
        add_idr_expectations(&mut probe, BP_COUNT);

        // Read BP values and controls
        probe.expected_read(Dbgbvr::get_mmio_address(TEST_BASE_ADDRESS), BP1 as u32);
        probe.expected_read(Dbgbcr::get_mmio_address(TEST_BASE_ADDRESS), 1);

        probe.expected_read(
            Dbgbvr::get_mmio_address(TEST_BASE_ADDRESS) + (1 * 4),
            BP2 as u32,
        );
        probe.expected_read(Dbgbcr::get_mmio_address(TEST_BASE_ADDRESS) + (1 * 4), 1);

        probe.expected_read(Dbgbvr::get_mmio_address(TEST_BASE_ADDRESS) + (2 * 4), 0);
        probe.expected_read(Dbgbcr::get_mmio_address(TEST_BASE_ADDRESS) + (2 * 4), 0);

        probe.expected_read(Dbgbvr::get_mmio_address(TEST_BASE_ADDRESS) + (3 * 4), 0);
        probe.expected_read(Dbgbcr::get_mmio_address(TEST_BASE_ADDRESS) + (3 * 4), 0);

        let mock_mem = Memory::new(
            probe,
            MemoryAp::new(ApAddress {
                ap: 0,
                dp: DpAddress::Default,
            }),
        );

        let mut armv7a = Armv7a::new(
            mock_mem,
            &mut state,
            TEST_BASE_ADDRESS,
            DefaultArmSequence::create(),
        )
        .unwrap();

        let results = armv7a.hw_breakpoints().unwrap();
        assert_eq!(Some(BP1), results[0]);
        assert_eq!(Some(BP2), results[1]);
        assert_eq!(None, results[2]);
        assert_eq!(None, results[3]);
    }

    #[test]
    fn armv7a_set_hw_breakpoint() {
        const BP_VALUE: u64 = 0x2345;
        let mut probe = MockProbe::new();
        let mut state = CortexAState::new();

        // Add expectations
        add_status_expectations(&mut probe, true);

        // Update BP value and control
        let mut dbgbcr = Dbgbcr(0);
        // Match on all modes
        dbgbcr.set_hmc(true);
        dbgbcr.set_pmc(0b11);
        // Match on all bytes
        dbgbcr.set_bas(0b1111);
        // Enable
        dbgbcr.set_e(true);

        probe.expected_write(Dbgbvr::get_mmio_address(TEST_BASE_ADDRESS), BP_VALUE as u32);
        probe.expected_write(Dbgbcr::get_mmio_address(TEST_BASE_ADDRESS), dbgbcr.into());

        let mock_mem = Memory::new(
            probe,
            MemoryAp::new(ApAddress {
                ap: 0,
                dp: DpAddress::Default,
            }),
        );

        let mut armv7a = Armv7a::new(
            mock_mem,
            &mut state,
            TEST_BASE_ADDRESS,
            DefaultArmSequence::create(),
        )
        .unwrap();

        armv7a.set_hw_breakpoint(0, BP_VALUE).unwrap();
    }

    #[test]
    fn armv7a_clear_hw_breakpoint() {
        let mut probe = MockProbe::new();
        let mut state = CortexAState::new();

        // Add expectations
        add_status_expectations(&mut probe, true);

        // Update BP value and control
        probe.expected_write(Dbgbvr::get_mmio_address(TEST_BASE_ADDRESS), 0);
        probe.expected_write(Dbgbcr::get_mmio_address(TEST_BASE_ADDRESS), 0);

        let mock_mem = Memory::new(
            probe,
            MemoryAp::new(ApAddress {
                ap: 0,
                dp: DpAddress::Default,
            }),
        );

        let mut armv7a = Armv7a::new(
            mock_mem,
            &mut state,
            TEST_BASE_ADDRESS,
            DefaultArmSequence::create(),
        )
        .unwrap();

        armv7a.clear_hw_breakpoint(0).unwrap();
    }

    #[test]
    fn armv7a_read_word_32() {
        const MEMORY_VALUE: u32 = 0xBA5EBA11;
        const MEMORY_ADDRESS: u64 = 0x12345678;

        let mut probe = MockProbe::new();
        let mut state = CortexAState::new();

        // Add expectations
        add_status_expectations(&mut probe, true);
        add_enable_itr_expectations(&mut probe);

        // Read memory
        add_read_reg_expectations(&mut probe, 0, 0);
        add_read_memory_expectations(&mut probe, MEMORY_ADDRESS, MEMORY_VALUE);

        let mock_mem = Memory::new(
            probe,
            MemoryAp::new(ApAddress {
                ap: 0,
                dp: DpAddress::Default,
            }),
        );

        let mut armv7a = Armv7a::new(
            mock_mem,
            &mut state,
            TEST_BASE_ADDRESS,
            DefaultArmSequence::create(),
        )
        .unwrap();

        assert_eq!(MEMORY_VALUE, armv7a.read_word_32(MEMORY_ADDRESS).unwrap());
    }

    #[test]
    fn armv7a_read_word_8() {
        const MEMORY_VALUE: u32 = 0xBA5EBA11;
        const MEMORY_ADDRESS: u64 = 0x12345679;
        const MEMORY_WORD_ADDRESS: u64 = 0x12345678;

        let mut probe = MockProbe::new();
        let mut state = CortexAState::new();

        // Add expectations
        add_status_expectations(&mut probe, true);
        add_enable_itr_expectations(&mut probe);

        // Read memory
        add_read_reg_expectations(&mut probe, 0, 0);
        add_read_memory_expectations(&mut probe, MEMORY_WORD_ADDRESS, MEMORY_VALUE);

        let mock_mem = Memory::new(
            probe,
            MemoryAp::new(ApAddress {
                ap: 0,
                dp: DpAddress::Default,
            }),
        );

        let mut armv7a = Armv7a::new(
            mock_mem,
            &mut state,
            TEST_BASE_ADDRESS,
            DefaultArmSequence::create(),
        )
        .unwrap();

        assert_eq!(0xBA, armv7a.read_word_8(MEMORY_ADDRESS).unwrap());
    }
}
