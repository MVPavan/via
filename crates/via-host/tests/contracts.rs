//! Host launch parameters do not expose child environment values in diagnostics.

use std::ffi::OsString;

use via_host::EnvAllowList;

#[test]
fn explicit_environment_rejects_invalid_names_and_redacts_values() {
    assert!(
        EnvAllowList::try_from_entries(vec![(OsString::from("A=B"), OsString::from("x"))]).is_err()
    );
    let env = EnvAllowList::try_from_entries(vec![(
        OsString::from("TOKEN"),
        OsString::from("sensitive-value"),
    )])
    .unwrap();
    assert_eq!(env.entries().len(), 1);
    assert!(!format!("{env:?}").contains("sensitive-value"));
}
