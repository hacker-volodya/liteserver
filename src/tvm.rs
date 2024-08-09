use std::ffi::{c_void, CStr, CString};

use base64::Engine as _;
use ton_liteapi::tl::common::AccountId;
use tonlib_sys::*;

#[derive(Default)]
pub struct EmulatorBuilder {
    code: CString,
    data: CString,
    libs: Option<CString>,
    c7: Option<(CString, u32, u64, CString, CString)>,
    gas_limit: Option<u64>,
    debug: Option<bool>,
}

impl EmulatorBuilder {
    pub fn new(code: &[u8], data: &[u8]) -> Self {
        Self {
            code: b64(code),
            data: b64(data),
           ..Default::default()
        }
    }

    pub fn with_libs(mut self, libs: &[u8]) -> Self {
        self.libs = Some(b64(libs));
        self
    }

    pub fn with_c7(mut self, address: &AccountId, unixtime: u32, balance: u64, rand_seed: &[u8], config: &[u8]) -> Self {
        let address = CString::new(address.workchain.to_string() + ":" + &address.id.to_hex()).unwrap(); // does not contain nul
        self.c7 = Some((address, unixtime, balance, hex(rand_seed), b64(config)));
        self
    }

    pub fn with_gas_limit(mut self, gas_limit: u64) -> Self {
        self.gas_limit = Some(gas_limit);
        self
    }

    pub fn with_debug(mut self, debug: bool) -> Self {
        self.debug = Some(debug);
        self
    }

    fn build(self) -> *mut c_void {
        let emulator = unsafe { tvm_emulator_create(self.code.as_ptr(), self.data.as_ptr(), 10) };
        unsafe {
            if let Some(libs) = self.libs {
                tracing::info!(?libs, "set libraries");
                tvm_emulator_set_libraries(emulator, libs.as_ptr());
            }
            if let Some((address, unixtime, balance, rand_seed, config)) = self.c7 {
                tracing::info!(?address, ?unixtime, ?balance, ?rand_seed, ?config, "set c7");
                //tvm_emulator_set_c7(emulator, address.as_ptr(), unixtime, balance, rand_seed.as_ptr(), config.as_ptr());
            }
            if let Some(gas_limit) = self.gas_limit {
                tracing::info!(?gas_limit, "set gas limit");
                tvm_emulator_set_gas_limit(emulator, gas_limit);
            }
            if let Some(debug) = self.debug {
                tracing::info!("set debug");
                tvm_emulator_set_debug_enabled(emulator, debug as i32);
            }
        }
        emulator
    }

    pub fn run_external(self, message: &[u8]) -> TvmEmulatorSendExternalMessageResult {
        let emulator = self.build();
        let message = b64(message);
        let result = unsafe {
            let result = tvm_emulator_send_external_message(emulator, message.as_ptr());
            CStr::from_ptr(result).to_str().unwrap().to_string()
        };
        unsafe { tvm_emulator_destroy(emulator); }
        serde_json::from_str(&result).unwrap()
    }
    
    pub fn run_get_method(self, method_id: i32, stack_boc: &[u8]) -> TvmEmulatorRunGetMethodResult {
        let emulator = self.build();
        let stack_boc = b64(stack_boc);
        tracing::info!(?stack_boc, ?method_id, "run get method");
        let result = unsafe {
            let result = tvm_emulator_run_get_method(emulator, method_id, stack_boc.as_ptr());
            CStr::from_ptr(result).to_str().unwrap().to_string()
        };
        tracing::info!(?result, "run get method result");
        unsafe { tvm_emulator_destroy(emulator); }
        serde_json::from_str(&result).unwrap()
    }
}

fn b64(input: &[u8]) -> CString {
    let output = base64::engine::general_purpose::STANDARD.encode(input);
    // output is not containing nul byte, because it is base64-encoded
    CString::new(output).unwrap()
}

fn hex(input: &[u8]) -> CString {
    let output = hex::encode(input);
    // output is not containing nul byte, because it is hex-encoded
    CString::new(output).unwrap()
}

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
pub enum TvmEmulatorRunGetMethodResult {
    Error(TvmEmulatorError),
    Success(TvmEmulatorRunGetMethodSuccess),
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
pub enum TvmEmulatorSendExternalMessageResult {
    Error(TvmEmulatorError),
    Success(TvmEmulatorSendExternalMessageSuccess),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct TvmEmulatorError {
    pub success: IsSuccess<false>,
    pub error: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct TvmEmulatorRunGetMethodSuccess {
    pub success: IsSuccess<true>,
    pub vm_log: String,
    pub vm_exit_code: i32,
    pub stack: String,
    pub missing_library: Option<String>,
    pub gas_used: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct TvmEmulatorSendExternalMessageSuccess {
    pub success: IsSuccess<true>,
    pub new_code: String,
    pub new_data: String,
    pub accepted: bool,
    pub vm_exit_code: i32,
    pub vm_log: String,
    pub missing_library: Option<String>,
    pub gas_used: u64,
    pub actions: String,
}

#[derive(Debug)]
pub struct IsSuccess<const V: bool>;

impl<const V: bool> Serialize for IsSuccess<V> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bool(V)
    }
}

impl<'de, const V: bool> Deserialize<'de> for IsSuccess<V> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = bool::deserialize(deserializer)?;
        if value == V {
            Ok(IsSuccess::<V>)
        } else {
            Err(serde::de::Error::custom("Expected another success value"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jsons() {
        let x: TvmEmulatorRunGetMethodResult = serde_json::from_str(r#"
{
    "success": true,
    "vm_log": "...",
    "vm_exit_code": 0,
    "stack": "Base64 encoded BoC serialized stack (VmStack)",
    "missing_library": null,
    "gas_used": 1212
}
"#).unwrap();
        println!("{:?}", x);
        let x: TvmEmulatorRunGetMethodResult = serde_json::from_str(r#"
        {
  "success": false,
  "error": "Error description"
}"#).unwrap();
        println!("{:?}", x);
        let x: TvmEmulatorSendExternalMessageResult = serde_json::from_str(r#"
{
  "success": true,
  "new_code": "Base64 boc decoded new code cell",
  "new_data": "Base64 boc decoded new data cell",
  "accepted": true,
  "vm_exit_code": 0,
  "vm_log": "...",
  "missing_library": null,
  "gas_used": 1212,
  "actions": "Base64 boc decoded actions cell of type (OutList n)"
}
"#).unwrap();
        println!("{:?}", x);
    }

    #[test]
    fn test_run_get_method() {
        let result = EmulatorBuilder::new(
            &hex::decode("b5ee9c72010214010002d4000114ff00f4a413f4bcf2c80b010201200203020148040504f8f28308d71820d31fd31fd31f02f823bbf264ed44d0d31fd31fd3fff404d15143baf2a15151baf2a205f901541064f910f2a3f80024a4c8cb1f5240cb1f5230cbff5210f400c9ed54f80f01d30721c0009f6c519320d74a96d307d402fb00e830e021c001e30021c002e30001c0039130e30d03a4c8cb1f12cb1fcbff1011121302e6d001d0d3032171b0925f04e022d749c120925f04e002d31f218210706c7567bd22821064737472bdb0925f05e003fa403020fa4401c8ca07cbffc9d0ed44d0810140d721f404305c810108f40a6fa131b3925f07e005d33fc8258210706c7567ba923830e30d03821064737472ba925f06e30d06070201200809007801fa00f40430f8276f2230500aa121bef2e0508210706c7567831eb17080185004cb0526cf1658fa0219f400cb6917cb1f5260cb3f20c98040fb0006008a5004810108f45930ed44d0810140d720c801cf16f400c9ed540172b08e23821064737472831eb17080185005cb055003cf1623fa0213cb6acb1fcb3fc98040fb00925f03e20201200a0b0059bd242b6f6a2684080a06b90fa0218470d4080847a4937d29910ce6903e9ff9837812801b7810148987159f31840201580c0d0011b8c97ed44d0d70b1f8003db29dfb513420405035c87d010c00b23281f2fff274006040423d029be84c600201200e0f0019adce76a26840206b90eb85ffc00019af1df6a26840106b90eb858fc0006ed207fa00d4d422f90005c8ca0715cbffc9d077748018c8cb05cb0222cf165005fa0214cb6b12ccccc973fb00c84014810108f451f2a7020070810108d718fa00d33fc8542047810108f451f2a782106e6f746570748018c8cb05cb025006cf165004fa0214cb6a12cb1fcb3fc973fb0002006c810108d718fa00d33f305224810108f459f2a782106473747270748018c8cb05cb025005cf165003fa0213cb6acb1f12cb3fc973fb00000af400c9ed54").unwrap(), 
            &hex::decode("b5ee9c7201010101002b0000510000009629a9a31729a335f44b54ab056e34d0de7bcad14abc71400a24e1f785217ec138d2dffbda40").unwrap()
        ).run_get_method(85143, &hex::decode("b5ee9c7201010101002b0000510000009629a9a31729a335f44b54ab056e34d0de7bcad14abc71400a24e1f785217ec138d2dffbda40").unwrap());
        println!("{:?}", result);
    }
}