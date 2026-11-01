use uda_ffi::{status_message, UdaStatus, UDA_OK};

#[test]
fn status_alias_remains_available_to_rust_callers() {
    let status: UdaStatus = UDA_OK;
    let describe: fn(UdaStatus) -> &'static str = status_message;
    let _: i32 = status;
    assert_eq!(describe(status), "success");
}
