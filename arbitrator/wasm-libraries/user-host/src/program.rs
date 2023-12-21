// Copyright 2022-2023, Offchain Labs, Inc.
// For license information, see https://github.com/OffchainLabs/nitro/blob/master/LICENSE
use core::sync::atomic::{compiler_fence, Ordering};
use arbutil::{
    evm::{js::JsEvmApi, js::RequestHandler, EvmData, api::EvmApiMethod},
    wavm, Bytes20, Bytes32, Color,
};
use eyre::{eyre, Result};
use prover::programs::prelude::*;
use std::fmt::Display;
use user_host_trait::UserHost;

// allows introspection into user modules
#[link(wasm_import_module = "hostio")]
extern "C" {
    fn program_memory_size(module: u32) -> u32;
}

/// Signifies an out-of-bounds memory access was requested.
pub(crate) struct MemoryBoundsError;

impl From<MemoryBoundsError> for eyre::ErrReport {
    fn from(_: MemoryBoundsError) -> Self {
        eyre!("memory access out of bounds")
    }
}

/// The list of active programs. The current program is always the last.
///
/// Note that this data-structure may re-alloc while references to [`Program`] are held.
/// This is sound due to [`Box`] providing a level of indirection.
///
/// Normal Rust rules would suggest using a [`Vec`] of cells would be better. The issue is that,
/// should an error guard recover, this WASM will reset to an earlier state but with the current
/// memory. This means that stack unwinding won't happen, rendering these primitives unhelpful.
#[allow(clippy::vec_box)]
static mut PROGRAMS: Vec<Box<Program>> = vec![];

static mut LAST_REQUEST_ID: u32 = 0x10000;

#[derive(Clone)]
pub (crate) struct UserHostRequester {
    data: Option<Vec<u8>>,
    answer: Option<Vec<u8>>,
    req_type: u32,
    id: u32,
    gas: u64,
}

impl UserHostRequester {
    pub fn default() -> Self {
        Self {
            req_type: 0,
            data: None,
            answer: None,
            id: 0,
            gas: 0,
        }
    }
}

/// An active user program.
pub(crate) struct Program {
    /// Arguments passed via the VM.
    pub args: Vec<u8>,
    /// Output generated by the program.
    pub outs: Vec<u8>,
    /// Mechanism for calling back into Geth.
    pub evm_api: JsEvmApi<UserHostRequester>,
    /// EVM Context info.
    pub evm_data: EvmData,
    /// WAVM module index.
    pub module: u32,
    /// Call configuration.
    pub config: StylusConfig,
}

#[link(wasm_import_module = "hostio")]
extern "C" {
    fn program_request(status: u32) -> u32;
}

impl UserHostRequester {
    #[no_mangle]
    pub unsafe fn set_response(&mut self, req_id: u32, data: Vec<u8>, gas: u64) {
        self.answer = Some(data);
        self.gas = gas;
        if req_id != self.id {
            panic!("bad req id returning from send_request")
        }
        compiler_fence(Ordering::SeqCst);
    }

    pub unsafe fn set_request(&mut self, req_type: u32, data: &[u8]) -> u32 {
        LAST_REQUEST_ID += 1;
        self.id = LAST_REQUEST_ID;
        self.req_type = req_type;
        self.data = Some(data.to_vec());
        self.answer = None;
        self.id
    }

    pub unsafe fn get_request(&self, id: u32) -> (u32, Vec<u8>) {
        if self.id != id {
            panic!("get_request got wrong id");
        }
        (self.req_type, self.data.as_ref().unwrap().clone())
    }

    #[no_mangle]
    unsafe fn send_request(&mut self, req_type: u32, data: Vec<u8>) -> (Vec<u8>, u64) {
        let req_id = self.set_request(req_type, &data);
        compiler_fence(Ordering::SeqCst);
        let got_id = program_request(req_id);
        compiler_fence(Ordering::SeqCst);
        if got_id != req_id {
            panic!("bad req id returning from send_request")
        }
        (self.answer.take().unwrap(), self.gas)
    }
}

impl RequestHandler for UserHostRequester {
    fn handle_request(&mut self, req_type: EvmApiMethod, req_data: &[u8]) -> (Vec<u8>, u64) {
        unsafe {
            self.send_request(req_type as u32 + 0x10000000, req_data.to_vec())
        }
    }
}

impl Program {
    /// Adds a new program, making it current.
    pub fn push_new(
        args: Vec<u8>,
        evm_data: EvmData,
        module: u32,
        config: StylusConfig,
    ) {
        let program = Self {
            args,
            outs: vec![],
            evm_api: JsEvmApi::new(UserHostRequester::default()),
            evm_data,
            module,
            config,
        };
        unsafe { PROGRAMS.push(Box::new(program)) }
    }

    /// Removes the current program
    pub fn pop() {
        unsafe { PROGRAMS.pop().expect("no program"); }
    }

    /// Provides a reference to the current program.
    pub fn current() -> &'static mut Self {
        unsafe { PROGRAMS.last_mut().expect("no program") }
    }

    /// Reads the program's memory size in pages
    fn memory_size(&self) -> u32 {
        unsafe { program_memory_size(self.module) }
    }

    /// Ensures an access is within bounds
    fn check_memory_access(&self, ptr: u32, bytes: u32) -> Result<(), MemoryBoundsError> {
        let last_page = ptr.saturating_add(bytes) / wavm::PAGE_SIZE;
        if last_page > self.memory_size() {
            return Err(MemoryBoundsError);
        }
        Ok(())
    }
}

#[allow(clippy::unit_arg)]
impl UserHost for Program {
    type Err = eyre::ErrReport;
    type MemoryErr = MemoryBoundsError;
    type A = JsEvmApi<UserHostRequester>;

    fn args(&self) -> &[u8] {
        &self.args
    }

    fn outs(&mut self) -> &mut Vec<u8> {
        &mut self.outs
    }

    fn evm_api(&mut self) -> &mut Self::A {
        &mut self.evm_api
    }

    fn evm_data(&self) -> &EvmData {
        &self.evm_data
    }

    fn evm_return_data_len(&mut self) -> &mut u32 {
        &mut self.evm_data.return_data_len
    }

    fn read_bytes20(&self, ptr: u32) -> Result<Bytes20, MemoryBoundsError> {
        self.check_memory_access(ptr, 20)?;
        unsafe { Ok(wavm::read_bytes20(ptr)) }
    }

    fn read_bytes32(&self, ptr: u32) -> Result<Bytes32, MemoryBoundsError> {
        self.check_memory_access(ptr, 32)?;
        unsafe { Ok(wavm::read_bytes32(ptr)) }
    }

    fn read_slice(&self, ptr: u32, len: u32) -> Result<Vec<u8>, MemoryBoundsError> {
        self.check_memory_access(ptr, len)?;
        unsafe { Ok(wavm::read_slice_u32(ptr, len)) }
    }

    fn write_u32(&mut self, ptr: u32, x: u32) -> Result<(), MemoryBoundsError> {
        self.check_memory_access(ptr, 4)?;
        unsafe { Ok(wavm::caller_store32(ptr as usize, x)) }
    }

    fn write_bytes20(&self, ptr: u32, src: Bytes20) -> Result<(), MemoryBoundsError> {
        self.check_memory_access(ptr, 20)?;
        unsafe { Ok(wavm::write_bytes20(ptr, src)) }
    }

    fn write_bytes32(&self, ptr: u32, src: Bytes32) -> Result<(), MemoryBoundsError> {
        self.check_memory_access(ptr, 32)?;
        unsafe { Ok(wavm::write_bytes32(ptr, src)) }
    }

    fn write_slice(&self, ptr: u32, src: &[u8]) -> Result<(), MemoryBoundsError> {
        self.check_memory_access(ptr, src.len() as u32)?;
        unsafe { Ok(wavm::write_slice_u32(src, ptr)) }
    }

    fn say<D: Display>(&self, text: D) {
        println!("{} {text}", "Stylus says:".yellow());
    }

    fn trace(&self, name: &str, args: &[u8], outs: &[u8], _end_ink: u64) {
        let args = hex::encode(args);
        let outs = hex::encode(outs);
        println!("Error: unexpected hostio tracing info for {name} while proving: {args}, {outs}");
    }
}
