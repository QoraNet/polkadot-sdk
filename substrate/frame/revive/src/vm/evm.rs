// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
use crate::{
	tracing,
	vm::{
		evm::instructions::{instruction_table, InstructionTable},
		BytecodeType, ExecResult, Ext,
	},
	AccountIdOf, CodeInfo, Config, ContractBlob, DispatchError, Error, H256, LOG_TARGET, U256,
};
use alloc::vec::Vec;
use core::{convert::Infallible, ops::ControlFlow};
use revm::{bytecode::Bytecode, primitives::Bytes};

#[cfg(feature = "runtime-benchmarks")]
pub mod instructions;
#[cfg(not(feature = "runtime-benchmarks"))]
mod instructions;

mod interpreter;
mod util;
pub use interpreter::{Halt, Interpreter};

mod memory;
pub use memory::Memory;

mod stack;
pub use stack::Stack;

mod ext_bytecode;
use ext_bytecode::ExtBytecode;

/// Hard-coded value returned by the EVM `DIFFICULTY` opcode.
///
/// After Ethereum's Merge (Sept 2022), the `DIFFICULTY` opcode was redefined to return
/// `prevrandao`, a randomness value from the beacon chain. In Substrate pallet-revive
/// a fixed constant is returned instead for compatibility with contracts that still read this
/// opcode. The value is aligned with the difficulty hardcoded for PVM contracts.
pub(crate) const DIFFICULTY: u64 = 2500000000000000_u64;

/// The base fee per gas used in the network as defined by EIP-1559.
///
/// For `pallet-revive`, this is hardcoded to 0
pub(crate) const BASE_FEE: U256 = U256::zero();

impl<T: Config> ContractBlob<T> {
	/// Create a new contract from EVM init code.
	pub fn from_evm_init_code(code: Vec<u8>, owner: AccountIdOf<T>) -> Result<Self, DispatchError> {
		if code.len() > revm::primitives::eip3860::MAX_INITCODE_SIZE {
			return Err(<Error<T>>::BlobTooLarge.into());
		}

		// EIP-3541: Reject new contract code starting with the 0xEF byte
		if code.first() == Some(&0xEF) {
			return Err(<Error<T>>::CodeRejected.into());
		}

		let code_len = code.len() as u32;
		let code_info = CodeInfo {
			owner,
			deposit: Default::default(),
			refcount: 0,
			code_len,
			code_type: BytecodeType::Evm,
			behaviour_version: Default::default(),
		};

		Bytecode::new_raw_checked(Bytes::from(code.to_vec())).map_err(|err| {
			log::debug!(target: LOG_TARGET, "failed to create evm bytecode from init code: {err:?}" );
			<Error<T>>::CodeRejected
		})?;

		// Code hash is not relevant for init code, since it is not stored on-chain.
		let code_hash = H256::default();
		Ok(ContractBlob { code, code_info, code_hash })
	}

	/// Create a new contract from EVM runtime code.
	pub fn from_evm_runtime_code(
		code: Vec<u8>,
		owner: AccountIdOf<T>,
	) -> Result<Self, DispatchError> {
		if code.len() > revm::primitives::eip170::MAX_CODE_SIZE {
			return Err(<Error<T>>::BlobTooLarge.into());
		}

		let code_len = code.len() as u32;
		let deposit = super::calculate_code_deposit::<T>(code_len);

		let code_info = CodeInfo {
			owner,
			deposit,
			refcount: 0,
			code_len,
			code_type: BytecodeType::Evm,
			behaviour_version: Default::default(),
		};

		Bytecode::new_raw_checked(Bytes::from(code.to_vec())).map_err(|err| {
			log::debug!(target: LOG_TARGET, "failed to create evm bytecode from code: {err:?}" );
			<Error<T>>::CodeRejected
		})?;

		let code_hash = H256(sp_io::hashing::keccak_256(&code));
		Ok(ContractBlob { code, code_info, code_hash })
	}
}

/// Calls the EVM interpreter with the provided bytecode and inputs.
pub fn call<'a, E: Ext>(bytecode: Bytecode, ext: &'a mut E, input: Vec<u8>) -> ExecResult {
	let mut interpreter = Interpreter::new(ExtBytecode::new(bytecode), input, ext);
	let table = instruction_table::<E>();

	let use_opcode_tracing =
		tracing::if_tracing(|tracer| tracer.is_opcode_tracing_enabled()).unwrap_or(false);

	let ControlFlow::Break(halt) = if use_opcode_tracing {
		run_plain_with_tracing(&mut interpreter, &table)
	} else {
		run_plain(&mut interpreter, &table)
	};

	interpreter.into_exec_result(halt)
}

/// Re-implementation of REVM run_plain function to add trace logging to our EVM interpreter loop.
/// NB: copied directly from revm tag v82
fn run_plain<'a, E: Ext>(
	interpreter: &mut Interpreter<E>,
	table: &InstructionTable<E>,
) -> ControlFlow<Halt, Infallible> {
	use revm::interpreter::interpreter_types::Jumps;
	loop {
		let opcode = interpreter.bytecode.opcode();
		table[opcode as usize](interpreter)?;
		interpreter.bytecode.relative_jump(1);
	}
}

/// Re-implementation of REVM run_plain function to add trace logging to our EVM interpreter loop.
/// NB: copied directly from revm tag v82
fn run_plain_with_tracing<'a, E: Ext>(
	interpreter: &mut Interpreter<'a, E>,
	table: &InstructionTable<E>,
) -> ControlFlow<Halt, Infallible> {
	use revm::interpreter::interpreter_types::Jumps;
	loop {
		let opcode = interpreter.bytecode.opcode();

		tracing::if_tracing(|tracer| {
			let gas_before = interpreter.ext.gas_meter().gas_left();
			tracer.enter_opcode(
				interpreter.bytecode.pc() as u64,
				opcode,
				gas_before,
				&interpreter.stack,
				&interpreter.memory,
				interpreter.ext.last_frame_output(),
			);
		});

		interpreter.bytecode.relative_jump(1);
		table[opcode as usize](interpreter)?;

		tracing::if_tracing(|tracer| {
			let gas_left = interpreter.ext.gas_meter().gas_left();
			tracer.exit_opcode(gas_left);
		});
	}
}
