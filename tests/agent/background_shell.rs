
use super::*;

#[test]
fn mint_shell_id_is_bg_n() {
    assert_eq!(mint_shell_id(1), "bg_1");
    assert_eq!(mint_shell_id(7), "bg_7");
}
