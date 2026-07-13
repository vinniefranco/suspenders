//! shopcart: a small shopping-cart library.
//!
//! All money amounts in this crate are integer **cents** (`u64`); there are
//! no floating-point dollars anywhere in the API.

pub mod cart;
pub mod checkout;
pub mod item;
